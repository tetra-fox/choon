//! the band energies of Opus's CELT layer, looked for in decoded samples.
//!
//! CELT codes each band's energy in each frame as a step from a prediction: half the band's
//! energy a frame earlier, plus a sum over the frame's lower bands. the step is a whole number
//! of 6 dB refined by a few bits. the decoder scales each band's unit-norm shape by the energy,
//! runs the inverse MDCT and a one-pole de-emphasis. so with the de-emphasis undone and the MDCT
//! run again with frames where the encoder put them, each band's energy comes back, and taking
//! the prediction off it leaves a number on the quantizer's lattice.
//!
//! a frame comes back exactly when the pitch post-filter was off in it and in the frame before.
//! one coded as eight short blocks holds, in each band, the values of eight transforms side by
//! side, so its band's energy is the sum over the eight. one coded without the frame before it
//! predicts from its own lower bands alone. the hybrid mode codes only the top four bands this
//! way, over a voice coder's output that reaches into the lowest of them.
//!
//! the analysis is of frames at 48 kHz; samples at 44.1 kHz are brought back to 48 kHz first.
//! frames of 20 ms are read every way above. frames of 10, 5 and 2.5 ms hold the same bands in
//! a half, a quarter and an eighth of the bins and predict with other shares; they are read as
//! one block, after the frame before or without it.

use rayon::prelude::*;

use super::mdct::{DctIv, fold};
use super::resample::Resampler;
use crate::decode::opus::RATE;

/// a 20 ms frame at 48 kHz
pub(crate) const FRAME: usize = 960;
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
/// how much of each coarse step is kept out of the sum over the lower bands in a frame coded
/// without the frame before it, which predicts from that sum alone
const BETA_INTRA: f64 = 4915.0 / 32768.0;

/// a frame shorter than 20 ms
#[derive(Clone, Copy)]
pub(crate) struct Short {
  /// the samples a frame brings at 48 kHz
  pub(crate) length: usize,
  /// how much of a band's energy a frame earlier predicts it
  alpha: f64,
  /// how much of each coarse step is kept out of the sum over the lower bands
  beta: f64,
}

/// the samples libopus's encoder delays by: 312, or 120 in its low-delay mode, which ffmpeg
/// sets for frames under 10 ms
pub(crate) const DELAYS: [usize; 2] = [312, 120];

/// frames of 10, 5 and 2.5 ms
pub(crate) const SHORT: [Short; 3] = [
  Short {
    length: 480,
    alpha: 21248.0 / 32768.0,
    beta: 12124.0 / 32768.0,
  },
  Short {
    length: 240,
    alpha: 26112.0 / 32768.0,
    beta: 22282.0 / 32768.0,
  },
  Short {
    length: 120,
    alpha: 29440.0 / 32768.0,
    beta: 30147.0 / 32768.0,
  },
];

impl Short {
  /// where the frames of a decode sit when nothing was cut from its start, from the file's
  /// first sample, for each of [`DELAYS`]: a frame's window reaches half of what its length has
  /// over the overlap back from where its rise starts
  pub(crate) fn uncut_offsets(&self) -> [usize; 2] {
    DELAYS.map(|delay| (4 * self.length - delay - (self.length - OVERLAP) / 2) % self.length)
  }
}

/// the short blocks of a transient frame, and the samples each brings
const BLOCKS: usize = 8;
const BLOCK: usize = FRAME / BLOCKS;
/// the first band the hybrid mode codes with CELT, from 8 kHz. the voice coder's output
/// reaches into it, so its energy only serves to predict the bands above it
const HYBRID_FROM: usize = 17;
/// the bands a hybrid frame is tested on, from 9.6 to 15.6 kHz. the band above them comes back
/// short of exact from a file at 44.1 kHz, whose conversion rolled its top off
const HYBRID_TESTED: [usize; 2] = [18, 19];
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

/// the lattice's period, in units of 6 dB
const PERIOD: f64 = 1.0 / LATTICE;
/// the dB of gain that move a band's energy by one unit of 6 dB: 20 log10(2)
const DB: f64 = 6.020_599_913_279_624;
/// a shift and one this much further put all but a frame's lowest bands on one lattice: a low
/// band's coarse step rounds one further, and the sum over the lower bands takes that off the
/// bands above. the search for a shift is this wide
const ALIAS: f64 = 1.0 - BETA;
/// the cells a shift is searched in. a step rounds as the decoder's did from anywhere in a cell
/// this narrow, unless it is refined by more than three bits
const CELLS: usize = 8;
/// how near the places two frames name sit to be one place, in units of 6 dB
const AGREE: f64 = 0.002;
/// the largest shift taken for a channel, 18 dB of gain: a step moves by half the gain
const SHIFT_LIMIT: f64 = 18.0 / (2.0 * DB);

/// where the frames of a decode sit when nothing was cut from its start, from the file's first
/// sample: libopus's encoders delay by the first of [`DELAYS`], and a frame's window reaches
/// half of what its length has over the overlap back from where its rise starts
pub(crate) const UNCUT_OFFSET: usize = (2 * FRAME - DELAYS[0] - (FRAME - OVERLAP) / 2) % FRAME;
/// the most windows the pass at that offset reads
const UNCUT_WINDOWS: usize = 60;

/// a sample rate the analysis reads
#[derive(Clone, Copy)]
pub(crate) enum Rate {
  /// 48 kHz, the rate CELT runs at
  Native,
  /// 44.1 kHz, brought back to 48 kHz
  From44100,
}

impl Rate {
  /// `rate` in Hz when it is one the analysis reads
  pub(crate) fn of(rate: u32) -> Option<Self> {
    match rate {
      RATE => Some(Self::Native),
      44_100 => Some(Self::From44100),
      _ => None,
    }
  }
}

/// the bins of `band` in a frame of `length` samples
fn bins(band: usize, length: usize) -> std::ops::Range<usize> {
  EDGES[band] * length / FRAME..EDGES[band + 1] * length / FRAME
}

