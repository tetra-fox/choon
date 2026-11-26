//! the scan of a file's segments: each codec's analysis of each segment as a job on rayon's
//! thread pool, and what the jobs found gathered into a [`Scan`]

use rayon::prelude::*;

use super::aac::{self, Aac, AacStepper, Window, aac_bands};
use super::lattice;
use super::mp3::{GranuleSteps, Mp3, mp3_bands};
use super::resample::Resampler;
use super::segments::{Segment, Segments};
use super::{
  AacNeighbours, AacSteps, CodecScan, MP3_HOP, Mp3Neighbours, Mp3Steps, OPUS_UNCUT_OFFSET,
  OpusHybrid, OpusScan, OpusShifted, OpusShort, OpusUnfiltered, OtherRate, SEGMENTS, Scan,
  in_stretch,
};

/// MAD times this estimates the standard deviation of normally distributed values
const MAD_TO_SD: f64 = 1.4826;

/// the offsets either side of one whose counts its own is measured against
const NEAR: usize = 16;

/// what the scan for Opus's frames needs beside the segments
pub(crate) struct Opus {
  /// the file's sample rate
  pub(crate) rate: lattice::Rate,
  /// the bits of its samples, whose rounding noise a band must stand over
  pub(crate) bits: u32,
  /// what the frames in the windows measured count, on the grid of an uncut decode
  pub(crate) uncut: lattice::Counted,
  /// the same for the short frames of each length, on the grid each delay of the encoder leaves
  pub(crate) uncut_short: [[[usize; 2]; 2]; 3],
  /// the pairs of frames on that grid the pitch post-filter was searched in, and the ones that
  /// sit on the lattice with it taken off
  pub(crate) unfiltered: [usize; 2],
}

/// the number of jobs in one scan: five in each segment, for MP3, for AAC under each of its
/// windows, for Vorbis and for AC-3, and with `wma` one more for WMA. with `opus`, set at a
/// rate Opus is looked for at, one more per segment and one per length of short frame, and
/// with `other_rate` too MP3's and AAC's three again in each segment at the other rate
pub(crate) fn jobs(opus: bool, other_rate: bool, wma: bool) -> usize {
  (5 + usize::from(wma) + usize::from(opus) + 3 * usize::from(opus && other_rate)) * SEGMENTS
    + lattice::SHORT.len() * usize::from(opus)
}

