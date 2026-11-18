//! the `choon` program: reads the files it is given, several at once, says of each whether it
//! is what its format claims, as a line or a JSON object or, in a terminal, with progress and
//! tables, and exits with a status a script can act on

mod cli {
  pub mod inputs;
  pub mod terminal;
}

use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use choon::grid::{Evidence, Scan};
use choon::{
  CancelCheck, Claimed, Coverage, Error, Flag, HeAac, MeasureThresholds, Measured, OpusUse,
  Reading, ReadingThresholds, Settings, Verdict,
};
use clap::{Args, ColorChoice, Parser, Subcommand};
use rayon::iter::{ParallelBridge, ParallelIterator};
use serde::{Deserialize, Serialize};

use crate::cli::inputs::{EXTENSIONS, Inputs};
use crate::cli::terminal::{self, Bars, Row};

/// every file read is consistent with its format, possibly lossy, or the analyzer cannot tell
const CLEAN: u8 = 0;
/// at least one file reads as lossy audio in a lossless file
const LOSSY: u8 = 1;
/// a file could not be read or an argument was wrong
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
Exit status:
  0  every file is consistent with its format, possibly lossy or inconclusive
  1  at least one file reads as a likely transcode or mixed sources
  2  a file could not be read, or an argument was wrong

A file the analyzer cannot open as audio is one that could not be read.")]
  Check(Check),
}

#[derive(Args)]
struct Check {
  /// Audio files and directories to read, or - for the paths listed on stdin
  #[arg(
    required_unless_present = "defaults",
    value_name = "PATH",
    long_help = paths_help()
  )]
  paths: Vec<PathBuf>,

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
  #[arg(long, value_name = "COUNT", requires = "max_full_minutes")]
  sampled_windows: Option<NonZeroUsize>,

  /// Read up to N files at once, on N threads
  ///
  /// Without this option N is the number of cores. A file's grid scan runs on the same threads,
  /// so one file alone uses them all.
  #[arg(short, long, value_name = "N")]
  jobs: Option<NonZeroUsize>,

  /// Take the paths on stdin as ended by NUL in place of a newline
  #[arg(short = '0', long)]
  null: bool,

  /// Follow symbolic links in a directory
  ///
  /// A link that leads back to a directory it is in is reported and not followed.
  #[arg(short = 'L', long)]
  follow: bool,

  /// Print every file in a terminal, and every flagged file in detail
  ///
  /// A terminal otherwise lists only the flagged files, read as a likely transcode, mixed
  /// sources or possibly lossy, and those not read. Lines and JSON always hold every file.
  #[arg(short, long)]
  verbose: bool,

  /// Colour the verdicts in a terminal
  ///
  /// Lines and JSON written to a pipe or a file are never coloured.
  #[arg(long, value_name = "WHEN", default_value_t = ColorChoice::Auto)]
  color: ColorChoice,
}

/// the long help of the paths, which names the extensions a walk reads
fn paths_help() -> String {
  format!(
    "Audio files and directories to read, or - for the paths listed on stdin\n\n\
     A directory is read with the directories under it. Hidden files and directories are left \
     out, and of the others those with an audio extension are read, in any case: {}. A file \
     named here or on stdin is read whatever its name. Stdin lists one path per line.",
    EXTENSIONS.join(", ")
  )
}

/// what a file is measured and read with: a profile
#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Thresholds {
  measure: MeasureThresholds,
  reading: ReadingThresholds,
}

/// what a file measured, without its windows, and its reading under each profile
struct Checked {
  claimed: Claimed,
  duration: f64,
  bitrate_kbps: Option<u32>,
  sampled: bool,
  windows: usize,
  grid: Option<Scan>,
  opus: Vec<OpusUse>,
  he_aac: Option<HeAac>,
  dsd_rate: Option<u32>,
  /// the multiple of 44.1 or 48 kHz a DSD file's rate is named by, as DSD64
  dsd_multiple: Option<u32>,
  readings: Vec<Read>,
}

/// what is printed for a file with `--json`
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Analyzed<'a> {
  file: Cow<'a, str>,
  claimed: &'a Claimed,
  duration_seconds: f64,
  bitrate_kbps: Option<u32>,
  sampled: bool,
  windows: usize,
  grid: Option<&'a Scan>,
  /// left out for a file that is not Opus
  #[serde(skip_serializing_if = "<[OpusUse]>::is_empty")]
  opus: &'a [OpusUse],
  /// left out for a file that is not HE-AAC
  #[serde(skip_serializing_if = "Option::is_none")]
  he_aac: Option<HeAac>,
  /// left out for a file that is not DSD
  #[serde(skip_serializing_if = "Option::is_none")]
  dsd_rate: Option<u32>,
  readings: &'a [Read],
}

