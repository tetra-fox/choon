//! The `choon` program: reads audio files and says, a line or a JSON object for each,
//! whether each is what its format claims, with an exit status a script can act on.

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use choon::decode::Claimed;
use choon::engine::{self, Measured, Settings};
use choon::grid::Scan;
use choon::{Flag, MeasureThresholds, Reading, ReadingThresholds, Verdict, reading};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// Every file read is consistent with its format, possibly lossy, or the analyzer cannot tell.
const CLEAN: u8 = 0;
/// At least one file reads as lossy audio in a lossless file.
const LOSSY: u8 = 1;
/// A file could not be read or an argument was wrong.
const FAILED: u8 = 2;

#[derive(Parser)]
#[command(
  version,
  about = "Tells whether audio files are what their format claims, such as a FLAC made from an MP3"
)]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
  /// Read each file and print what its audio is
  #[command(after_help = "\
Exit status: 0 when every file is consistent with its format, possibly lossy or inconclusive, 1 \
when at least one reads as a likely transcode or mixed sources, 2 when a file could not be read or \
an argument was wrong. A file the analyzer cannot open as audio is one that could not be read.")]
  Check(Check),
}

#[derive(Args)]
struct Check {
  /// Audio files to read
  #[arg(required_unless_present = "defaults")]
  files: Vec<PathBuf>,

  /// Print each file as one line of JSON, with every measure and reading
  #[arg(long)]
  json: bool,

  /// Read with the thresholds in FILE in place of the defaults
  ///
  /// FILE holds a JSON object like the one --defaults prints. Keys left out keep their default,
  /// and unknown keys are refused. Repeat the option to read each file with several profiles,
  /// each named after its file.
  #[arg(long, value_name = "FILE")]
  profile: Vec<PathBuf>,

  /// Print the default thresholds as JSON and exit
  #[arg(long, exclusive = true)]
  defaults: bool,

  /// Read files up to MINUTES long in full and sample longer ones
  ///
  /// A longer file is read at windows spread over it, as many as --sampled-windows says.
  /// Without this option every file is read in full.
  #[arg(
    long,
    value_name = "MINUTES",
    requires = "sampled_windows",
    value_parser = minutes
  )]
  max_full_minutes: Option<f64>,

  /// Read a file longer than --max-full-minutes at COUNT one-second windows
  #[arg(
    long,
    value_name = "COUNT",
    requires = "max_full_minutes",
    value_parser = clap::value_parser!(u32).range(1..)
  )]
  sampled_windows: Option<u32>,
}

/// What a file is measured and read with: a profile.
#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Thresholds {
  measure: MeasureThresholds,
  reading: ReadingThresholds,
}

/// What is printed for a file with `--json`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Analyzed<'a> {
  file: Cow<'a, str>,
  claimed: Option<&'a Claimed>,
  duration_seconds: f64,
  bitrate_kbps: Option<u32>,
  sampled: bool,
  windows: usize,
  grid: Option<&'a Scan>,
  readings: Vec<Read>,
}

/// One profile's reading of a file, with how long it took.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Read {
  profile: String,
  /// how long decoding and measuring the file took, shared by the profiles that measure alike
  measure_seconds: f64,
  read_seconds: f64,
  #[serde(flatten)]
  reading: Reading,
}

/// How files are read: in full up to a length, sampled past it.
struct Reach {
  max_full_seconds: f64,
  sampled_windows: usize,
}

fn main() -> ExitCode {
  let Command::Check(check) = Cli::parse().command;
  match run(&check) {
    Ok(code) => ExitCode::from(code),
    Err(e) => {
      eprintln!("choon: {e}");
      ExitCode::from(FAILED)
    }
  }
}

/// What ends a check before every file is read.
enum Failed {
  Profile(PathBuf, String),
  Output(io::Error),
}

impl std::fmt::Display for Failed {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Profile(path, e) => write!(f, "{}: {e}", path.display()),
      Self::Output(e) => write!(f, "writing the output: {e}"),
    }
  }
}

