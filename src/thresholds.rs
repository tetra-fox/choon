//! thresholds the analyzer measures and reads with, all data the caller passes

/// a cutoff at or above this fraction of the band's top counts as reaching it, when a window is
/// measured and when it is read
const FULL_BAND: f64 = 0.9;

/// Thresholds each window is measured with.
///
/// Changing one means measuring the audio again.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
  feature = "serde",
  serde(default, deny_unknown_fields, rename_all = "camelCase")
)]
pub struct MeasureThresholds {
  /// Level in dBFS under which a window is near-silent.
  ///
  /// A near-silent window has no cutoff.
  pub silence_dbfs: f64,
  /// Range in dB under the loudest band that the cutoff lies in.
  ///
  /// The cutoff is the highest band within this range.
  pub cutoff_range_db: f64,
  /// Margin in dB of the cutoff line over quantization noise.
  ///
  /// The line never sits closer to the noise of the samples' bit depth, so a quiet passage's
  /// quantization noise does not read as content.
  pub floor_margin_db: f64,
  /// Gap in Hz to the bands compared across a frequency.
  pub shelf_gap_hz: f64,
  /// Width in Hz of the bands compared across a frequency.
  pub shelf_span_hz: f64,
  /// Fraction of the top a cutoff needs for an inner step search.
  ///
  /// A window is searched for a step inside its band when its cutoff is at or above this
  /// fraction of the band's top.
  pub full_band: f64,
  /// Lowest frequency in Hz an inner step is looked for at.
  pub inner_shelf_from_hz: f64,
  /// Start in Hz of the band gaps are counted in.
  ///
  /// It is where MP3's top scale-factor band sits.
  pub holes_from_hz: f64,
  /// Top in Hz of the band gaps are counted in.
  pub holes_to_hz: f64,
  /// Depth in dB under its neighbors at which a bin is a gap.
  pub hole_depth_db: f64,
}

impl Default for MeasureThresholds {
  fn default() -> Self {
    Self {
      silence_dbfs: -70.0,
      cutoff_range_db: 60.0,
      floor_margin_db: 10.0,
      shelf_gap_hz: 100.0,
      shelf_span_hz: 500.0,
      full_band: FULL_BAND,
      inner_shelf_from_hz: 10_000.0,
      holes_from_hz: 16_000.0,
      holes_to_hz: 20_000.0,
      hole_depth_db: 20.0,
    }
  }
}

