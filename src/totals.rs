//! figures kept over every analyzed sample: the mean, the peak, the count at full scale, the sums
//! for two channels' correlation and side energy, and a histogram of short blocks' levels

/// the length of the blocks the quietest level is measured in, in samples
const LEVEL_BLOCK: usize = 1024;
/// the quietest block level the histogram counts, in dBFS; one bucket per whole dB up to 0
const LEVELS_FROM_DB: f64 = -160.0;
/// a sample this far from zero or further counts as at full scale
pub(crate) const CLIPPED: f64 = 0.9999;

#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub(crate) struct Totals {
  samples: u64,
  sum: f64,
  clipped: u64,
  /// the largest sample magnitude
  peak: f64,
  /// frames of two channels, with the sums correlation and the side energy need
  pairs: u64,
  left: f64,
  right: f64,
  left_squared: f64,
  right_squared: f64,
  product: f64,
  /// the count of level blocks per whole dB from [`LEVELS_FROM_DB`]
  levels: Vec<u64>,
}

impl Totals {
  /// the mean sample value, relative to full scale
  pub fn dc_offset(&self) -> Option<f64> {
    (self.samples > 0).then(|| self.sum / self.samples as f64)
  }

  /// the percentage of samples at full scale
  pub fn clipped_pct(&self) -> Option<f64> {
    (self.samples > 0).then(|| 100.0 * self.clipped as f64 / self.samples as f64)
  }

  /// the loudest sample in dBFS; `None` when every sample is silent
  pub fn peak_dbfs(&self) -> Option<f64> {
    (self.peak > 0.0).then(|| 20.0 * self.peak.log10())
  }

  /// the correlation between the two channels, from -1 to 1; `None` for mono or when either
  /// channel is constant, or so near it that rounding hides its spread
  pub fn stereo_correlation(&self) -> Option<f64> {
    if self.pairs == 0 {
      return None;
    }
    let n = self.pairs as f64;
    let covariance = self.product - self.left * self.right / n;
    let left = self.left_squared - self.left * self.left / n;
    let right = self.right_squared - self.right * self.right / n;
    // summing n terms can be off by about n * EPSILON of their sum, so a variance under that
    // share of the channel's sum of squares cannot be told from 0, as a constant channel's is
    let rounding = n * f64::EPSILON;
    if left <= rounding * self.left_squared || right <= rounding * self.right_squared {
      return None;
    }
    Some((covariance / (left * right).sqrt()).clamp(-1.0, 1.0))
  }

  /// the side channel's energy relative to the mid channel's, in dB, at least -120 dB
  pub fn side_energy_db(&self) -> Option<f64> {
    if self.pairs == 0 {
      return None;
    }
    let mid = self.left_squared + self.right_squared + 2.0 * self.product;
    let side = self.left_squared + self.right_squared - 2.0 * self.product;
    (mid > 0.0).then(|| (10.0 * (side.max(0.0) / mid).log10()).max(-120.0))
  }

  /// the level of the quietest passages, in dBFS: the second percentile of the loudness of
  /// short blocks, leaving out blocks of digital silence
  pub fn noise_floor_dbfs(&self) -> Option<f64> {
    // counts read back from kept measures are any numbers, so they are summed without overflow
    let total = self
      .levels
      .iter()
      .fold(0u64, |sum, count| sum.saturating_add(*count));
    if total == 0 {
      return None;
    }
    let wanted = total.div_ceil(50);
    let mut seen = 0u64;
    for (index, count) in self.levels.iter().enumerate() {
      seen = seen.saturating_add(*count);
      if seen >= wanted {
        return Some(LEVELS_FROM_DB + index as f64);
      }
    }
    None
  }

  /// adds a window's samples, one vector per channel
  pub fn add(&mut self, channels: &[Vec<f32>]) {
    if self.levels.is_empty() {
      self.levels = vec![0; -LEVELS_FROM_DB as usize + 1];
    }
    for channel in channels {
      for sample in channel {
        let value = f64::from(*sample);
        self.samples += 1;
        self.sum += value;
        self.peak = self.peak.max(value.abs());
        if value.abs() >= CLIPPED {
          self.clipped += 1;
        }
      }
    }
    if let [left, right] = channels {
      for (l, r) in left.iter().zip(right) {
        let (l, r) = (f64::from(*l), f64::from(*r));
        self.pairs += 1;
        self.left += l;
        self.right += r;
        self.left_squared += l * l;
        self.right_squared += r * r;
        self.product += l * r;
      }
    }
    for start in (0..channels[0].len()).step_by(LEVEL_BLOCK) {
      let end = (start + LEVEL_BLOCK).min(channels[0].len());
      let squares: f64 = channels
        .iter()
        .flat_map(|c| &c[start..end])
        .map(|s| f64::from(*s) * f64::from(*s))
        .sum();
      if squares == 0.0 {
        continue;
      }
      let level = 10.0 * (squares / ((end - start) * channels.len()) as f64).log10();
      let index = (level - LEVELS_FROM_DB).floor().clamp(0.0, -LEVELS_FROM_DB) as usize;
      self.levels[index] += 1;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::fixtures::{music_for, pink};

  /// the totals of `left` and `right`, a second at a time
  fn of(left: &[f32], right: &[f32]) -> Totals {
    let mut totals = Totals::default();
    for (l, r) in left.chunks(44_100).zip(right.chunks(44_100)) {
      totals.add(&[l.to_vec(), r.to_vec()]);
    }
    totals
  }

  #[test]
  fn a_constant_channel_has_no_correlation_with_the_other() {
    let noise = music_for(30, 1, 44_100, pink).channels.swap_remove(1);
    for level in [0.1, 0.3, 0.7, 1.0 / 3.0] {
      let constant = vec![level; noise.len()];
      let totals = of(&constant, &noise);
      assert_eq!(totals.stereo_correlation(), None, "{level}");
    }
    let inverted: Vec<f32> = noise.iter().map(|s| -s).collect();
    let same = of(&noise, &noise).stereo_correlation().unwrap();
    let opposite = of(&noise, &inverted).stereo_correlation().unwrap();
    assert!((same - 1.0).abs() < 1e-9, "{same}");
    assert!((opposite + 1.0).abs() < 1e-9, "{opposite}");
  }
}
