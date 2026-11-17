//! Measures of a whole file.
//!
//! They are gathered from its windows and running totals: the cutoff histogram and median,
//! percentiles of the per-window measures, and the file-wide figures.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::engine::Measured;
use crate::window::Window;

/// The width of a cutoff histogram bucket, in Hz.
const HISTOGRAM_HZ: f64 = 500.0;

/// Aggregate measures of a file.
///
/// Each optional measure is `None` when the file had too little non-silent audio to measure it
/// or the measure does not apply to the file.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Measures {
  /// Pairs of a cutoff in Hz and how many non-silent windows have it.
  ///
  /// Cutoffs are rounded to the nearest 500 Hz, and the pairs ascend by cutoff.
  pub cutoff_hz_histogram: Vec<[u32; 2]>,
  /// Median cutoff over the non-silent windows, in Hz.
  pub cutoff_median_hz: Option<u32>,
  /// Drop across the cutoff at the 95th percentile, in dB.
  pub shelf_sharpness_db_p95: Option<f64>,
  /// Median over the windows of the empty bins per populated bin.
  ///
  /// The bins are those of the band gaps are counted in.
  pub sfb21_holes: Option<f64>,
  /// Median over the windows of the noise's repetition at a codec frame length.
  ///
  /// It says how strongly the noise repeats at a lossy codec's frame length, from 0 to 1.
  pub frame_artifact_score: Option<f64>,
  /// Share of the energy above half the band.
  ///
  /// `None` unless the file's rate is above 48 kHz.
  pub upsample_ratio: Option<f64>,
  /// Bits per sample the integer samples use.
  ///
  /// `None` for codecs without integer samples.
  pub bit_depth_effective: Option<u32>,
  /// Level of the quietest passages, in dBFS.
  pub noise_floor_dbfs: Option<f64>,
  /// Correlation between the two channels, `None` for mono.
  pub stereo_correlation: Option<f64>,
  /// Side channel energy relative to the mid channel's, in dB.
  ///
  /// `None` for mono.
  pub side_energy_db: Option<f64>,
  /// Mean sample value relative to full scale.
  pub dc_offset: Option<f64>,
  /// Percentage of samples at full scale.
  pub clipped_samples_pct: Option<f64>,
  /// Loudest sample read on either channel, in dBFS.
  ///
  /// `None` for silence.
  pub peak_dbfs: Option<f64>,
}

/// The aggregate measures over the non-silent `windows`.
pub(crate) fn measures(
  measured: &Measured,
  windows: &[&Window],
  upsample_ratio: Option<f64>,
) -> Measures {
  let mut histogram: BTreeMap<u32, u32> = BTreeMap::new();
  for cutoff in windows.iter().filter_map(|w| w.cutoff_hz) {
    let bucket = ((cutoff / HISTOGRAM_HZ).round() * HISTOGRAM_HZ) as u32;
    *histogram.entry(bucket).or_default() += 1;
  }
  let cutoffs: Vec<f64> = windows.iter().filter_map(|w| w.cutoff_hz).collect();
  let shelves: Vec<f64> = windows.iter().filter_map(|w| w.shelf_db).collect();
  let holes: Vec<f64> = windows.iter().filter_map(|w| w.holes).collect();
  let scores: Vec<f64> = windows.iter().filter_map(|w| w.frame_score).collect();
  let claimed_bits = measured.claimed.bit_depth;
  Measures {
    cutoff_hz_histogram: histogram.into_iter().map(|(hz, n)| [hz, n]).collect(),
    cutoff_median_hz: (!cutoffs.is_empty()).then(|| middle(&cutoffs).round() as u32),
    shelf_sharpness_db_p95: percentile(&shelves, 0.95),
    sfb21_holes: percentile(&holes, 0.5),
    frame_artifact_score: percentile(&scores, 0.5),
    upsample_ratio,
    bit_depth_effective: measured
      .bits_used
      .map(|used| claimed_bits.map_or(used, |claimed| used.min(claimed))),
    noise_floor_dbfs: measured.totals.noise_floor_dbfs(),
    stereo_correlation: measured.totals.stereo_correlation(),
    side_energy_db: measured.totals.side_energy_db(),
    dc_offset: measured.totals.dc_offset(),
    clipped_samples_pct: measured.totals.clipped_pct(),
    peak_dbfs: measured.totals.peak_dbfs(),
  }
}

/// The share of the energy of `windows` above half the band.
pub(crate) fn upsample_ratio(windows: &[&Window]) -> f64 {
  let (high, total) = windows
    .iter()
    .fold((0.0, 0.0), |(h, t), w| (h + w.energy.0, t + w.energy.1));
  if total > 0.0 { high / total } else { 0.0 }
}

/// The median of `values`, 0 for none.
pub(crate) fn middle(values: &[f64]) -> f64 {
  percentile(values, 0.5).unwrap_or(0.0)
}

/// The value `fraction` of the way through `values` in order, by nearest rank.
pub(crate) fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
  if values.is_empty() {
    return None;
  }
  let mut sorted = values.to_vec();
  sorted.sort_by(f64::total_cmp);
  let rank = ((fraction * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
  Some(sorted[rank - 1])
}
