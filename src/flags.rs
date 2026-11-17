//! The findings reported beside the reading, independent of the cutoff: a file made from
//! audio at a lower sample rate, samples that use fewer bits than they claim, and two channels
//! that hold the same audio.

use serde::Serialize;

use crate::aggregate::Measures;
use crate::decode::Claimed;
use crate::measure::Window;
use crate::thresholds::ReadingThresholds;

/// A finding reported beside the reading, independent of the cutoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
  /// the content stops with a cliff at the top of a lower sample rate's band where no encoder
  /// cuts, so the audio was made at that rate and converted up
  Upsampled,
  /// a file claiming more than 16 bits carries no more than 16
  BitPadded,
}

/// What was found beside the reading.
pub(crate) struct Findings {
  pub flags: Vec<Flag>,
  /// a sentence for the summary per finding
  pub notes: Vec<String>,
  /// the top of the band of a lower rate the content stops at with a cliff, in Hz; flagged
  /// `upsampled` only where no encoder cuts at the same frequency
  pub source_edge: Option<f64>,
}

/// The findings in a file claiming `claimed`, from its non-silent `windows`, which cut off at
/// `median`, and its `measures`.
pub(crate) fn findings(
  claimed: &Claimed,
  windows: &[&Window],
  median: Option<f64>,
  measures: &Measures,
  thresholds: &ReadingThresholds,
) -> Findings {
  let mut found = Findings {
    flags: Vec::new(),
    notes: Vec::new(),
    source_edge: None,
  };
  let nyquist = f64::from(claimed.sample_rate) / 2.0;
  found.source_edge = median.and_then(|m| source_edge(windows, m, nyquist, thresholds));
  // where an encoder cuts at the same edge the cliff proves nothing on its own, and the
  // reading names both
  if let Some(edge) = found
    .source_edge
    .filter(|edge| thresholds.profile_at(*edge).is_none())
  {
    found.flags.push(Flag::Upsampled);
    found.notes.push(format!(
      "The content stops sharply at {:.2} kHz, the upper limit of a lower sample rate, so the \
       audio was probably recorded at that rate.",
      edge / 1000.0
    ));
  }
  if let (Some(claim), Some(used)) = (claimed.bit_depth, measures.bit_depth_effective)
    && claim > 16
    && used <= 16
  {
    found.flags.push(Flag::BitPadded);
    found.notes.push(format!(
      "The samples use only {used} of their {claim} bits."
    ));
  }
  if measures
    .stereo_correlation
    .is_some_and(|c| c > thresholds.mono_correlation)
    && measures
      .side_energy_db
      .is_some_and(|s| s <= thresholds.mono_side_db)
  {
    found
      .notes
      .push("The two channels are identical, so the source was probably mono.".to_string());
  }
  found
}

/// The band edge of a rate lower than the file's (whose band ends at `nyquist`) the content
/// stops at with a cliff: the median cutoff is at the edge and most windows near it have a
/// sharp shelf there. A natural roll-off, however little it leaves above, has no cliff.
fn source_edge(
  windows: &[&Window],
  median: f64,
  nyquist: f64,
  thresholds: &ReadingThresholds,
) -> Option<f64> {
  let cluster = thresholds.cluster_hz;
  let edge = thresholds
    .lower_rate_edges
    .iter()
    .copied()
    .filter(|edge| *edge < nyquist - cluster)
    .find(|edge| (median - edge).abs() <= cluster)?;
  let near: Vec<&&Window> = windows
    .iter()
    .filter(|w| w.cutoff_hz.is_some_and(|c| (c - edge).abs() <= cluster))
    .collect();
  let sharp = near.iter().filter(|w| w.sharp(thresholds)).count();
  (near.len() * 2 > windows.len() && sharp * 2 > near.len()).then_some(edge)
}