/// one profile's reading of a file, with how long it took
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

/// what is printed with `--json` for a file not read as audio
#[derive(Serialize)]
struct NotAudio<'a> {
  file: Cow<'a, str>,
  error: String,
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

/// what ends a check before every file is read
enum Failed {
  Profile(PathBuf, String),
  Stdin(io::Error),
  Threads(rayon::ThreadPoolBuildError),
  Output(io::Error),
}

/// what the threads reading files tell the one that prints
enum Event {
  /// the file at this index is being read, and says here how far each measuring of it is, in
  /// thousandths
  Started {
    file: usize,
    progress: Vec<Arc<AtomicU32>>,
  },
  Done {
    file: usize,
    result: Result<Box<Checked>, Error>,
  },
}

/// how often a terminal's bars are brought up to date
const TICK: Duration = Duration::from_millis(100);

impl std::fmt::Display for Failed {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Profile(path, e) => write!(f, "{}: {e}", path.display()),
      Self::Stdin(e) => write!(f, "reading the paths on stdin: {e}"),
      Self::Threads(e) => write!(f, "starting the threads: {e}"),
      Self::Output(e) => write!(f, "writing the output: {e}"),
    }
  }
}

/// reads every file named and returns the exit status
fn run(check: &Check) -> Result<u8, Failed> {
  if check.defaults {
    // the defaults are numbers, strings and lists of them, which always serialize
    let defaults =
      serde_json::to_string_pretty(&Thresholds::default()).expect("thresholds serialize");
    print(&mut io::stdout().lock(), &defaults)?;
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
  let coverage = match (check.max_full_minutes, check.sampled_windows) {
    (Some(minutes), Some(windows)) => Coverage::Sampled {
      over_seconds: minutes * 60.0,
      windows,
    },
    // clap lets neither through without the other
    _ => Coverage::Full,
  };
  let inputs = Inputs::gather(
    &check.paths,
    &mut io::stdin().lock(),
    check.null,
    check.follow,
  )
  .map_err(Failed::Stdin)?;
  let mut code = CLEAN;
  for problem in &inputs.problems {
    eprintln!("{}", complaint(&problem.path, &problem.why));
    if problem.unread {
      code = FAILED;
    }
  }
  let files = inputs.files;
  if files.is_empty() {
    eprintln!("choon: no audio files found");
    return Ok(code);
  }
  let jobs = match check.jobs {
    Some(jobs) => jobs.get(),
    // one job where the system does not say how many cores it has
    None => thread::available_parallelism().map_or(1, NonZeroUsize::get),
  };
  // files and each file's grid scan share these threads, so no more than `jobs` are ever busy
  let pool = rayon::ThreadPoolBuilder::new()
    .num_threads(jobs)
    .build()
    .map_err(Failed::Threads)?;
  let started = Instant::now();
  let stop = Arc::new(AtomicBool::new(false));
  let canceled: CancelCheck = {
    let stop = stop.clone();
    Arc::new(move || stop.load(Ordering::Relaxed))
  };
  // profiles that measure alike share one measuring of a file
  let measurings = profiles
    .iter()
    .enumerate()
    .filter(|(at, (_, thresholds))| {
      profiles[..*at]
        .iter()
        .all(|(_, earlier)| earlier.measure != thresholds.measure)
    })
    .count();
  let (events, received) = mpsc::sync_channel(jobs);
  let read = {
    let (stop, profiles) = (&*stop, &profiles);
    move |(file, path): (usize, &PathBuf)| {
      if stop.load(Ordering::Relaxed) {
        return;
      }
      let progress: Vec<Arc<AtomicU32>> = (0..measurings).map(|_| Arc::default()).collect();
      let event = Event::Started {
        file,
        progress: progress.clone(),
      };
      // nothing receives once the output is closed, which `stop` says to the files not begun
      if events.send(event).is_err() {
        return;
      }
      let result = analyzed(path, profiles, coverage, &progress, &canceled).map(Box::new);
      if matches!(result, Err(Error::Canceled)) {
        return;
      }
      // as above: a closed output has no use for the result
      let _ = events.send(Event::Done { file, result });
    }
  };
  thread::scope(|scope| {
    let (pool, files) = (&pool, &files);
    // the files start in the order given, each thread taking the next when it is free
    scope.spawn(move || pool.install(|| files.iter().enumerate().par_bridge().for_each(read)));
    let code = if !check.json && io::stdout().is_terminal() {
      shown(received, files, check, profiles.len() > 1, started, code)
    } else {
      printed(received, files, check.json, code)
    };
    // an output that ended early stops the files under way, and the scope waits for them
    stop.store(true, Ordering::Relaxed);
    code
  })
}

/// the exit status `result` asks for
fn status(result: &Result<Box<Checked>, Error>) -> u8 {
  match result {
    Ok(checked)
      if checked
        .readings
        .iter()
        .any(|read| lossy(read.reading.verdict)) =>
    {
      LOSSY
    }
    Ok(_) => CLEAN,
    Err(_) => FAILED,
  }
}

/// prints each file as it is done, a line or a JSON object, and returns the exit status, which
/// starts at `code`
fn printed(
  received: Receiver<Event>,
  files: &[PathBuf],
  json: bool,
  mut code: u8,
) -> Result<u8, Failed> {
  let mut out = io::stdout().lock();
  for event in received {
    let Event::Done { file, result } = event else {
      // only a terminal shows the files under way
      continue;
    };
    let file = &files[file];
    code = code.max(status(&result));
    let line = match result {
      Ok(checked) => {
        if json {
          // the readings are numbers, strings and lists of them, which always serialize
          serde_json::to_string(&Analyzed {
            file: file.to_string_lossy(),
            claimed: &checked.claimed,
            duration_seconds: checked.duration,
            bitrate_kbps: checked.bitrate_kbps,
            sampled: checked.sampled,
            windows: checked.windows,
            grid: checked.grid.as_ref(),
            opus: &checked.opus,
            he_aac: checked.he_aac,
            dsd_rate: checked.dsd_rate,
            readings: &checked.readings,
          })
          .expect("a reading serializes")
        } else {
          format!("{}: {}", file.display(), said(&checked, false))
        }
      }
      Err(Error::Open(e)) => {
        eprintln!("{}", complaint(file, e));
        continue;
      }
      Err(Error::NotAudio(why) | Error::Unsupported(why)) => {
        if json {
          serde_json::to_string(&NotAudio {
            file: file.to_string_lossy(),
            error: why,
          })
          .expect("two strings serialize")
        } else {
          format!("{}: not read as audio: {why}", file.display())
        }
      }
      Err(Error::Canceled | Error::OutOfTime) => {
        unreachable!("a canceled analysis is not sent, and none has a deadline")
      }
    };
    if !print(&mut out, &line)? {
      // the files printed so far decide the status
      return Ok(code);
    }
  }
  Ok(code)
}

/// shows the files under way in a terminal with each finding above them, then one file's reading
/// in detail or the tables of many, and returns the exit status, which starts at `code`
fn shown(
  received: Receiver<Event>,
  files: &[PathBuf],
  check: &Check,
  profiled: bool,
  started: Instant,
  mut code: u8,
) -> Result<u8, Failed> {
  let colour = match check.color {
    ColorChoice::Always => true,
    ColorChoice::Never => false,
    ColorChoice::Auto => {
      anstream::AutoStream::choice(&io::stdout()) != anstream::ColorChoice::Never
    }
  };
  let one = files.len() == 1;
  let mut bars = Bars::new(files.len());
  let mut rows = Vec::with_capacity(files.len());
  loop {
    match received.recv_timeout(TICK) {
      Ok(Event::Started { file, progress }) => {
        let name = files[file].file_name().unwrap_or(files[file].as_os_str());
        bars.started(file, terminal::legible(Path::new(name)), progress);
      }
      Ok(Event::Done { file, result }) => {
        bars.ended(file);
        let path = &files[file];
        code = code.max(status(&result));
        let outcome = match result {
          Ok(checked) => {
            let flagged = checked
              .readings
              .iter()
              .any(|read| flagged(read.reading.verdict));
            if !one && (flagged || check.verbose) {
              let line = format!("{}: {}", terminal::legible(path), said(&checked, colour));
              bars.say(&line).map_err(Failed::Output)?;
            }
            Ok(checked)
          }
          Err(Error::Open(e)) => {
            bars.complain(&complaint(path, &e));
            Err(e.to_string())
          }
          Err(Error::NotAudio(why) | Error::Unsupported(why)) => {
            let line = format!(
              "{}: {}: {why}",
              terminal::legible(path),
              terminal::painted(None, "not read as audio", colour)
            );
            bars.say(&line).map_err(Failed::Output)?;
            Err(why)
          }
          Err(Error::Canceled | Error::OutOfTime) => {
            unreachable!("a canceled analysis is not sent, and none has a deadline")
          }
        };
        rows.push(Row { path, outcome });
      }
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => break,
    }
    bars.tick();
  }
  bars.finish();
  rows.sort_by_key(|row| row.path);
  let width = console::Term::stdout().size().1;
  let text = match &rows[..] {
    [
      Row {
        path,
        outcome: Ok(checked),
      },
    ] => terminal::readout(path, checked, profiled, colour, width),
    // its failure is already on the screen
    [_] => return Ok(code),
    _ => terminal::tables(
      &rows,
      profiled,
      check.verbose,
      started.elapsed(),
      colour,
      width,
    ),
  };
  print(&mut io::stdout().lock(), text.trim_end())?;
  Ok(code)
}

/// an error about `path` as stderr shows it, with the path made legible when stderr is a
/// terminal
fn complaint(path: &Path, why: impl std::fmt::Display) -> String {
  if io::stderr().is_terminal() {
    format!("choon: {}: {why}", terminal::legible(path))
  } else {
    format!("choon: {}: {why}", path.display())
  }
}

/// writes `line` to `out`; false when the reader has closed the pipe, as `head` does once it has
/// what it wanted
fn print(out: &mut impl Write, line: &str) -> Result<bool, Failed> {
  match writeln!(out, "{line}") {
    Ok(()) => Ok(true),
    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
    Err(e) => Err(Failed::Output(e)),
  }
}

/// whether `verdict` says a lossless file holds lossy audio
fn lossy(verdict: Verdict) -> bool {
  match verdict {
    Verdict::LikelyTranscode | Verdict::MixedSources => true,
    Verdict::Consistent | Verdict::PossiblyLossy | Verdict::Inconclusive => false,
  }
}

/// whether a terminal lists a file read as `verdict` among its findings
fn flagged(verdict: Verdict) -> bool {
  lossy(verdict) || verdict == Verdict::PossiblyLossy
}

/// `verdict` in words
fn label(verdict: Verdict) -> &'static str {
  match verdict {
    Verdict::Consistent => "consistent",
    Verdict::LikelyTranscode => "likely transcode",
    Verdict::MixedSources => "mixed sources",
    Verdict::PossiblyLossy => "possibly lossy",
    Verdict::Inconclusive => "inconclusive",
  }
}

