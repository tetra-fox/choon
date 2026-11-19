//! the stretches of a file the grid is scanned in, filled as its samples are decoded

use super::resample::Resampler;
use super::{SEGMENT, SEGMENTS};

/// where the segments start, as fractions of the file's length
const SEGMENT_AT: [f64; SEGMENTS] = [0.2, 0.4, 0.6, 0.8];

/// one stretch of the file the grid is scanned in, filled as its samples are decoded
pub(super) struct Segment {
  /// the sample frame it starts at
  pub(super) start: u64,
  pub(super) left: Vec<f64>,
  pub(super) right: Vec<f64>,
}

impl Segment {
  /// the sample frame the segment needs next
  fn next(&self) -> u64 {
    self.start + self.left.len() as u64
  }

  fn full(&self) -> bool {
    self.left.len() == SEGMENT
  }

  /// the segment at the other of 44.1 and 48 kHz, cut to the length of one at the file's rate.
  /// its start is a frame of the file at that rate. `None` when nothing is left of it
  pub(super) fn brought(&self, resampler: &Resampler) -> Option<Self> {
    let (channels, start) = resampler.brought(&[&self.left, &self.right], self.start)?;
    let [mut left, mut right] = <[Vec<f64>; 2]>::try_from(channels).ok()?;
    left.truncate(SEGMENT);
    right.truncate(SEGMENT);
    Some(Self { start, left, right })
  }

  /// how many of `available` frames from `first` on the segment takes, and from which
  fn wanted(&self, first: u64, available: usize) -> Option<(usize, usize)> {
    let next = self.next();
    let from = usize::try_from(next.checked_sub(first)?).ok()?;
    let count = (SEGMENT - self.left.len()).min(available.checked_sub(from)?);
    (count > 0).then_some((from, count))
  }
}

/// the segments of a file the grid is scanned in
pub(crate) struct Segments(pub(super) Vec<Segment>);

impl Segments {
  /// the segments of a file of `frames` sample frames; `None` when it is shorter than one
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

  /// takes what the segments need of `window`, one vector per analyzed channel, whose first
  /// sample is frame `first`. a mono window counts as both channels.
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

  /// takes what the segments need of `samples`, interleaved over `channels` channels, each at
  /// the top of its 32 bits, whose first sample is frame `first`
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

  /// the indices of the segments not yet full
  pub(crate) fn missing(&self) -> Vec<usize> {
    (0..self.0.len()).filter(|i| !self.0[*i].full()).collect()
  }

  /// the sample frame segment `index` needs next; `None` once it is full
  pub(crate) fn needs(&self, index: usize) -> Option<u64> {
    let segment = &self.0[index];
    (!segment.full()).then(|| segment.next())
  }

  /// whether segment `index` has no samples yet
  pub(crate) fn untouched(&self, index: usize) -> bool {
    self.0[index].left.is_empty()
  }

  /// whether every segment is full
  pub(crate) fn complete(&self) -> bool {
    self.0.iter().all(Segment::full)
  }
}
