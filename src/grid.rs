//! Frame grids that MP3, AAC and Opus encoders leave in decoded audio.
//!
//! An encoder transforms its input in frames on a fixed grid and quantizes what the transform
//! gives. The decoder's synthesis inverts the encoder's analysis, so running that analysis again
//! on the decoded samples, with frames on the encoder's grid, gives the quantized values back.
//! With frames at any other offset they are mixed with the neighbouring frames'. Lossless audio
//! has no such offset.
//!
//! The scan reads four stretches of 40960 samples, at 20, 40, 60 and 80% of the file.
//!
//! # MP3 and AAC
//!
//! Both zero many transform values, and on the grid every zeroed value comes out at the level
//! of the samples' rounding noise. For each offset of a codec's grid, the scan counts the
//! transform values that stand above the rounding noise of 16-bit samples, on a log scale, in
//! left and right or in mid and side, whichever pair counts fewer in a frame. The offset with
//! the lowest count, and how far that count sits below the others, is the codec's
//! [`CodecScan`]. The AAC analysis is the long window of ffmpeg's AAC encoder: a 2048-point
//! MDCT at a hop of 1024 with the Kaiser-Bessel-derived window. The MP3 analysis is the layer
//! III hybrid filterbank: the 512-tap polyphase filterbank, an 18-line MDCT per subband and the
//! alias-reduction butterflies, at a hop of 576. Short blocks are not analyzed.
//!
//! # Opus
//!
//! Opus's CELT layer codes the energy of each of its 21 bands, in each frame, as a step from a
//! prediction: half the band's energy a frame earlier plus a sum over the frame's lower bands.
//! The step is a whole number of 6 dB refined by a few bits. On the grid, each band's energy
//! comes back, and taking the prediction off it leaves a number on that lattice. A frame counts
//! when at least 80% of the bands that stand over the samples' rounding noise, in each channel,
//! are within 0.003 of a 32nd of 6 dB. The offset that counts the most frames is the
//! [`OpusScan`].
//!
//! The analysis is of 20 ms frames at 48 kHz, the rate Opus runs at: a 1920-point MDCT at a
//! hop of 960 with CELT's low-overlap window, after the encoder's pre-emphasis. Samples at
//! 44.1 kHz are brought back to 48 kHz first; other rates are not scanned. Only a frame coded
//! as one long block with the pitch post-filter off, after another such frame, counts. The
//! windows the file is measured at are read too, on the one grid a decode has when nothing was
//! cut from its start.
//!
//! # Limits
//!
//! A grid is evidence that the samples were decoded from that codec, at some point.
//!
//! - Resampling after decoding removes an MP3 or AAC grid. Opus's energies are still found
//!   after a conversion from 48 to 44.1 kHz.
//! - Clipping hides a grid frame by frame.
//! - Vorbis's grid shows through the AAC scan, so a Vorbis decode reads as AAC.
//! - Opus frames of 2.5, 5 and 10 ms, and its SILK and hybrid modes, are not found.
//! - A gain change after an Opus decode moves the energies off the lattice.

use std::fmt;
use std::sync::Arc;

use crate::lattice;
use crate::thresholds::ReadingThresholds;
use rayon::prelude::*;
use rustfft::num_complex::Complex;
use rustfft::num_traits::Float;
use rustfft::{Fft, FftPlanner};

/// AAC's hop, the length of its long frames' new samples
const AAC_HOP: usize = 1024;
/// MP3's hop, a granule
const MP3_HOP: usize = 576;
/// the number of segments scanned
pub(crate) const SEGMENTS: usize = 4;
/// the samples of one segment: 40 AAC hops
pub(crate) const SEGMENT: usize = 40 * AAC_HOP;
/// where the segments start, as fractions of the file's length
const SEGMENT_AT: [f64; SEGMENTS] = [0.2, 0.4, 0.6, 0.8];
/// the rounding noise of 16-bit samples in samples scaled to [-1, 1): 2^-15 / sqrt(12)
const NOISE16: f64 = 8.809_710_218_959_115e-6;
/// the rms of a hybrid filterbank value per unit rms of white input, which puts the MP3 noise
/// level at the 16-bit rounding noise of the filterbank's output
const MP3_NOISE_GAIN: f64 = 0.527_861_394_696_74;
/// MAD times this estimates the standard deviation of normally distributed values
const MAD_TO_SD: f64 = 1.4826;

/// Codec whose frame grid the scan looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Codec {
  /// AAC, read with the long blocks of ffmpeg's encoder.
  ///
  /// Vorbis decodes read as AAC too.
  Aac,
  /// MP3, layer III.
  Mp3,
  /// Opus, by the band energies of its CELT layer in 20 ms frames.
  Opus,
}

impl fmt::Display for Codec {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Aac => "AAC",
      Self::Mp3 => "MP3",
      Self::Opus => "Opus",
    })
  }
}

/// How far one codec's grid stands out in a file.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct CodecScan {
  /// Offset of the frames that count the fewest values, in samples.
  ///
  /// It is where frames start, modulo the codec's hop, counted from the file's first sample.
  pub offset: usize,
  /// How far the count at `offset` sits below the others, in robust standard deviations.
  ///
  /// The counts of the four segments are summed per offset; the standard deviation is
  /// estimated from the median absolute deviation.
  pub z: f64,
  /// Number of segments whose own lowest count is at `offset`.
  pub segments_agreeing: usize,
  /// Each segment's own z, in file order.
  pub segment_z: [f64; SEGMENTS],
}

/// Offset of an Opus decode's frames when nothing was cut from its start, in samples.
///
/// It is where frames start at 48 kHz, modulo the 960 samples of a 20 ms frame, counted from
/// the file's first sample.
pub const OPUS_UNCUT_OFFSET: usize = lattice::UNCUT_OFFSET;