/// the encoder setting a reading names. beside a verdict that finds no lossy audio, a named
/// encoder would read as a finding
fn named_guess(reading: &Reading) -> Option<&str> {
  reading
    .encoder_guess
    .as_ref()
    .filter(|_| flagged(reading.verdict))
    .map(|guess| guess.profile.as_str())
}

/// the minutes `text` names, refusing an endless length and one of no time, under which a file
/// that states no length would not be read at all
fn minutes(text: &str) -> Result<f64, String> {
  let minutes: f64 = text.parse().map_err(|e| format!("{e}"))?;
  if !minutes.is_finite() || minutes <= 0.0 {
    return Err("not a length in minutes".to_string());
  }
  Ok(minutes)
}

/// the profile in the file at `path`, named after the file
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

/// what the file at `path` measured, and its readings under each of `profiles`. profiles whose
/// measure thresholds are the same share one measuring, so the file is decoded once for them.
/// each measuring says how far it is in the next of `progress`, which holds one for each.
fn analyzed(
  path: &Path,
  profiles: &[(String, Thresholds)],
  coverage: Coverage,
  progress: &[Arc<AtomicU32>],
  canceled: &CancelCheck,
) -> Result<Checked, Error> {
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
          coverage,
          thresholds: thresholds.measure.clone(),
          progress: Some(progress[measured.len()].clone()),
          canceled: Some(canceled.clone()),
          ..Settings::default()
        };
        let started = Instant::now();
        let done = choon::measure(path, &settings)?;
        measured.push((&thresholds.measure, done, started.elapsed().as_secs_f64()));
        measured.len() - 1
      }
    };
    let (_, done, measure_seconds) = &measured[at];
    let started = Instant::now();
    let reading = done.read(&thresholds.reading);
    readings.push(Read {
      profile: name.clone(),
      measure_seconds: *measure_seconds,
      read_seconds: started.elapsed().as_secs_f64(),
      reading,
    });
  }
  // every profile reads the same windows of the file; what differs is what is made of them
  let (_, first, _) = measured.swap_remove(0);
  Ok(Checked {
    dsd_multiple: first.dsd_multiple(),
    claimed: first.claimed,
    duration: first.duration,
    bitrate_kbps: first.bitrate_kbps,
    sampled: first.sampled,
    windows: first.windows.len(),
    grid: first.grid,
    opus: first.opus,
    he_aac: first.he_aac,
    dsd_rate: first.dsd_rate,
    readings,
  })
}