/// Thresholds the measures are read with.
///
/// A reading is quick, so these can be swept over audio measured once.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
  feature = "serde",
  serde(default, deny_unknown_fields, rename_all = "camelCase")
)]
pub struct ReadingThresholds {
  /// Least drop in dB across the cutoff for an encoder's low-pass.
  ///
  /// A smaller drop is a natural roll-off.
  pub sharp_shelf_db: f64,
  /// Fraction of the top from which a cutoff counts as full band.
  ///
  /// A cutoff at or above this fraction of the band's top counts as reaching it. Set below
  /// [`MeasureThresholds::full_band`], it counts as full some windows that measuring never
  /// searched for inner steps.
  pub full_band: f64,
  /// Fraction of the top under which a gentle roll-off is not judged.
  ///
  /// Content that rolls off gently below it is band-limited by nature, and the reading says it
  /// cannot tell.
  pub natural_limit: f64,
  /// Least inner step in dB that reads as a band-limited layer.
  ///
  /// A step of at least this size inside a full band is a band-limited layer under it.
  pub inner_shelf_db: f64,
  /// Least step in dB at a top-bitrate cutoff that casts doubt.
  ///
  /// Such a step under a band that reaches the top reads as something the analyzer cannot tell,
  /// which costs an honest file nothing. Honest spectra step by a decibel or two there.
  pub high_step_db: f64,
  /// Height in dB over the cutoff line that makes a band content.
  ///
  /// The band above a step is content only at this height or more. Nearer the line it is the
  /// recording's noise floor, hiss or dither.
  pub clear_db: f64,
  /// Fewest non-silent windows a reading needs.
  pub min_windows: usize,
  /// Fewest seconds of non-silent audio a reading needs.
  pub min_seconds: f64,
  /// Seconds past `min_seconds` with little to go on.
  ///
  /// Within this many seconds over `min_seconds` a reading says it has little to go on. There
  /// each second moves the shares its rules compare by about 2.5 percent, so a few odd
  /// seconds can tip it.
  pub few_seconds_over: f64,
  /// Share of windows at the top of the band for consistent content.
  ///
  /// At least this share of windows must reach the top for the content to be consistent with
  /// its claim.
  pub consistent_share: f64,
  /// Share of windows that stop at one encoder cutoff for a transcode.
  pub transcode_share: f64,
  /// Distance in Hz at which a cutoff still shares the common one.
  pub cluster_hz: f64,
  /// Percentile of a lossy file's cutoffs read as its encoder's ceiling.
  pub ceiling: f64,
  /// Share of windows each of two sources has for mixed sources.
  pub mixed_share: f64,
  /// Band tops in Hz of the sample rates audio is commonly made at.
  ///
  /// Content that stops with a cliff at one, in a file whose band reaches past it, was made at
  /// that rate.
  pub lower_rate_edges: Vec<f64>,
  /// Band edges of rates some old equipment recorded at.
  ///
  /// They are left out of `lower_rate_edges` because they sit on encoder cutoffs (18.9 kHz
  /// beside 192 kbps MP3's). A cliff at one reads as that encoder, with a limitation naming the
  /// rate.
  pub other_rate_edges: Vec<RateEdge>,
  /// Correlation above which two channels hold the same audio.
  ///
  /// It applies together with a side channel at or below `mono_side_db`.
  pub mono_correlation: f64,
  /// Side-to-mid level in dB at or below which channels match.
  ///
  /// At or below this level of the side channel relative to the mid channel, the two channels
  /// hold the same audio.
  pub mono_side_db: f64,
  /// Highest confidence a reading has.
  pub confidence_cap: f64,
  /// Median window level in dBFS under which a file is quiet.
  ///
  /// A quiet file makes a reading less sure.
  pub quiet_dbfs: f64,
  /// Bitrate in kbit/s from which a cutoff may be a filter.
  ///
  /// Profiles at this bitrate and above cut where some recordings are filtered when they are
  /// made, so a sharp shelf there reads as something the analyzer cannot tell, never as a
  /// transcode.
  pub high_bitrate_kbps: u32,
  /// Cutoffs of common lossy encoder settings, lowest first.
  pub profiles: Vec<EncoderProfile>,
  /// Robust z over which the AAC scan finds a frame grid.
  ///
  /// It is compared with [`CodecScan::z`](crate::grid::CodecScan::z). A lower value finds more
  /// grids in decodes that hide them, such as clipped ones, and finds them in more lossless
  /// files whose counts happen to dip at one offset.
  pub aac_grid_z: f64,
  /// Fewest segments that put their lowest count at the file's offset, for an AAC grid.
  ///
  /// It is compared with [`CodecScan::segments_agreeing`]. A decode carries one grid from start
  /// to end, so every segment finds it at the same offset. The segments of a lossless file can
  /// agree on an offset too, so agreement alone is no grid: it only narrows what the z
  /// threshold finds. A file that holds a lossy section only in part falls short, and so does
  /// a Vorbis decode, whose grid the AAC scan finds at an offset that moves between segments.
  ///
  /// [`CodecScan::segments_agreeing`]: crate::grid::CodecScan::segments_agreeing
  pub aac_grid_segments_agreeing: usize,
  /// Robust z over which the MP3 scan finds a frame grid.
  ///
  /// It is compared with [`CodecScan::z`](crate::grid::CodecScan::z), as `aac_grid_z` is.
  pub mp3_grid_z: f64,
  /// Fewest segments that put their lowest count at the file's offset, for an MP3 grid.
  ///
  /// It is compared with [`CodecScan::segments_agreeing`], as `aac_grid_segments_agreeing` is.
  ///
  /// [`CodecScan::segments_agreeing`]: crate::grid::CodecScan::segments_agreeing
  pub mp3_grid_segments_agreeing: usize,
  /// Fewest frames on Opus's lattice at one offset of the four segments, for an Opus grid.
  ///
  /// It is compared with [`OpusScan::frames`](crate::grid::OpusScan::frames). A frame of
  /// lossless audio lands on the lattice by chance at some offset in some files, never at the
  /// same offset often.
  pub opus_grid_frames: usize,
  /// Fewest frames on Opus's lattice in the windows measured, for an Opus grid.
  ///
  /// It is compared with [`OpusScan::uncut_frames`](crate::grid::OpusScan::uncut_frames), the
  /// count on the one grid a decode has when nothing was cut from its start.
  pub opus_uncut_frames: usize,
  /// Fewest frames on Opus's lattice at another level, at one offset of the four segments.
  ///
  /// It is compared with [`OpusShifted::frames`](crate::grid::OpusShifted::frames), where
  /// neither count at the decoder's own level makes an Opus grid. The search for the level
  /// gives chance more frames than `opus_grid_frames` meets.
  pub opus_shifted_frames: usize,
  /// Fewest frames on Opus's lattice at another level in the windows measured.
  ///
  /// It is compared with
  /// [`OpusShifted::uncut_frames`](crate::grid::OpusShifted::uncut_frames), as
  /// `opus_shifted_frames` is with its count.
  pub opus_shifted_uncut_frames: usize,
}