/// the scan of full `segments`. each codec of each segment is a job on rayon's thread pool that
/// splits its offsets into tasks on the same pool. Opus is among the codecs with `opus`, WMA
/// with `wma`, and with `other_rate` a file at 44.1 or 48 kHz is scanned for MP3 and AAC at
/// the other of the two too. `go_on` is asked before each job and the scan is `None` once it
/// says not to; `ended` is called as each job ends. panics when a segment is not full.
pub(crate) fn scan(
  segments: &Segments,
  rate: u32,
  opus: Option<Opus>,
  other_rate: bool,
  wma: bool,
  go_on: impl Fn() -> bool + Sync,
  ended: impl Fn() + Sync,
) -> Option<Scan> {
  assert!(segments.complete(), "every segment is full before a scan");
  let stretches = opus.as_ref().map(|opus| {
    let stretches: Vec<_> = segments
      .0
      .iter()
      .map(|segment| ([&segment.left[..], &segment.right[..]], segment.start))
      .collect();
    lattice::Stretches::of(&stretches, opus.rate, opus.bits)
  });
  // the segments at the other rate, each brought there as a task on the pool
  let to_other = match rate {
    48_000 => Some((44_100, Resampler::to_44100())),
    44_100 => Some((48_000, Resampler::to_48000())),
    _ => None,
  };
  let other = to_other.filter(|_| other_rate).map(|(rate, resampler)| {
    let segments: Vec<Option<Segment>> = segments
      .0
      .par_iter()
      .map(|segment| segment.brought(resampler))
      .collect();
    (rate, segments)
  });
  // Opus's offsets are split into as many jobs as there are segments, each over every segment
  let part = lattice::FRAME / SEGMENTS;
  let counts = |segment, rate| {
    [
      Job::Aac(Window::Kbd, segment, rate),
      Job::Aac(Window::Sine, segment, rate),
      Job::Mp3(segment, rate),
    ]
  };
  let jobs: Vec<Job> = segments
    .0
    .iter()
    .flat_map(|segment| counts(segment, Rate::Own))
    .chain(segments.0.iter().flat_map(|segment| {
      [
        Some(Window::Vorbis),
        Some(Window::Ac3),
        wma.then_some(Window::Wma),
      ]
      .into_iter()
      .flatten()
      .map(move |window| Job::Aac(window, segment, Rate::Own))
    }))
    .chain(
      other
        .iter()
        .flat_map(|(_, segments)| segments.iter().flatten())
        .flat_map(|segment| counts(segment, Rate::Other)),
    )
    .chain(
      stretches
        .iter()
        .flat_map(|stretches| (0..SEGMENTS).map(move |i| Job::Opus(stretches, i * part))),
    )
    .chain(
      stretches
        .iter()
        .flat_map(|stretches| lattice::SHORT.map(|short| Job::OpusShort(stretches, short))),
    )
    .collect();
  let curves: Vec<Curve> = jobs
    .into_par_iter()
    .map(|job| {
      go_on().then(|| {
        let curve = match job {
          Job::Aac(window, segment, rate) => Curve::Aac(
            window,
            rate,
            in_file(&levelled(&Aac::curve(segment, window)), segment.start),
          ),
          Job::Mp3(segment, rate) => {
            let (hybrid, subbands) = Mp3::new().curves(segment);
            Curve::Mp3(
              rate,
              in_file(&levelled(&hybrid), segment.start),
              in_file(&levelled(&subbands), segment.start),
            )
          }
          // the counts come in the file's offsets
          Job::Opus(stretches, from) => Curve::Opus(stretches.counted(from..from + part)),
          Job::OpusShort(stretches, short) => Curve::OpusShort(stretches.short_counted(short)),
        };
        ended();
        curve
      })
    })
    .collect::<Option<_>>()?;
  // each codec's curves at the file's rate and at the other
  let (mut aac, mut sine, mut mp3) = <[[Vec<Vec<f64>>; 2]; 3]>::default().into();
  // Vorbis's, WMA's, AC-3's and layer II's, which are scanned at the file's rate alone
  let (mut vorbis, mut wma_curves, mut mp2) = (Vec::new(), Vec::new(), Vec::new());
  let mut ac3 = Vec::new();
  let mut frames = Vec::new();
  let mut short = Vec::new();
  for curve in curves {
    match curve {
      Curve::Aac(Window::Kbd, rate, curve) => aac[rate as usize].push(curve),
      Curve::Aac(Window::Sine, rate, curve) => sine[rate as usize].push(curve),
      Curve::Aac(Window::Vorbis, _, curve) => vorbis.push(curve),
      Curve::Aac(Window::Wma, _, curve) => wma_curves.push(curve),
      Curve::Aac(Window::Ac3, _, curve) => ac3.push(curve),
      Curve::Mp3(rate, curve, subbands) => {
        mp3[rate as usize].push(curve);
        if matches!(rate, Rate::Own) {
          mp2.push(subbands);
        }
      }
      // the jobs come back in the order of their offsets
      Curve::Opus(counted) => frames.extend(counted),
      // and these in the order of the lengths
      Curve::OpusShort(counted) => short.push(counted),
    }
  }
  let [aac, other_aac] = aac;
  let [sine, other_sine] = sine;
  let [mp3, other_mp3] = mp3;
  // a segment nothing is left of at the other rate leaves that rate unscanned
  let other_rate = other
    .filter(|_| other_mp3.len() == SEGMENTS)
    .map(|(rate, _)| OtherRate {
      rate,
      aac: codec_scan(&other_aac),
      aac_sine: codec_scan(&other_sine),
      mp3: codec_scan(&other_mp3),
    });
  let (aac, sine, mp3) = (codec_scan(&aac), codec_scan(&sine), codec_scan(&mp3));
  // the quantizers' steps on the grids the counts found: each segment a task on the pool,
  // which reads MP3's granules and AAC's frames under each window side by side. they take a
  // small part of a count's time and are not among the jobs
  let (mp3_bands, aac_bands) = (mp3_bands(rate), aac_bands(rate));
  let stepped: Vec<SegmentSteps> = segments
    .0
    .par_iter()
    .map(|segment| {
      go_on().then(|| {
        let mp3_steps = || {
          mp3_bands.map(|bands| {
            let offset = in_stretch(mp3.offset, segment.start, MP3_HOP);
            Mp3::new().steps(segment, offset, bands)
          })
        };
        let aac_steps = |window, count: &CodecScan| {
          aac_bands.map(|bands| AacStepper::new(window).steps(segment, count.offset, bands))
        };
        let (mp3_steps, (kbd, sine)) = rayon::join(mp3_steps, || {
          rayon::join(
            || aac_steps(Window::Kbd, &aac),
            || aac_steps(Window::Sine, &sine),
          )
        });
        SegmentSteps {
          mp3: mp3_steps,
          aac: kbd.zip(sine).map(<[_; 2]>::from),
        }
      })
    })
    .collect::<Option<_>>()?;
  // the granules are summed in file order, so the sums of logarithms come out the same
  // however the tasks ran
  let mp3_steps = mp3_bands.map(|_| {
    let mut steps = Mp3Steps {
      tested: 0,
      stepped: 0,
      whole: 0,
      segment_whole: [0; SEGMENTS],
      neighbours: None,
    };
    let mut neighbours = Mp3Neighbours::default();
    for (segment, in_segment) in stepped.iter().zip(&mut steps.segment_whole) {
      for granule in segment.mp3.iter().flatten() {
        steps.tested += granule.tested;
        steps.stepped += granule.stepped;
        steps.whole += granule.whole;
        *in_segment += granule.whole;
        neighbours.add(&granule.neighbours);
      }
    }
    steps.neighbours = Some(neighbours);
    steps
  });
  let aac_steps = |window: usize| {
    aac_bands.map(|_| {
      let mut steps = AacSteps {
        tested: 0,
        stepped: 0,
        whole: 0,
        segment_whole: [0; SEGMENTS],
        neighbours: None,
      };
      let mut neighbours = AacNeighbours::default();
      for (segment, in_segment) in stepped.iter().zip(&mut steps.segment_whole) {
        for read in segment.aac.iter().map(|windows| &windows[window]) {
          let [tested, stepped, whole] = read.counts;
          steps.tested += tested;
          steps.stepped += stepped;
          steps.whole += whole;
          *in_segment += whole;
          neighbours.add(&read.neighbours);
        }
      }
      steps.neighbours = Some(neighbours);
      steps
    })
  };
  let (aac_steps, aac_sine_steps) = (aac_steps(0), aac_steps(1));
  Some(Scan {
    other_rate,
    aac,
    aac_sine: Some(sine),
    vorbis: Some(codec_scan(&vorbis)),
    wma: (!wma_curves.is_empty()).then(|| codec_scan(&wma_curves)),
    ac3: Some(codec_scan(&ac3)),
    mp2: Some(codec_scan(&mp2)),
    mp3,
    mp3_steps,
    aac_steps,
    aac_sine_steps,
    opus: opus.map(|opus| opus_scan(&frames, &short, &opus)),
  })
}