/// How many frames of a file carry Opus's band energies.
///
/// A frame counts when the energies of its bands, taken with the analysis of Opus's CELT layer,
/// sit on the lattice of the codec's energy quantizer. Samples at 44.1 kHz are brought back to
/// the 48 kHz Opus runs at first.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpusScan {
  /// Offset of the frame grid that counts the most frames in the four segments, in samples.
  ///
  /// It is where frames start at 48 kHz, modulo the 960 samples of a 20 ms frame, counted
  /// from the file's first sample. Where no offset counts a frame it is the uncut decode's.
  pub offset: usize,
  /// Number of frames counted at `offset`, over the four segments.
  pub frames: usize,
  /// Each segment's count at `offset`, in file order.
  pub segment_frames: [usize; SEGMENTS],
  /// Number of frames counted in the windows measured, on the grid of an uncut decode.
  ///
  /// The grid is the one a decode has when nothing was cut from its start: libopus's encoders
  /// all delay their input by the same 312 samples. Up to 60 windows are read.
  pub uncut_frames: usize,
}

impl OpusScan {
  /// Returns the offset and the number of frames of the larger of the two counts.
  ///
  /// The segments are counted at [`OpusScan::offset`], the windows at [`OPUS_UNCUT_OFFSET`].
  pub fn most(&self) -> (usize, usize) {
    if self.uncut_frames > self.frames {
      (OPUS_UNCUT_OFFSET, self.uncut_frames)
    } else {
      (self.offset, self.frames)
    }
  }
}

/// Scan of a file for the frame grids of MP3, AAC and Opus.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Scan {
  /// AAC's grid.
  pub aac: CodecScan,
  /// MP3's grid.
  pub mp3: CodecScan,
  /// Opus's frames, `None` at a sample rate other than 44.1 and 48 kHz.
  #[cfg_attr(feature = "serde", serde(default))]
  pub opus: Option<OpusScan>,
}

/// What shows a codec's frame grid.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Evidence {
  /// Offset whose transform values count far fewer than the others, for MP3 and AAC.
  Count(CodecScan),
  /// Frames whose band energies sit on the quantizer's lattice, for Opus.
  Lattice(OpusScan),
}

impl Evidence {
  /// Returns the offset of the frame grid, in samples.
  ///
  /// For Opus it is the offset of the larger count, as [`OpusScan::most`] gives it.
  pub fn offset(&self) -> usize {
    match self {
      Self::Count(scan) => scan.offset,
      Self::Lattice(scan) => scan.most().0,
    }
  }
}

/// Frame grid a reading finds.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Found {
  /// Codec whose grid it is.
  pub codec: Codec,
  /// What shows the grid.
  #[cfg_attr(feature = "serde", serde(flatten))]
  pub evidence: Evidence,
}

/// the grid `scan` shows under `thresholds`: MP3 or AAC over its z threshold with as many
/// segments agreeing as its own threshold asks, the one furthest over its z threshold when both
/// are. without either, Opus with enough frames on its lattice in the segments or in the
/// windows: the codec decoded last leaves the grid that counts, and Opus's energies can show
/// through an MP3 made from its decode
pub(crate) fn found(scan: &Scan, thresholds: &ReadingThresholds) -> Option<Found> {
  let counted = [
    (
      Codec::Aac,
      &scan.aac,
      thresholds.aac_grid_z,
      thresholds.aac_grid_segments_agreeing,
    ),
    (
      Codec::Mp3,
      &scan.mp3,
      thresholds.mp3_grid_z,
      thresholds.mp3_grid_segments_agreeing,
    ),
  ]
  .into_iter()
  .filter(|(_, scan, z, agreeing)| scan.z > *z && scan.segments_agreeing >= *agreeing)
  .max_by(|(_, a, za, _), (_, b, zb, _)| (a.z / za).total_cmp(&(b.z / zb)))
  .map(|(codec, scan, _, _)| Found {
    codec,
    evidence: Evidence::Count(scan.clone()),
  });
  counted.or_else(|| {
    let opus = scan.opus.as_ref()?;
    (opus.frames >= thresholds.opus_grid_frames
      || opus.uncut_frames >= thresholds.opus_uncut_frames)
      .then(|| Found {
        codec: Codec::Opus,
        evidence: Evidence::Lattice(opus.clone()),
      })
  })
}

/// one stretch of the file the grid is scanned in, filled as its samples are decoded
struct Segment {
  /// the sample frame it starts at
  start: u64,
  left: Vec<f64>,
  right: Vec<f64>,
}

impl Segment {
  /// the sample frame the segment needs next
  fn next(&self) -> u64 {
    self.start + self.left.len() as u64
  }

  fn full(&self) -> bool {
    self.left.len() == SEGMENT
  }

  /// how many of `available` frames from `first` on the segment takes, and from which
  fn wanted(&self, first: u64, available: usize) -> Option<(usize, usize)> {
    let next = self.next();
    let from = usize::try_from(next.checked_sub(first)?).ok()?;
    let count = (SEGMENT - self.left.len()).min(available.checked_sub(from)?);
    (count > 0).then_some((from, count))
  }
}

/// the segments of a file the grid is scanned in
pub(crate) struct Segments(Vec<Segment>);

impl Segments {
  /// the segments of a file of `frames` sample frames; `None` when it is shorter than one
  pub(crate) fn of(frames: u64) -> Option<Self> {
    let length = SEGMENT as u64;
    if frames < length {
      return None;
    }
    let segments = SEGMENT_AT
      .iter()
      .map(|at| Segment {
        // a length below 2^53 is exact as an f64; the start is the fraction floored
        start: ((frames as f64 * at) as u64).min(frames - length),
        left: Vec::with_capacity(SEGMENT),
        right: Vec::with_capacity(SEGMENT),
      })
      .collect();
    Some(Self(segments))
  }