/// the rounding noise of a transform value in each band of a frame of `length` pre-emphasized
/// `bits`-bit samples
fn noise(bits: u32, length: usize) -> [f64; BANDS] {
  let sigma = 2f64.powi(1 - bits.min(24) as i32) / 12f64.sqrt();
  std::array::from_fn(|band| {
    let bins = bins(band, length);
    let width = bins.len() as f64;
    let gain: f64 = bins
      .map(|bin| {
        let w = std::f64::consts::PI * (bin as f64 + 0.5) / length as f64;
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

/// the CELT analysis of a frame, as one long block or, at 20 ms, as eight short ones: the
/// low-overlap window and an orthonormal MDCT
struct Celt {
  /// the samples a frame brings
  length: usize,
  /// the window's rise over the overlap
  rise: Vec<f32>,
  dct: DctIv,
  frame: Vec<f32>,
  folded: Vec<f32>,
  values: Vec<f32>,
  /// the transform of a short block, its windowed samples and their fold
  short: DctIv,
  block: Vec<f32>,
  block_folded: Vec<f32>,
  block_values: Vec<f32>,
  noise: [f64; BANDS],
}

/// how a frame is read: as the one long block or as the eight short ones it was coded in
#[derive(Clone, Copy, PartialEq)]
enum Blocks {
  Long,
  Short,
}

/// one frame's bands in one channel: each band's energy in units of 6 dB as the codec counts it,
/// and whether it stands over the rounding noise
#[derive(Clone, Copy)]
struct Bands {
  energy: [f64; BANDS],
  loud: [bool; BANDS],
}

impl Celt {
  /// for frames of `length` samples of `bits` bits
  fn new(bits: u32, length: usize) -> Self {
    let rise = (0..OVERLAP)
      .map(|i| {
        let inner = (0.5 * std::f64::consts::PI * (i as f64 + 0.5) / OVERLAP as f64).sin();
        (0.5 * std::f64::consts::PI * inner * inner).sin() as f32
      })
      .collect();
    Self {
      length,
      rise,
      dct: DctIv::new(length),
      frame: vec![0.0; 2 * length],
      folded: vec![0.0; length],
      values: vec![0.0; length],
      short: DctIv::new(BLOCK),
      block: vec![0.0; 2 * BLOCK],
      block_folded: vec![0.0; BLOCK],
      block_values: vec![0.0; BLOCK],
      noise: noise(bits, length),
    }
  }

  /// the bands of the 20 ms frame whose 1920 pre-emphasized samples are `samples`, read as
  /// `blocks`
  fn read(&mut self, samples: &[f32], blocks: Blocks) -> Bands {
    match blocks {
      Blocks::Long => self.bands(samples),
      Blocks::Short => self.short_bands(samples),
    }
  }

  /// the bands of a 20 ms frame read as eight short blocks: each block is 240 samples under the
  /// window's rise and fall, 120 after the one before, and a band's energy is the sum of its
  /// values in all eight
  fn short_bands(&mut self, samples: &[f32]) -> Bands {
    let flat = (FRAME - OVERLAP) / 2;
    let mut squares = [0.0f64; BANDS];
    for block in 0..BLOCKS {
      let from = flat + block * BLOCK;
      let (rising, falling) = self.block.split_at_mut(BLOCK);
      for ((out, x), w) in rising.iter_mut().zip(&samples[from..]).zip(&self.rise) {
        *out = x * w;
      }
      let fall = self.rise.iter().rev();
      for ((out, x), w) in falling.iter_mut().zip(&samples[from + BLOCK..]).zip(fall) {
        *out = x * w;
      }
      fold(&self.block, &mut self.block_folded);
      self
        .short
        .process(&self.block_folded, &mut self.block_values);
      for (band, squares) in squares.iter_mut().enumerate() {
        *squares += self.block_values[EDGES[band] / BLOCKS..EDGES[band + 1] / BLOCKS]
          .iter()
          .map(|v| f64::from(*v).powi(2))
          .sum::<f64>();
      }
    }
    let mut bands = Bands {
      energy: [0.0; BANDS],
      loud: [false; BANDS],
    };
    // an inverse transform of a short block gains sqrt(60) where a long one's gains sqrt(480)
    let scale = SCALE + 0.5 * (BLOCKS as f64).log2();
    for band in 0..BANDS {
      bands.energy[band] = 0.5 * (squares[band] + 1e-300).log2() - MEANS[band] + scale;
      bands.loud[band] = squares[band].sqrt() >= LOUD * self.noise[band];
    }
    bands
  }

  /// the bands of the frame whose pre-emphasized samples, twice its length, are `samples`
  fn bands(&mut self, samples: &[f32]) -> Bands {
    let length = self.length;
    // the window is zero, rises over the overlap, is one, falls and is zero again
    let flat = (length - OVERLAP) / 2;
    let (rise_from, fall_from) = (flat, 2 * length - flat - OVERLAP);
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
    // an inverse transform gains the root of half the frame's length
    let scale = SCALE + 0.5 * (FRAME as f64 / length as f64).log2();
    for band in 0..BANDS {
      let squares: f64 = self.values[bins(band, length)]
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum();
      bands.energy[band] = 0.5 * (squares + 1e-300).log2() - MEANS[band] + scale;
      bands.loud[band] = squares.sqrt() >= LOUD * self.noise[band];
    }
    bands
  }
}

/// 1.5 x 2^52: adding it to a number under 2^51 leaves no bits below one, so the sum is rounded
/// to a whole number, ties to even, and taking it off again leaves that whole number
const ROUNDS: f64 = 6_755_399_441_055_744.0;

/// `x` rounded to the nearest whole number, ties to even, for an `x` under 2^51. the same
/// number `f64::round_ties_even` gives, without the call it compiles to on a target that has no
/// rounding instruction
fn nearest(x: f64) -> f64 {
  (x + ROUNDS) - ROUNDS
}

/// each band's step from its prediction in a 20 ms frame after the frame `before`, with `shift`
/// taken off every step: a gain on the samples moves every energy by twice the shift
fn steps(before: &Bands, bands: &Bands, shift: f64) -> [f64; BANDS] {
  steps_with((ALPHA, BETA), before, bands, shift)
}

/// [`steps`] for frames that predict with the shares `alpha` and `beta`
fn steps_with(
  (alpha, beta): (f64, f64),
  before: &Bands,
  bands: &Bands,
  shift: f64,
) -> [f64; BANDS] {
  let level = 2.0 * shift;
  let mut steps = [0.0; BANDS];
  // the sum over the lower bands of what their coarse steps leave
  let mut lower = 0.0;
  for band in 0..BANDS {
    let predicted = alpha * (before.energy[band] - level).max(LOWEST_PREDICTING) + lower;
    let step = bands.energy[band] - level - predicted;
    lower += (1.0 - beta) * nearest(step);
    steps[band] = step;
  }
  steps
}

/// each band's step in a frame coded without the frame before it, with the gain that `shift`
/// is half of taken off the energies: such a step moves by the whole of it
fn steps_alone(bands: &Bands, shift: f64) -> [f64; BANDS] {
  let level = 2.0 * shift;
  let mut steps = [0.0; BANDS];
  let mut lower = 0.0;
  for band in 0..BANDS {
    let step = bands.energy[band] - level - lower;
    lower += (1.0 - BETA_INTRA) * nearest(step);
    steps[band] = step;
  }
  steps
}

/// whether a step sits on the lattice
fn on_point(step: f64) -> bool {
  let scaled = step * LATTICE;
  (scaled - nearest(scaled)).abs() / LATTICE < NEAR
}

/// how many bands of a frame could be tested, and how many of their steps from the prediction
/// sit on the lattice once `shift` is taken off them: after the frame `before`, or coded without
/// it when there is `None`
fn on_lattice(before: Option<&Bands>, bands: &Bands, shift: f64) -> (usize, usize) {
  let steps = match before {
    Some(before) => steps(before, bands, shift),
    None => steps_alone(bands, shift),
  };
  tested_and_on(steps, before, bands)
}

/// of `steps`, the ones of bands loud in the frame and in the frame `before`, and how many of
/// those sit on the lattice
fn tested_and_on(steps: [f64; BANDS], before: Option<&Bands>, bands: &Bands) -> (usize, usize) {
  let (mut tested, mut hits) = (0, 0);
  for (band, step) in steps.into_iter().enumerate() {
    if bands.loud[band] && before.is_none_or(|before| before.loud[band]) {
      tested += 1;
      hits += usize::from(on_point(step));
    }
  }
  (tested, hits)
}

/// what the bands of [`HYBRID_TESTED`] show in one channel of a frame
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hybrid {
  /// one of the bands is not loud in the frame and the frame before
  Untested,
  /// one of them is off the lattice
  Off,
  /// the lattice points the two sit on
  On([i32; 2]),
}

/// the lattice points a frame after the frame `before` sits on in the bands of
/// [`HYBRID_TESTED`], as the hybrid mode codes them: the sum over the lower bands starts at
/// [`HYBRID_FROM`]
fn hybrid(before: &Bands, bands: &Bands) -> Hybrid {
  let loud = |band: &usize| bands.loud[*band] && before.loud[*band];
  if !HYBRID_TESTED.iter().all(loud) {
    return Hybrid::Untested;
  }
  let mut lower = 0.0;
  let mut steps = [0.0; BANDS];
  for band in HYBRID_FROM..=HYBRID_TESTED[HYBRID_TESTED.len() - 1] {
    let predicted = ALPHA * before.energy[band].max(LOWEST_PREDICTING) + lower;
    steps[band] = bands.energy[band] - predicted;
    lower += (1.0 - BETA) * nearest(steps[band]);
  }
  let tested = HYBRID_TESTED.map(|band| steps[band]);
  // a step is within a few thousand lattice points of none, so its point fits
  let points = tested.map(|step| nearest(step * LATTICE) as i32);
  if tested.iter().all(|step| on_point(*step)) {
    Hybrid::On(points)
  } else {
    Hybrid::Off
  }
}

/// whether a channel with `tested` bands, `hits` of them on the lattice, is judged, and whether
/// it then sits on the lattice
fn judged(tested: usize, hits: usize) -> Option<bool> {
  (tested >= MIN_TESTED).then_some(hits as f64 >= SHARE * tested as f64)
}

/// the most of `sorted` that sit less than `width` above one of them, around a circle of
/// `period`: how many, and their mean with the ones past the circle's end counted on from it
fn gathered(sorted: &[f64], width: f64, period: f64) -> (usize, f64) {
  let count = sorted.len();
  let at = |i: usize| {
    if i < count {
      sorted[i]
    } else {
      sorted[i - count] + period
    }
  };
  let (mut most, mut mean) = (0, 0.0);
  let mut end = 0;
  for (start, first) in sorted.iter().enumerate() {
    while end < start + count && at(end) - first < width {
      end += 1;
    }
    if end - start > most {
      most = end - start;
      mean = (start..end).map(at).sum::<f64>() / most as f64;
    }
  }
  (most, mean)
}

/// the shift a frame after the frame `before` names in one channel: in the cell where the most
/// of its tested bands gather at one place in the lattice's period, the cell's middle plus that
/// place. `None` when the channel is not judged or its share of bands gathers in no cell
fn named(before: &Bands, bands: &Bands) -> Option<f64> {
  let tested = (0..BANDS).filter(|band| bands.loud[*band] && before.loud[*band]);
  let count = tested.clone().count();
  if count < MIN_TESTED {
    return None;
  }
  let mut most: Option<(usize, f64)> = None;
  for cell in 0..CELLS {
    let middle = ((cell as f64 + 0.5) / CELLS as f64 - 0.5) * ALIAS;
    let steps = steps(before, bands, middle);
    // where each tested band's step sits in the lattice's period, from -0.5 to 0.5
    let mut places = [0.0; BANDS];
    for (place, band) in places.iter_mut().zip(tested.clone()) {
      let scaled = steps[band] * LATTICE;
      *place = scaled - nearest(scaled);
    }
    let places = &mut places[..count];
    // a transform value past what an f32 holds leaves its band and the bands over it without
    // a step, in every cell alike
    if places.iter().any(|place| !place.is_finite()) {
      return None;
    }
    // bands that gather within less than a quarter of the period sit in three eighths of it
    // in a row, which is cheaper to rule out than the gathering itself
    let mut eighths = [0; 8];
    for place in places.iter() {
      eighths[((place + 0.5) * 8.0) as usize % 8] += 1;
    }
    let most_in_three = (0..8)
      .map(|i| eighths[i] + eighths[(i + 1) % 8] + eighths[(i + 2) % 8])
      .max()
      .unwrap_or(0);
    if f64::from(most_in_three) < SHARE * count as f64 {
      continue;
    }
    places.sort_by(f64::total_cmp);
    let (members, mean) = gathered(places, 2.0 * NEAR * LATTICE, 1.0);
    if members as f64 >= SHARE * count as f64 && most.is_none_or(|(most, _)| members > most) {
      most = Some((members, middle + (mean - nearest(mean)) * PERIOD));
    }
  }
  most.map(|(_, shift)| shift)
}

/// the place in the lattice's period that the most of `shifts` name, within [`AGREE`] of each
/// other: frames of audio that was never Opus name places at random, a decode's name one
fn place(shifts: &[f64]) -> Option<f64> {
  let mut places: Vec<f64> = shifts.iter().map(|s| s.rem_euclid(PERIOD)).collect();
  places.sort_by(f64::total_cmp);
  let (agreeing, mean) = gathered(&places, 2.0 * AGREE, PERIOD);
  (agreeing > 0).then(|| mean.rem_euclid(PERIOD))
}

/// of the shifts at `place`, a lattice step apart up to [`SHIFT_LIMIT`], the one `passing`
/// counts the most frames at, the nearest to none of those that count alike. neighbouring
/// shifts differ only in the frames whose steps round another way
fn shift_at(place: f64, mut passing: impl FnMut(f64) -> usize) -> f64 {
  let first = ((-SHIFT_LIMIT - place) / PERIOD).ceil() as i32;
  let last = ((SHIFT_LIMIT - place) / PERIOD).floor() as i32;
  let (mut most, mut best) = (0, 0.0);
  for step in first..=last {
    let shift = place + f64::from(step) * PERIOD;
    let count = passing(shift);
    if count > most || (count == most && shift.abs() < f64::abs(best)) {
      (most, best) = (count, shift);
    }
  }
  best
}

/// what the frames of one frame grid count
pub(crate) struct Counted {
  /// the frames on the lattice, by run
  pub(crate) frames: Vec<usize>,
  /// the frames on the lattice once each channel's shift is taken off
  pub(crate) shifted: usize,
  /// the frames whose top bands could be tested as the hybrid mode codes them
  pub(crate) hybrid_tested: usize,
  /// the ones of them on the lattice
  pub(crate) hybrid: usize,
  /// the different pairs of lattice points those sit on, in the first channel tested
  pub(crate) hybrid_points: usize,
}

/// the ways a frame is read: as a long block or as short ones, after the frame before it read
/// either way or coded without it. the first is the one that says whether a grid is there
const WAYS: [(Blocks, Option<Blocks>); 6] = [
  (Blocks::Long, Some(Blocks::Long)),
  (Blocks::Long, Some(Blocks::Short)),
  (Blocks::Short, Some(Blocks::Long)),
  (Blocks::Short, Some(Blocks::Short)),
  (Blocks::Long, None),
  (Blocks::Short, None),
];

/// whether a frame sits on the lattice read `way`, with each channel's shift taken off: every
/// channel with enough tested bands has its share of them on the lattice, and one at least has
/// enough. a channel is asked for only when the channels before it leave the frame open
fn sits(
  (run, frame): (usize, usize),
  (now, before): (Blocks, Option<Blocks>),
  shifts: &[f64],
  bands: &mut impl FnMut(usize, usize, usize, Blocks) -> Bands,
) -> bool {
  let mut any = false;
  for (channel, shift) in shifts.iter().enumerate() {
    let read = bands(run, channel, frame, now);
    let before = before.map(|blocks| bands(run, channel, frame - 1, blocks));
    let (tested, hits) = on_lattice(before.as_ref(), &read, *shift);
    match judged(tested, hits) {
      Some(true) => any = true,
      Some(false) => return false,
      None => {}
    }
  }
  any
}

/// how many frames of each of `runs` sit on the lattice read one of `ways`, with each
/// channel's shift taken off
fn sitting(
  runs: &[usize],
  ways: &[(Blocks, Option<Blocks>)],
  shifts: &[f64],
  bands: &mut impl FnMut(usize, usize, usize, Blocks) -> Bands,
) -> Vec<usize> {
  runs
    .iter()
    .enumerate()
    .map(|(run, frames)| {
      (1..*frames)
        .filter(|frame| {
          ways
            .iter()
            .any(|way| sits((run, *frame), *way, shifts, bands))
        })
        .count()
    })
    .collect()
}

/// counts the frames on the lattice in `runs` of consecutive frames of `channels` channels,
/// each run that many frames long. `bands(run, channel, frame, blocks)` gives a frame's bands
/// read as `blocks`, and is asked for a channel's only in the frames that the channels before
/// it leave open.
///
/// a frame after the first of its run counts when it sits on the lattice read as a long block
/// after a long block. the shifted count takes a shift off each channel first: the one at the
/// place the most of its frames name. where either count finds a frame, the frames that sit on
/// the lattice read another way are counted with it
fn counted(
  runs: &[usize],
  channels: usize,
  mut bands: impl FnMut(usize, usize, usize, Blocks) -> Bands,
) -> Counted {
  let unshifted = vec![0.0; channels];
  let mut frames = sitting(runs, &WAYS[..1], &unshifted, &mut bands);
  if frames.iter().sum::<usize>() > 0 {
    frames = sitting(runs, &WAYS, &unshifted, &mut bands);
  }
  let pairs = || {
    runs
      .iter()
      .enumerate()
      .flat_map(|(run, frames)| (1..*frames).map(move |frame| (run, frame)))
  };
  // the frames still open, and whether a channel so far was judged in each
  let mut open: Vec<(usize, usize, bool)> =
    pairs().map(|(run, frame)| (run, frame, false)).collect();
  let mut shifts = Vec::with_capacity(channels);
  for channel in 0..channels {
    let read: Vec<(Bands, Bands)> = open
      .iter()
      .map(|(run, frame, _)| {
        (
          bands(*run, channel, frame - 1, Blocks::Long),
          bands(*run, channel, *frame, Blocks::Long),
        )
      })
      .collect();
    let named: Vec<f64> = read
      .iter()
      .filter_map(|(before, now)| named(before, now))
      .collect();
    let shift = place(&named).map_or(0.0, |place| {
      shift_at(place, |shift| {
        read
          .iter()
          .filter(|(before, now)| {
            let (tested, hits) = on_lattice(Some(before), now, shift);
            judged(tested, hits) == Some(true)
          })
          .count()
      })
    });
    open = open
      .into_iter()
      .zip(&read)
      .filter_map(|((run, frame, any), (before, now))| {
        let (tested, hits) = on_lattice(Some(before), now, shift);
        match judged(tested, hits) {
          Some(true) => Some((run, frame, true)),
          Some(false) => None,
          None => Some((run, frame, any)),
        }
      })
      .collect();
    shifts.push(shift);
  }
  let mut shifted = open.iter().filter(|(_, _, any)| *any).count();
  if shifted > 0 {
    shifted = sitting(runs, &WAYS, &shifts, &mut bands).iter().sum();
  }
  let (mut hybrid_tested, mut hybrid_frames) = (0, 0);
  let mut points = std::collections::HashSet::new();
  for (run, frame) in pairs() {
    // the first tested channel's points, while every tested channel sits on the lattice
    let mut sits = Hybrid::Untested;
    for channel in 0..channels {
      let before = bands(run, channel, frame - 1, Blocks::Long);
      match hybrid(&before, &bands(run, channel, frame, Blocks::Long)) {
        Hybrid::On(_) if sits != Hybrid::Untested => {}
        on @ Hybrid::On(_) => sits = on,
        Hybrid::Off => {
          sits = Hybrid::Off;
          break;
        }
        Hybrid::Untested => {}
      }
    }
    hybrid_tested += usize::from(sits != Hybrid::Untested);
    if let Hybrid::On(on) = sits {
      hybrid_frames += 1;
      points.insert(on);
    }
  }
  Counted {
    frames,
    shifted,
    hybrid_tested,
    hybrid: hybrid_frames,
    hybrid_points: points.len(),
  }
}

/// the frames a stretch of `length` samples holds at every offset
fn frames_in(length: usize) -> usize {
  (length / FRAME).saturating_sub(3)
}

/// whether a short frame sits on the lattice after the frame before it, or with `alone`
/// without it, by the rule of [`sits`] over its channels
fn short_sits(
  short: Short,
  frame: usize,
  alone: bool,
  channels: usize,
  bands: &mut impl FnMut(usize, usize) -> Bands,
) -> bool {
  let mut any = false;
  for channel in 0..channels {
    let now = bands(channel, frame);
    let (tested, hits) = if alone {
      on_lattice(None, &now, 0.0)
    } else {
      let before = bands(channel, frame - 1);
      let steps = steps_with((short.alpha, short.beta), &before, &now, 0.0);
      tested_and_on(steps, Some(&before), &now)
    };
    match judged(tested, hits) {
      Some(true) => any = true,
      Some(false) => return false,
      None => {}
    }
  }
  any
}

/// how many of a run of `frames` short frames sit on the lattice after the frame before them,
/// and how many do that or sit on it without the frame before. `bands(channel, frame)` gives a
/// frame's bands, and is asked for a channel's only where the channels before it leave the
/// frame open
fn short_sitting(
  short: Short,
  frames: usize,
  channels: usize,
  mut bands: impl FnMut(usize, usize) -> Bands,
) -> [usize; 2] {
  let mut counts = [0; 2];
  for frame in 1..frames {
    let after = short_sits(short, frame, false, channels, &mut bands);
    counts[0] += usize::from(after);
    counts[1] += usize::from(after || short_sits(short, frame, true, channels, &mut bands));
  }
  counts
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
  fn of(channels: &[&[f64]], first: u64, rate: Rate) -> Option<Self> {
    let resampler = match rate {
      Rate::Native => {
        return Some(Self {
          channels: channels
            .iter()
            .map(|c| pre_emphasized(c.iter().copied()))
            .collect(),
          start: first,
        });
      }
      Rate::From44100 => Resampler::to_48000(),
    };
    let (resampled, start) = resampler.brought(channels, first)?;
    Some(Self {
      channels: resampled
        .iter()
        .map(|c| pre_emphasized(c.iter().copied()))
        .collect(),
      start,
    })
  }
}

/// the stretches of a file the scan reads, at 48 kHz
pub(crate) struct Stretches {
  /// `None` for a stretch nothing is left of at 48 kHz
  stretches: Vec<Option<Stretch>>,
  bits: u32,
}

impl Stretches {
  /// the stretches of `bits`-bit samples at `rate`, each its two channels and the frame of the
  /// file its first sample is
  pub(crate) fn of(stretches: &[([&[f64]; 2], u64)], rate: Rate, bits: u32) -> Self {
    Self {
      stretches: stretches
        .iter()
        .map(|(channels, first)| Stretch::of(channels, *first, rate))
        .collect(),
      bits,
    }
  }

  /// what the stretches' frames count at each of `offsets` of the file's frame grid, a run of
  /// frames to a stretch. runs of offsets are tasks on rayon's thread pool
  pub(crate) fn counted(&self, offsets: std::ops::Range<usize>) -> Vec<Counted> {
    let runs: Vec<usize> = self
      .stretches
      .iter()
      .map(|s| s.as_ref().map_or(0, |s| frames_in(s.channels[0].len())))
      .collect();
    let most = runs.iter().copied().max().unwrap_or(0);
    offsets
      .into_par_iter()
      .with_min_len(FRAME / 32)
      .map_init(
        || {
          (
            Celt::new(self.bits, FRAME),
            vec![None; self.stretches.len() * 4 * most],
          )
        },
        |(celt, kept), offset| {
          kept.fill(None);
          counted(&runs, 2, |run, channel, frame, blocks| {
            let stretch = self.stretches[run]
              .as_ref()
              .expect("a stretch with frames has samples");
            // below the frame, so it fits
            let shift = (stretch.start % FRAME as u64) as usize;
            let start = (offset + FRAME - shift) % FRAME + frame * FRAME;
            let short = usize::from(blocks == Blocks::Short);
            *kept[((run * 2 + channel) * 2 + short) * most + frame].get_or_insert_with(|| {
              celt.read(&stretch.channels[channel][start..start + 2 * FRAME], blocks)
            })
          })
        },
      )
      .collect()
  }

  /// at each offset of the file's grid of `short` frames, how many of the stretches' frames
  /// sit on the lattice after the frame before them, and how many do that or sit on it without
  /// it. runs of offsets are tasks on rayon's thread pool
  pub(crate) fn short_counted(&self, short: Short) -> Vec<[usize; 2]> {
    let length = short.length;
    (0..length)
      .into_par_iter()
      .with_min_len((length / 32).max(1))
      .map_init(
        || (Celt::new(self.bits, length), Vec::new()),
        |(celt, kept), offset| {
          let mut counts = [0; 2];
          for stretch in self.stretches.iter().flatten() {
            let frames = (stretch.channels[0].len() / length).saturating_sub(3);
            // below the frame, so it fits
            let shift = (stretch.start % length as u64) as usize;
            let first = (offset + length - shift) % length;
            kept.clear();
            kept.resize(2 * frames, None);
            let run = short_sitting(short, frames, 2, |channel, frame| {
              *kept[channel * frames + frame].get_or_insert_with(|| {
                celt.bands(&stretch.channels[channel][first + frame * length..][..2 * length])
              })
            });
            counts[0] += run[0];
            counts[1] += run[1];
          }
          counts
        },
      )
      .collect()
  }
}

/// counts the frames on the lattice in the windows a file is measured at, with the frame grid
/// of a decode whose start was not cut
pub(crate) struct Uncut {
  celt: Celt,
  /// the analysis of each length of short frame, and its counts as [`Uncut::short_counted`]
  /// gives them
  short: [(Celt, [[usize; 2]; 2]); 3],
  rate: Rate,
  /// each window's frames on that grid, by channel: a frame read as a long block and as short
  /// ones
  windows: Vec<Vec<Vec<[Bands; 2]>>>,
}

impl Uncut {
  pub(crate) fn new(rate: Rate, bits: u32) -> Self {
    Self {
      celt: Celt::new(bits, FRAME),
      short: SHORT.map(|short| (Celt::new(bits, short.length), [[0; 2]; 2])),
      rate,
      windows: Vec::new(),
    }
  }

  /// takes the frames of `window`, one vector per analyzed channel, whose first sample is
  /// frame `first` of the file. windows past the most it reads are left out
  pub(crate) fn take(&mut self, first: u64, window: &[Vec<f32>]) {
    if self.windows.len() >= UNCUT_WINDOWS {
      return;
    }
    let doubled: Vec<Vec<f64>> = window
      .iter()
      .map(|c| c.iter().map(|s| f64::from(*s)).collect())
      .collect();
    let channels: Vec<&[f64]> = doubled.iter().map(Vec::as_slice).collect();
    let Some(stretch) = Stretch::of(&channels, first, self.rate) else {
      self.windows.push(Vec::new());
      return;
    };
    let length = stretch.channels[0].len();
    for (short, (celt, counts)) in SHORT.iter().zip(&mut self.short) {
      let frame = short.length;
      // below the frame, so it fits
      let shift = (stretch.start % frame as u64) as usize;
      for (grid, counts) in short.uncut_offsets().into_iter().zip(counts) {
        let offset = (grid + frame - shift) % frame;
        let frames = (length.saturating_sub(offset) / frame).saturating_sub(1);
        let mut kept = vec![None; stretch.channels.len() * frames];
        let run = short_sitting(*short, frames, stretch.channels.len(), |channel, at| {
          *kept[channel * frames + at].get_or_insert_with(|| {
            celt.bands(&stretch.channels[channel][offset + at * frame..][..2 * frame])
          })
        });
        counts[0] += run[0];
        counts[1] += run[1];
      }
    }
    // the first frame of the grid at or after the stretch's first sample
    let grid = UNCUT_OFFSET as u64;
    let frame = FRAME as u64;
    let offset = ((grid + frame - stretch.start % frame) % frame) as usize;
    let frames = (length.saturating_sub(offset) / FRAME).saturating_sub(1);
    let bands = stretch
      .channels
      .iter()
      .map(|channel| {
        (0..frames)
          .map(|frame| {
            let samples = &channel[offset + frame * FRAME..][..2 * FRAME];
            [Blocks::Long, Blocks::Short].map(|blocks| self.celt.read(samples, blocks))
          })
          .collect()
      })
      .collect();
    self.windows.push(bands);
  }

  /// what the frames taken count, a run of frames to a window
  pub(crate) fn counted(&self) -> Counted {
    let runs: Vec<usize> = self
      .windows
      .iter()
      .map(|window| window.first().map_or(0, Vec::len))
      .collect();
    let channels = self.windows.iter().map(Vec::len).max().unwrap_or(0);
    counted(&runs, channels, |run, channel, frame, blocks| {
      self.windows[run][channel][frame][usize::from(blocks == Blocks::Short)]
    })
  }

  /// what the windows' frames of each length of [`SHORT`] count on the grid of each of
  /// [`DELAYS`]: on the lattice after the frame before, and that or without it
  pub(crate) fn short_counted(&self) -> [[[usize; 2]; 2]; 3] {
    std::array::from_fn(|i| self.short[i].1)
  }
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
    assert_eq!(on_lattice(Some(&quiet), &bands, 0.0), (BANDS, BANDS));
    // a frame 1.3 dB louder than the codec made it has every step off the lattice
    let mut louder = bands;
    for energy in &mut louder.energy {
      *energy += 0.21;
    }
    assert_eq!(on_lattice(Some(&quiet), &louder, 0.0), (BANDS, 0));
    // bands under the noise in either frame are not tested
    let mut faint = bands;
    faint.loud[3] = false;
    faint.loud[7] = false;
    assert_eq!(
      on_lattice(Some(&quiet), &faint, 0.0),
      (BANDS - 2, BANDS - 2)
    );
  }

  #[test]
  fn a_short_frame_predicts_with_its_lengths_shares_and_sits_where_its_delay_puts_it() {
    let quiet = Bands {
      energy: [-2.0; BANDS],
      loud: [true; BANDS],
    };
    for short in SHORT {
      // each band a coarse step of its own with a refinement, built as the decoder builds it
      let mut bands = quiet;
      let mut lower = 0.0;
      for band in 0..BANDS {
        let coarse = (band % 5) as f64 - 2.0;
        let fine = if band % 2 == 0 { 0.25 } else { -0.125 };
        bands.energy[band] = short.alpha * quiet.energy[band] + lower + coarse + fine;
        lower += (1.0 - short.beta) * coarse;
      }
      let own = steps_with((short.alpha, short.beta), &quiet, &bands, 0.0);
      assert_eq!(tested_and_on(own, Some(&quiet), &bands), (BANDS, BANDS));
      // read with the shares of a 20 ms frame it is off the lattice
      let (_, on) = on_lattice(Some(&quiet), &bands, 0.0);
      assert!(on < BANDS / 2, "{}: {on}", short.length);
      let run = [quiet, bands, bands];
      let counts = short_sitting(short, run.len(), 1, |_, frame| run[frame]);
      assert_eq!(counts, [1, 1], "{}", short.length);
    }
    let offsets = SHORT.map(|short| short.uncut_offsets());
    assert_eq!(offsets, [[468, 180], [108, 60], [48, 0]]);
  }

  /// a frame after `before`: each band a coarse step of its own, refined by a quarter or an
  /// eighth, built as the decoder builds it
  fn coded_after(before: &Bands, turn: usize) -> Bands {
    let mut bands = *before;
    let mut lower = 0.0;
    for band in 0..BANDS {
      let coarse = ((band + turn) % 5) as f64 - 2.0;
      let fine = if (band + turn).is_multiple_of(2) {
        0.25
      } else {
        -0.125
      };
      bands.energy[band] = ALPHA * before.energy[band] + lower + coarse + fine;
      lower += (1.0 - BETA) * coarse;
    }
    bands
  }

  /// a run of coded frames, every band's energy then raised by `level` units of 6 dB
  fn run_at(level: f64, frames: usize) -> Vec<Bands> {
    let mut run = vec![Bands {
      energy: [-2.0; BANDS],
      loud: [true; BANDS],
    }];
    for turn in 1..frames {
      let next = coded_after(&run[turn - 1], turn);
      run.push(next);
    }
    for bands in &mut run {
      for energy in &mut bands.energy {
        *energy += level;
      }
    }
    run
  }

  #[test]
  fn frames_at_another_level_name_one_place_and_count_once_it_is_taken_off() {
    // 2.7 dB louder moves every energy by 0.4485 and every step by half of that
    let level = 2.7 / DB;
    let run = run_at(level, 9);
    let place_of = |shift: f64| shift.rem_euclid(PERIOD);
    for pair in run.windows(2) {
      assert_eq!(on_lattice(Some(&pair[0]), &pair[1], 0.0), (BANDS, 0));
      assert_eq!(
        on_lattice(Some(&pair[0]), &pair[1], level / 2.0),
        (BANDS, BANDS)
      );
      let named = named(&pair[0], &pair[1]).expect("every band gathers");
      assert!(
        (place_of(named) - place_of(level / 2.0)).abs() < 1e-9,
        "{named}"
      );
    }
    let moved = counted(&[run.len()], 1, |_, _, frame, _| run[frame]);
    assert_eq!(moved.frames, [0]);
    assert_eq!(moved.shifted, run.len() - 1);
    // frames at the decoder's own level count either way
    let run = run_at(0.0, 9);
    let unmoved = counted(&[run.len()], 1, |_, _, frame, _| run[frame]);
    assert_eq!(unmoved.frames, [run.len() - 1]);
    assert_eq!(unmoved.shifted, run.len() - 1);
  }

  #[test]
  fn each_channel_has_a_level_of_its_own() {
    let (left, right) = (run_at(-1.0 / DB, 6), run_at(-2.5 / DB, 6));
    let both = counted(&[6], 2, |_, channel, frame, _| {
      [&left, &right][channel][frame]
    });
    assert_eq!((both.frames, both.shifted), (vec![0], 5));
    // a channel no shift fits closes the frames the other left open
    let mut noise = run_at(0.0, 6);
    for (frame, bands) in noise.iter_mut().enumerate() {
      for (band, energy) in bands.energy.iter_mut().enumerate() {
        *energy += ((frame * 7 + band * 13) % 17) as f64 * 0.0531;
      }
    }
    let one = counted(&[6], 2, |_, channel, frame, _| {
      [&left, &noise][channel][frame]
    });
    assert_eq!(one.shifted, 0);
  }

  #[test]
  fn a_shift_one_alias_further_loses_only_the_lowest_band() {
    let run = run_at(0.0, 2);
    let (tested, hits) = on_lattice(Some(&run[0]), &run[1], ALIAS);
    assert_eq!((tested, hits), (BANDS, BANDS - 1));
  }

  /// a frame coded without the frame before it: each band a coarse step of its own, refined
  /// by a quarter or an eighth, predicted from the lower bands alone
  fn coded_alone() -> Bands {
    let mut bands = Bands {
      energy: [0.0; BANDS],
      loud: [true; BANDS],
    };
    let mut lower = 0.0;
    for (band, energy) in bands.energy.iter_mut().enumerate() {
      let coarse = (band % 5) as f64 - 2.0;
      let fine = if band.is_multiple_of(2) { 0.25 } else { -0.125 };
      *energy = lower + coarse + fine;
      lower += (1.0 - BETA_INTRA) * coarse;
    }
    bands
  }

  #[test]
  fn a_frame_coded_without_the_one_before_sits_on_the_lattice_alone() {
    let alone = coded_alone();
    let before = run_at(0.0, 2)[1];
    assert_eq!(on_lattice(None, &alone, 0.0), (BANDS, BANDS));
    assert!(on_lattice(Some(&before), &alone, 0.0).1 < BANDS / 2);
    // the whole gain comes off such a frame's steps, where half of it comes off the others'
    let mut louder = alone;
    for energy in &mut louder.energy {
      *energy += 0.3;
    }
    assert_eq!(on_lattice(None, &louder, 0.0), (BANDS, 0));
    assert_eq!(on_lattice(None, &louder, 0.15), (BANDS, BANDS));
  }

  #[test]
  fn frames_read_another_way_count_only_where_a_frame_sits_as_long_blocks() {
    // four frames coded as long blocks, the third as short ones: read as a long block it is
    // noise, and read as short ones the others are
    let run = run_at(0.0, 5);
    let noise = |frame: usize| {
      let mut bands = run[frame];
      for (band, energy) in bands.energy.iter_mut().enumerate() {
        *energy += ((frame * 7 + band * 13) % 17) as f64 * 0.0531;
      }
      bands
    };
    let transient = |_, _, frame: usize, blocks: Blocks| {
      if (frame == 2) == (blocks == Blocks::Short) {
        run[frame]
      } else {
        noise(frame)
      }
    };
    // frame 1 after 0 and frame 4 after 3 sit as long blocks; 2 as short ones after a long
    // one, 3 as a long one after short ones
    assert_eq!(counted(&[5], 1, transient).frames, [4]);
    // with every frame short, none sits as long blocks, and none is counted
    let short = |_, _, frame: usize, blocks: Blocks| match blocks {
      Blocks::Short => run[frame],
      Blocks::Long => noise(frame),
    };
    assert_eq!(counted(&[5], 1, short).frames, [0]);
  }

  #[test]
  fn a_hybrid_frame_is_tested_on_the_two_bands_under_15_6_khz() {
    let before = Bands {
      energy: [-2.0; BANDS],
      loud: [true; BANDS],
    };
    // bands 17 to 20 coded as the hybrid mode does, the sum over the lower bands from 17
    let mut bands = before;
    let mut lower = 0.0;
    for band in HYBRID_FROM..BANDS {
      let coarse = (band % 3) as f64 - 1.0;
      bands.energy[band] = ALPHA * before.energy[band] + lower + coarse + 0.25;
      lower += (1.0 - BETA) * coarse;
    }
    assert!(matches!(hybrid(&before, &bands), Hybrid::On(_)));
    // the voice coder's output in band 17 moves its energy a little, and nothing else
    let mut reached = bands;
    reached.energy[HYBRID_FROM] += 0.1;
    assert_eq!(hybrid(&before, &reached), hybrid(&before, &bands));
    let mut off = bands;
    off.energy[19] += 0.01;
    assert_eq!(hybrid(&before, &off), Hybrid::Off);
    let mut faint = bands;
    faint.loud[18] = false;
    assert_eq!(hybrid(&before, &faint), Hybrid::Untested);
    // frames that land on the same points over and over, as a steady tone's do, are one pair
    let run = [before, bands, before, bands, before, bands];
    let counted = counted(&[6], 1, |_, _, frame, _| run[frame]);
    assert_eq!(
      (counted.hybrid_tested, counted.hybrid, counted.hybrid_points),
      (5, 3, 1)
    );
  }

  #[test]
  fn short_blocks_give_a_band_the_energy_of_its_eight_transforms() {
    let samples: Vec<f32> = (0..2 * FRAME)
      .map(|i| ((i * i) as f32 * 0.000_37).sin() * 0.5)
      .collect();
    let mut celt = Celt::new(16, FRAME);
    let bands = celt.read(&samples, Blocks::Short);
    let flat = (FRAME - OVERLAP) / 2;
    let pi = std::f64::consts::PI;
    let length = BLOCK as f64;
    for band in [0, 7, 12, 20] {
      let mut squares = 0.0;
      for block in 0..BLOCKS {
        for k in EDGES[band] / BLOCKS..EDGES[band + 1] / BLOCKS {
          let value: f64 = (0..2 * BLOCK)
            .map(|n| {
              let window = f64::from(celt.rise[if n < BLOCK { n } else { 2 * BLOCK - 1 - n }]);
              let angle = pi / length * (n as f64 + 0.5 + length / 2.0) * (k as f64 + 0.5);
              window * f64::from(samples[flat + block * BLOCK + n]) * angle.cos()
            })
            .sum::<f64>()
            * (2.0 / length).sqrt();
          squares += value * value;
        }
      }
      let energy = 0.5 * squares.log2() - MEANS[band] + SCALE + 1.5;
      // the chirp leaves the top band little, which the single-precision transform rounds
      assert!(
        (bands.energy[band] - energy).abs() < 1e-3,
        "{band}: {} {energy}",
        bands.energy[band]
      );
    }
  }

  #[test]
  fn values_gather_around_the_end_of_the_circle() {
    // 0.97, 0.99 and 0.01 sit within 0.05 of each other around a circle of 1
    let (most, mean) = gathered(&[0.01, 0.4, 0.97, 0.99], 0.05, 1.0);
    assert_eq!(most, 3);
    assert!((mean - 0.99).abs() < 1e-12, "{mean}");
    assert_eq!(gathered(&[], 0.05, 1.0).0, 0);
  }

  #[test]
  fn rounding_by_the_sum_is_rounding_to_even() {
    for x in [
      -2.5,
      -1.5,
      -0.5,
      -0.3,
      0.0,
      0.3,
      0.5,
      1.5,
      2.5,
      1e6 + 0.5,
      -77.49,
    ] {
      assert_eq!(nearest(x), f64::round_ties_even(x), "{x}");
    }
  }

  #[test]
  fn the_transform_is_the_direct_sum_under_the_low_overlap_window() {
    let samples: Vec<f32> = (0..2 * FRAME)
      .map(|i| ((i * i) as f32 * 0.000_37).sin() * 0.5)
      .collect();
    let mut celt = Celt::new(16, FRAME);
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