/// the quantizers' steps in one segment: MP3's by granule, and AAC's under the
/// Kaiser-Bessel-derived window and under the sine one. each is `None` at a rate its steps
/// are not read at
struct SegmentSteps {
  mp3: Option<Vec<GranuleSteps>>,
  aac: Option<[aac::SegmentSteps; 2]>,
}

/// the rate a segment is scanned at: the file's own, or the other of 44.1 and 48 kHz
#[derive(Clone, Copy)]
enum Rate {
  Own,
  Other,
}

/// what one job of a scan looks for: MP3's grid or the grid under one window in one segment
/// at one rate, Opus's frames in every segment at the offsets from the one given, or its
/// short frames of one length in every segment
#[derive(Clone, Copy)]
enum Job<'a> {
  Aac(Window, &'a Segment, Rate),
  Mp3(&'a Segment, Rate),
  Opus(&'a lattice::Stretches, usize),
  OpusShort(&'a lattice::Stretches, lattice::Short),
}

/// what a job found: MP3's and a window's counts of transform values on a log scale at each
/// offset of the file's grid at the job's rate, Opus's counts of frames at each of the job's
/// offsets
enum Curve {
  Aac(Window, Rate, Vec<f64>),
  /// with the counts of the subband samples at the 32 phases of the filterbank
  Mp3(Rate, Vec<f64>, Vec<f64>),
  Opus(Vec<lattice::Counted>),
  /// at each offset, the short frames on the lattice after the frame before, and those with
  /// the ones on it without the frame before
  OpusShort(Vec<[usize; 2]>),
}

/// the first of `counts` with the most, each an offset and its count; `none` when no offset
/// counts any
fn most(none: usize, counts: impl Iterator<Item = (usize, usize)>) -> (usize, usize) {
  counts.fold((none, 0), |(at, most), (o, count)| {
    if count > most { (o, count) } else { (at, most) }
  })
}

/// the scan for Opus from what the segments' frames count at each offset of the file's grid,
/// those of 20 ms and the short ones of each length, and what the windows' count on the grid
/// of an uncut decode
fn opus_scan(counted: &[lattice::Counted], short: &[Vec<[usize; 2]>], opus: &Opus) -> OpusScan {
  let uncut = &opus.uncut;
  let [searched, unfiltered] = opus.unfiltered;
  // the frames read without the frame before count where one sits on the lattice after it
  let with_alone = |[after, either]: [usize; 2]| if after > 0 { either } else { 0 };
  let short = lattice::SHORT
    .iter()
    .zip(short)
    .zip(opus.uncut_short)
    .map(|((length, counted), uncut)| {
      let grids = length.uncut_offsets();
      // the first grid's offset where none counts any
      let (offset, frames) = most(
        grids[0],
        counted.iter().copied().map(with_alone).enumerate(),
      );
      let (uncut_offset, uncut_frames) =
        most(grids[0], grids.into_iter().zip(uncut.map(with_alone)));
      OpusShort {
        length: length.length,
        offset,
        frames,
        uncut_offset,
        uncut_frames,
      }
    })
    .collect();
  let (offset, frames) = most(
    OPUS_UNCUT_OFFSET,
    counted.iter().map(|c| c.frames.iter().sum()).enumerate(),
  );
  let (shifted_offset, shifted_frames) = most(
    OPUS_UNCUT_OFFSET,
    counted.iter().map(|c| c.shifted).enumerate(),
  );
  let (hybrid_offset, hybrid_frames) = most(
    OPUS_UNCUT_OFFSET,
    counted.iter().map(|c| c.hybrid).enumerate(),
  );
  OpusScan {
    offset,
    frames,
    segment_frames: std::array::from_fn(|i| counted[offset].frames[i]),
    uncut_frames: uncut.frames.iter().sum(),
    shifted: Some(OpusShifted {
      offset: shifted_offset,
      frames: shifted_frames,
      uncut_frames: uncut.shifted,
    }),
    hybrid: Some(OpusHybrid {
      offset: hybrid_offset,
      frames: hybrid_frames,
      tested: counted[hybrid_offset].hybrid_tested,
      steps: counted[hybrid_offset].hybrid_points,
      uncut_frames: uncut.hybrid,
      uncut_tested: uncut.hybrid_tested,
      uncut_steps: uncut.hybrid_points,
    }),
    short,
    unfiltered: Some(OpusUnfiltered {
      searched,
      frames: unfiltered,
    }),
  }
}

/// `curve` with each entry less the median of the entries within `NEAR` of it. a segment's
/// count drifts from offset to offset with the audio that enters and leaves its frames, and
/// the drifts of four segments can sum to a low stretch of offsets. a grid lowers one offset
fn levelled(curve: &[f64]) -> Vec<f64> {
  let mut near = Vec::with_capacity(2 * NEAR + 1);
  (0..curve.len())
    .map(|o| {
      near.clear();
      near.extend_from_slice(&curve[o.saturating_sub(NEAR)..(o + NEAR + 1).min(curve.len())]);
      curve[o] - median(&mut near)
    })
    .collect()
}

/// a segment's curve, whose entry `o` is for frames starting `o` samples into the segment
/// modulo the hop, turned into the file's offsets for a segment starting at frame `start`
fn in_file(curve: &[f64], start: u64) -> Vec<f64> {
  let hop = curve.len();
  (0..hop).map(|o| curve[in_stretch(o, start, hop)]).collect()
}

/// the scan of one codec from its segments' curves in the file's offsets
fn codec_scan(curves: &[Vec<f64>]) -> CodecScan {
  let hop = curves[0].len();
  let summed: Vec<f64> = (0..hop)
    .map(|o| curves.iter().map(|c| c[o]).sum())
    .collect();
  let (z, offset) = robust_z(&summed);
  let own: [(f64, usize); SEGMENTS] = std::array::from_fn(|i| robust_z(&curves[i]));
  CodecScan {
    offset,
    z,
    segments_agreeing: own.iter().filter(|(_, at)| *at == offset).count(),
    segment_z: own.map(|(z, _)| z),
    segment_offset: own.map(|(_, at)| at),
  }
}

/// how far the lowest entry of `curve` sits below the median in robust standard deviations,
/// and its index: the first lowest
fn robust_z(curve: &[f64]) -> (f64, usize) {
  let mut sorted = curve.to_vec();
  let middle = median(&mut sorted);
  for value in &mut sorted {
    *value = (*value - middle).abs();
  }
  let mad = MAD_TO_SD * median(&mut sorted);
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

/// the median of `values`, the mean of the middle two for an even count. sorts them
fn median(values: &mut [f64]) -> f64 {
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

/// the products a count keeps side by side, so they can be multiplied in parallel
const LANES: usize = 16;

/// the groups of [`LANES`] pairs a count takes between two logs: a lane's product of as many
/// mantissas stays under 2^32, and the lanes' under 2^512 in double precision
const GROUPS: usize = 32;

/// the logs of pairs of values, summed in single precision.
///
/// the log of a product is the sum of the logs, so each pair's squares, no lower than t2, are
/// multiplied, the products' exponents summed and their mantissas, each in [1, 2), multiplied
/// in [`LANES`] products with one log for all of them every [`GROUPS`] groups.
struct Pairs {
  t2: f32,
  exponents: [i32; LANES],
  products: [f32; LANES],
  /// the groups in `products`, and those of the logs taken
  held: usize,
  logged: usize,
  logs: f64,
}

impl Pairs {
  fn new(t2: f32) -> Self {
    Self {
      t2,
      exponents: [0; LANES],
      products: [1.0; LANES],
      held: 0,
      logged: 0,
      logs: 0.0,
    }
  }

  #[inline(always)]
  fn take(&mut self, a: &[f32; LANES], b: &[f32; LANES]) {
    let t2 = self.t2;
    // a square that is not a number counts as t2
    let floored = |v: f32| if v * v > t2 { v * v } else { t2 };
    for j in 0..LANES {
      // t2 squared is a normal number, so every product is one too, and none is negative
      let bits = (floored(a[j]) * floored(b[j])).to_bits();
      self.exponents[j] += (bits >> 23) as i32;
      self.products[j] *= f32::from_bits((bits & !EXPONENT_BITS) | ONE);
    }
    self.held += 1;
    if self.held == GROUPS {
      self.log();
    }
  }

  fn log(&mut self) {
    self.logs += self
      .products
      .iter()
      .map(|p| f64::from(*p))
      .product::<f64>()
      .log2();
    self.products = [1.0; LANES];
    self.logged += self.held;
    self.held = 0;
  }

  /// the active count of the values taken: the sum of log10(max(v^2 / t2, 1))
  fn active(mut self) -> f64 {
    if self.held > 0 {
      self.log();
    }
    let pairs = (self.logged * LANES) as f64;
    let exponents: i32 = self.exponents.iter().sum();
    // a pair under t2 is t2 squared as it rounds
    let floor = f64::from(self.t2 * self.t2).log2();
    (f64::from(exponents) - 127.0 * pairs + self.logs - pairs * floor) * std::f64::consts::LOG10_2
  }
}

/// the active count of `a` and `b` together, each a multiple of [`LANES`] values
pub(super) fn active(a: &[f32], b: &[f32], t2: f32) -> f64 {
  let ((a, a_rest), (b, b_rest)) = (a.as_chunks(), b.as_chunks());
  debug_assert!(a_rest.is_empty() && b_rest.is_empty() && a.len() == b.len());
  let mut pairs = Pairs::new(t2);
  for (a, b) in a.iter().zip(b) {
    pairs.take(a, b);
  }
  pairs.active()
}

/// the active count of left and right together, and of mid and side together, for the lines
/// whose halves are `left` and `right`, each an `even` and an `odd` as
/// [`SplitMdct`](super::mdct::SplitMdct) writes them, a multiple of [`LANES`] lines each
pub(super) fn active_stereo(left: [&[f32]; 2], right: [&[f32]; 2], t2: f32) -> [f64; 2] {
  // twice mid and side squared are counted against twice t2, which is the same count
  let (mut apart, mut joint) = (Pairs::new(t2), Pairs::new(2.0 * t2));
  for (left, right) in left.into_iter().zip(right) {
    let ((left, left_rest), (right, right_rest)) = (left.as_chunks(), right.as_chunks());
    debug_assert!(left_rest.is_empty() && right_rest.is_empty() && left.len() == right.len());
    for (l, r) in left.iter().zip(right) {
      let sum: [f32; LANES] = std::array::from_fn(|j| l[j] + r[j]);
      let difference: [f32; LANES] = std::array::from_fn(|j| l[j] - r[j]);
      apart.take(l, r);
      joint.take(&sum, &difference);
    }
  }
  [apart.active(), joint.active()]
}

#[cfg(test)]
mod tests {
  use super::*;

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
  fn a_low_stretch_of_offsets_is_no_dip_and_one_low_offset_on_a_slope_is() {
    let noise = |o: usize| ((o * 37) % 11) as f64;
    // counts 60 lower over the first 128 offsets, as the drifts of four segments can sum to
    let shelf: Vec<f64> = (0..1024)
      .map(|o| noise(o) - if o < 128 { 60.0 } else { 0.0 })
      .collect();
    assert!(
      robust_z(&shelf).0 > 10.0,
      "the counts as they are read as a dip"
    );
    assert!(robust_z(&levelled(&shelf)).0 < 4.0);
    // a slope far steeper than the noise, with offset 700 alone 80 lower
    let slope: Vec<f64> = (0..1024)
      .map(|o| o as f64 * 3.0 + noise(o) - if o == 700 { 80.0 } else { 0.0 })
      .collect();
    assert!(robust_z(&slope).0 < 2.0, "the slope hides the dip");
    let (z, at) = robust_z(&levelled(&slope));
    assert_eq!(at, 700);
    assert!(z > 8.0, "{z}");
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

  #[test]
  fn the_counts_are_the_sum_of_the_logs() {
    let t2 = 1e-9f32;
    let direct = |lines: &[f32]| -> f64 {
      lines
        .iter()
        .map(|v| (f64::from(*v).powi(2) / f64::from(t2)).max(1.0).log10())
        .sum()
    };
    for n in [32usize, 1024, 2048] {
      let lines = |seed: f32| -> Vec<f32> {
        (0..n)
          .map(|i| ((i * i) as f32 * seed).sin() * 10f32.powi((i % 9) as i32 - 6))
          .collect()
      };
      let (left, right) = (lines(0.37), lines(0.53));
      let half = std::f32::consts::FRAC_1_SQRT_2;
      let mid: Vec<f32> = left
        .iter()
        .zip(&right)
        .map(|(l, r)| (l + r) * half)
        .collect();
      let side: Vec<f32> = left
        .iter()
        .zip(&right)
        .map(|(l, r)| (l - r) * half)
        .collect();
      let near = |counted: f64, direct: f64| (counted - direct).abs() < 1e-5 * direct.max(1.0);
      let apart = direct(&left) + direct(&right);
      let counted = active(&left, &right, t2);
      assert!(near(counted, apart), "{n}: {counted} {apart}");
      let (l, r) = (left.split_at(n / 2), right.split_at(n / 2));
      let [counted, joint] = active_stereo([l.0, l.1], [r.0, r.1], t2);
      assert!(near(counted, apart), "{n}: {counted} {apart}");
      let both = direct(&mid) + direct(&side);
      assert!(near(joint, both), "{n}: {joint} {both}");
    }
    let under = active(&[1e-6; 64], &[0.0; 64], t2);
    assert!(
      under.abs() < 1e-6,
      "values under the level count nothing: {under}"
    );
  }
}
