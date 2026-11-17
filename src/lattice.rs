//! the band energies of Opus's CELT layer, looked for in decoded samples.
//!
//! CELT codes each band's energy in each frame as a step from a prediction: half the band's
//! energy a frame earlier, plus a sum over the frame's lower bands. the step is a whole number
//! of 6 dB refined by a few bits. the decoder scales each band's unit-norm shape by the energy,
//! runs the inverse MDCT and a one-pole de-emphasis. so with the de-emphasis undone and the MDCT
//! run again with frames where the encoder put them, each band's energy comes back, and taking
//! the prediction off it leaves a number on the quantizer's lattice.
//!
//! only a frame coded as one long block with the pitch post-filter off, after another such
//! frame, comes back exactly. the analysis is of 20 ms frames at 48 kHz; samples at 44.1 kHz
//! are brought back to 48 kHz first.

use rayon::prelude::*;

use crate::grid::{DctIv, bessel_i0, fold};

/// a 20 ms frame at 48 kHz
pub(crate) const FRAME: usize = 960;
/// the rate CELT runs at
const RATE: u32 = 48_000;
/// the samples over which a frame's window rises and falls
const OVERLAP: usize = 120;
/// the encoder's pre-emphasis at 48 kHz, which the decoder's de-emphasis inverts
const PRE_EMPHASIS: f64 = 0.850_006_103_5;
/// the bands' edges in bins of a 20 ms frame, up to 20 kHz
const EDGES: [usize; BANDS + 1] = [
  0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 272, 320, 384, 480, 624, 800,
];
const BANDS: usize = 21;
/// the mean energy the codec takes off each band before it codes it, in units of 6 dB
const MEANS: [f64; BANDS] = [
  6.4375, 6.25, 5.75, 5.3125, 5.0625, 4.8125, 4.5, 4.375, 4.875, 4.6875, 4.5625, 4.4375, 4.875,
  4.625, 4.3125, 4.5, 4.375, 4.625, 4.75, 4.4375, 3.75,
];
/// the decoder's scale in units of 6 dB: samples of 2^15 from an inverse MDCT that gains
/// sqrt(480), 15 - log2(480) / 2
const SCALE: f64 = 10.546_554_702_195_74;
/// how much of a band's energy a frame earlier predicts it, and how much of each coarse step is
/// kept out of the sum over the lower bands, for 20 ms frames
const ALPHA: f64 = 0.5;
const BETA: f64 = 6554.0 / 32768.0;
/// the lowest energy a frame earlier that predicts, in units of 6 dB
const LOWEST_PREDICTING: f64 = -9.0;
/// the lattice a step is tested against: 1/32 of 6 dB holds a coarse step with up to three
/// refining bits
const LATTICE: f64 = 32.0;
/// how far from the lattice a step still sits on it, in units of 6 dB
const NEAR: f64 = 0.003;
/// a band is tested when its amplitude stands this many times over the rounding noise of one of
/// its transform values, in its frame and the frame before
const LOUD: f64 = 600.0;
/// a channel is judged on at least this many tested bands
const MIN_TESTED: usize = 12;
/// the share of a channel's tested bands that sit on the lattice in a frame that does
const SHARE: f64 = 0.8;

/// where the frames of a decode sit when nothing was cut from its start, from the file's first
/// sample: libopus's encoders delay by 312 samples, and a frame's window reaches 420 samples back
/// from where its rise starts
pub(crate) const UNCUT_OFFSET: usize = (2 * FRAME - 312 - 420) % FRAME;
/// the most windows the pass at that offset reads
const UNCUT_WINDOWS: usize = 60;

/// input samples on each side of an output sample that the resampling filter reads
const HALF: usize = 64;
/// the Kaiser window's beta of the resampling filter
const KAISER_BETA: f64 = 12.0;
/// the output samples at each end of a resampled stretch that lack input the filter reads
const EDGE: usize = 70;

/// whether samples at `rate` are looked at: 48 kHz, and 44.1 kHz brought back to it
pub(crate) fn reads(rate: u32) -> bool {
  rate == RATE || rate == 44_100
}

