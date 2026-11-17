//! Measures of each one-second window.
//!
//! A window's measures are its loudness, its spectrum averaged over short overlapping frames,
//! where that spectrum stops (the cutoff) and how steeply, gaps in the band just below the
//! cutoff, a step inside a full band, and how strongly the noise repeats at a lossy codec's
//! frame length. Each window's samples are also added to the file-wide totals, and what the
//! spectrogram and the waveform show of it is kept.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::picture::{self, BINS, Shown};
use crate::thresholds::{MeasureThresholds, ReadingThresholds};
use crate::totals::Totals;

/// The length of one spectrum frame in samples.
pub(crate) const FFT_SIZE: usize = 4096;
/// How far one frame starts after the one before.
const HOP: usize = FFT_SIZE / 2;
/// How wide the band is that a spectrum is smoothed over, either side of a bin.
const SMOOTH_HZ: f64 = 50.0;
/// The lowest level a spectrum is measured down to, in dB; anything quieter reads as this.
const FLOOR_DB: f64 = -200.0;
/// The length of the blocks a window's noise envelope is measured in, in samples.
const ENVELOPE_BLOCK: usize = 64;
/// Lossy codecs' frame lengths in samples, as envelope blocks: 1024 (AAC), 1152 (MP3) and 2048
/// (Vorbis long blocks).
const CODEC_PERIODS: [usize; 3] = [
  1024 / ENVELOPE_BLOCK,
  1152 / ENVELOPE_BLOCK,
  2048 / ENVELOPE_BLOCK,
];

/// Step down in level inside a band that reaches the top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
  /// Frequency of the step, in Hz.
  pub hz: f64,
  /// Drop from the band below to the band above, in dB.
  pub drop: f64,
  /// Height of the band above the step over the cutoff line, in dB.
  ///
  /// The cutoff line is the loudest band less `cutoff_range_db`, or the quantization noise plus
  /// `floor_margin_db` when that is higher. A few dB is a noise floor that only just reaches the
  /// line.
  pub clearance: f64,
}

/// Measures of one window.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
  /// Start of the window, in seconds.
  pub t: f64,
  /// Length of the window, in seconds.
  ///
  /// It is 1 but for a shorter last window.
  pub seconds: f64,
  /// Loudness over every analyzed channel, in dBFS.
  pub rms_dbfs: f64,
  /// Highest frequency within the measured range of the loudest band, in Hz.
  ///
  /// `None` for a near-silent window or one too short for a spectrum.
  pub cutoff_hz: Option<f64>,
  /// Drop across the cutoff, in dB.
  ///
  /// It is how much quieter the band just above the cutoff is than the band just below. `None`
  /// without a cutoff or when the cutoff is too near the top of the band to tell.
  pub shelf_db: Option<f64>,
  /// Empty bins per populated bin in the upper band below the cutoff.
  pub holes: Option<f64>,
  /// Steepest step inside a band that reaches the top.
  pub inner_shelf: Option<Step>,
  /// How strongly the noise repeats at a lossy codec's frame length, from 0 to 1.
  pub frame_score: Option<f64>,
  /// Power above half the band, and in the whole band.
  pub energy: (f64, f64),
  /// What the spectrogram and the waveform show of the window.
  pub shown: Shown,
}

impl Window {
  /// Whether the window stops with an encoder's low-pass rather than a natural roll-off: its
  /// shelf drops by `sharp_shelf_db` or more.
  pub(crate) fn sharp(&self, thresholds: &ReadingThresholds) -> bool {
    self
      .shelf_db
      .is_some_and(|shelf| shelf >= thresholds.sharp_shelf_db)
  }

  /// Whether the window is too quiet to measure a spectrum in.
  fn silent(&self, thresholds: &MeasureThresholds) -> bool {
    self.rms_dbfs < thresholds.silence_dbfs
  }
}

/// The spectrum analyzer for one sample rate: the frame transform and the window function.
pub(crate) struct Analyzer<'a> {
  fft: Arc<dyn RealToComplex<f32>>,
  hann: Vec<f32>,
  input: Vec<f32>,
  output: Vec<Complex<f32>>,
  scratch: Vec<Complex<f32>>,
  rate: f64,
  /// the width of one bin in Hz
  bin_hz: f64,
  /// scratch spectra: per frame, the power of every bin over the channels
  frames: Vec<Vec<f64>>,
  /// the level of one bin of the quantization noise integer samples carry, when they are
  /// integers
  quantization_db: Option<f64>,
  /// what the windows are measured with
  thresholds: &'a MeasureThresholds,
}

