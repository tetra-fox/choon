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

/// each band's step from its prediction in a frame after the frame `before`, with `shift` taken
/// off every step: a gain on the samples moves every energy by twice the shift
fn steps(before: &Bands, bands: &Bands, shift: f64) -> [f64; BANDS] {
  let level = 2.0 * shift;
  let mut steps = [0.0; BANDS];
  // the sum over the lower bands of what their coarse steps leave
  let mut lower = 0.0;
  for band in 0..BANDS {
    let predicted = ALPHA * (before.energy[band] - level).max(LOWEST_PREDICTING) + lower;
    let step = bands.energy[band] - level - predicted;
    lower += (1.0 - BETA) * nearest(step);
    steps[band] = step;
  }
  steps
}

/// how many bands of a frame, after the frame `before`, could be tested, and how many of their
/// steps from the prediction sit on the lattice once `shift` is taken off them
fn on_lattice(before: &Bands, bands: &Bands, shift: f64) -> (usize, usize) {
  let (mut tested, mut hits) = (0, 0);
  for (band, step) in steps(before, bands, shift).into_iter().enumerate() {
    if bands.loud[band] && before.loud[band] {
      tested += 1;
      let scaled = step * LATTICE;
      if (scaled - nearest(scaled)).abs() / LATTICE < NEAR {
        hits += 1;
      }
    }
  }
  (tested, hits)
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
}

/// counts the frames on the lattice in `runs` of consecutive frames of `channels` channels,
/// each run that many frames long. `bands(run, channel, frame)` gives a frame's bands, and is
/// asked for a channel's only in the frames that the channels before it leave open.
///
/// a frame after the first of its run counts when every channel with enough tested bands has
/// its share of them on the lattice, and one channel at least has enough. the shifted count
/// takes a shift off each channel first: the one at the place the most of its frames name
fn counted(
  runs: &[usize],
  channels: usize,
  mut bands: impl FnMut(usize, usize, usize) -> Bands,
) -> Counted {
  let frames = runs
    .iter()
    .enumerate()
    .map(|(run, frames)| {
      (1..*frames)
        .filter(|frame| {
          let mut any = false;
          for channel in 0..channels {
            let (before, now) = (bands(run, channel, frame - 1), bands(run, channel, *frame));
            let (tested, hits) = on_lattice(&before, &now, 0.0);
            match judged(tested, hits) {
              Some(true) => any = true,
              Some(false) => return false,
              None => {}
            }
          }
          any
        })
        .count()
    })
    .collect();
  // the frames still open, and whether a channel so far was judged in each
  let mut open: Vec<(usize, usize, bool)> = runs
    .iter()
    .enumerate()
    .flat_map(|(run, frames)| (1..*frames).map(move |frame| (run, frame, false)))
    .collect();
  for channel in 0..channels {
    let pairs: Vec<(Bands, Bands)> = open
      .iter()
      .map(|(run, frame, _)| {
        (
          bands(*run, channel, frame - 1),
          bands(*run, channel, *frame),
        )
      })
      .collect();
    let named: Vec<f64> = pairs
      .iter()
      .filter_map(|(before, now)| named(before, now))
      .collect();
    let shift = place(&named).map_or(0.0, |place| {
      shift_at(place, |shift| {
        pairs
          .iter()
          .filter(|(before, now)| {
            let (tested, hits) = on_lattice(before, now, shift);
            judged(tested, hits) == Some(true)
          })
          .count()
      })
    });
    open = open
      .into_iter()
      .zip(&pairs)
      .filter_map(|((run, frame, any), (before, now))| {
        let (tested, hits) = on_lattice(before, now, shift);
        match judged(tested, hits) {
          Some(true) => Some((run, frame, true)),
          Some(false) => None,
          None => Some((run, frame, any)),
        }
      })
      .collect();
  }
  Counted {
    frames,
    shifted: open.iter().filter(|(_, _, any)| *any).count(),
  }
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
      Rate::From44100 => Resampler::from_44100(),
    };
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
            Celt::new(self.bits),
            vec![None; self.stretches.len() * 2 * most],
          )
        },
        |(celt, kept), offset| {
          kept.fill(None);
          counted(&runs, 2, |run, channel, frame| {
            let stretch = self.stretches[run]
              .as_ref()
              .expect("a stretch with frames has samples");
            // below the frame, so it fits
            let shift = (stretch.start % FRAME as u64) as usize;
            let start = (offset + FRAME - shift) % FRAME + frame * FRAME;
            *kept[(run * 2 + channel) * most + frame].get_or_insert_with(|| {
              celt.bands(&stretch.channels[channel][start..start + 2 * FRAME])
            })
          })
        },
      )
      .collect()
  }
}