/// Reads every file named and returns the exit status.
fn run(check: &Check) -> Result<u8, Failed> {
  let mut out = io::stdout().lock();
  if check.defaults {
    // the defaults are numbers, strings and lists of them, which always serialize
    let defaults =
      serde_json::to_string_pretty(&Thresholds::default()).expect("thresholds serialize");
    print(&mut out, &defaults)?;
    return Ok(CLEAN);
  }
  let profiles = if check.profile.is_empty() {
    vec![("default".to_string(), Thresholds::default())]
  } else {
    check
      .profile
      .iter()
      .map(|path| profile(path))
      .collect::<Result<_, _>>()?
  };
  let reach = match (check.max_full_minutes, check.sampled_windows) {
    (Some(minutes), Some(count)) => Reach {
      max_full_seconds: minutes * 60.0,
      sampled_windows: count as usize,
    },
    // clap lets neither through without the other
    _ => Reach {
      max_full_seconds: f64::INFINITY,
      sampled_windows: 0,
    },
  };
  let mut code = CLEAN;
  for file in &check.files {
    if let Err(e) = openable(file) {
      eprintln!("choon: {}: {e}", file.display());
      code = FAILED;
      continue;
    }
    let analyzed = analyzed(file, &profiles, &reach);
    let audio = analyzed.0.claimed.is_some();
    if !audio {
      code = FAILED;
    } else if code == CLEAN && analyzed.1.iter().any(|read| lossy(read.reading.verdict)) {
      code = LOSSY;
    }
    let line = if check.json {
      let (measured, readings) = analyzed;
      // the readings are numbers, strings and lists of them, which always serialize
      serde_json::to_string(&Analyzed {
        file: file.to_string_lossy(),
        claimed: measured.claimed.as_ref(),
        duration_seconds: measured.duration,
        bitrate_kbps: measured.bitrate_kbps,
        sampled: measured.sampled,
        windows: measured.windows.len(),
        grid: measured.grid.as_ref(),
        readings,
      })
      .expect("a reading serializes")
    } else if audio {
      format!("{}: {}", file.display(), said(&analyzed.1))
    } else {
      format!("{}: {}", file.display(), not_audio(&analyzed.1))
    };
    if !print(&mut out, &line)? {
      // the files read so far decide the status
      return Ok(code);
    }
  }
  Ok(code)
}

/// Writes `line` to `out`; false when the reader has closed the pipe, as `head` does once it has
/// what it wanted.
fn print(out: &mut impl Write, line: &str) -> Result<bool, Failed> {
  match writeln!(out, "{line}") {
    Ok(()) => Ok(true),
    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
    Err(e) => Err(Failed::Output(e)),
  }
}

/// Whether `verdict` says a lossless file holds lossy audio.
fn lossy(verdict: Verdict) -> bool {
  match verdict {
    Verdict::LikelyTranscode | Verdict::MixedSources => true,
    Verdict::Consistent | Verdict::PossiblyLossy | Verdict::Inconclusive => false,
  }
}

/// The minutes `text` names, refusing a negative or endless length.
fn minutes(text: &str) -> Result<f64, String> {
  let minutes: f64 = text.parse().map_err(|e| format!("{e}"))?;
  if !minutes.is_finite() || minutes < 0.0 {
    return Err("not a length in minutes".to_string());
  }
  Ok(minutes)
}

/// Whether the file at `path` can be opened for reading.
fn openable(path: &Path) -> io::Result<()> {
  let file = std::fs::File::open(path)?;
  if file.metadata()?.is_dir() {
    return Err(io::Error::other("it is a directory"));
  }
  Ok(())
}

/// The profile in the file at `path`, named after the file.
fn profile(path: &Path) -> Result<(String, Thresholds), Failed> {
  let failed = |e: String| Failed::Profile(path.to_path_buf(), e);
  let text = std::fs::read_to_string(path).map_err(|e| failed(e.to_string()))?;
  let thresholds = serde_json::from_str(&text).map_err(|e| failed(e.to_string()))?;
  let name = path.file_stem().map_or_else(
    || path.display().to_string(),
    |stem| stem.to_string_lossy().into_owned(),
  );
  Ok((name, thresholds))
}