impl<'a> Analyzer<'a> {
  /// An analyzer for audio at `rate` samples per second.
  pub fn new(rate: u32, bits: Option<u32>, thresholds: &'a MeasureThresholds) -> Self {
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
    let hann = (0..FFT_SIZE)
      .map(|i| {
        let phase = 2.0 * std::f64::consts::PI * i as f64 / FFT_SIZE as f64;
        (0.5 - 0.5 * phase.cos()) as f32
      })
      .collect();
    let rate = f64::from(rate);
    let bin_hz = rate / FFT_SIZE as f64;
    Self {
      input: fft.make_input_vec(),
      output: fft.make_output_vec(),
      scratch: vec![Complex::default(); fft.get_scratch_len()],
      fft,
      hann,
      rate,
      bin_hz,
      frames: Vec::new(),
      quantization_db: bits.map(quantization_db),
      thresholds,
    }
  }

  /// Measures the window starting at `t` seconds, one sample vector per analyzed channel, and
  /// adds its samples to `totals`.
  pub fn measure(&mut self, t: f64, channels: &[Vec<f32>], totals: &mut Totals) -> Window {
    let length = channels[0].len();
    totals.add(channels);
    let squares: f64 = channels
      .iter()
      .flat_map(|c| c.iter())
      .map(|s| f64::from(*s) * f64::from(*s))
      .sum();
    let count = (length * channels.len()).max(1) as f64;
    let rms_dbfs = (10.0 * (squares / count).log10()).max(FLOOR_DB);
    let frame_score = frame_score(channels);
    let spectrum = self.spectrum(channels);
    let shown = self.shown(channels, spectrum.as_deref());
    let mut window = Window {
      t,
      seconds: length as f64 / self.rate,
      rms_dbfs,
      cutoff_hz: None,
      shelf_db: None,
      holes: None,
      inner_shelf: None,
      frame_score: None,
      energy: (0.0, 0.0),
      shown,
    };
    let Some(power) = spectrum else {
      return window;
    };
    let half = (self.rate / 4.0 / self.bin_hz).ceil() as usize;
    window.energy = (power[half..].iter().sum(), power[1..].iter().sum());
    let t = self.thresholds;
    if window.silent(t) {
      return window;
    }
    let smoothed = smooth_db(&power, (SMOOTH_HZ / self.bin_hz).round() as usize);
    let peak = smoothed[1..].iter().copied().fold(FLOOR_DB, f64::max);
    // in a quiet passage the range would reach the quantization noise, which is no content
    let line = self
      .quantization_db
      .map_or(peak - t.cutoff_range_db, |floor| {
        (peak - t.cutoff_range_db).max(floor + t.floor_margin_db)
      });
    let top = smoothed
      .iter()
      .rposition(|level| *level >= line)
      .unwrap_or(0);
    let cutoff = top as f64 * self.bin_hz;
    window.cutoff_hz = Some(cutoff);
    window.shelf_db = self
      .step(&smoothed, cutoff)
      .map(|(below, above)| below - above);
    window.holes = self.holes(cutoff);
    window.frame_score = frame_score;
    if cutoff >= t.full_band * self.rate / 2.0 {
      window.inner_shelf = self.inner_shelf(&smoothed, cutoff, line);
    }
    window
  }

