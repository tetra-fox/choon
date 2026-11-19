//! the AAC analysis: a long block's 2048-point MDCT at a hop of 1024 under either of the
//! format's two windows, for the count of a grid's offsets and for the quantizer's steps

use rayon::prelude::*;

use super::mdct::{DctIv, fold};
use super::scan::active;
use super::segments::Segment;
use super::steps::{BandScratch, WHOLE_STEP, band_step};
use super::{AAC_HOP, NOISE16};

/// the tasks a job's offsets are split into at most, so that one file's scan can fill the pool
const TASKS: usize = 32;

/// the window of an AAC long block
#[derive(Clone, Copy)]
pub(crate) enum Window {
  /// Kaiser-Bessel derived, with an alpha of 4
  Kbd,
  Sine,
}

impl Window {
  /// the window over the 2048 samples of a long block
  pub(crate) fn samples(self) -> Vec<f32> {
    self.exact().into_iter().map(|w| w as f32).collect()
  }

  /// the same in double precision
  fn exact(self) -> Vec<f64> {
    let length = 2 * AAC_HOP;
    match self {
      Self::Kbd => kbd_window(length, 4.0),
      Self::Sine => (0..length)
        .map(|n| (std::f64::consts::PI * (n as f64 + 0.5) / length as f64).sin())
        .collect(),
    }
  }
}

/// the AAC analysis: a long block's window and an orthonormal MDCT, transformed in single
/// precision
pub(super) struct Aac {
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
  fn new(window: Window) -> Self {
    Self {
      window: window.samples(),
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
  pub(super) fn curve(segment: &Segment, window: Window) -> Vec<f64> {
    let left: Vec<f32> = segment.left.iter().map(|s| *s as f32).collect();
    let right: Vec<f32> = segment.right.iter().map(|s| *s as f32).collect();
    (0..AAC_HOP)
      .into_par_iter()
      .with_min_len(AAC_HOP / TASKS)
      .map_init(
        || Self::new(window),
        |aac, offset| aac.count(&left, &right, offset),
      )
      .collect()
  }

  /// the active count summed over the long frames of `left` and `right` on the grid at `offset`
  fn count(&mut self, left: &[f32], right: &[f32], offset: usize) -> f64 {
    // the frames every offset holds
    let frames = (left.len() + 1 - 2 * AAC_HOP) / AAC_HOP;
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

/// the edges of a long block's scalefactor bands in lines at `rate`, for the rates the steps
/// are read at
pub(crate) fn aac_bands(rate: u32) -> Option<&'static [usize; 50]> {
  matches!(rate, 44_100 | 48_000).then_some(&[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 1024,
  ])
}

/// the AAC analysis the steps are read with: a long block's window and an orthonormal MDCT in
/// double precision, whose rounding stays far under the samples'
pub(super) struct AacStepper {
  window: Vec<f64>,
  dct: DctIv<f64>,
  frame: Vec<f64>,
  folded: Vec<f64>,
}

impl AacStepper {
  pub(super) fn new(window: Window) -> Self {
    Self {
      window: window.exact(),
      dct: DctIv::new(AAC_HOP),
      frame: vec![0.0; 2 * AAC_HOP],
      folded: vec![0.0; AAC_HOP],
    }
  }

  /// the bands of `segment`'s long frames on the grid at `offset` from the file's first
  /// sample: how many were tested, sit on one step, and on a whole one. each band is taken as
  /// left and right or as mid and side, whichever has more whole steps.
  ///
  /// in this transform's scale a step is a whole power of the fourth root of two as it is
  /// coded, and a mid or side value is the coded one times the square root of two, two such
  /// powers.
  pub(super) fn steps(
    &mut self,
    segment: &Segment,
    offset: usize,
    bands: &[usize; 50],
  ) -> [usize; 3] {
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let mut counts = [0; 3];
    let mut lines = [const { Vec::new() }; 4];
    let mut scratch = BandScratch::default();
    // below the hop, so it fits
    let start = (segment.start % AAC_HOP as u64) as usize;
    let offset = (offset + AAC_HOP - start) % AAC_HOP;
    // the frames every offset holds, as the count takes them
    let frames = (segment.left.len() + 1 - 2 * AAC_HOP) / AAC_HOP;
    for frame in 0..frames {
      let at = offset + frame * AAC_HOP;
      let [l, r, m, s] = &mut lines;
      self.mdct(&segment.left[at..at + 2 * AAC_HOP], l);
      self.mdct(&segment.right[at..at + 2 * AAC_HOP], r);
      m.clear();
      m.extend(l.iter().zip(r.iter()).map(|(l, r)| (l + r) * half));
      s.clear();
      s.extend(l.iter().zip(r.iter()).map(|(l, r)| (l - r) * half));
      for band in bands.windows(2) {
        // tested, stepped and whole, over the two channels of a pair
        let mut count = |pair: [&Vec<f64>; 2]| {
          pair.iter().fold([0; 3], |[tested, stepped, whole], lines| {
            let step = band_step(&lines[band[0]..band[1]], NOISE16, &mut scratch);
            let fourths = step.step().map(|step| 4.0 * step.log2());
            [
              tested + usize::from(step.tested()),
              stepped + usize::from(fourths.is_some()),
              whole + usize::from(fourths.is_some_and(|f| (f - f.round()).abs() < WHOLE_STEP)),
            ]
          })
        };
        let [l, r, m, s] = &lines;
        let (apart, joint) = (count([l, r]), count([m, s]));
        let more = if joint[2] > apart[2] { joint } else { apart };
        for (count, more) in counts.iter_mut().zip(more) {
          *count += more;
        }
      }
    }
    counts
  }

  /// the orthonormal MDCT of `input`, 2048 samples, windowed, into `lines`
  fn mdct(&mut self, input: &[f64], lines: &mut Vec<f64>) {
    for ((f, x), w) in self.frame.iter_mut().zip(input).zip(&self.window) {
      *f = x * w;
    }
    fold(&self.frame, &mut self.folded);
    lines.resize(AAC_HOP, 0.0);
    self.dct.process(&self.folded, lines);
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

#[cfg(test)]
mod tests {
  use super::*;

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
}