  /// takes what the segments need of `window`, one vector per analyzed channel, whose first
  /// sample is frame `first`. a mono window counts as both channels.
  pub(crate) fn take(&mut self, first: u64, window: &[Vec<f32>]) {
    let [left, right] = match window {
      [mono] => [mono, mono],
      [left, right, ..] => [left, right],
      [] => return,
    };
    for segment in &mut self.0 {
      if let Some((from, count)) = segment.wanted(first, left.len()) {
        let range = from..from + count;
        segment
          .left
          .extend(left[range.clone()].iter().map(|s| f64::from(*s)));
        segment
          .right
          .extend(right[range].iter().map(|s| f64::from(*s)));
      }
    }
  }

  /// takes what the segments need of `samples`, interleaved over `channels` channels, each at
  /// the top of its 32 bits, whose first sample is frame `first`
  pub(crate) fn take_interleaved(&mut self, first: u64, samples: &[i32], channels: usize) {
    let frames = samples.len() / channels;
    let scale = 1.0 / 2_147_483_648.0;
    for segment in &mut self.0 {
      if let Some((from, count)) = segment.wanted(first, frames) {
        for frame in samples.chunks_exact(channels).skip(from).take(count) {
          let right = frame.get(1).unwrap_or(&frame[0]);
          segment.left.push(f64::from(frame[0]) * scale);
          segment.right.push(f64::from(*right) * scale);
        }
      }
    }
  }

  /// the indices of the segments not yet full
  pub(crate) fn missing(&self) -> Vec<usize> {
    (0..self.0.len()).filter(|i| !self.0[*i].full()).collect()
  }

  /// the sample frame segment `index` needs next; `None` once it is full
  pub(crate) fn needs(&self, index: usize) -> Option<u64> {
    let segment = &self.0[index];
    (!segment.full()).then(|| segment.next())
  }

  /// whether segment `index` has no samples yet
  pub(crate) fn untouched(&self, index: usize) -> bool {
    self.0[index].left.is_empty()
  }

  /// whether every segment is full
  pub(crate) fn complete(&self) -> bool {
    self.0.iter().all(Segment::full)
  }
}

/// what the scan for Opus's frames needs beside the segments
#[derive(Clone, Copy)]
pub(crate) struct Opus {
  /// the file's sample rate
  pub(crate) rate: lattice::Rate,
  /// the bits of its samples, whose rounding noise a band must stand over
  pub(crate) bits: u32,
  /// the frames counted in the windows measured, on the grid of an uncut decode
  pub(crate) uncut_frames: usize,
}

/// the jobs of one scan: MP3 and AAC in each segment, and Opus where `opus`
pub(crate) fn jobs(opus: bool) -> usize {
  (2 + usize::from(opus)) * SEGMENTS
}
/// the tasks a job's offsets are split into at most, so that one file's scan can fill the pool
const TASKS: usize = 32;

/// the scan of full `segments`, each codec of each segment a job on rayon's thread pool that
/// splits its offsets into tasks on the same pool; Opus is among the codecs with `opus`. `go_on`
/// is asked before each job, `None` once it says not to, and `ended` is called as each job ends.
/// panics when a segment is not full.
pub(crate) fn scan(
  segments: &Segments,
  opus: Option<Opus>,
  go_on: impl Fn() -> bool + Sync,
  ended: impl Fn() + Sync,
) -> Option<Scan> {
  assert!(segments.complete(), "every segment is full before a scan");
  let kinds = [Some(Job::Aac), Some(Job::Mp3), opus.map(Job::Opus)];
  let jobs: Vec<(Job, &Segment)> = segments
    .0
    .iter()
    .flat_map(|segment| kinds.into_iter().flatten().map(move |job| (job, segment)))
    .collect();
  let curves: Vec<Curve> = jobs
    .into_par_iter()
    .map(|(job, segment)| {
      go_on().then(|| {
        let curve = match job {
          Job::Aac => Curve::Aac(in_file(&Aac::curve(segment), segment.start)),
          Job::Mp3 => Curve::Mp3(in_file(&Mp3::new().curve(segment), segment.start)),
          // the counts come in the file's offsets
          Job::Opus(opus) => {
            let channels = [&segment.left[..], &segment.right[..]];
            Curve::Opus(lattice::curve(
              &channels,
              segment.start,
              opus.rate,
              opus.bits,
            ))
          }
        };
        ended();
        curve
      })
    })
    .collect::<Option<_>>()?;
  let (mut aac, mut mp3, mut frames) = (Vec::new(), Vec::new(), Vec::new());
  for curve in curves {
    match curve {
      Curve::Aac(curve) => aac.push(curve),
      Curve::Mp3(curve) => mp3.push(curve),
      Curve::Opus(curve) => frames.push(curve),
    }
  }
  Some(Scan {
    aac: codec_scan(&aac),
    mp3: codec_scan(&mp3),
    opus: opus.map(|opus| opus_scan(&frames, opus.uncut_frames)),
  })
}

/// what one job of a scan looks for in its segment
#[derive(Clone, Copy)]
enum Job {
  Aac,
  Mp3,
  Opus(Opus),
}

/// what a job found at each offset of the file's grid: MP3's and AAC's counts of transform
/// values on a log scale, Opus's counts of frames
enum Curve {
  Aac(Vec<f64>),
  Mp3(Vec<f64>),
  Opus(Vec<usize>),
}

/// the scan for Opus from its segments' counts in the file's offsets: the first offset with the
/// most frames over the segments, the uncut decode's when none has any
fn opus_scan(curves: &[Vec<usize>], uncut_frames: usize) -> OpusScan {
  let (offset, frames) = (0..lattice::FRAME)
    .map(|o| curves.iter().map(|c| c[o]).sum::<usize>())
    .enumerate()
    .fold((OPUS_UNCUT_OFFSET, 0), |(at, most), (o, count)| {
      if count > most { (o, count) } else { (at, most) }
    });
  OpusScan {
    offset,
    frames,
    segment_frames: std::array::from_fn(|i| curves[i][offset]),
    uncut_frames,
  }
}