/// the rounding noise of a transform value in each band of pre-emphasized `bits`-bit samples
fn noise(bits: u32) -> [f64; BANDS] {
  let sigma = 2f64.powi(1 - bits.min(24) as i32) / 12f64.sqrt();
  std::array::from_fn(|band| {
    let bins = EDGES[band]..EDGES[band + 1];
    let width = bins.len() as f64;
    let gain: f64 = bins
      .map(|bin| {
        let w = std::f64::consts::PI * (bin as f64 + 0.5) / FRAME as f64;
        // |1 - p e^(-jw)|^2
        1.0 - 2.0 * PRE_EMPHASIS * w.cos() + PRE_EMPHASIS * PRE_EMPHASIS
      })
      .sum();
    sigma * (gain / width).sqrt()
  })
}

/// `samples` with the encoder's pre-emphasis; the first sample has nothing before it
fn pre_emphasized(samples: impl Iterator<Item = f64>) -> Vec<f32> {
  let mut before = 0.0;
  samples
    .map(|sample| {
      let out = sample - PRE_EMPHASIS * before;
      before = sample;
      out as f32
    })
    .collect()
}

/// the CELT analysis of long frames: the low-overlap window and an orthonormal MDCT
struct Celt {
  /// the window's rise over the overlap
  rise: Vec<f32>,
  dct: DctIv,
  frame: Vec<f32>,
  folded: Vec<f32>,
  values: Vec<f32>,
  noise: [f64; BANDS],
}

/// one frame's bands in one channel: each band's energy in units of 6 dB as the codec counts it,
/// and whether it stands over the rounding noise
#[derive(Clone, Copy)]
struct Bands {
  energy: [f64; BANDS],
  loud: [bool; BANDS],
}

impl Celt {
  fn new(bits: u32) -> Self {
    let rise = (0..OVERLAP)
      .map(|i| {
        let inner = (0.5 * std::f64::consts::PI * (i as f64 + 0.5) / OVERLAP as f64).sin();
        (0.5 * std::f64::consts::PI * inner * inner).sin() as f32
      })
      .collect();
    Self {
      rise,
      dct: DctIv::new(FRAME),
      frame: vec![0.0; 2 * FRAME],
      folded: vec![0.0; FRAME],
      values: vec![0.0; FRAME],
      noise: noise(bits),
    }
  }

  /// the bands of the frame whose 1920 pre-emphasized samples are `samples`
  fn bands(&mut self, samples: &[f32]) -> Bands {
    // the window is zero, rises over the overlap, is one, falls and is zero again
    let flat = (FRAME - OVERLAP) / 2;
    let (rise_from, fall_from) = (flat, 2 * FRAME - flat - OVERLAP);
    self.frame[..rise_from].fill(0.0);
    self.frame[fall_from + OVERLAP..].fill(0.0);
    self.frame[rise_from + OVERLAP..fall_from]
      .copy_from_slice(&samples[rise_from + OVERLAP..fall_from]);
    for (i, w) in self.rise.iter().enumerate() {
      self.frame[rise_from + i] = samples[rise_from + i] * w;
      self.frame[fall_from + OVERLAP - 1 - i] = samples[fall_from + OVERLAP - 1 - i] * w;
    }
    fold(&self.frame, &mut self.folded);
    self.dct.process(&self.folded, &mut self.values);
    let mut bands = Bands {
      energy: [0.0; BANDS],
      loud: [false; BANDS],
    };
    for band in 0..BANDS {
      let squares: f64 = self.values[EDGES[band]..EDGES[band + 1]]
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum();
      bands.energy[band] = 0.5 * (squares + 1e-300).log2() - MEANS[band] + SCALE;
      bands.loud[band] = squares.sqrt() >= LOUD * self.noise[band];
    }
    bands
  }

  /// how many frames of `channels`, pre-emphasized, sit on the lattice with the frame grid at
  /// `offset` from their first sample: frames after the first in which every channel with
  /// enough tested bands has its share of them on the lattice, and one channel at least has
  /// enough. a channel is transformed only for a frame that the channels before it leave open
  fn count(&mut self, channels: &[Vec<f32>], offset: usize, frames: usize) -> usize {
    // each channel's bands in the frame before and in this one, where they were needed
    let mut bands: Vec<[Option<Bands>; 2]> = vec![[None; 2]; channels.len()];
    let mut count = 0;
    for frame in 1..frames {
      let mut judged = false;
      let mut good = true;
      for (channel, bands) in channels.iter().zip(&mut bands) {
        let [before, this] = bands;
        let at = |frame: usize| {
          let start = offset + frame * FRAME;
          &channel[start..start + 2 * FRAME]
        };
        let before = match *before {
          Some(before) => before,
          None => self.bands(at(frame - 1)),
        };
        let now = self.bands(at(frame));
        *this = Some(now);
        let (tested, hits) = on_lattice(&before, &now);
        if tested >= MIN_TESTED {
          judged = true;
          good = hits as f64 >= SHARE * tested as f64;
          if !good {
            break;
          }
        }
      }
      if judged && good {
        count += 1;
      }
      for [before, this] in &mut bands {
        *before = this.take();
      }
    }
    count
  }
}

