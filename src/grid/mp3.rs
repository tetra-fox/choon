//! the MP3 analysis: the layer III hybrid filterbank at a hop of 576, for the count of a grid's
//! offsets and for the quantizer's steps

use rayon::prelude::*;
use rustfft::num_traits::Float;

use super::mdct::fold_with;
use super::scan::active;
use super::segments::Segment;
use super::steps::{BandScratch, WHOLE_STEP, band_step};
use super::{MP3_HOP, Mp3Neighbours, NOISE16};

/// the rms of a hybrid filterbank value per unit rms of white input, which puts the MP3 noise
/// level at the 16-bit rounding noise of the filterbank's output
const MP3_NOISE_GAIN: f64 = 0.527_861_394_696_74;

/// the same for a subband sample of the polyphase filterbank, before the MDCT: the noise level
/// of the values layer II quantizes
const SUBBAND_NOISE_GAIN: f64 = 0.176_776_736_260_801_6;

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

/// the granules every offset of the grid holds in a segment of `samples` samples
fn granules(samples: usize) -> usize {
  (samples - MP3_HOP - 512 - SUBBAND_SAMPLES * 32) / MP3_HOP
}

/// the MP3 analysis: the layer III hybrid filterbank. the count of a grid's offsets runs it in
/// single precision, the quantizer's steps in double
pub(super) struct Mp3<T = f64> {
  /// the analysis window C over 512 taps in blocks of 64, each block's last tap first, which is
  /// the order the taps meet the input samples in
  window: [[T; 64]; 8],
  /// cos((2i + 1)(k - 16) pi / 64) for k in 0..64, by subband i
  matrix: [[T; SUBBANDS]; 64],
  /// the sine window over 36 subband samples, by subband: every odd sample of an odd subband is
  /// negated
  windows: [[T; SUBBANDS]; SUBBAND_SAMPLES],
  /// cos(pi/18 (n + 1/2)(k + 1/2)), the DCT-IV that gives the 18-line MDCT
  /// sum_n w_n z_n cos(pi/72 (2n + 1 + 18)(2k + 1)) of the folded windowed samples, by line
  dct: Vec<[T; LINES]>,
  /// cs and ca of each butterfly
  butterflies: [(T, T); 8],
}

