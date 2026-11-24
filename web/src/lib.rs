//! the analyzer for a web page: one function over the library, exported to JavaScript by
//! wasm-bindgen. `README.md` has the JSON it takes and gives

use choon::{
  Claimed, HeAac, MeasureThresholds, Measured, OpusUse, Reading, ReadingThresholds, Settings,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::wasm_bindgen;

/// the version of the JSON [`read`] gives. it goes up with any change to the shape, the
/// library's own of a reading included, so a page can drop what it kept of another version
const VERSION: u32 = 1;

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
  reading: Reading,
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
  let checked = Checked {
    version: VERSION,
    measured: Shown::of(&measured),
    reading: measured.read(&thresholds.reading),
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

/// [`read`] for JavaScript, which gets the message as a thrown `Error`
#[wasm_bindgen]
pub fn check(bytes: Vec<u8>, thresholds: &str) -> Result<String, wasm_bindgen::JsError> {
  read(bytes, thresholds).map_err(|why| wasm_bindgen::JsError::new(&why))
}
