//! Frame grids that MP3 and AAC encoders leave in decoded audio.
//!
//! An encoder transforms its input in frames on a fixed grid and quantizes the transform values,
//! zeroing many of them. The decoder's synthesis inverts the encoder's analysis, so running that
//! analysis again on the decoded samples, with frames on the encoder's grid, gives back the
//! quantized values, and every value the encoder zeroed comes out at the level of the samples'
//! rounding noise. With frames at any other offset those values are filled by the neighbouring
//! frames. Lossless audio has no such offset.
//!
//! The scan reads four stretches of 40960 samples, at 20, 40, 60 and 80% of the file. For each
//! codec and each offset of its grid, it counts the transform values that stand above the
//! rounding noise of 16-bit samples, on a log scale, in left and right or in mid and side,
//! whichever pair counts fewer in a frame. The offset with the lowest count, and how far that
//! count sits below the others, is the codec's [`CodecScan`]. The AAC analysis is the long
//! window of ffmpeg's AAC encoder: a 2048-point MDCT at a hop of 1024 with the
//! Kaiser-Bessel-derived window. The MP3 analysis is the layer III hybrid filterbank: the
//! 512-tap polyphase filterbank, an 18-line MDCT per subband and the alias-reduction butterflies,
//! at a hop of 576. Short blocks are not analyzed.
//!
//! A grid is evidence that the samples were decoded from that codec, at some point. Resampling
//! after decoding removes it, Opus leaves none, and clipping hides it frame by frame. Vorbis's
//! grid shows through the AAC scan, so a Vorbis decode reads as AAC.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::num_traits::Float;
use rustfft::{Fft, FftPlanner};
use serde::Serialize;

use crate::thresholds::ReadingThresholds;

/// AAC's hop, the length of its long frames' new samples.
const AAC_HOP: usize = 1024;
/// MP3's hop, a granule.
const MP3_HOP: usize = 576;
/// The number of segments scanned.
const SEGMENTS: usize = 4;
/// The samples of one segment: 40 AAC hops.
pub(crate) const SEGMENT: usize = 40 * AAC_HOP;
/// Where the segments start, as fractions of the file's length.
const SEGMENT_AT: [f64; SEGMENTS] = [0.2, 0.4, 0.6, 0.8];
/// The rounding noise of 16-bit samples in samples scaled to [-1, 1): 2^-15 / sqrt(12).
const NOISE16: f64 = 8.809_710_218_959_115e-6;
/// The rms of a hybrid filterbank value per unit rms of white input, which puts the MP3 noise
/// level at the 16-bit rounding noise of the filterbank's output.
const MP3_NOISE_GAIN: f64 = 0.527_861_394_696_74;
/// MAD times this estimates the standard deviation of normally distributed values.
const MAD_TO_SD: f64 = 1.4826;

/// Codec whose frame grid the scan looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
  /// AAC, read with the long blocks of ffmpeg's encoder.
  ///
  /// Vorbis decodes read as AAC too.
  Aac,
  /// MP3, layer III.
  Mp3,
}

impl Codec {
  /// Returns the codec's name as a reading writes it.
  pub fn name(self) -> &'static str {
    match self {
      Self::Aac => "AAC",
      Self::Mp3 => "MP3",
    }
  }
}

/// How far one codec's grid stands out in a file.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
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
  pub agree: usize,
  /// Each segment's own z, in file order.
  pub segment_z: [f64; SEGMENTS],
}

/// Scan of a file for the frame grids of MP3 and AAC.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Scan {
  /// AAC's grid.
  pub aac: CodecScan,
  /// MP3's grid.
  pub mp3: CodecScan,
}

/// Frame grid a reading finds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Found {
  /// Codec whose grid it is.
  pub codec: Codec,
  /// How far the grid stands out.
  #[serde(flatten)]
  pub scan: CodecScan,
}

