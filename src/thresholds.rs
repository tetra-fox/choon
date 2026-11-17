//! Every threshold the analyzer measures and reads with, in one place, as data a caller can pass
//! so a sweep needs no rebuild; `Default` holds the values the analyzer ships with. They come
//! from what common encoders do and are checked against generated audio in the tests; they are
//! rules of thumb that real files can break, which is why the reading is evidence and not proof.

use serde::{Deserialize, Serialize};

/// A cutoff at or above this fraction of the band's top counts as reaching it, when a window is
/// measured and when it is read.
const FULL_BAND: f64 = 0.9;

/// What each window is measured with. Changing one means measuring the audio again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct MeasureThresholds {
  /// a window quieter than this, in dBFS, is near-silent and has no cutoff
  pub silence_dbfs: f64,
  /// the cutoff is the highest band within this many dB of a window's loudest band
  pub cutoff_range_db: f64,
  /// the cutoff line never sits less than this many dB over the noise of the samples' bit
  /// depth, so a quiet passage's quantization noise does not read as content
  pub floor_margin_db: f64,
  /// how far from a frequency the bands compared across it start, in Hz
  pub shelf_gap_hz: f64,
  /// how wide the bands compared across a frequency are, in Hz
  pub shelf_span_hz: f64,
  /// a window is searched for a step inside its band when its cutoff is at or above this
  /// fraction of the band's top
  pub full_band: f64,
  /// the lowest frequency a step inside a full band is looked for at, in Hz
  pub inner_shelf_from_hz: f64,
  /// the band gaps are counted in starts here, in Hz: where MP3's top scale-factor band sits
  pub holes_from_hz: f64,
  /// the top of that band, in Hz
  pub holes_to_hz: f64,
  /// a bin this many dB below the bins around it is a gap
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

/// What the measures are read with. A reading is quick, so these can be swept over audio
/// measured once.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ReadingThresholds {
  /// a drop across the cutoff of at least this many dB is an encoder's low-pass, not a natural
  /// roll-off
  pub sharp_shelf_db: f64,
  /// a cutoff at or above this fraction of the band's top counts as reaching it. Set below the
  /// measure `full_band`, it counts as full some windows measuring never searched for inner steps
  pub full_band: f64,
  /// content that rolls off gently below this fraction of the band's top is band-limited by
  /// nature, and the reading says it cannot tell
  pub natural_limit: f64,
  /// a step of at least this many dB inside a full band is a band-limited layer under it
  pub inner_shelf_db: f64,
  /// a step of at least this many dB where the highest bitrates cut, under a band that reaches
  /// the top, is reason enough to doubt the band: it reads as something the analyzer cannot
  /// tell, which costs an honest file nothing. Honest spectra step by a decibel or two there
  pub high_step_db: f64,
  /// the band above such a step is content only when it is at least this many dB over the
  /// cutoff line; nearer the line it is the recording's noise floor, hiss or dither
  pub clear_db: f64,
  /// a reading needs at least this many non-silent windows
  pub min_windows: usize,
  /// a reading needs at least this many seconds of non-silent audio
  pub min_seconds: f64,
  /// within this many seconds over `min_seconds` a reading says it has little to go on: there
  /// each second moves the shares its rules compare by about 2.5 percent, so a few odd seconds
  /// can tip it
  pub few_seconds_over: f64,
  /// the share of windows that must reach the top of the band for the content to be consistent
  /// with its claim
  pub consistent_share: f64,
  /// the share of windows that must stop at one encoder cutoff for a transcode
  pub transcode_share: f64,
  /// how far from the common cutoff a window's cutoff may be and still share it, in Hz
  pub cluster_hz: f64,
  /// the share of a lossy file's cutoffs under which its encoder's ceiling is read
  pub ceiling: f64,
  /// the share of windows each of two sources must have for mixed sources
  pub mixed_share: f64,
  /// the tops of the bands of the sample rates audio is commonly made at, in Hz. Content that
  /// stops with a cliff at one, in a file whose band reaches past it, was made at that rate
  pub lower_rate_edges: Vec<f64>,
  /// band edges of rates some old equipment recorded at. They are left out of
  /// `lower_rate_edges` because they sit on encoder cutoffs (18.9 kHz beside 192 kbps MP3's): a
  /// cliff at one reads as that encoder, with a limitation naming the rate
  pub other_rate_edges: Vec<RateEdge>,
  /// channels correlated above this, with the side channel at or below `mono_side_db`, hold the
  /// same audio
  pub mono_correlation: f64,
  /// the side channel's level relative to the mid channel, in dB, at or below which the two
  /// channels hold the same audio
  pub mono_side_db: f64,
  /// no reading is surer than this
  pub confidence_cap: f64,
  /// a file whose median window is quieter than this, in dBFS, is quiet, which makes a reading
  /// less sure
  pub quiet_dbfs: f64,
  /// profiles at this bitrate and above cut where some recordings are filtered when they are
  /// made, so a sharp shelf there reads as something the analyzer cannot tell, never as a
  /// transcode
  pub high_bitrate_kbps: u32,
  /// the cutoffs of common lossy encoder settings, lowest first
  pub profiles: Vec<Profile>,
}