/// a segment's curve, whose entry `o` is for frames starting `o` samples into the segment
/// modulo the hop, turned into the file's offsets for a segment starting at frame `start`
fn in_file(curve: &[f64], start: u64) -> Vec<f64> {
  let hop = curve.len();
  // below the hop, so it fits
  let shift = (start % hop as u64) as usize;
  (0..hop).map(|o| curve[(o + hop - shift) % hop]).collect()
}

/// the scan of one codec from its segments' curves in the file's offsets
fn codec_scan(curves: &[Vec<f64>]) -> CodecScan {
  let hop = curves[0].len();
  let summed: Vec<f64> = (0..hop)
    .map(|o| curves.iter().map(|c| c[o]).sum())
    .collect();
  let (z, offset) = robust_z(&summed);
  CodecScan {
    offset,
    z,
    segments_agreeing: curves.iter().filter(|c| robust_z(c).1 == offset).count(),
    segment_z: std::array::from_fn(|i| robust_z(&curves[i]).0),
  }
}

/// how far the lowest entry of `curve` sits below the median in robust standard deviations,
/// and its index: the first lowest
fn robust_z(curve: &[f64]) -> (f64, usize) {
  let middle = median(curve.to_vec());
  let mad = MAD_TO_SD * median(curve.iter().map(|c| (c - middle).abs()).collect());
  let (lowest, at) = curve
    .iter()
    .enumerate()
    .fold((f64::INFINITY, 0), |(low, at), (i, c)| {
      if *c < low { (*c, i) } else { (low, at) }
    });
  let z = if mad > 0.0 {
    (middle - lowest) / mad
  } else {
    0.0
  };
  (z, at)
}

/// the median of `values`, the mean of the middle two for an even count
fn median(mut values: Vec<f64>) -> f64 {
  values.sort_by(f64::total_cmp);
  let half = values.len() / 2;
  if values.len().is_multiple_of(2) {
    (values[half - 1] + values[half]) / 2.0
  } else {
    values[half]
  }
}

/// the bits of an f32 exponent and the exponent that puts a value in [1, 2)
const EXPONENT_BITS: u32 = 0x7f80_0000;
const ONE: u32 = 0x3f80_0000;

/// the products [`active`] keeps side by side, so they can be multiplied in parallel
const LANES: usize = 16;

/// the active count of `values`: the sum of log10(max(v^2 / t2, 1)), counted in single
/// precision.
///
/// the log of a product is the sum of the logs, so the values' exponents are summed and their
/// mantissas, each in [1, 2), multiplied in [`LANES`] products of up to 32 each, which stay
/// under 2^32, with one log per product.
fn active(values: &[f32], t2: f32) -> f64 {
  let mut exponents = [0i32; LANES];
  let mut products = [1.0f32; LANES];
  let mut logs = 0.0;
  for (i, group) in values.chunks(LANES).enumerate() {
    for ((product, exponent), v) in products.iter_mut().zip(&mut exponents).zip(group) {
      // t2 is a normal number, so every square at or above it is one too
      let bits = (v * v).max(t2).to_bits();
      *exponent += ((bits & EXPONENT_BITS) >> 23) as i32 - 127;
      *product *= f32::from_bits((bits & !EXPONENT_BITS) | ONE);
    }
    if i % 32 == 31 {
      logs += products.iter().map(|p| f64::from(*p).log2()).sum::<f64>();
      products = [1.0; LANES];
    }
  }
  logs += products.iter().map(|p| f64::from(*p).log2()).sum::<f64>();
  let exponents: i32 = exponents.iter().sum();
  (f64::from(exponents) + logs - values.len() as f64 * f64::from(t2).log2())
    * std::f64::consts::LOG10_2
}

/// the AAC analysis: the KBD long window and an orthonormal MDCT, transformed in single
/// precision
struct Aac {
  window: Vec<f32>,
  dct: DctIv,
  frame: Vec<f32>,
  folded: Vec<f32>,
  /// the transform of left and right
  values: [Vec<f32>; 2],
  /// mid and side of the transform
  mid_side: [Vec<f32>; 2],
}

impl Aac {
  fn new() -> Self {
    Self {
      window: kbd_window(2 * AAC_HOP, 4.0)
        .into_iter()
        .map(|w| w as f32)
        .collect(),
      dct: DctIv::new(AAC_HOP),
      frame: vec![0.0; 2 * AAC_HOP],
      folded: vec![0.0; AAC_HOP],
      values: std::array::from_fn(|_| vec![0.0; AAC_HOP]),
      mid_side: std::array::from_fn(|_| vec![0.0; AAC_HOP]),
    }
  }

  /// the active count summed over the long frames of `segment`, for each offset of the grid
  /// from its first sample. every offset counts the same frames. runs of offsets are tasks on
  /// rayon's thread pool, each with an analysis of its own.
  fn curve(segment: &Segment) -> Vec<f64> {
    let left: Vec<f32> = segment.left.iter().map(|s| *s as f32).collect();
    let right: Vec<f32> = segment.right.iter().map(|s| *s as f32).collect();
    (0..AAC_HOP)
      .into_par_iter()
      .with_min_len(AAC_HOP / TASKS)
      .map_init(Self::new, |aac, offset| aac.count(&left, &right, offset))
      .collect()
  }

