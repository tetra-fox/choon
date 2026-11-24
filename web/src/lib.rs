//! the analyzer for a web page: one function over the library, exported to JavaScript by
//! wasm-bindgen. `README.md` has the JSON it takes and gives

use choon::grid::{Evidence, Family};
use choon::{
  Claimed, Flag, HeAac, MeasureThresholds, Measured, OpusUse, Reading, ReadingThresholds, Settings,
  Verdict,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::wasm_bindgen;

/// the version of the JSON [`read`] gives. it goes up with any change to the shape, the
/// library's own of a reading included, so a page can drop what it kept of another version
const VERSION: u32 = 2;

/// what a file is measured and read with, in the shape of the program's profile
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Thresholds {
  measure: MeasureThresholds,
  reading: ReadingThresholds,
}

#[derive(Serialize)]
struct Checked<'a> {
  version: u32,
  measured: Shown<'a>,
  words: Words,
  reading: Reading,
}

/// a reading as the program words one, so a page says what the program says. the wording is
/// `src/main.rs`'s, which the library does not export; `tests/node.rs` holds the two together
#[derive(Serialize)]
struct Words {
  /// the verdict, as `likely transcode`
  verdict: &'static str,
  /// what the program puts in parentheses after the verdict, a few words each
  found: Vec<String>,
  /// what the file claims to be, as the program's `Format` line
  format: String,
  flags: Vec<&'static str>,
}

/// the fields of [`Measured`] a page shows, under the names the library writes them with, and
/// three made from it: `dsd_multiple`, the number of windows and whether the grid scan ran
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Shown<'a> {
  claimed: &'a Claimed,
  duration: f64,
  bitrate_kbps: Option<u32>,
  opus: &'a [OpusUse],
  he_aac: Option<HeAac>,
  dsd_rate: Option<u32>,
  dsd_multiple: Option<u32>,
  sampled: bool,
  window_count: usize,
  grid_scanned: bool,
}

/// Measures a file's bytes in full and reads the measures, both under `thresholds`.
///
/// `thresholds` is a profile as JSON, and an empty string for the defaults. The reading comes
/// back as JSON.
///
/// # Errors
///
/// A message in words: `thresholds` is not a profile, or the bytes were not measured
/// ([`choon::Error`]).
pub fn read(bytes: Vec<u8>, thresholds: &str) -> Result<String, String> {
  let thresholds = if thresholds.trim().is_empty() {
    Thresholds::default()
  } else {
    serde_json::from_str(thresholds).map_err(|e| format!("the thresholds were not read: {e}"))?
  };
  let settings = Settings {
    thresholds: thresholds.measure,
    ..Settings::default()
  };
  let measured = choon::measure_bytes(bytes, &settings).map_err(|e| e.to_string())?;
  let reading = measured.read(&thresholds.reading);
  let checked = Checked {
    version: VERSION,
    measured: Shown::of(&measured),
    words: Words::of(&measured, &reading),
    reading,
  };
  serde_json::to_string(&checked).map_err(|e| format!("the reading was not written: {e}"))
}

impl<'a> Shown<'a> {
  fn of(measured: &'a Measured) -> Self {
    Self {
      claimed: &measured.claimed,
      duration: measured.duration,
      bitrate_kbps: measured.bitrate_kbps,
      opus: &measured.opus,
      he_aac: measured.he_aac,
      dsd_rate: measured.dsd_rate,
      dsd_multiple: measured.dsd_multiple(),
      sampled: measured.sampled,
      window_count: measured.windows.len(),
      grid_scanned: measured.grid.is_some(),
    }
  }
}

impl Words {
  fn of(measured: &Measured, reading: &Reading) -> Self {
    let mut found = found(reading, measured.grid.is_some());
    found.extend(kind(measured));
    Self {
      verdict: label(reading.verdict),
      found,
      format: format(measured),
      flags: reading.flags.iter().copied().map(flag_words).collect(),
    }
  }
}

fn label(verdict: Verdict) -> &'static str {
  match verdict {
    Verdict::Consistent => "consistent",
    Verdict::LikelyTranscode => "likely transcode",
    Verdict::MixedSources => "mixed sources",
    Verdict::PossiblyLossy => "possibly lossy",
    Verdict::Inconclusive => "inconclusive",
  }
}

fn flag_words(flag: Flag) -> &'static str {
  match flag {
    Flag::Upsampled => "upsampled",
    Flag::BitPadded => "bit padded",
    Flag::DualMono => "dual mono",
  }
}

/// the encoder family of a grid in a word or two; `None` for the family that names no encoder,
/// and for one the program has no word for
fn family_words(family: Family) -> Option<&'static str> {
  match family {
    Family::Lame => Some("LAME"),
    Family::Helix => Some("Helix"),
    Family::Apple => Some("Apple"),
    Family::Faac => Some("FAAC"),
    Family::Nero => Some("Nero"),
    Family::FfmpegOrFdk => Some("ffmpeg or fdk-aac"),
    Family::Fraunhofer => Some("Fraunhofer"),
    Family::Libopus => Some("libopus"),
    // Untagged, and what the library's enum, which is non-exhaustive, may hold besides
    _ => None,
  }
}

/// a frequency as kHz to one decimal
fn khz(hz: f64) -> String {
  format!("{:.1} kHz", hz / 1000.0)
}

