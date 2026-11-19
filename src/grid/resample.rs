//! brings samples from 44.1 to 48 kHz or back with a long filter that keeps the sample times
//! the two rates share, so that a frame grid laid at one rate is looked for in a file at the
//! other

use std::sync::LazyLock;

use super::aac::bessel_i0;

/// samples of the lower rate on each side of an output sample that the filter reads
const HALF: usize = 64;
/// the Kaiser window's beta of the filter
const KAISER_BETA: f64 = 12.0;

/// a polyphase windowed-sinc filter from one rate to another. output sample j sits at input
/// time j * down / up from the input's first sample, so the sample times the two rates share
/// stay shared
pub(crate) struct Resampler {
  pub(crate) up: usize,
  pub(crate) down: usize,
  /// the taps on each side of the filter's middle, at the rate both rates divide
  reach: usize,
  /// for each of the `up` phases, the taps that meet the input from the oldest sample read to
  /// the newest
  phases: Vec<Vec<f64>>,
}

impl Resampler {
  /// the filter from 44.1 to 48 kHz, made once
  pub(crate) fn to_48000() -> &'static Self {
    static FILTER: LazyLock<Resampler> = LazyLock::new(|| Resampler::new(44_100, 48_000));
    &FILTER
  }

  /// the filter from 48 to 44.1 kHz, made once
  pub(crate) fn to_44100() -> &'static Self {
    static FILTER: LazyLock<Resampler> = LazyLock::new(|| Resampler::new(48_000, 44_100));
    &FILTER
  }

  fn new(from: usize, to: usize) -> Self {
    let common = gcd(from, to);
    let (up, down) = (to / common, from / common);
    // the filter at the rate both divide: a sinc cut at the lower rate's band edge under a
    // Kaiser window, HALF samples of the lower rate to each side
    let lower = up.max(down);
    let reach = HALF * lower;
    let cutoff = 1.0 / lower as f64;
    let taps: Vec<f64> = (0..=2 * reach)
      .map(|n| {
        let m = n as f64 - reach as f64;
        let x = std::f64::consts::PI * cutoff * m;
        let sinc = if m == 0.0 { 1.0 } else { x.sin() / x };
        let r = m / reach as f64;
        let kaiser =
          bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(KAISER_BETA);
        cutoff * sinc * kaiser
      })
      .collect();
    // unity gain for the `up` phases together
    let gain = up as f64 / taps.iter().sum::<f64>();
    // y_j is the sum over i of x_i h[j down + reach - i up]: tap `phase + k up` meets the
    // sample k before the newest
    let phases = (0..up)
      .map(|phase| {
        let mut taps: Vec<f64> = (0..=2 * reach / up)
          .map(|k| taps.get(phase + k * up).map_or(0.0, |tap| tap * gain))
          .collect();
        taps.reverse();
        taps
      })
      .collect();
    Self {
      up,
      down,
      reach,
      phases,
    }
  }

  /// the output samples at each end that lack input the filter reads
  fn edge(&self) -> usize {
    self.reach.div_ceil(self.down)
  }

  /// `input` at the other rate: floor(len * up / down) samples, with silence past both ends
  fn run(&self, input: &[f64]) -> Vec<f64> {
    let (up, down) = (self.up, self.down);
    // the samples before its newest that an output reads, and silence on both sides for them
    let before = 2 * self.reach / up;
    let mut padded = vec![0.0; input.len() + 2 * before];
    padded[before..before + input.len()].copy_from_slice(input);
    (0..input.len() * up / down)
      .map(|j| {
        let at = j * down + self.reach;
        let (newest, phase) = (at / up, at % up);
        // the newest sample read is `newest` of the input, `before` further in `padded`
        dot(&self.phases[phase], &padded[newest..=newest + before])
      })
      .collect()
  }

  /// `channels`, whose first sample is frame `first` of the file, at the other rate: from the
  /// first sample at a time both rates share, without the ends that lack input, and the frame
  /// of the file at that rate that the first one left is. `None` when nothing is left
  pub(crate) fn brought(&self, channels: &[&[f64]], first: u64) -> Option<(Vec<Vec<f64>>, u64)> {
    let down = self.down as u64;
    let skip = ((down - first % down) % down) as usize;
    let edge = self.edge();
    let mut resampled: Vec<Vec<f64>> = channels
      .iter()
      .map(|c| self.run(c.get(skip..).unwrap_or(&[])))
      .collect();
    let length = resampled.first()?.len();
    if length <= 2 * edge {
      return None;
    }
    for channel in &mut resampled {
      channel.truncate(length - edge);
      channel.drain(..edge);
    }
    let start = (first + skip as u64) / down * self.up as u64 + edge as u64;
    Some((resampled, start))
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
  fn resampling_keeps_a_tone_and_its_place_in_time() {
    // 1 kHz at one rate, read at the other: sample j is the tone at j seconds over that rate
    let tone = |n: usize, rate: f64| (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / rate).sin();
    for (resampler, from, to) in [
      (Resampler::to_48000(), 44_100, 48_000),
      (Resampler::to_44100(), 48_000, 44_100),
    ] {
      let input: Vec<f64> = (0..from / 10).map(|n| tone(n, from as f64)).collect();
      let output = resampler.run(&input);
      assert_eq!(output.len(), to / 10);
      let edge = resampler.edge();
      for (j, sample) in output.iter().enumerate().take(to / 10 - edge).skip(edge) {
        assert!((sample - tone(j, to as f64)).abs() < 1e-5, "{j}: {sample}");
      }
    }
  }

  #[test]
  fn a_stretch_starts_at_a_time_both_rates_share() {
    // a stretch from frame 1000 of a file at 44.1 kHz: the next shared time is frame 1029,
    // which is frame 1120 at 48 kHz, and 70 samples after it lack input
    let silence = vec![0.0; 44_100];
    let (channels, start) = Resampler::to_48000()
      .brought(&[&silence], 1000)
      .expect("a second leaves samples");
    assert_eq!(start, 1120 + 70);
    assert_eq!(channels[0].len(), (44_100 - 29) * 160 / 147 - 140);
    assert!(
      Resampler::to_48000()
        .brought(&[&silence[..100]], 0)
        .is_none()
    );
  }
}
