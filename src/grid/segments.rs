//! the stretches of a file the grid is scanned in, filled as its samples are decoded: four at
//! fixed places, and for each of those that is clipped or far quieter the loudest stretch of the
//! file that is not clipped

use super::resample::Resampler;
use super::{SEGMENT, SEGMENTS};
use crate::totals::CLIPPED;

/// where the segments start, as fractions of the file's length
const SEGMENT_AT: [f64; SEGMENTS] = [0.2, 0.4, 0.6, 0.8];

/// the share of a stretch's frames at full scale past which it is clipped. clipping after a
/// decode fills the bands the encoder emptied, and the grid's count with them
const CLIPPED_SHARE: f64 = 0.05;

/// the mean square under which a stretch is too quiet to stand in for a clipped segment,
/// -40 dBFS: a fade or a pause holds too few values over the noise for a count
const QUIET_POWER: f64 = 1e-4;

/// the share of the loudest free stretch's power under which a segment gives way to it, 10 dB:
/// a quiet passage holds few values an encoder had to quantize
const QUIETER: f64 = 0.1;

/// one stretch of the file the grid is scanned in, filled as its samples are decoded
pub(super) struct Segment {
  /// the sample frame it starts at
  pub(super) start: u64,
  pub(super) left: Vec<f64>,
  pub(super) right: Vec<f64>,
  /// the frames with a channel at full scale
  clipped: usize,
  /// the sum of both channels' squares
  power: f64,
}

impl Segment {
  fn starting(start: u64) -> Self {
    Self {
      start,
      left: Vec::with_capacity(SEGMENT),
      right: Vec::with_capacity(SEGMENT),
      clipped: 0,
      power: 0.0,
    }
  }

  fn push(&mut self, left: f64, right: f64) {
    self.clipped += usize::from(left.abs() >= CLIPPED || right.abs() >= CLIPPED);
    self.power += left * left + right * right;
    self.left.push(left);
    self.right.push(right);
  }

  /// whether more of its frames are at full scale than a stretch the grid is counted in holds
  fn is_clipped(&self) -> bool {
    self.clipped as f64 > CLIPPED_SHARE * SEGMENT as f64
  }

  fn overlaps(&self, other: &Self) -> bool {
    self.start < other.start + SEGMENT as u64 && other.start < self.start + SEGMENT as u64
  }

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
    // a stretch brought to another rate is scanned as it is, whatever clips
    Some(Self {
      start,
      left,
      right,
      clipped: 0,
      power: 0.0,
    })
  }

  /// how many of `available` frames from `first` on the segment takes, and from which
  fn wanted(&self, first: u64, available: usize) -> Option<(usize, usize)> {
    let next = self.next();
    let from = usize::try_from(next.checked_sub(first)?).ok()?;
    let count = (SEGMENT - self.left.len()).min(available.checked_sub(from)?);
    (count > 0).then_some((from, count))
  }
}

/// the stretches of the stream beside the segments: the one being filled, and the loudest of
/// those not clipped, for the segments that are
#[derive(Default)]
struct Spares {
  filling: Option<Segment>,
  kept: Vec<Segment>,
}

impl Spares {
  /// more than the segments, since one that overlaps a segment kept cannot stand in
  const KEPT: usize = 2 * SEGMENTS;

  /// cuts the stream into stretches a segment long. `first` is the frame of the first sample;
  /// a stretch the stream leaves before it is full is dropped
  fn take(&mut self, first: u64, left: &[f32], right: &[f32]) {
    if self.filling.as_ref().is_none_or(|s| s.next() != first) {
      self.filling = Some(Segment::starting(first));
    }
    for (l, r) in left.iter().zip(right) {
      let filling = self.filling.get_or_insert_with(|| Segment::starting(first));
      filling.push(f64::from(*l), f64::from(*r));
      if filling.full() {
        let next = filling.next();
        let full = self.filling.replace(Segment::starting(next));
        self.keep(full.expect("the stretch was just filled"));
      }
    }
  }

  /// keeps `stretch` among the loudest that are not clipped
  fn keep(&mut self, stretch: Segment) {
    let quiet = stretch.power < QUIET_POWER * 2.0 * SEGMENT as f64;
    if stretch.is_clipped() || quiet {
      return;
    }
    self.kept.push(stretch);
    // the loudest first, and of two as loud the earlier
    self
      .kept
      .sort_by(|a, b| b.power.total_cmp(&a.power).then(a.start.cmp(&b.start)));
    self.kept.truncate(Self::KEPT);
  }
}

/// the segments of a file the grid is scanned in, and the stretches that can stand in for them
pub(crate) struct Segments(pub(super) Vec<Segment>, Spares);

impl Segments {
  /// the segments of a file of `frames` sample frames; `None` when it is shorter than one
  pub(crate) fn of(frames: u64) -> Option<Self> {
    let length = SEGMENT as u64;
    if frames < length {
      return None;
    }
    let segments = SEGMENT_AT
      .iter()
      // a length below 2^53 is exact as an f64; the start is the fraction floored
      .map(|at| Segment::starting(((frames as f64 * at) as u64).min(frames - length)))
      .collect();
    Some(Self(segments, Spares::default()))
  }

  /// puts the loudest stretch of the stream that is not clipped in the place of each segment
  /// that is, or that is far quieter than it, and returns how many it replaced. a stretch that
  /// overlaps a segment kept is passed over, so no samples are counted twice
  pub(crate) fn settle(&mut self) -> usize {
    let mut replaced = 0;
    for index in 0..self.0.len() {
      if !self.0[index].full() {
        continue;
      }
      let free = self.1.kept.iter().position(|spare| {
        self
          .0
          .iter()
          .enumerate()
          .all(|(other, segment)| other == index || !spare.overlaps(segment))
      });
      let gives_way =
        |spare: &Segment| self.0[index].is_clipped() || self.0[index].power < QUIETER * spare.power;
      if let Some(at) = free.filter(|at| gives_way(&self.1.kept[*at])) {
        self.0[index] = self.1.kept.remove(at);
        replaced += 1;
      }
    }
    self.0.sort_by_key(|segment| segment.start);
    replaced
  }

  /// how many segments are clipped: after [`Segments::settle`], the ones no stretch of the
  /// stream could stand in for
  pub(crate) fn clipped(&self) -> usize {
    self.0.iter().filter(|s| s.full() && s.is_clipped()).count()
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
        for (l, r) in left[range.clone()].iter().zip(&right[range]) {
          segment.push(f64::from(*l), f64::from(*r));
        }
      }
    }
    self.1.take(first, left, right);
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
          segment.push(f64::from(frame[0]) * scale, f64::from(*right) * scale);
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