  /// the active count summed over the long frames of `left` and `right` on the grid at `offset`
  fn count(&mut self, left: &[f32], right: &[f32], offset: usize) -> f64 {
    let frames = (SEGMENT - 2 * AAC_HOP) / AAC_HOP;
    let t2 = (NOISE16 as f32).powi(2);
    let half = std::f32::consts::FRAC_1_SQRT_2;
    (0..frames)
      .map(|j| {
        let start = offset + j * AAC_HOP;
        self.mdct(&left[start..start + 2 * AAC_HOP], 0);
        self.mdct(&right[start..start + 2 * AAC_HOP], 1);
        let [m, s] = &mut self.mid_side;
        let [left, right] = &self.values;
        for ((m, s), (l, r)) in m.iter_mut().zip(s.iter_mut()).zip(left.iter().zip(right)) {
          *m = (l + r) * half;
          *s = (l - r) * half;
        }
        let lr = active(left, t2) + active(right, t2);
        let ms = active(m, t2) + active(s, t2);
        lr.min(ms)
      })
      .sum()
  }

  /// the orthonormal MDCT of `input`, 2048 samples, windowed, into `values[into]`
  fn mdct(&mut self, input: &[f32], into: usize) {
    for ((f, x), w) in self.frame.iter_mut().zip(input).zip(&self.window) {
      *f = x * w;
    }
    fold(&self.frame, &mut self.folded);
    self.dct.process(&self.folded, &mut self.values[into]);
  }
}

/// the input of the DCT-IV whose output is the MDCT of `frame`, 2N windowed samples:
/// (-c_r - d, a - b_r) for its quarters a, b, c, d, so that the DCT-IV gives
/// X_k = sqrt(2/N) sum_n x_n cos(pi/N (n + 1/2 + N/2)(k + 1/2))
pub(crate) fn fold<T: Float>(frame: &[T], folded: &mut [T]) {
  fold_with(frame, folded, |c, d| -*c - *d, |a, b| *a - *b);
}

/// [`fold`] over any elements: `negated_sum(c, d)` is -c - d and `difference(a, b)` is a - b
fn fold_with<T, U>(
  frame: &[T],
  folded: &mut [U],
  negated_sum: impl Fn(&T, &T) -> U,
  difference: impl Fn(&T, &T) -> U,
) {
  let q = folded.len() / 2;
  let (ab, cd) = frame.split_at(2 * q);
  let ((a, b), (c, d)) = (ab.split_at(q), cd.split_at(q));
  let (low, high) = folded.split_at_mut(q);
  for ((folded, c), d) in low.iter_mut().zip(c.iter().rev()).zip(d) {
    *folded = negated_sum(c, d);
  }
  for ((folded, a), b) in high.iter_mut().zip(a).zip(b.iter().rev()) {
    *folded = difference(a, b);
  }
}

/// the Kaiser-Bessel-derived window of `length` samples with `alpha`: the square root of the
/// running sum of a Kaiser window of `length / 2 + 1` points with beta pi alpha over its total,
/// mirrored
fn kbd_window(length: usize, alpha: f64) -> Vec<f64> {
  let points = length / 2 + 1;
  let beta = std::f64::consts::PI * alpha;
  let kaiser: Vec<f64> = (0..points)
    .map(|n| {
      let r = 2.0 * n as f64 / (points - 1) as f64 - 1.0;
      bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta)
    })
    .collect();
  let total: f64 = kaiser.iter().sum();
  let mut half = Vec::with_capacity(length / 2);
  let mut running = 0.0;
  for k in &kaiser[..points - 1] {
    running += k;
    half.push((running / total).sqrt());
  }
  let mirrored: Vec<f64> = half.iter().rev().copied().collect();
  half.extend(mirrored);
  half
}

/// the modified Bessel function of the first kind of order 0, from its power series
pub(crate) fn bessel_i0(x: f64) -> f64 {
  let quarter = x * x / 4.0;
  let mut term = 1.0;
  let mut sum = 1.0;
  for k in 1..100 {
    term *= quarter / (k * k) as f64;
    sum += term;
    if term < sum * 1e-17 {
      break;
    }
  }
  sum
}

/// the orthonormal DCT-IV of length N through a complex FFT of length N/2:
/// X_k = sqrt(2/N) sum_n u_n cos(pi/N (n + 1/2)(k + 1/2))
pub(crate) struct DctIv {
  fft: Arc<dyn Fft<f32>>,
  /// exp(-i pi n / N), for the input
  pre: Vec<Complex<f32>>,
  /// sqrt(2/N) exp(-i pi (k + 1/4) / N), for the output
  post: Vec<Complex<f32>>,
  buffer: Vec<Complex<f32>>,
  scratch: Vec<Complex<f32>>,
}

impl DctIv {
  pub(crate) fn new(n: usize) -> Self {
    let half = n / 2;
    let fft = FftPlanner::new().plan_fft_forward(half);
    let turn = |angle: f64, scale: f64| {
      Complex::new((scale * angle.cos()) as f32, (scale * angle.sin()) as f32)
    };
    let step = std::f64::consts::PI / n as f64;
    let scale = (2.0 / n as f64).sqrt();
    Self {
      pre: (0..half).map(|i| turn(-step * i as f64, 1.0)).collect(),
      post: (0..half)
        .map(|k| turn(-step * (k as f64 + 0.25), scale))
        .collect(),
      buffer: vec![Complex::default(); half],
      scratch: vec![Complex::default(); fft.get_inplace_scratch_len()],
      fft,
    }
  }

  /// writes the transform of `input` to `output`, both of length N.
  ///
  /// with z_n = u_2n + i u_(N-1-2n) and Y_k = sum_n z_n exp(-i pi (2n + 1/2)(2k + 1/2) / N),
  /// X_2k is the real part of Y_k and X_(N-1-2k) minus its imaginary part.
  pub(crate) fn process(&mut self, input: &[f32], output: &mut [f32]) {
    // pair i holds u_2i and u_(2i+1), so u_(N-1-2n) is the second of pair N/2-1-n
    let (pairs, _) = input.as_chunks::<2>();
    let turned = self.buffer.iter_mut().zip(&self.pre);
    for ((b, pre), (even, odd)) in turned.zip(pairs.iter().zip(pairs.iter().rev())) {
      *b = Complex::new(even[0], odd[1]) * pre;
    }
    self
      .fft
      .process_with_scratch(&mut self.buffer, &mut self.scratch);
    let (pairs, _) = output.as_chunks_mut::<2>();
    let turned = || self.buffer.iter().zip(&self.post).map(|(b, post)| b * post);
    for (pair, y) in pairs.iter_mut().zip(turned()) {
      pair[0] = y.re;
    }
    for (pair, y) in pairs.iter_mut().rev().zip(turned()) {
      pair[1] = -y.im;
    }
  }
}

