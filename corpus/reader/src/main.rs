//! what corpus/corpus reads a file with: the library, with the measures kept so that a later
//! commit's rules can read them again without the audio
//!
//! reader measure MINUTES WINDOWS FILE   prints {"measured", "reading"} for the file
//! reader read                           prints the reading of each line of measures on stdin

use std::io::{self, BufRead, Write};
use std::num::NonZeroUsize;
use std::process::ExitCode;

use choon::{Coverage, Measured, ReadingThresholds, Settings};

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let done = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
    ["measure", minutes, windows, path] => measure(minutes, windows, path),
    ["read"] => read(),
    _ => Err("usage: reader measure MINUTES WINDOWS FILE | reader read".into()),
  };
  match done {
    Ok(()) => ExitCode::SUCCESS,
    Err(why) => {
      eprintln!("reader: {why}");
      ExitCode::FAILURE
    }
  }
}

fn measure(minutes: &str, windows: &str, path: &str) -> Result<(), String> {
  let minutes: f64 = minutes.parse().map_err(|e| format!("minutes: {e}"))?;
  let windows: NonZeroUsize = windows.parse().map_err(|e| format!("windows: {e}"))?;
  let settings = Settings {
    coverage: Coverage::Sampled {
      over_seconds: minutes * 60.0,
      windows,
    },
    ..Settings::default()
  };
  let mut measured = choon::measure(path, &settings).map_err(|e| format!("{path}: {e}"))?;
  // the picture of each window is most of the measures' size, and no rule reads it
  for window in &mut measured.windows {
    window.shown.spectrum.clear();
    window.shown.columns.clear();
    window.shown.peaks.clear();
  }
  let reading = measured.read(&ReadingThresholds::default());
  let line = serde_json::json!({ "measured": measured, "reading": reading });
  println!("{line}");
  Ok(())
}

fn read() -> Result<(), String> {
  let thresholds = ReadingThresholds::default();
  let mut out = io::stdout().lock();
  for line in io::stdin().lock().lines() {
    let line = line.map_err(|e| format!("stdin: {e}"))?;
    let measured: Measured = serde_json::from_str(&line).map_err(|e| format!("measures: {e}"))?;
    let reading = serde_json::to_string(&measured.read(&thresholds)).map_err(|e| e.to_string())?;
    writeln!(out, "{reading}").map_err(|e| format!("stdout: {e}"))?;
  }
  Ok(())
}