/// the claimed codec, rate, bit depth, channels, bitrate and length, a few words each
fn format(measured: &Measured) -> String {
  let claimed = &measured.claimed;
  let mut format = vec![claimed.codec.clone(), format!("{} Hz", claimed.sample_rate)];
  if let Some(bits) = claimed.bit_depth {
    format.push(format!("{bits} bit"));
  }
  format.push(match claimed.channels {
    1 => "1 channel".to_string(),
    channels => format!("{channels} channels"),
  });
  if let Some(kbps) = measured.bitrate_kbps {
    format.push(format!("{kbps} kbit/s"));
  }
  let seconds = measured.duration.round() as u64;
  format.push(format!("{}:{:02}", seconds / 60, seconds % 60));
  format.join(", ")
}

/// what a file is beside its codec: what an Opus file's packets state, the profile of an HE-AAC
/// file, the rate of a DSD file
fn kind(measured: &Measured) -> Option<String> {
  match (
    OpusUse::describe(&measured.opus),
    measured.he_aac,
    measured.dsd_multiple(),
  ) {
    (Some(coding), ..) => Some(format!("Opus {coding}")),
    (None, Some(he_aac), _) => Some(he_aac.to_string()),
    (None, None, Some(multiple)) => Some(format!("DSD{multiple}")),
    (None, None, None) => None,
  }
}

/// the encoder a reading names: without a grid the setting its cutoff matches, with one the
/// grid's codec, its encoder family in parentheses and the setting. `None` where a grid has
/// neither a family nor a setting to name, and beside a verdict that finds no lossy audio,
/// where a named encoder would read as a finding
fn encoder(reading: &Reading) -> Option<String> {
  let flagged = matches!(
    reading.verdict,
    Verdict::LikelyTranscode | Verdict::MixedSources | Verdict::PossiblyLossy
  );
  let profile = reading
    .encoder_guess
    .as_ref()
    .filter(|_| flagged)
    .map(|guess| &guess.profile);
  let Some(grid) = &reading.grid else {
    return profile.map(ToString::to_string);
  };
  let codec = grid.codec;
  // the shipped profiles are LAME's cutoffs: a setting is named beside LAME or no named
  // encoder, from a profile of the grid's codec
  let setting = profile
    .filter(|_| matches!(grid.family, None | Some(Family::Lame | Family::Untagged)))
    .filter(|profile| profile.codec == codec.to_string())
    .map(|profile| profile.setting.as_str());
  match (grid.family.and_then(family_words), setting) {
    (None, None) => None,
    (None, Some(setting)) => Some(format!("{codec} {setting}")),
    (Some(family), None) => Some(format!("{codec} ({family})")),
    (Some(family), Some(setting)) => Some(format!("{codec} ({family}) {setting}")),
  }
}

/// what a reading found, a few words each: the grid, the cutoff, the encoder, the flags, the
/// confidence. `scanned` is whether the file's grid scan ran
fn found(reading: &Reading, scanned: bool) -> Vec<String> {
  let mut found = Vec::new();
  let cutoff = reading.cutoff_hz.map_or_else(
    || "no cutoff".to_string(),
    |hz| format!("cutoff {}", khz(hz)),
  );
  let guess = encoder(reading);
  if let Some(grid) = &reading.grid {
    let codec = grid.codec;
    found.push(match &grid.evidence {
      Evidence::Count(_)
      | Evidence::Steps { .. }
      | Evidence::AacSteps { .. }
      | Evidence::Lattice(_) => format!("{codec} grid"),
      Evidence::CountAt { rate, .. } => {
        format!("{codec} grid at {} kHz", f64::from(*rate) / 1000.0)
      }
      Evidence::Part { segments, scan } => format!(
        "{codec} grid in {segments} of {} stretches",
        scan.segment_z.len()
      ),
      Evidence::Shifted { .. } => format!("{codec} grid at another level"),
      Evidence::Hybrid { .. } => format!("{codec} hybrid grid"),
      Evidence::Unfiltered { .. } => format!("{codec} grid under its pitch filter"),
      Evidence::Short { short } => format!("{codec} grid, {} ms frames", short.milliseconds()),
    });
    found.push(cutoff);
    found.extend(guess);
    return found;
  }
  if reading.verdict == Verdict::PossiblyLossy {
    if reading.cutoff_verdict == Verdict::MixedSources {
      found.push("mixed sources by cutoff".to_string());
    }
    found.push(cutoff);
    found.extend(guess);
    found.push(
      if scanned {
        "no grid"
      } else {
        "grid not scanned"
      }
      .to_string(),
    );
    return found;
  }
  if reading.cutoff_hz.is_some() {
    found.push(cutoff);
  }
  found.extend(guess);
  found.extend(
    reading
      .flags
      .iter()
      .map(|flag| flag_words(*flag).to_string()),
  );
  if reading.verdict != Verdict::Inconclusive {
    found.push(format!("confidence {:.2}", reading.confidence));
  }
  found
}

/// [`read`] for JavaScript, which gets the message as a thrown `Error`
#[wasm_bindgen]
pub fn check(bytes: Vec<u8>, thresholds: &str) -> Result<String, wasm_bindgen::JsError> {
  read(bytes, thresholds).map_err(|why| wasm_bindgen::JsError::new(&why))
}

/// the version of the JSON [`check`] returns, which a page asks for before it has read a file
#[wasm_bindgen]
pub fn version() -> u32 {
  VERSION
}