/// half of the MPEG audio synthesis window D in units of 2^-16, as ffmpeg's encoder carries it;
/// the analysis window C is D / 32
const ENWINDOW: [i32; 257] = [
  0, -1, -1, -1, -1, -1, -1, -2, -2, -2, -2, -3, -3, -4, -4, -5, -5, -6, -7, -7, -8, -9, -10, -11,
  -13, -14, -16, -17, -19, -21, -24, -26, -29, -31, -35, -38, -41, -45, -49, -53, -58, -63, -68,
  -73, -79, -85, -91, -97, -104, -111, -117, -125, -132, -139, -147, -154, -161, -169, -176, -183,
  -190, -196, -202, -208, 213, 218, 222, 225, 227, 228, 228, 227, 224, 221, 215, 208, 200, 189,
  177, 163, 146, 127, 106, 83, 57, 29, -2, -36, -72, -111, -153, -197, -244, -294, -347, -401,
  -459, -519, -581, -645, -711, -779, -848, -919, -991, -1064, -1137, -1210, -1283, -1356, -1428,
  -1498, -1567, -1634, -1698, -1759, -1817, -1870, -1919, -1962, -2001, -2032, -2057, -2075, -2085,
  -2087, -2080, -2063, 2037, 2000, 1952, 1893, 1822, 1739, 1644, 1535, 1414, 1280, 1131, 970, 794,
  605, 402, 185, -45, -288, -545, -814, -1095, -1388, -1692, -2006, -2330, -2663, -3004, -3351,
  -3705, -4063, -4425, -4788, -5153, -5517, -5879, -6237, -6589, -6935, -7271, -7597, -7910, -8209,
  -8491, -8755, -8998, -9219, -9416, -9585, -9727, -9838, -9916, -9959, -9966, -9935, -9863, -9750,
  -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134, 6574, 5959, 5288, 4561, 3776, 2935, 2037,
  1082, 70, -998, -2122, -3300, -4533, -5818, -7154, -8540, -9975, -11455, -12980, -14548, -16155,
  -17799, -19478, -21189, -22929, -24694, -26482, -28289, -30112, -31947, -33791, -35640, -37489,
  -39336, -41176, -43006, -44821, -46617, -48390, -50137, -51853, -53534, -55178, -56778, -58333,
  -59838, -61289, -62684, -64019, -65290, -66494, -67629, -68692, -69679, -70590, -71420, -72169,
  -72835, -73415, -73908, -74313, -74630, -74856, -74992, 75038,
];

/// the alias-reduction coefficients of layer III
const ALIAS: [f64; 8] = [
  -0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037,
];

/// subband samples per granule and the lines each subband's MDCT gives
const SUBBAND_SAMPLES: usize = 36;
const LINES: usize = 18;
const SUBBANDS: usize = 32;
/// the subbands whose line sums [`Mp3::hybrid`] keeps side by side
const SIDE_BY_SIDE: usize = 16;

/// the MP3 analysis: the layer III hybrid filterbank, in double precision
struct Mp3 {
  /// the analysis window C over 512 taps in blocks of 64, each block's last tap first, which is
  /// the order the taps meet the input samples in
  window: [[f64; 64]; 8],
  /// cos((2i + 1)(k - 16) pi / 64) for k in 0..64, by subband i
  matrix: [[f64; SUBBANDS]; 64],
  /// the sine window over 36 subband samples, by subband: every odd sample of an odd subband is
  /// negated
  windows: [[f64; SUBBANDS]; SUBBAND_SAMPLES],
  /// cos(pi/18 (n + 1/2)(k + 1/2)), the DCT-IV that gives the 18-line MDCT
  /// sum_n w_n z_n cos(pi/72 (2n + 1 + 18)(2k + 1)) of the folded windowed samples, by line
  dct: Vec<[f64; LINES]>,
  /// cs and ca of each butterfly
  butterflies: [(f64, f64); 8],
}

impl Mp3 {
  fn new() -> Self {
    let mut d = [0.0; 512];
    for (i, v) in ENWINDOW.iter().enumerate() {
      let v = f64::from(*v);
      d[i] = v;
      if i > 0 {
        d[512 - i] = if i % 64 == 0 { v } else { -v };
      }
    }
    let pi = std::f64::consts::PI;
    Self {
      window: std::array::from_fn(|j| std::array::from_fn(|i| d[64 * j + 63 - i] / 65536.0 / 32.0)),
      matrix: std::array::from_fn(|k| {
        std::array::from_fn(|i| ((2 * i + 1) as f64 * (k as f64 - 16.0) * pi / 64.0).cos())
      }),
      windows: std::array::from_fn(|n| {
        std::array::from_fn(|sb| {
          let sign = if n % 2 == 1 && sb % 2 == 1 { -1.0 } else { 1.0 };
          sign * (pi / 36.0 * (n as f64 + 0.5)).sin()
        })
      }),
      dct: (0..LINES)
        .map(|k| std::array::from_fn(|n| (pi / 18.0 * (n as f64 + 0.5) * (k as f64 + 0.5)).cos()))
        .collect(),
      butterflies: ALIAS.map(|c| {
        let norm = (1.0 + c * c).sqrt();
        (1.0 / norm, c / norm)
      }),
    }
  }