impl<T: Float> Mp3<T> {
  pub(super) fn new() -> Self {
    let narrowed = |value: f64| T::from(value).expect("a float holds a number of size one");
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
      window: std::array::from_fn(|j| {
        std::array::from_fn(|i| narrowed(d[64 * j + 63 - i] / 65536.0 / 32.0))
      }),
      matrix: std::array::from_fn(|k| {
        std::array::from_fn(|i| {
          narrowed(((2 * i + 1) as f64 * (k as f64 - 16.0) * pi / 64.0).cos())
        })
      }),
      windows: std::array::from_fn(|n| {
        std::array::from_fn(|sb| {
          let sign = if n % 2 == 1 && sb % 2 == 1 { -1.0 } else { 1.0 };
          narrowed(sign * (pi / 36.0 * (n as f64 + 0.5)).sin())
        })
      }),
      dct: (0..LINES)
        .map(|k| {
          std::array::from_fn(|n| narrowed((pi / 18.0 * (n as f64 + 0.5) * (k as f64 + 0.5)).cos()))
        })
        .collect(),
      butterflies: ALIAS.map(|c| {
        let norm = (1.0 + c * c).sqrt();
        (narrowed(1.0 / norm), narrowed(c / norm))
      }),
    }
  }

  /// the subband samples of `x`, 32 for every 32 input samples: sample t reads the 512 inputs
  /// from 32 t on, the newest first.
  ///
  /// every sum here and in [`Mp3::hybrid`] adds its terms in the order of the formula, in one
  /// running total per output, so the inner loops run over the outputs side by side.
  fn polyphase(&self, x: &[T]) -> Vec<[T; SUBBANDS]> {
    let count = (x.len() - 512) / 32 + 1;
    (0..count)
      .map(|t| {
        // with the newest input at 32 t + 511, y[63 - k] is the sum over j of
        // x[newest - k - 64 j] window[k + 64 j]
        let (blocks, _) = x[32 * t..32 * t + 512].as_chunks::<64>();
        let mut y = [T::zero(); 64];
        for (taps, x) in self.window.iter().zip(blocks.iter().rev()) {
          for ((y, x), tap) in y.iter_mut().zip(x).zip(taps) {
            *y = *y + *x * *tap;
          }
        }
        let mut subbands = [T::zero(); SUBBANDS];
        for (row, y) in self.matrix.iter().zip(y.iter().rev()) {
          for (subband, m) in subbands.iter_mut().zip(row) {
            *subband = *subband + *m * *y;
          }
        }
        subbands
      })
      .collect()
  }

  /// the 576 values of the granule whose 36 subband samples are `samples`, subband by subband
  fn hybrid(&self, samples: &[[T; SUBBANDS]], out: &mut [T; MP3_HOP]) {
    // each subband sample with its window, multiplied as the two are folded
    let windowed: [_; SUBBAND_SAMPLES] = std::array::from_fn(|n| (&samples[n], &self.windows[n]));
    let mut folded = [[T::zero(); SUBBANDS]; LINES];
    fold_with(
      &windowed,
      &mut folded,
      |c, d| std::array::from_fn(|sb| -(c.0[sb] * c.1[sb]) - d.0[sb] * d.1[sb]),
      |a, b| std::array::from_fn(|sb| a.0[sb] * a.1[sb] - b.0[sb] * b.1[sb]),
    );
    for (k, dct) in self.dct.iter().enumerate() {
      for from in (0..SUBBANDS).step_by(SIDE_BY_SIDE) {
        let mut line = [T::zero(); SIDE_BY_SIDE];
        for (d, folded) in dct.iter().zip(&folded) {
          for (l, f) in line.iter_mut().zip(&folded[from..from + SIDE_BY_SIDE]) {
            *l = *l + *d * *f;
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
        out[sb * LINES + LINES - 1 - i] = lo * *cs + hi * *ca;
        out[(sb + 1) * LINES + i] = hi * *cs - lo * *ca;
      }
    }
  }
}

impl Mp3<f32> {
  /// the active count summed over the granules of `segment`, for each offset of the granule
  /// grid from its first sample. every offset counts the same number of granules. the 32
  /// phases of the polyphase filterbank are tasks on rayon's thread pool.
  ///
  /// beside it, the active count of the subband samples themselves at each of the 32 phases:
  /// layer II quantizes those, so its decode counts fewest at the phase of its frames
  pub(super) fn curves(&self, segment: &Segment) -> (Vec<f64>, Vec<f64>) {
    let left: Vec<f32> = segment.left.iter().map(|s| *s as f32).collect();
    let right: Vec<f32> = segment.right.iter().map(|s| *s as f32).collect();
    let granules = granules(left.len());
    let phases: Vec<([f64; LINES], f64)> = (0..32)
      .into_par_iter()
      .map(|phase| self.phase(&left[phase..], &right[phase..], granules))
      .collect();
    let hybrid = (0..MP3_HOP)
      .map(|offset| phases[offset % 32].0[offset / 32])
      .collect();
    (hybrid, phases.iter().map(|phase| phase.1).collect())
  }

  /// the counts over `granules` granules of the offsets 32 g from the first sample of `left`
  /// and `right`, for g in 0..18: the granules that start at subband sample g of the
  /// filterbank run from there. the two are a segment's channels from one of the 32 phases on.
  /// with them, the count of the phase's subband samples over as many as the granules span
  fn phase(&self, left: &[f32], right: &[f32], granules: usize) -> ([f64; LINES], f64) {
    let t2 = (NOISE16 * MP3_NOISE_GAIN).powi(2) as f32;
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let (mut l, mut r) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    let (mut m, mut s) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    let left = self.polyphase(left);
    let right = self.polyphase(right);
    // layer II has no mid and side, so the two channels are counted as they are
    let subband_t2 = (NOISE16 * SUBBAND_NOISE_GAIN).powi(2) as f32;
    let spanned = LINES * granules;
    let subbands = active(left[..spanned].as_flattened(), subband_t2)
      + active(right[..spanned].as_flattened(), subband_t2);
    let mut counts = [0.0; LINES];
    for (g, count) in counts.iter_mut().enumerate() {
      *count = (0..granules)
        .map(|granule| {
          let at = g + LINES * granule;
          self.hybrid(&left[at..at + SUBBAND_SAMPLES], &mut l);
          self.hybrid(&right[at..at + SUBBAND_SAMPLES], &mut r);
          for ((m, s), (l, r)) in m.iter_mut().zip(&mut s).zip(l.iter().zip(&r)) {
            *m = (l + r) * half;
            *s = (l - r) * half;
          }
          let apart = active(&l, t2) + active(&r, t2);
          let joint = active(&m, t2) + active(&s, t2);
          apart.min(joint)
        })
        .sum();
    }
    (counts, subbands)
  }
}

/// the value of a line quantized to 1 at a step of one, with samples from -1 to 1
const MP3_UNIT: f64 = 9.0;
/// the edges of a long block's scalefactor bands in lines at `rate`, for the rates MPEG-1
/// layer III codes
pub(super) fn mp3_bands(rate: u32) -> Option<&'static [usize; 23]> {
  match rate {
    44_100 => Some(&[
      0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 52, 62, 74, 90, 110, 134, 162, 196, 238, 288, 342, 418,
      576,
    ]),
    48_000 => Some(&[
      0, 4, 8, 12, 16, 20, 24, 30, 36, 42, 50, 60, 72, 88, 106, 128, 156, 190, 230, 276, 330, 384,
      576,
    ]),
    32_000 => Some(&[
      0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 54, 66, 82, 102, 126, 156, 194, 240, 296, 364, 448, 550,
      576,
    ]),
    _ => None,
  }
}