/// The grid `scan` shows under `thresholds`: a codec over its z threshold with enough segments
/// agreeing, the one furthest over its threshold when both are.
pub(crate) fn found(scan: &Scan, thresholds: &ReadingThresholds) -> Option<Found> {
  [
    (Codec::Aac, &scan.aac, thresholds.aac_grid_z),
    (Codec::Mp3, &scan.mp3, thresholds.mp3_grid_z),
  ]
  .into_iter()
  .filter(|(_, scan, z)| scan.z > *z && scan.agree >= thresholds.grid_segments_agreeing)
  .max_by(|(_, a, za), (_, b, zb)| (a.z / za).total_cmp(&(b.z / zb)))
  .map(|(codec, scan, _)| Found {
    codec,
    scan: scan.clone(),
  })
}

/// One stretch of the file the grid is scanned in, filled as its samples are decoded.
struct Segment {
  /// the sample frame it starts at
  start: u64,
  left: Vec<f64>,
  right: Vec<f64>,
}

impl Segment {
  /// The sample frame the segment needs next.
  fn next(&self) -> u64 {
    self.start + self.left.len() as u64
  }

  fn full(&self) -> bool {
    self.left.len() == SEGMENT
  }

  /// How many of `available` frames from `first` on the segment takes, and from which.
  fn wanted(&self, first: u64, available: usize) -> Option<(usize, usize)> {
    let next = self.next();
    let from = usize::try_from(next.checked_sub(first)?).ok()?;
    let count = (SEGMENT - self.left.len()).min(available.checked_sub(from)?);
    (count > 0).then_some((from, count))
  }
}

/// The segments of a file the grid is scanned in.
pub(crate) struct Segments(Vec<Segment>);

impl Segments {
  /// The segments of a file of `frames` sample frames; `None` when it is shorter than one.
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

  /// Takes what the segments need of `window`, one vector per analyzed channel, whose first
  /// sample is frame `first`. A mono window counts as both channels.
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

  /// Takes what the segments need of `samples`, interleaved over `channels` channels, each at
  /// the top of its 32 bits, whose first sample is frame `first`.
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

  /// The indices of the segments not yet full.
  pub(crate) fn missing(&self) -> Vec<usize> {
    (0..self.0.len()).filter(|i| !self.0[*i].full()).collect()
  }

  /// The sample frame segment `index` needs next; `None` once it is full.
  pub(crate) fn needs(&self, index: usize) -> Option<u64> {
    let segment = &self.0[index];
    (!segment.full()).then(|| segment.next())
  }

  /// Whether segment `index` has no samples yet.
  pub(crate) fn untouched(&self, index: usize) -> bool {
    self.0[index].left.is_empty()
  }

  /// Whether every segment is full.
  pub(crate) fn complete(&self) -> bool {
    self.0.iter().all(Segment::full)
  }
}

/// The scan of full `segments`, one codec of one segment at a time, asking `go_on` before each
/// whether to carry on; `None` once it says not to.
///
/// # Panics
///
/// When a segment is not full.
pub(crate) fn scan(segments: &Segments, mut go_on: impl FnMut() -> bool) -> Option<Scan> {
  assert!(segments.complete(), "every segment is full before a scan");
  let mut aac = Aac::new();
  let mut mp3 = Mp3::new();
  let mut aac_curves = Vec::with_capacity(SEGMENTS);
  let mut mp3_curves = Vec::with_capacity(SEGMENTS);
  for segment in &segments.0 {
    if !go_on() {
      return None;
    }
    aac_curves.push(in_file(&aac.curve(segment), segment.start));
    if !go_on() {
      return None;
    }
    mp3_curves.push(in_file(&mp3.curve(segment), segment.start));
  }
  Some(Scan {
    aac: codec_scan(&aac_curves),
    mp3: codec_scan(&mp3_curves),
  })
}

