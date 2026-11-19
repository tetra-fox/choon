//! the analyzer on generated audio, each test from measuring to reading: the settings, the
//! files and the readings the tests of each part share

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::engine::{Coverage, Measured, Settings, measure};
use crate::fixtures::{self, Audio, music};
use crate::reading::Reading;
use crate::thresholds::ReadingThresholds;

mod engine;
mod grid;
mod hostile;
mod reading;

/// a long file at `windows` windows, and one of up to `over_seconds` in full
fn sampled(over_seconds: f64, windows: usize) -> Coverage {
  Coverage::Sampled {
    over_seconds,
    windows: NonZeroUsize::new(windows).unwrap(),
  }
}

fn settings() -> Settings {
  Settings {
    coverage: sampled(900.0, 240),
    deadline: Some(Instant::now() + Duration::from_secs(600)),
    ..Settings::default()
  }
}

fn read(measured: &Measured) -> Reading {
  measured.read(&ReadingThresholds::default())
}

fn analyzed(path: &Path) -> Reading {
  explained(path).0
}

/// the reading of the file at `path`, with what it saw for an assertion to print
fn explained(path: &Path) -> (Reading, String) {
  let measured = measure(path, &settings()).unwrap();
  let reading = read(&measured);
  let why = explain(&measured, &reading);
  (reading, why)
}

/// what a reading saw, for a failing test to print: the reading, then each window's start,
/// level, cutoff, shelf and steepest inner step
fn explain(measured: &Measured, reading: &Reading) -> String {
  let mut text = format!(
    "{:?} at {:.2}, guess {:?}, flags {:?}\n  {}\n  limitations {:?}\n  median {:?} Hz, \
     bitrate {:?} kbps\n",
    reading.verdict,
    reading.confidence,
    reading.encoder_guess.as_ref().map(|g| &g.profile),
    reading.flags,
    reading.summary,
    reading.limitations,
    reading.measures.cutoff_median_hz,
    measured.bitrate_kbps,
  );
  for window in &measured.windows {
    let step = window
      .inner_shelf
      .map(|s| format!("{:.0} Hz -{:.0} dB clear {:.0}", s.hz, s.drop, s.clearance));
    text += &format!(
      "  t {:>4} {:>6.1} dBFS cutoff {:?} shelf {:?} step {}\n",
      window.start,
      window.rms_dbfs,
      window.cutoff_hz.map(f64::round),
      window.shelf_db.map(f64::round),
      step.unwrap_or_default()
    );
  }
  text
}

fn flac(dir: &Path, name: &str, audio: &Audio, bits: u32) -> PathBuf {
  let path = dir.join(name);
  fixtures::write_flac(audio, bits, false, &path);
  path
}

/// content cut where an encoder cuts, which the cutoff alone reads as a transcode with the
/// thresholds the analyzer ships with
fn a_17_khz_cut(dir: &Path) -> PathBuf {
  flac(
    dir,
    "cut.flac",
    &music(1, 44_100, fixtures::cut_at(17_000.0)),
    16,
  )
}