  /// The power of every bin averaged over the window's frames and summed over its channels,
  /// relative to a full-scale sine; `None` for a window shorter than one frame. The power of
  /// each frame is kept in `frames` for [`Analyzer::holes`].
  fn spectrum(&mut self, channels: &[Vec<f32>]) -> Option<Vec<f64>> {
    let length = channels[0].len();
    if length < FFT_SIZE {
      self.frames.clear();
      return None;
    }
    let count = (length - FFT_SIZE) / HOP + 1;
    let bins = FFT_SIZE / 2 + 1;
    // a full-scale sine's bin reads 1 under a Hann window
    let norm = (FFT_SIZE as f64 / 4.0).powi(2);
    self.frames.resize_with(count, Vec::new);
    let mut total = vec![0.0; bins];
    for (index, frame) in self.frames.iter_mut().enumerate() {
      frame.clear();
      frame.resize(bins, 0.0);
      let start = index * HOP;
      for channel in channels {
        let samples = &channel[start..start + FFT_SIZE];
        for ((input, sample), weight) in self.input.iter_mut().zip(samples).zip(&self.hann) {
          *input = sample * weight;
        }
        self
          .fft
          .process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch)
          .expect("the buffers are the ones the plan made");
        for (power, value) in frame.iter_mut().zip(&self.output) {
          *power += f64::from(value.norm_sqr()) / norm;
        }
      }
      for (sum, power) in total.iter_mut().zip(frame.iter()) {
        *sum += power;
      }
    }
    for sum in &mut total {
      *sum /= count as f64;
    }
    Some(total)
  }

  /// What the spectrogram and the waveform show of a window of `channels`, from its spectrum
  /// and its frames' spectra; silence for a window too short for a spectrum.
  fn shown(&self, channels: &[Vec<f32>], spectrum: Option<&[f64]>) -> Shown {
    let length = channels[0].len();
    let rate = self.rate as usize;
    Shown {
      spectrum: spectrum.map_or_else(|| vec![0; BINS], picture::spectrum_bytes),
      columns: picture::columns(&self.frames, length, rate, FFT_SIZE, HOP),
      peaks: picture::peaks(channels, rate),
    }
  }

  /// The bins from `low` to `high` Hz, clamped to the spectrum.
  fn bins(&self, low: f64, high: f64) -> std::ops::RangeInclusive<usize> {
    let last = FFT_SIZE / 2;
    let first = ((low / self.bin_hz).ceil().max(1.0) as usize).min(last);
    let end = ((high / self.bin_hz).floor().max(0.0) as usize).min(last);
    first..=end
  }

  /// The mean level of `smoothed` between `low` and `high` Hz.
  fn level(&self, smoothed: &[f64], low: f64, high: f64) -> Option<f64> {
    let range = self.bins(low, high);
    let count = range.clone().count();
    (count > 0).then(|| smoothed[range].iter().sum::<f64>() / count as f64)
  }

  /// The levels either side of `hz`: of the band below it and of the band above it, each
  /// `shelf_span_hz` wide and `shelf_gap_hz` away; `None` when the band above passes the top of
  /// the spectrum.
  fn step(&self, smoothed: &[f64], hz: f64) -> Option<(f64, f64)> {
    let nyquist = self.rate / 2.0;
    let (gap, span) = (self.thresholds.shelf_gap_hz, self.thresholds.shelf_span_hz);
    if hz + gap + span > nyquist || hz < gap + span {
      return None;
    }
    let below = self.level(smoothed, hz - gap - span, hz - gap)?;
    let above = self.level(smoothed, hz + gap, hz + gap + span)?;
    Some((below, above))
  }

  /// The steepest step in the upper band of a window whose content reaches `cutoff` near the
  /// top, searched every 100 Hz, with how far the band above it is over `line`.
  fn inner_shelf(&self, smoothed: &[f64], cutoff: f64, line: f64) -> Option<Step> {
    let mut best: Option<Step> = None;
    let t = self.thresholds;
    // a step is only measured inside the spectrum, so the search covers no more than it
    let mut hz = t.inner_shelf_from_hz.max(0.0);
    while hz <= self.rate / 2.0 && hz + t.shelf_gap_hz + t.shelf_span_hz <= cutoff {
      if let Some((below, above)) = self.step(smoothed, hz)
        && best.is_none_or(|deepest| below - above > deepest.drop)
      {
        best = Some(Step {
          hz,
          drop: below - above,
          clearance: above - line,
        });
      }
      hz += 100.0;
    }
    best
  }

  /// How many bins from `holes_from_hz` to `holes_to_hz`, below `cutoff`, are much quieter than
  /// the bins around them, per bin that is not, averaged over the window's frames. `None` when
  /// less than a kilohertz of that band is below the cutoff.
  fn holes(&self, cutoff: f64) -> Option<f64> {
    let t = self.thresholds;
    let high = t.holes_to_hz.min(cutoff - t.shelf_gap_hz * 2.0);
    if high - t.holes_from_hz < 1000.0 || self.frames.is_empty() {
      return None;
    }
    let range = self.bins(t.holes_from_hz, high);
    let radius = (SMOOTH_HZ * 2.0 / self.bin_hz).round() as usize;
    let mut ratios = 0.0;
    for frame in &self.frames {
      let smoothed = smooth_db(frame, radius);
      let (mut holes, mut populated) = (0u32, 0u32);
      for bin in range.clone() {
        if db(frame[bin]) < smoothed[bin] - t.hole_depth_db {
          holes += 1;
        } else {
          populated += 1;
        }
      }
      ratios += f64::from(holes) / f64::from(populated.max(1));
    }
    Some(ratios / self.frames.len() as f64)
  }
}