/// A segment's curve, whose entry `o` is for frames starting `o` samples into the segment
/// modulo the hop, turned into the file's offsets for a segment starting at frame `start`.
fn in_file(curve: &[f64], start: u64) -> Vec<f64> {
  let hop = curve.len();
  // below the hop, so it fits
  let shift = (start % hop as u64) as usize;
  (0..hop).map(|o| curve[(o + hop - shift) % hop]).collect()
}

/// The scan of one codec from its segments' curves in the file's offsets.
fn codec_scan(curves: &[Vec<f64>]) -> CodecScan {
  let hop = curves[0].len();
  let summed: Vec<f64> = (0..hop)
    .map(|o| curves.iter().map(|c| c[o]).sum())
    .collect();
  let (z, offset) = robust_z(&summed);
  CodecScan {
    offset,
    z,
    agree: curves.iter().filter(|c| robust_z(c).1 == offset).count(),
    segment_z: std::array::from_fn(|i| robust_z(&curves[i]).0),
  }
}

/// How far the lowest entry of `curve` sits below the median in robust standard deviations,
/// and its index: the first lowest.
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

/// The median of `values`, the mean of the middle two for an even count.
fn median(mut values: Vec<f64>) -> f64 {
  values.sort_by(f64::total_cmp);
  let half = values.len() / 2;
  if values.len().is_multiple_of(2) {
    (values[half - 1] + values[half]) / 2.0
  } else {
    values[half]
  }
}

/// The bits of an f64 exponent and the exponent that puts a value in [1, 2).
const EXPONENT_BITS: u64 = 0x7ff0_0000_0000_0000;
const ONE: u64 = 0x3ff0_0000_0000_0000;

/// The active count of `values`: the sum of log10(max(v^2 / t2, 1)).
///
/// The log of a product is the sum of the logs, so the values' exponents are summed and their
/// mantissas, each in [1, 2), multiplied 32 at a time, which stays under 2^32, with one log per
/// product.
fn active(values: &[f64], t2: f64) -> f64 {
  let mut exponents = 0i64;
  let mut logs = 0.0;
  for chunk in values.chunks(32) {
    let mut product = 1.0;
    for v in chunk {
      // t2 is a normal number, so every square at or above it is one too
      let bits = (v * v).max(t2).to_bits();
      exponents += ((bits & EXPONENT_BITS) >> 52) as i64 - 1023;
      product *= f64::from_bits((bits & !EXPONENT_BITS) | ONE);
    }
    logs += product.log2();
  }
  (exponents as f64 + logs - values.len() as f64 * t2.log2()) * std::f64::consts::LOG10_2
}

/// The AAC analysis: the KBD long window and an orthonormal MDCT, transformed in single
/// precision.
struct Aac {
  window: Vec<f32>,
  dct: DctIv,
  frame: Vec<f32>,
  folded: Vec<f32>,
  /// the transform of left and right
  values: [Vec<f32>; 2],
  /// the transform of left, right, mid and side in double precision, for the counts
  wide: [Vec<f64>; 4],
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
      wide: std::array::from_fn(|_| vec![0.0; AAC_HOP]),
    }
  }

  /// The active count summed over the long frames of `segment`, for each offset of the grid
  /// from its first sample. Every offset counts the same frames.
  fn curve(&mut self, segment: &Segment) -> Vec<f64> {
    let left: Vec<f32> = segment.left.iter().map(|s| *s as f32).collect();
    let right: Vec<f32> = segment.right.iter().map(|s| *s as f32).collect();
    let frames = (SEGMENT - 2 * AAC_HOP) / AAC_HOP;
    let t2 = f64::from((NOISE16 as f32).powi(2));
    let half = std::f64::consts::FRAC_1_SQRT_2;
    (0..AAC_HOP)
      .map(|offset| {
        (0..frames)
          .map(|j| {
            let start = offset + j * AAC_HOP;
            self.mdct(&left[start..start + 2 * AAC_HOP], 0);
            self.mdct(&right[start..start + 2 * AAC_HOP], 1);
            let [l, r, m, s] = &mut self.wide;
            for k in 0..AAC_HOP {
              l[k] = f64::from(self.values[0][k]);
              r[k] = f64::from(self.values[1][k]);
              m[k] = (l[k] + r[k]) * half;
              s[k] = (l[k] - r[k]) * half;
            }
            let lr = active(l, t2) + active(r, t2);
            let ms = active(m, t2) + active(s, t2);
            lr.min(ms)
          })
          .sum()
      })
      .collect()
  }

  /// The orthonormal MDCT of `input`, 2048 samples, windowed, into `values[into]`.
  fn mdct(&mut self, input: &[f32], into: usize) {
    for ((f, x), w) in self.frame.iter_mut().zip(input).zip(&self.window) {
      *f = x * w;
    }
    fold(&self.frame, &mut self.folded);
    self.dct.process(&self.folded, &mut self.values[into]);
  }
}