  /// the subband samples of `x`, 32 for every 32 input samples: sample t reads the 512 inputs
  /// from 32 t on, the newest first.
  ///
  /// every sum here and in [`Mp3::hybrid`] adds its terms in the order of the formula, in one
  /// running total per output, so the inner loops run over the outputs side by side.
  fn polyphase(&self, x: &[f64]) -> Vec<[f64; SUBBANDS]> {
    let count = (x.len() - 512) / 32 + 1;
    (0..count)
      .map(|t| {
        // with the newest input at 32 t + 511, y[63 - k] is the sum over j of
        // x[newest - k - 64 j] window[k + 64 j]
        let (blocks, _) = x[32 * t..32 * t + 512].as_chunks::<64>();
        let mut y = [0.0; 64];
        for (taps, x) in self.window.iter().zip(blocks.iter().rev()) {
          for ((y, x), tap) in y.iter_mut().zip(x).zip(taps) {
            *y += x * tap;
          }
        }
        let mut subbands = [0.0; SUBBANDS];
        for (row, y) in self.matrix.iter().zip(y.iter().rev()) {
          for (subband, m) in subbands.iter_mut().zip(row) {
            *subband += m * y;
          }
        }
        subbands
      })
      .collect()
  }

  /// the 576 values of the granule whose 36 subband samples are `samples`, subband by subband
  fn hybrid(&self, samples: &[[f64; SUBBANDS]], out: &mut [f64; MP3_HOP]) {
    // each subband sample with its window, multiplied as the two are folded
    let windowed: [_; SUBBAND_SAMPLES] = std::array::from_fn(|n| (&samples[n], &self.windows[n]));
    let mut folded = [[0.0; SUBBANDS]; LINES];
    fold_with(
      &windowed,
      &mut folded,
      |c, d| std::array::from_fn(|sb| -(c.0[sb] * c.1[sb]) - d.0[sb] * d.1[sb]),
      |a, b| std::array::from_fn(|sb| a.0[sb] * a.1[sb] - b.0[sb] * b.1[sb]),
    );
    for (k, dct) in self.dct.iter().enumerate() {
      for from in (0..SUBBANDS).step_by(SIDE_BY_SIDE) {
        let mut line = [0.0; SIDE_BY_SIDE];
        for (d, folded) in dct.iter().zip(&folded) {
          for (l, f) in line.iter_mut().zip(&folded[from..from + SIDE_BY_SIDE]) {
            *l += d * f;
          }
        }
        for (i, l) in line.iter().enumerate() {
          out[(from + i) * LINES + k] = *l;
        }
      }
    }
    for sb in 0..SUBBANDS - 1 {
      for (i, (cs, ca)) in self.butterflies.iter().enumerate() {
        let lo = out[sb * LINES + LINES - 1 - i];
        let hi = out[(sb + 1) * LINES + i];
        out[sb * LINES + LINES - 1 - i] = lo * cs + hi * ca;
        out[(sb + 1) * LINES + i] = hi * cs - lo * ca;
      }
    }
  }

  /// the active count summed over the granules of `segment`, for each offset of the granule
  /// grid from its first sample. every offset counts the same number of granules. the 32
  /// phases of the polyphase filterbank are tasks on rayon's thread pool.
  fn curve(&self, segment: &Segment) -> Vec<f64> {
    let phases: Vec<[f64; LINES]> = (0..32)
      .into_par_iter()
      .map(|phase| self.phase(segment, phase))
      .collect();
    (0..MP3_HOP)
      .map(|offset| phases[offset % 32][offset / 32])
      .collect()
  }