/// how many bands of a frame, after the frame `before`, could be tested, and how many of their
/// steps from the prediction sit on the lattice
fn on_lattice(before: &Bands, bands: &Bands) -> (usize, usize) {
  let (mut tested, mut hits) = (0, 0);
  // the sum over the lower bands of what their coarse steps leave
  let mut lower = 0.0;
  for band in 0..BANDS {
    let predicted = ALPHA * before.energy[band].max(LOWEST_PREDICTING) + lower;
    let step = bands.energy[band] - predicted;
    lower += (1.0 - BETA) * step.round_ties_even();
    if bands.loud[band] && before.loud[band] {
      tested += 1;
      let scaled = step * LATTICE;
      if (scaled - scaled.round_ties_even()).abs() / LATTICE < NEAR {
        hits += 1;
      }
    }
  }
  (tested, hits)
}

/// the frames a stretch of `length` samples holds at every offset
fn frames_in(length: usize) -> usize {
  (length / FRAME).saturating_sub(3)
}

/// one stretch at 48 kHz: its channels, pre-emphasized, and where its first sample sits on the
/// file's 48 kHz lattice
struct Stretch {
  channels: Vec<Vec<f32>>,
  start: u64,
}

impl Stretch {
  /// the stretch of `channels` at `rate` whose first sample is frame `first` of the file;
  /// `None` when nothing is left of it at 48 kHz
  fn of(channels: &[&[f64]], first: u64, rate: u32) -> Option<Self> {
    if rate == RATE {
      return Some(Self {
        channels: channels
          .iter()
          .map(|c| pre_emphasized(c.iter().copied()))
          .collect(),
        start: first,
      });
    }
    let resampler = Resampler::get(rate);
    // start on a sample that is also one of the 48 kHz lattice
    let down = resampler.down as u64;
    let skip = ((down - first % down) % down) as usize;
    let resampled: Vec<Vec<f64>> = channels
      .iter()
      .map(|c| resampler.run(c.get(skip..).unwrap_or(&[])))
      .collect();
    let length = resampled.first()?.len();
    if length <= 2 * EDGE {
      return None;
    }
    Some(Self {
      channels: resampled
        .iter()
        .map(|c| pre_emphasized(c[EDGE..length - EDGE].iter().copied()))
        .collect(),
      start: (first + skip as u64) / down * resampler.up as u64 + EDGE as u64,
    })
  }

  /// for each offset of the file's frame grid, how many of the stretch's frames sit on the
  /// lattice. runs of offsets are tasks on rayon's thread pool
  fn curve(&self, bits: u32) -> Vec<usize> {
    let frames = frames_in(self.channels[0].len());
    let own: Vec<usize> = (0..FRAME)
      .into_par_iter()
      .with_min_len(FRAME / 32)
      .map_init(
        || Celt::new(bits),
        |celt, offset| celt.count(&self.channels, offset, frames),
      )
      .collect();
    // below the frame, so it fits
    let shift = (self.start % FRAME as u64) as usize;
    (0..FRAME)
      .map(|o| own[(o + FRAME - shift) % FRAME])
      .collect()
  }
}

/// for each offset of the file's frame grid at 48 kHz, how many frames of the stretch of
/// `channels` at `rate`, whose first sample is frame `first` of the file, sit on the lattice
pub(crate) fn curve(channels: &[&[f64]], first: u64, rate: u32, bits: u32) -> Vec<usize> {
  match Stretch::of(channels, first, rate) {
    Some(stretch) => stretch.curve(bits),
    None => vec![0; FRAME],
  }
}

/// counts the frames on the lattice in the windows a file is measured at, with the frame grid
/// of a decode whose start was not cut
pub(crate) struct Uncut {
  celt: Celt,
  rate: u32,
  windows: usize,
  frames: usize,
}