/// The input of the DCT-IV whose output is the MDCT of `frame`, 2N windowed samples:
/// (-c_r - d, a - b_r) for its quarters a, b, c, d, so that the DCT-IV gives
/// X_k = sqrt(2/N) sum_n x_n cos(pi/N (n + 1/2 + N/2)(k + 1/2)).
fn fold<T: Float>(frame: &[T], folded: &mut [T]) {
  let n = folded.len();
  let q = n / 2;
  for i in 0..q {
    folded[i] = -frame[3 * q - 1 - i] - frame[3 * q + i];
    folded[q + i] = frame[i] - frame[n - 1 - i];
  }
}

/// The Kaiser-Bessel-derived window of `length` samples with `alpha`: the square root of the
/// running sum of a Kaiser window of `length / 2 + 1` points with beta pi alpha over its total,
/// mirrored.
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

/// The modified Bessel function of the first kind of order 0, from its power series.
fn bessel_i0(x: f64) -> f64 {
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

/// The orthonormal DCT-IV of length N through a complex FFT of length N/2:
/// X_k = sqrt(2/N) sum_n u_n cos(pi/N (n + 1/2)(k + 1/2)).
struct DctIv {
  fft: Arc<dyn Fft<f32>>,
  /// exp(-i pi n / N), for the input
  pre: Vec<Complex<f32>>,
  /// sqrt(2/N) exp(-i pi (k + 1/4) / N), for the output
  post: Vec<Complex<f32>>,
  buffer: Vec<Complex<f32>>,
  scratch: Vec<Complex<f32>>,
}

impl DctIv {
  fn new(n: usize) -> Self {
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

  /// Writes the transform of `input` to `output`, both of length N.
  ///
  /// With z_n = u_2n + i u_(N-1-2n) and Y_k = sum_n z_n exp(-i pi (2n + 1/2)(2k + 1/2) / N),
  /// X_2k is the real part of Y_k and X_(N-1-2k) minus its imaginary part.
  fn process(&mut self, input: &[f32], output: &mut [f32]) {
    let n = input.len();
    for (i, (b, pre)) in self.buffer.iter_mut().zip(&self.pre).enumerate() {
      *b = Complex::new(input[2 * i], input[n - 1 - 2 * i]) * pre;
    }
    self
      .fft
      .process_with_scratch(&mut self.buffer, &mut self.scratch);
    for (k, (b, post)) in self.buffer.iter().zip(&self.post).enumerate() {
      let y = b * post;
      output[2 * k] = y.re;
      output[n - 1 - 2 * k] = -y.im;
    }
  }
}

/// Half of the MPEG audio synthesis window D in units of 2^-16, as ffmpeg's encoder carries it;
/// the analysis window C is D / 32.
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

/// The alias-reduction coefficients of layer III.
const ALIAS: [f64; 8] = [
  -0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037,
];

/// Subband samples per granule and the lines each subband's MDCT gives.
const SUBBAND_SAMPLES: usize = 36;
const LINES: usize = 18;
const SUBBANDS: usize = 32;

/// The MP3 analysis: the layer III hybrid filterbank, in double precision.
struct Mp3 {
  /// the analysis window C over 512 taps
  window: [f64; 512],
  /// cos((2i + 1)(k - 16) pi / 64) for subband i and k in 0..64
  matrix: Vec<[f64; 64]>,
  /// the sine window over 36 subband samples, for even subbands and with every odd sample
  /// negated for odd subbands
  windows: [[f64; SUBBAND_SAMPLES]; 2],
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
      window: d.map(|v| v / 65536.0 / 32.0),
      matrix: (0..SUBBANDS)
        .map(|i| {
          std::array::from_fn(|k| ((2 * i + 1) as f64 * (k as f64 - 16.0) * pi / 64.0).cos())
        })
        .collect(),
      windows: [1.0, -1.0].map(|odd| {
        std::array::from_fn(|n| {
          let sign = if n % 2 == 1 { odd } else { 1.0 };
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

  /// The subband samples of `x`, 32 for every 32 input samples: sample t reads the 512 inputs
  /// from 32 t on, the newest first.
  fn polyphase(&self, x: &[f64]) -> Vec<[f64; SUBBANDS]> {
    let count = (x.len() - 512) / 32 + 1;
    (0..count)
      .map(|t| {
        let newest = 32 * t + 511;
        let y: [f64; 64] = std::array::from_fn(|k| {
          (0..8)
            .map(|j| x[newest - (k + 64 * j)] * self.window[k + 64 * j])
            .sum()
        });
        std::array::from_fn(|i| self.matrix[i].iter().zip(&y).map(|(m, y)| m * y).sum())
      })
      .collect()
  }

  /// The 576 values of the granule whose 36 subband samples are `samples`, subband by subband.
  fn hybrid(&self, samples: &[[f64; SUBBANDS]], out: &mut [f64; MP3_HOP]) {
    let mut windowed = [0.0; SUBBAND_SAMPLES];
    let mut folded = [0.0; LINES];
    for sb in 0..SUBBANDS {
      // odd subbands have every odd subband sample negated before the MDCT
      for ((w, s), window) in windowed.iter_mut().zip(samples).zip(&self.windows[sb % 2]) {
        *w = s[sb] * window;
      }
      fold(&windowed, &mut folded);
      for (k, dct) in self.dct.iter().enumerate() {
        out[sb * LINES + k] = dct.iter().zip(&folded).map(|(d, f)| d * f).sum();
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

  /// The active count summed over the granules of `segment`, for each offset of the granule
  /// grid from its first sample. Every offset counts the same number of granules.
  fn curve(&mut self, segment: &Segment) -> Vec<f64> {
    let granules = (SEGMENT - MP3_HOP - 512 - SUBBAND_SAMPLES * 32) / MP3_HOP;
    let t2 = (NOISE16 * MP3_NOISE_GAIN).powi(2);
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let mut curve = vec![0.0; MP3_HOP];
    let (mut l, mut r) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    let (mut m, mut s) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    for phase in 0..32 {
      let left = self.polyphase(&segment.left[phase..]);
      let right = self.polyphase(&segment.right[phase..]);
      for g in 0..LINES {
        curve[phase + 32 * g] = (0..granules)
          .map(|granule| {
            let at = g + LINES * granule;
            self.hybrid(&left[at..at + SUBBAND_SAMPLES], &mut l);
            self.hybrid(&right[at..at + SUBBAND_SAMPLES], &mut r);
            for k in 0..MP3_HOP {
              m[k] = (l[k] + r[k]) * half;
              s[k] = (l[k] - r[k]) * half;
            }
            let lr = active(&l, t2) + active(&r, t2);
            let ms = active(&m, t2) + active(&s, t2);
            lr.min(ms)
          })
          .sum();
      }
    }
    curve
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
    assert_eq!(scan.agree, 3);
  }
}