/// a file's readings in a few words each, named after their profile when there are several, with
/// the verdicts coloured when `colour`, then what an Opus file's packets state or the profile of
/// an HE-AAC file
fn said(checked: &Checked, colour: bool) -> String {
  let words = |reading: &Reading| {
    let (verdict, rest) = words(reading);
    terminal::painted(Some(reading.verdict), verdict, colour) + &rest
  };
  let readings = match &checked.readings[..] {
    [read] => words(&read.reading),
    readings => readings
      .iter()
      .map(|read| format!("[{}] {}", read.profile, words(&read.reading)))
      .collect::<Vec<_>>()
      .join("; "),
  };
  match (
    OpusUse::describe(&checked.opus),
    checked.he_aac,
    checked.dsd_multiple,
  ) {
    (Some(coding), ..) => format!("{readings}; Opus {coding}"),
    (None, Some(he_aac), _) => format!("{readings}; {he_aac}"),
    (None, None, Some(multiple)) => format!("{readings}; DSD{multiple}"),
    (None, None, None) => readings,
  }
}

/// a reading in a few words: its verdict, and what follows it
fn words(reading: &Reading) -> (&'static str, String) {
  let verdict = label(reading.verdict);
  let cut = reading.cutoff_hz.map_or_else(
    || "no cutoff found".to_string(),
    |hz| format!("cut at {:.1} kHz", hz / 1000.0),
  );
  let guess = named_guess(reading);
  if let Some(grid) = &reading.grid {
    let at = guess.map_or_else(String::new, |g| format!(", where {g} cuts"));
    let how = match &grid.evidence {
      Evidence::Count(scan) => format!("z {:.0}", scan.z),
      Evidence::CountAt { rate, scan } => {
        format!("z {:.0} at {} kHz", scan.z, f64::from(*rate) / 1000.0)
      }
      Evidence::Steps { steps, .. } => format!("{} bands on quantizer steps", steps.whole),
      Evidence::Lattice(scan) => format!("{} frames", scan.most().1),
      Evidence::Shifted { shifted } => format!("{} frames at another level", shifted.most().1),
      Evidence::Hybrid { hybrid } => format!("{} hybrid frames", hybrid.most().1),
    };
    let rest = format!(
      ": {} grid at offset {} ({how}), {cut}{at}",
      grid.codec,
      grid.evidence.offset()
    );
    return (verdict, rest);
  }
  if reading.verdict == Verdict::PossiblyLossy {
    let read_as = match reading.cutoff_verdict {
      Verdict::MixedSources => "mixed sources",
      _ => "an encoder's cutoff",
    };
    let like = guess.map_or_else(String::new, |g| format!(", like {g}"));
    let rest =
      format!(", cannot confirm: no codec grid; the spectrum reads as {read_as} ({cut}{like})");
    return (verdict, rest);
  }
  let mut rest = String::new();
  if let Some(hz) = reading.cutoff_hz {
    rest += &format!(", cutoff {:.1} kHz", hz / 1000.0);
  }
  if let Some(guess) = guess {
    rest += &format!(", {guess}");
  }
  for flag in &reading.flags {
    rest += ", ";
    rest += flag_words(*flag);
  }
  if reading.verdict != Verdict::Inconclusive {
    rest += &format!(", confidence {:.2}", reading.confidence);
  }
  (verdict, rest)
}

/// `flag` in words
fn flag_words(flag: Flag) -> &'static str {
  match flag {
    Flag::Upsampled => "upsampled",
    Flag::BitPadded => "bit padded",
    Flag::DualMono => "dual mono",
  }
}