impl Uncut {
  pub(crate) fn new(rate: u32, bits: u32) -> Self {
    Self {
      celt: Celt::new(bits),
      rate,
      windows: 0,
      frames: 0,
    }
  }

  /// counts the frames of `window`, one vector per analyzed channel, whose first sample is
  /// frame `first` of the file. windows past the most it reads are left out
  pub(crate) fn take(&mut self, first: u64, window: &[Vec<f32>]) {
    if self.windows >= UNCUT_WINDOWS {
      return;
    }
    self.windows += 1;
    let doubled: Vec<Vec<f64>> = window
      .iter()
      .map(|c| c.iter().map(|s| f64::from(*s)).collect())
      .collect();
    let channels: Vec<&[f64]> = doubled.iter().map(Vec::as_slice).collect();
    let Some(stretch) = Stretch::of(&channels, first, self.rate) else {
      return;
    };
    let length = stretch.channels[0].len();
    // the first frame of the grid at or after the stretch's first sample
    let grid = UNCUT_OFFSET as u64;
    let frame = FRAME as u64;
    let offset = ((grid + frame - stretch.start % frame) % frame) as usize;
    let frames = (length.saturating_sub(offset) / FRAME).saturating_sub(1);
    self.frames += self.celt.count(&stretch.channels, offset, frames);
  }

  /// the frames counted on the lattice
  pub(crate) fn frames(&self) -> usize {
    self.frames
  }
}

/// a polyphase windowed-sinc filter from a lower rate to 48 kHz. output sample j sits at input
/// time j * down / up from the input's first sample, so a lattice the two rates share stays
/// shared
struct Resampler {
  up: usize,
  down: usize,
  /// for each of the `up` phases, the taps that meet the input from the oldest sample read to
  /// the newest, 2 * HALF + 1 of them
  phases: Vec<Vec<f64>>,
}

impl Resampler {
  /// the filter from `rate`, made once
  fn get(rate: u32) -> &'static Self {
    static FROM_44100: std::sync::LazyLock<Resampler> =
      std::sync::LazyLock::new(|| Resampler::new(44_100));
    assert_eq!(rate, 44_100, "only 44.1 kHz is brought to 48 kHz");
    &FROM_44100
  }

  fn new(rate: u32) -> Self {
    let common = gcd(rate as usize, RATE as usize);
    let (up, down) = (RATE as usize / common, rate as usize / common);
    // the filter at the rate both share: a sinc cut at the lower rate's band edge under a
    // Kaiser window, 2 * HALF * up + 1 taps
    let count = 2 * HALF * up + 1;
    let middle = (count - 1) as f64 / 2.0;
    let cutoff = 1.0 / up as f64;
    let taps: Vec<f64> = (0..count)
      .map(|n| {
        let m = n as f64 - middle;
        let x = std::f64::consts::PI * cutoff * m;
        let sinc = if m == 0.0 { 1.0 } else { x.sin() / x };
        let r = m / middle;
        let kaiser =
          bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(KAISER_BETA);
        cutoff * sinc * kaiser
      })
      .collect();
    // unity gain for the `up` phases together
    let gain = up as f64 / taps.iter().sum::<f64>();
    // y_j is the sum over i of x_i h[j down + HALF up - i up]: tap `phase + k up` meets the
    // sample k before the newest
    let phases = (0..up)
      .map(|phase| {
        let mut taps: Vec<f64> = (0..=2 * HALF)
          .map(|k| taps.get(phase + k * up).map_or(0.0, |tap| tap * gain))
          .collect();
        taps.reverse();
        taps
      })
      .collect();
    Self { up, down, phases }
  }

  /// `input` at 48 kHz: floor(len * up / down) samples, with silence past both ends
  fn run(&self, input: &[f64]) -> Vec<f64> {
    let (up, down) = (self.up, self.down);
    // silence on both sides, so every output reads 2 * HALF + 1 samples
    let mut padded = vec![0.0; input.len() + 4 * HALF];
    padded[2 * HALF..2 * HALF + input.len()].copy_from_slice(input);
    (0..input.len() * up / down)
      .map(|j| {
        let at = j * down + HALF * up;
        let (newest, phase) = (at / up, at % up);
        // the newest sample read is `newest` of the input, 2 * HALF further in `padded`
        dot(&self.phases[phase], &padded[newest..=newest + 2 * HALF])
      })
      .collect()
  }
}