/// the bands of one granule: how many were tested, sit on one step, and on a whole one, and
/// how the whole steps of neighbouring bands compare
pub(super) struct GranuleSteps {
  pub(super) tested: usize,
  pub(super) stepped: usize,
  pub(super) whole: usize,
  pub(super) neighbours: Mp3Neighbours,
}

impl Mp3<f64> {
  /// the bands of each of `segment`'s granules on the grid at `offset` from its first sample,
  /// each granule as left and right or as mid and side, whichever has more bands on whole
  /// steps
  pub(super) fn steps(
    &self,
    segment: &Segment,
    offset: usize,
    bands: &[usize; 23],
  ) -> Vec<GranuleSteps> {
    let mut scratch = BandScratch::default();
    let granules = granules(segment.left.len());
    let (phase, from) = (offset % 32, offset / 32);
    let left = self.polyphase(&segment.left[phase..]);
    let right = self.polyphase(&segment.right[phase..]);
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let (mut l, mut r) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    let (mut m, mut s) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    let mut read = Vec::with_capacity(granules);
    for granule in 0..granules {
      let at = from + LINES * granule;
      self.hybrid(&left[at..at + SUBBAND_SAMPLES], &mut l);
      self.hybrid(&right[at..at + SUBBAND_SAMPLES], &mut r);
      for k in 0..MP3_HOP {
        m[k] = (l[k] + r[k]) * half;
        s[k] = (l[k] - r[k]) * half;
      }
      let mut count = |pair: [&[f64; MP3_HOP]; 2]| {
        let (mut tested, mut stepped, mut whole) = (0, 0, 0);
        let mut neighbours = Mp3Neighbours::default();
        for lines in pair {
          // the whole step of the band under the one looked at
          let mut under = None;
          for (at, band) in bands.windows(2).enumerate() {
            let lines = &lines[band[0]..band[1]];
            let step = band_step(lines, NOISE16 * MP3_NOISE_GAIN, &mut scratch);
            tested += usize::from(step.tested());
            let fourths = step.step().and_then(|step| {
              stepped += 1;
              let fourths = 4.0 * (step / MP3_UNIT).log2();
              ((fourths - fourths.round()).abs() < WHOLE_STEP).then(|| {
                let top = lines.iter().fold(0.0f64, |top, line| top.max(line.abs()));
                let largest = (top / step).powf(0.75).round();
                // a step within 2^-100 and 2^100, so its quarter powers fit
                let fourths = fourths.round() as i32;
                neighbours.take(at, fourths, largest, under);
                fourths
              })
            });
            whole += usize::from(fourths.is_some());
            under = fourths;
          }
        }
        GranuleSteps {
          tested,
          stepped,
          whole,
          neighbours,
        }
      };
      let (apart, joint) = (count([&l, &r]), count([&m, &s]));
      read.push(if joint.whole > apart.whole {
        joint
      } else {
        apart
      });
    }
    read
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
}