/// The level of one bin of the noise `bits`-bit samples carry, on the scale a full-scale sine
/// reads 0 dB: dithered quantization adds a quarter of a step squared per sample, and the
/// windowed transform puts 6 / [`FFT_SIZE`] of a sample's noise power in each bin.
fn quantization_db(bits: u32) -> f64 {
  let step = 2f64.powi(1 - bits.clamp(2, 32) as i32);
  10.0 * (step * step / 4.0 * 6.0 / FFT_SIZE as f64).log10()
}

/// A power as dB, no lower than [`FLOOR_DB`].
fn db(power: f64) -> f64 {
  if power > 0.0 {
    (10.0 * power.log10()).max(FLOOR_DB)
  } else {
    FLOOR_DB
  }
}

/// `power` averaged over `radius` bins either side of each bin, in dB.
fn smooth_db(power: &[f64], radius: usize) -> Vec<f64> {
  let mut prefix = Vec::with_capacity(power.len() + 1);
  prefix.push(0.0);
  for value in power {
    prefix.push(prefix.last().copied().unwrap_or(0.0) + value);
  }
  (0..power.len())
    .map(|bin| {
      let first = bin.saturating_sub(radius);
      let last = (bin + radius).min(power.len() - 1);
      db((prefix[last + 1] - prefix[first]) / (last - first + 1) as f64)
    })
    .collect()
}

/// How strongly the energy of a window's high-frequency noise repeats every 1024, 1152 or
/// 2048 samples beyond what the neighboring periods show, from 0 to 1. A lossy codec
/// quantizes each frame on its own, which can leave that rhythm behind after decoding.
fn frame_score(channels: &[Vec<f32>]) -> Option<f64> {
  let length = channels[0].len();
  let blocks = length / ENVELOPE_BLOCK;
  let longest = CODEC_PERIODS.iter().max().copied().unwrap_or(0) + 3;
  if blocks < longest * 4 {
    return None;
  }
  let mut envelope = vec![0.0f64; blocks];
  for channel in channels {
    for (block, energy) in envelope.iter_mut().enumerate() {
      let start = (block * ENVELOPE_BLOCK).max(1);
      let end = (block + 1) * ENVELOPE_BLOCK;
      *energy += (start..end)
        .map(|i| {
          let difference = f64::from(channel[i]) - f64::from(channel[i - 1]);
          difference * difference
        })
        .sum::<f64>();
    }
  }
  let mean = envelope.iter().sum::<f64>() / blocks as f64;
  for energy in &mut envelope {
    *energy -= mean;
  }
  let lag = |k: usize| -> f64 { (0..blocks - k).map(|i| envelope[i] * envelope[i + k]).sum() };
  let zero = lag(0);
  if zero <= 0.0 {
    return Some(0.0);
  }
  let correlation = |k: usize| lag(k) / zero;
  // a rhythm at the period stands above every lag near it; an envelope that swings faster,
  // as a steady tone's does against the blocks, has neighbors as high
  let excess = CODEC_PERIODS
    .iter()
    .map(|&period| {
      let around = (period - 3..=period + 3)
        .filter(|k| *k != period)
        .map(correlation)
        .fold(f64::MIN, f64::max);
      correlation(period) - around
    })
    .fold(0.0, f64::max);
  Some(excess.clamp(0.0, 1.0))
}