/// the products `dot` adds up side by side
const SIDE_BY_SIDE: usize = 8;

/// the sum of the products of `a` and `b`, added up in several running totals so that the
/// additions do not wait on each other
fn dot(a: &[f64], b: &[f64]) -> f64 {
  let (a_runs, a_rest) = a.as_chunks::<SIDE_BY_SIDE>();
  let (b_runs, b_rest) = b.as_chunks::<SIDE_BY_SIDE>();
  let mut totals = [0.0; SIDE_BY_SIDE];
  for (a, b) in a_runs.iter().zip(b_runs) {
    for ((total, a), b) in totals.iter_mut().zip(a).zip(b) {
      *total += a * b;
    }
  }
  let rest: f64 = a_rest.iter().zip(b_rest).map(|(a, b)| a * b).sum();
  totals.iter().sum::<f64>() + rest
}

fn gcd(a: usize, b: usize) -> usize {
  if b == 0 { a } else { gcd(b, a % b) }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_step_is_on_the_lattice_when_its_prediction_is_taken_off() {
    let quiet = Bands {
      energy: [-2.0; BANDS],
      loud: [true; BANDS],
    };
    // each band a coarse step of its own with a refinement of a quarter or an eighth, built
    // as the decoder builds it
    let mut bands = quiet;
    let mut lower = 0.0;
    for band in 0..BANDS {
      let coarse = (band % 5) as f64 - 2.0;
      let fine = if band % 2 == 0 { 0.25 } else { -0.125 };
      bands.energy[band] = ALPHA * quiet.energy[band] + lower + coarse + fine;
      lower += (1.0 - BETA) * coarse;
    }
    assert_eq!(on_lattice(&quiet, &bands), (BANDS, BANDS));
    // a frame 1.3 dB louder than the codec made it has every step off the lattice
    let mut louder = bands;
    for energy in &mut louder.energy {
      *energy += 0.21;
    }
    assert_eq!(on_lattice(&quiet, &louder), (BANDS, 0));
    // bands under the noise in either frame are not tested
    let mut faint = bands;
    faint.loud[3] = false;
    faint.loud[7] = false;
    assert_eq!(on_lattice(&quiet, &faint), (BANDS - 2, BANDS - 2));
  }

  #[test]
  fn resampling_keeps_a_tone_and_its_place_in_time() {
    // 1 kHz at 44.1 kHz, read at 48 kHz: sample j is the tone at j / 48000 s
    let tone = |n: usize, rate: f64| (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / rate).sin();
    let input: Vec<f64> = (0..4410).map(|n| tone(n, 44_100.0)).collect();
    let output = Resampler::get(44_100).run(&input);
    assert_eq!(output.len(), 4800);
    for (j, sample) in output.iter().enumerate().take(4800 - EDGE).skip(EDGE) {
      assert!((sample - tone(j, 48_000.0)).abs() < 1e-5, "{j}: {sample}");
    }
  }

  #[test]
  fn the_transform_is_the_direct_sum_under_the_low_overlap_window() {
    let samples: Vec<f32> = (0..2 * FRAME)
      .map(|i| ((i * i) as f32 * 0.000_37).sin() * 0.5)
      .collect();
    let mut celt = Celt::new(16);
    celt.bands(&samples);
    let flat = (FRAME - OVERLAP) / 2;
    let window = |n: usize| -> f64 {
      let rise = |i: usize| f64::from(celt.rise[i]);
      match n {
        n if n < flat => 0.0,
        n if n < flat + OVERLAP => rise(n - flat),
        n if n < 2 * FRAME - flat - OVERLAP => 1.0,
        n if n < 2 * FRAME - flat => rise(2 * FRAME - flat - 1 - n),
        _ => 0.0,
      }
    };
    let pi = std::f64::consts::PI;
    let length = FRAME as f64;
    for k in [0, 7, 100, 479, 800, 959] {
      let direct: f64 = samples
        .iter()
        .enumerate()
        .map(|(n, x)| {
          let angle = pi / length * (n as f64 + 0.5 + length / 2.0) * (k as f64 + 0.5);
          window(n) * f64::from(*x) * angle.cos()
        })
        .sum::<f64>()
        * (2.0 / length).sqrt();
      let got = f64::from(celt.values[k]);
      assert!((got - direct).abs() < 1e-4, "{k}: {got} {direct}");
    }
  }
}