/// Band top of a rate some old equipment recorded at.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct RateEdge {
  /// Half the rate, in Hz.
  pub hz: f64,
  /// Rate and who used it, as a limitation names it.
  pub rate: String,
}

/// Lossy encoder setting known by where it cuts off.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct EncoderProfile {
  /// Cutoff at 44.1 and 48 kHz, in Hz.
  pub hz: f64,
  /// Name a reading gives it.
  pub name: String,
  /// Highest nominal bitrate cut here, in kbit/s.
  pub kbps: u32,
}

impl EncoderProfile {
  fn new(hz: f64, name: &str, kbps: u32) -> Self {
    Self {
      hz,
      name: name.to_string(),
      kbps,
    }
  }
}

impl Default for ReadingThresholds {
  fn default() -> Self {
    Self {
      sharp_shelf_db: 30.0,
      full_band: FULL_BAND,
      natural_limit: 0.75,
      inner_shelf_db: 20.0,
      high_step_db: 10.0,
      clear_db: 10.0,
      min_windows: 20,
      min_seconds: 30.0,
      few_seconds_over: 10.0,
      consistent_share: 0.9,
      transcode_share: 0.85,
      cluster_hz: 500.0,
      ceiling: 0.9,
      mixed_share: 0.15,
      // 22.05, 32, 44.1 and 48 kHz
      lower_rate_edges: vec![11_025.0, 16_000.0, 22_050.0, 24_000.0],
      other_rate_edges: [
        (18_900.0, "37.8 kHz (PlayStation and CD-i audio)"),
        (15_000.0, "30 kHz (some older samplers)"),
        (13_020.0, "26.04 kHz (some older samplers)"),
      ]
      .map(|(hz, rate)| RateEdge {
        hz,
        rate: rate.to_string(),
      })
      .to_vec(),
      mono_correlation: 0.9999,
      mono_side_db: -90.0,
      confidence_cap: 0.98,
      quiet_dbfs: -40.0,
      high_bitrate_kbps: 320,
      // LAME's defaults are the commonest; other MP3 encoders and AAC cut a little lower at the
      // same bitrate
      profiles: vec![
        EncoderProfile::new(11_000.0, "mp3 ~64 kbps", 64),
        EncoderProfile::new(13_500.0, "mp3 ~80 kbps", 80),
        EncoderProfile::new(15_100.0, "mp3 ~96 kbps", 96),
        EncoderProfile::new(16_000.0, "mp3 ~128 kbps", 128),
        EncoderProfile::new(17_000.0, "mp3 ~128 kbps", 128),
        EncoderProfile::new(17_500.0, "mp3 ~160 kbps", 160),
        EncoderProfile::new(18_600.0, "mp3 ~192 kbps", 192),
        EncoderProfile::new(19_400.0, "mp3 ~224 kbps", 224),
        // LAME 3.100 cuts 224 kbps here too, so the name says both
        EncoderProfile::new(19_530.0, "mp3 ~224-256 kbps", 256),
        EncoderProfile::new(20_000.0, "mp3 ~320 kbps or aac", 320),
        EncoderProfile::new(20_500.0, "mp3 ~320 kbps", 320),
      ],
      // the tuning's lossless files reach z 8.24 for AAC, and 11.57 for MP3 apart from one with
      // an MP3 section in one segment, which the agreement keeps out
      aac_grid_z: 9.0,
      aac_grid_segments_agreeing: 0,
      mp3_grid_z: 12.0,
      mp3_grid_segments_agreeing: 3,
      // the tuning's files that were never Opus reach one frame in the stretches and one in
      // the windows
      opus_grid_frames: 3,
      opus_uncut_frames: 3,
      // with a level searched for they reach four frames in the stretches and one in the
      // windows, apart from one master that carries such frames on an uncut decode's grid
      opus_shifted_frames: 6,
      opus_shifted_uncut_frames: 4,
    }
  }
}