/// The top of the band of a sample rate some old equipment recorded at.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateEdge {
  /// half the rate, in Hz
  pub hz: f64,
  /// the rate and who used it, as a limitation names it
  pub rate: String,
}

/// A lossy encoder setting known by where it cuts off.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
  /// where it cuts off at 44.1 and 48 kHz, in Hz
  pub hz: f64,
  /// how a report names it
  pub name: String,
  /// the highest nominal bitrate cut here, in kbit/s
  pub kbps: u32,
  /// the label of the guide line drawn across a spectrogram here, none for no line
  #[serde(default)]
  pub guide: Option<String>,
}

impl Profile {
  fn new(hz: f64, name: &str, kbps: u32, guide: Option<&str>) -> Self {
    Self {
      hz,
      name: name.to_string(),
      kbps,
      guide: guide.map(str::to_string),
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
        Profile::new(11_000.0, "mp3 ~64 kbps", 64, None),
        Profile::new(13_500.0, "mp3 ~80 kbps", 80, None),
        Profile::new(15_100.0, "mp3 ~96 kbps", 96, None),
        Profile::new(16_000.0, "mp3 ~128 kbps", 128, Some("MP3 128k")),
        Profile::new(17_000.0, "mp3 ~128 kbps", 128, None),
        Profile::new(17_500.0, "mp3 ~160 kbps", 160, None),
        Profile::new(18_600.0, "mp3 ~192 kbps", 192, Some("MP3 192k")),
        Profile::new(19_400.0, "mp3 ~224 kbps", 224, None),
        // LAME 3.100 cuts 224 kbps here too, so the name says both
        Profile::new(19_530.0, "mp3 ~224-256 kbps", 256, Some("MP3 224-256k")),
        Profile::new(
          20_000.0,
          "mp3 ~320 kbps or aac",
          320,
          Some("MP3 320k / AAC"),
        ),
        Profile::new(20_500.0, "mp3 ~320 kbps", 320, None),
      ],
    }
  }
}

impl ReadingThresholds {
  /// Fewer seconds of non-silent audio than this give a reading little to go on, which it says.
  pub(crate) fn few_seconds(&self) -> f64 {
    self.min_seconds + self.few_seconds_over
  }

  /// The profile whose cutoff is nearest `hz`, when one is within `cluster_hz`.
  pub(crate) fn profile_at(&self, hz: f64) -> Option<&Profile> {
    self
      .profiles
      .iter()
      .filter(|p| (p.hz - hz).abs() <= self.cluster_hz)
      .min_by(|a, b| (a.hz - hz).abs().total_cmp(&(b.hz - hz).abs()))
  }

  /// The guide lines drawn across a spectrogram: where common encoder settings cut off, as each
  /// profile with a guide labels it.
  pub fn guides(&self) -> impl Iterator<Item = (f64, &str)> {
    self
      .profiles
      .iter()
      .filter_map(|profile| Some((profile.hz, profile.guide.as_deref()?)))
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
    let profile: Profile =
      serde_json::from_str(r#"{"hz": 16000.0, "name": "mp3 ~128 kbps", "kbps": 128}"#).unwrap();
    assert_eq!(profile.guide, None);
  }

  fn refused<T: serde::de::DeserializeOwned>(json: &str) {
    assert!(serde_json::from_str::<T>(json).is_err(), "{json} was taken");
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