  /// the counts of the offsets `phase` + 32 g, for g in 0..18: the granules that start at
  /// subband sample g of the filterbank run from input sample `phase`
  fn phase(&self, segment: &Segment, phase: usize) -> [f64; LINES] {
    let granules = (SEGMENT - MP3_HOP - 512 - SUBBAND_SAMPLES * 32) / MP3_HOP;
    let t2 = (NOISE16 * MP3_NOISE_GAIN).powi(2) as f32;
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let (mut l, mut r) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    // left, right, mid and side in single precision, for the counts
    let mut narrow = [[0.0f32; MP3_HOP]; 4];
    let left = self.polyphase(&segment.left[phase..]);
    let right = self.polyphase(&segment.right[phase..]);
    let mut counts = [0.0; LINES];
    for (g, count) in counts.iter_mut().enumerate() {
      *count = (0..granules)
        .map(|granule| {
          let at = g + LINES * granule;
          self.hybrid(&left[at..at + SUBBAND_SAMPLES], &mut l);
          self.hybrid(&right[at..at + SUBBAND_SAMPLES], &mut r);
          for k in 0..MP3_HOP {
            narrow[0][k] = l[k] as f32;
            narrow[1][k] = r[k] as f32;
            narrow[2][k] = ((l[k] + r[k]) * half) as f32;
            narrow[3][k] = ((l[k] - r[k]) * half) as f32;
          }
          let count = narrow.each_ref().map(|values| active(values, t2));
          (count[0] + count[1]).min(count[2] + count[3])
        })
        .sum();
    }
    counts
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_opus_count_is_named_with_the_offset_it_was_counted_at() {
    // one chance frame in the segments, a decode's frames in the windows
    let scan = OpusScan {
      offset: 517,
      frames: 1,
      segment_frames: [0, 1, 0, 0],
      uncut_frames: 40,
    };
    assert_eq!(scan.most(), (OPUS_UNCUT_OFFSET, 40));
    assert_eq!(Evidence::Lattice(scan.clone()).offset(), OPUS_UNCUT_OFFSET);
    // a decode whose start was cut shows in the segments alone
    let cut = OpusScan {
      frames: 150,
      uncut_frames: 0,
      ..scan
    };
    assert_eq!(cut.most(), (517, 150));
  }

  #[test]
  fn the_mdct_is_the_direct_sum() {
    let n = 16;
    let frame: Vec<f32> = (0..2 * n)
      .map(|i| ((i * i) as f32 * 0.37).sin() * 0.8)
      .collect();
    let mut folded = vec![0.0; n];
    let mut out = vec![0.0; n];
    fold(&frame, &mut folded);
    DctIv::new(n).process(&folded, &mut out);
    let pi = std::f64::consts::PI;
    for (k, got) in out.iter().enumerate() {
      let direct: f64 = frame
        .iter()
        .enumerate()
        .map(|(i, x)| {
          f64::from(*x)
            * (pi / n as f64 * (i as f64 + 0.5 + n as f64 / 2.0) * (k as f64 + 0.5)).cos()
        })
        .sum::<f64>()
        * (2.0 / n as f64).sqrt();
      assert!(
        (f64::from(*got) - direct).abs() < 1e-5,
        "{k}: {got} {direct}"
      );
    }
  }

  #[test]
  fn the_kbd_window_is_ffmpegs_long_window() {
    let window = kbd_window(2048, 4.0);
    // values of the reference implementation's window
    for (i, value) in [
      (0, 0.000_292_561_534_837_649_7),
      (100, 0.024_457_477_148_620_538),
      (511, 0.706_119_339_105_633_4),
      (1023, 0.999_999_957_203_872_2),
    ] {
      assert!((window[i] - value).abs() < 1e-12, "{i}: {}", window[i]);
    }
    for i in 0..1024 {
      let power = window[i].powi(2) + window[i + 1024].powi(2);
      assert!((power - 1.0).abs() < 1e-12, "{i}: {power}");
    }
  }

  #[test]
  fn the_hybrid_filterbank_matches_the_reference() {
    let x: Vec<f64> = (0..1800)
      .map(|n| {
        let n = f64::from(n);
        0.5 * (0.37 * n).sin() + 0.25 * (0.0011 * n * n).sin()
      })
      .collect();
    let mp3 = Mp3::new();
    let sub = mp3.polyphase(&x);
    assert_eq!(sub.len(), 41);
    let mut out = [0.0; MP3_HOP];
    mp3.hybrid(&sub[5..5 + SUBBAND_SAMPLES], &mut out);
    // values of the reference implementation on the same input, granule from subband sample 5
    for (i, value) in [
      (0, -1.091_283_183_323_872_5e-6),
      (7, -9.081_711_237_383_263e-7),
      (17, 6.967_136_897_000_289e-7),
      (18, 1.931_395_673_253_308_3e-6),
      (100, -6.038_671_146_916_679e-5),
      (287, 0.176_172_793_341_240_09),
      (288, 0.134_650_302_532_005_74),
      (431, -0.184_243_819_723_131_5),
      (575, -0.035_325_179_781_597_5),
    ] {
      assert!((out[i] - value).abs() < 1e-12, "{i}: {}", out[i]);
    }
    let squares: f64 = out.iter().map(|v| v * v).sum();
    assert!((squares - 28.590_620_430_713_802).abs() < 1e-9, "{squares}");
  }

  #[test]
  fn z_is_the_lowest_entry_below_the_median_in_robust_deviations() {
    let curve = [10.0, 11.0, 9.0, 10.0, 12.0, 8.0, 2.0, 10.0, 11.0, 9.0];
    // median 10, absolute deviations with median 1
    let (z, at) = robust_z(&curve);
    assert_eq!(at, 6);
    assert!((z - 8.0 / MAD_TO_SD).abs() < 1e-12, "{z}");
    assert_eq!(robust_z(&[3.0; 8]), (0.0, 0), "a flat curve has no z");
  }

  #[test]
  fn each_codec_asks_for_its_own_agreement() {
    let thresholds = ReadingThresholds {
      aac_grid_z: 9.0,
      aac_grid_segments_agreeing: 0,
      mp3_grid_z: 12.0,
      mp3_grid_segments_agreeing: 3,
      ..ReadingThresholds::default()
    };
    let codec_scan = |z: f64, segments_agreeing: usize| CodecScan {
      offset: 0,
      z,
      segments_agreeing,
      segment_z: [0.0; SEGMENTS],
    };
    let scan = |aac: (f64, usize), mp3: (f64, usize)| Scan {
      aac: codec_scan(aac.0, aac.1),
      mp3: codec_scan(mp3.0, mp3.1),
      opus: None,
    };
    let codec = |scan: &Scan| found(scan, &thresholds).map(|f| f.codec);
    // an MP3 dip in one segment is no grid, an AAC dip the segments place differently is one
    assert_eq!(codec(&scan((2.0, 0), (50.0, 1))), None);
    assert_eq!(codec(&scan((19.0, 1), (2.0, 0))), Some(Codec::Aac));
    assert_eq!(codec(&scan((9.0, 4), (12.0, 4))), None);
    // both over: 30 / 9 against 36 / 12
    assert_eq!(codec(&scan((30.0, 4), (36.0, 3))), Some(Codec::Aac));
  }

  #[test]
  fn segment_curves_are_turned_into_the_files_offsets_before_they_agree() {
    let hop = 8;
    let dip = |at: usize| -> Vec<f64> {
      (0..hop)
        .map(|o| if o == at { 1.0 } else { 10.0 + o as f64 * 0.1 })
        .collect()
    };
    // segments starting 3, 8 + 1, 16 + 7 and 5 samples into the file see a grid at file
    // offset 6 at segment offsets 3, 5, 7 and 1; the last dips elsewhere
    let curves = [
      in_file(&dip(3), 3),
      in_file(&dip(5), 9),
      in_file(&dip(7), 23),
      in_file(&dip(4), 5),
    ];
    let scan = codec_scan(&curves);
    assert_eq!(scan.offset, 6);
    assert_eq!(scan.segments_agreeing, 3);
  }
}