impl ReadingThresholds {
  /// fewer seconds of non-silent audio than this give a reading little to go on, which it says
  pub(crate) fn few_seconds(&self) -> f64 {
    self.min_seconds + self.few_seconds_over
  }

  /// the profile whose cutoff is nearest `hz`, when one is within `cluster_hz`
  pub(crate) fn profile_at(&self, hz: f64) -> Option<&EncoderProfile> {
    self
      .profiles
      .iter()
      .filter(|p| (p.hz - hz).abs() <= self.cluster_hz)
      .min_by(|a, b| (a.hz - hz).abs().total_cmp(&(b.hz - hz).abs()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn thresholds_read_from_json_keep_the_defaults_they_do_not_name() {
    let measure: MeasureThresholds = serde_json::from_str(r#"{"cutoffRangeDb": 50.0}"#).unwrap();
    assert_eq!(
      measure,
      MeasureThresholds {
        cutoff_range_db: 50.0,
        ..MeasureThresholds::default()
      }
    );
    let reading: ReadingThresholds = serde_json::from_str(r#"{"sharpShelfDb": 40.0}"#).unwrap();
    assert_eq!(
      reading,
      ReadingThresholds {
        sharp_shelf_db: 40.0,
        ..ReadingThresholds::default()
      }
    );
  }

  fn refused<T: serde::de::DeserializeOwned>(json: &str) {
    assert!(
      serde_json::from_str::<T>(json).is_err(),
      "{json} is refused"
    );
  }

  #[test]
  fn a_key_no_threshold_has_is_refused_at_every_level() {
    refused::<MeasureThresholds>(r#"{"cutoffRange": 50.0}"#);
    refused::<ReadingThresholds>(r#"{"sharpShelf": 40.0}"#);
    refused::<ReadingThresholds>(
      r#"{"profiles": [{"hz": 16000.0, "name": "a", "kbps": 128, "guid": "A"}]}"#,
    );
    refused::<ReadingThresholds>(
      r#"{"otherRateEdges": [{"hz": 15000.0, "rate": "30 kHz", "source": "a"}]}"#,
    );
  }
}