/// counts the frames on the lattice in the windows a file is measured at, with the frame grid
/// of a decode whose start was not cut
pub(crate) struct Uncut {
  celt: Celt,
  rate: Rate,
  /// each window's frames on that grid, by channel
  windows: Vec<Vec<Vec<Bands>>>,
}

impl Uncut {
  pub(crate) fn new(rate: Rate, bits: u32) -> Self {
    Self {
      celt: Celt::new(bits),
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
            let start = offset + frame * FRAME;
            self.celt.bands(&channel[start..start + 2 * FRAME])
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
    counted(&runs, channels, |run, channel, frame| {
      self.windows[run][channel][frame]
    })
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
  /// the filter from 44.1 kHz, made once
  fn from_44100() -> &'static Self {
    static FROM_44100: std::sync::LazyLock<Resampler> =
      std::sync::LazyLock::new(|| Resampler::new(44_100));
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
    assert_eq!(on_lattice(&quiet, &bands, 0.0), (BANDS, BANDS));
    // a frame 1.3 dB louder than the codec made it has every step off the lattice
    let mut louder = bands;
    for energy in &mut louder.energy {
      *energy += 0.21;
    }
    assert_eq!(on_lattice(&quiet, &louder, 0.0), (BANDS, 0));
    // bands under the noise in either frame are not tested
    let mut faint = bands;
    faint.loud[3] = false;
    faint.loud[7] = false;
    assert_eq!(on_lattice(&quiet, &faint, 0.0), (BANDS - 2, BANDS - 2));
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
      assert_eq!(on_lattice(&pair[0], &pair[1], 0.0), (BANDS, 0));
      assert_eq!(on_lattice(&pair[0], &pair[1], level / 2.0), (BANDS, BANDS));
      let named = named(&pair[0], &pair[1]).expect("every band gathers");
      assert!(
        (place_of(named) - place_of(level / 2.0)).abs() < 1e-9,
        "{named}"
      );
    }
    let moved = counted(&[run.len()], 1, |_, _, frame| run[frame]);
    assert_eq!(moved.frames, [0]);
    assert_eq!(moved.shifted, run.len() - 1);
    // frames at the decoder's own level count either way
    let run = run_at(0.0, 9);
    let unmoved = counted(&[run.len()], 1, |_, _, frame| run[frame]);
    assert_eq!(unmoved.frames, [run.len() - 1]);
    assert_eq!(unmoved.shifted, run.len() - 1);
  }

  #[test]
  fn each_channel_has_a_level_of_its_own() {
    let (left, right) = (run_at(-1.0 / DB, 6), run_at(-2.5 / DB, 6));
    let both = counted(&[6], 2, |_, channel, frame| [&left, &right][channel][frame]);
    assert_eq!((both.frames, both.shifted), (vec![0], 5));
    // a channel no shift fits closes the frames the other left open
    let mut noise = run_at(0.0, 6);
    for (frame, bands) in noise.iter_mut().enumerate() {
      for (band, energy) in bands.energy.iter_mut().enumerate() {
        *energy += ((frame * 7 + band * 13) % 17) as f64 * 0.0531;
      }
    }
    let one = counted(&[6], 2, |_, channel, frame| [&left, &noise][channel][frame]);
    assert_eq!(one.shifted, 0);
  }

  #[test]
  fn a_shift_one_alias_further_loses_only_the_lowest_band() {
    let run = run_at(0.0, 2);
    let (tested, hits) = on_lattice(&run[0], &run[1], ALIAS);
    assert_eq!((tested, hits), (BANDS, BANDS - 1));
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
  fn resampling_keeps_a_tone_and_its_place_in_time() {
    // 1 kHz at 44.1 kHz, read at 48 kHz: sample j is the tone at j / 48000 s
    let tone = |n: usize, rate: f64| (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / rate).sin();
    let input: Vec<f64> = (0..4410).map(|n| tone(n, 44_100.0)).collect();
    let output = Resampler::from_44100().run(&input);
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