/// What the file at `path` measured, and its readings under each of `profiles`. Profiles whose
/// measure thresholds are the same share one measuring, so the file is decoded once for them.
fn analyzed(
  path: &Path,
  profiles: &[(String, Thresholds)],
  reach: &Reach,
) -> (Measured, Vec<Read>) {
  let mut measured: Vec<(&MeasureThresholds, Measured, f64)> = Vec::new();
  let mut readings = Vec::new();
  for (name, thresholds) in profiles {
    let at = match measured
      .iter()
      .position(|(alike, ..)| **alike == thresholds.measure)
    {
      Some(at) => at,
      None => {
        let settings = Settings {
          max_full_seconds: reach.max_full_seconds,
          sampled_windows: reach.sampled_windows,
          // a day, which no single file takes: the reading covers the whole file
          deadline: Instant::now() + Duration::from_secs(24 * 60 * 60),
          progress: None,
          canceled: None,
          thresholds: thresholds.measure.clone(),
        };
        let started = Instant::now();
        let Ok(done) = engine::run(path, &settings) else {
          unreachable!("nothing cancels the analysis")
        };
        measured.push((&thresholds.measure, done, started.elapsed().as_secs_f64()));
        measured.len() - 1
      }
    };
    let (_, done, measure_seconds) = &measured[at];
    let started = Instant::now();
    let reading = reading::read(done, &thresholds.reading);
    readings.push(Read {
      profile: name.clone(),
      measure_seconds: *measure_seconds,
      read_seconds: started.elapsed().as_secs_f64(),
      reading,
    });
  }
  // every profile reads the same windows of the file; what differs is what is made of them
  let (_, first, _) = measured.swap_remove(0);
  (first, readings)
}

/// Why a file that did not open as audio was not read: the first limitation of a reading, as
/// every profile's reading of it lists the same ones.
fn not_audio(readings: &[Read]) -> String {
  let why = readings
    .first()
    .and_then(|read| read.reading.limitations.first())
    .map_or_else(String::new, |why| {
      // the reading capitalizes the decoder's clause as a list item; here it follows a colon
      let mut chars = why.chars();
      chars.next().map_or_else(String::new, |first| {
        format!(
          ": {}",
          first.to_lowercase().chain(chars).collect::<String>()
        )
      })
    });
  format!("not read as audio{why}")
}

/// The readings in a few words each, named after their profile when there are several.
fn said(readings: &[Read]) -> String {
  match readings {
    [read] => words(&read.reading),
    _ => readings
      .iter()
      .map(|read| format!("[{}] {}", read.profile, words(&read.reading)))
      .collect::<Vec<_>>()
      .join("; "),
  }
}

/// A reading in a few words. A grid names its codec, offset and z, then where the content
/// stops and which encoder that matches; a doubt says what the cutoff alone read; any other
/// reading gives the verdict, the cutoff, the encoder of a lossy verdict, the findings beside it
/// and how sure it is.
fn words(reading: &Reading) -> String {
  let cut = reading.cutoff_hz.map_or_else(
    || "no cutoff found".to_string(),
    |hz| format!("cut at {:.1} kHz", hz / 1000.0),
  );
  let guess = reading.encoder_guess.as_ref().map(|g| g.profile.as_str());
  if let Some(grid) = &reading.grid {
    let at = guess.map_or_else(String::new, |g| format!(", where {g} cuts"));
    return format!(
      "likely transcode: {} grid at offset {} (z {:.0}), {cut}{at}",
      grid.codec.name(),
      grid.scan.offset,
      grid.scan.z
    );
  }
  if reading.verdict == Verdict::PossiblyLossy {
    let read_as = match reading.cutoff_verdict {
      Verdict::MixedSources => "mixed sources",
      _ => "an encoder's cutoff",
    };
    let like = guess.map_or_else(String::new, |g| format!(", like {g}"));
    return format!(
      "possibly lossy, cannot confirm: no codec grid; the spectrum reads as {read_as} \
       ({cut}{like})"
    );
  }
  let mut parts = vec![
    match reading.verdict {
      Verdict::Consistent => "consistent",
      Verdict::LikelyTranscode => "likely transcode",
      Verdict::MixedSources => "mixed sources",
      Verdict::PossiblyLossy => "possibly lossy",
      Verdict::Inconclusive => "inconclusive",
    }
    .to_string(),
  ];
  if let Some(hz) = reading.cutoff_hz {
    parts.push(format!("cutoff {:.1} kHz", hz / 1000.0));
  }
  // beside a verdict that finds no lossy audio, a named encoder would read as a finding
  if let Some(guess) = reading
    .encoder_guess
    .as_ref()
    .filter(|_| lossy(reading.verdict))
  {
    parts.push(guess.profile.clone());
  }
  parts.extend(reading.flags.iter().map(|flag| {
    match flag {
      Flag::Upsampled => "upsampled",
      Flag::BitPadded => "bit padded",
    }
    .to_string()
  }));
  if reading.verdict != Verdict::Inconclusive {
    parts.push(format!("confidence {:.2}", reading.confidence));
  }
  parts.join(", ")
}
