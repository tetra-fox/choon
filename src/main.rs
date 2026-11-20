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
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use choon::grid::{Evidence, Family, Scan};
use choon::{
  CancelCheck, Claimed, Coverage, Error, Flag, HeAac, Limitation, MeasureThresholds, Measured,
  OpusUse, Reading, ReadingThresholds, Settings, Verdict,
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
#[command(version, about)]
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
  #[arg(short, long, value_name = "N", value_parser = jobs)]
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
     out. Of the others, files with one of these extensions are read, in upper or lower case: \
     {}. A file named here or on stdin is read whatever its name. Stdin lists one path per line.",
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
  /// left out for a path that `file` holds whole
  #[serde(skip_serializing_if = "Option::is_none")]
  file_bytes: Option<&'a [u8]>,
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
#[serde(rename_all = "camelCase")]
struct NotAudio<'a> {
  file: Cow<'a, str>,
  /// left out for a path that `file` holds whole
  #[serde(skip_serializing_if = "Option::is_none")]
  file_bytes: Option<&'a [u8]>,
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
    result: Result<Box<Checked>, Unread>,
  },
}

/// why a file has no reading
enum Unread {
  /// the file could not be opened
  Open(io::Error),
  /// it was opened and is not audio the analyzer reads, or its analysis failed, with why in
  /// words
  NotRead(String),
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
    let profiles: Vec<(String, Thresholds)> = check
      .profile
      .iter()
      .map(|path| profile(path))
      .collect::<Result<_, _>>()?;
    // a reading is told from another's by its profile's name, on a line and in the JSON
    for (at, (name, _)) in profiles.iter().enumerate() {
      if profiles[..at].iter().any(|(earlier, _)| earlier == name) {
        let why = format!("another profile is named {name} too");
        return Err(Failed::Profile(check.profile[at].clone(), why));
      }
    }
    profiles
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
      // the library makes a panic while it measures an error. one in a reading, or in this
      // program's own part, is caught here, so that the run goes on to the other files either
      // way. the panic's own message is on stderr by then. nothing the analysis borrows is left
      // half changed, as it only reads what it is given
      let run = AssertUnwindSafe(|| analyzed(path, profiles, coverage, &progress, &canceled));
      let result = match catch_unwind(run) {
        Ok(Ok(checked)) => Ok(Box::new(checked)),
        Ok(Err(Error::Canceled)) => return,
        Ok(Err(Error::Open(e))) => Err(Unread::Open(e)),
        Ok(Err(Error::NotAudio(why) | Error::Unsupported(why))) => Err(Unread::NotRead(why)),
        Ok(Err(Error::OutOfTime)) => unreachable!("no analysis has a deadline"),
        Ok(Err(Error::Panicked)) | Err(_) => Err(Unread::NotRead(Error::Panicked.to_string())),
      };
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
fn status(result: &Result<Box<Checked>, Unread>) -> u8 {
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
            file_bytes: raw(file),
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
          piped(file, &checked)
        }
      }
      Err(Unread::Open(e)) => {
        eprintln!("{}", complaint(file, e));
        continue;
      }
      Err(Unread::NotRead(why)) => {
        if json {
          serde_json::to_string(&NotAudio {
            file: file.to_string_lossy(),
            file_bytes: raw(file),
            error: why,
          })
          .expect("strings and bytes serialize")
        } else {
          format!("{}: {NOT_READ} ({why})", file.display())
        }
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
              bars
                .say(&listed(path, &checked, colour))
                .map_err(Failed::Output)?;
            }
            Ok(checked)
          }
          Err(Unread::Open(e)) => {
            bars.complain(&complaint(path, &e));
            Err(e.to_string())
          }
          Err(Unread::NotRead(why)) => {
            bars
              .say(&not_read(path, &why, colour))
              .map_err(Failed::Output)?;
            Err(why)
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
    ] => terminal::readout(path, checked, profiled, check.verbose, colour, width),
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

/// the bytes of a path that is not Unicode. the JSON's `file` holds such a path with a
/// replacement character where it is not, so two paths can be the same `file`. serde writes
/// the bytes as a list of numbers, which a script reads without a decoder.
///
/// a piped line holds the path as it is and has no such key: a newline or a colon in a name
/// reads as the line's own, so a script that matches files to readings reads the JSON
#[cfg(unix)]
fn raw(path: &Path) -> Option<&[u8]> {
  use std::os::unix::ffi::OsStrExt;
  path.to_str().is_none().then(|| path.as_os_str().as_bytes())
}

/// off Unix a path's bytes are the system's own encoding, which nothing else reads
#[cfg(not(unix))]
fn raw(_: &Path) -> Option<&[u8]> {
  None
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

/// the encoder a reading names: without a grid the setting its cutoff matches, with one the
/// grid's codec, its encoder family in parentheses and the setting. `None` where a grid has
/// neither a family nor a setting to name, and beside a verdict that finds no lossy audio,
/// where a named encoder would read as a finding
fn encoder(reading: &Reading) -> Option<String> {
  let profile = reading
    .encoder_guess
    .as_ref()
    .filter(|_| flagged(reading.verdict))
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

/// the most threads `--jobs` starts. reading is work for a core, so threads past the cores only
/// wait their turn, and each costs memory and time to start: rayon stops at 65,536 of them,
/// which keep every core busy for minutes before a file is read
const MAX_JOBS: usize = 1024;

/// the number of jobs `text` names, from 1 to [`MAX_JOBS`]
fn jobs(text: &str) -> Result<NonZeroUsize, String> {
  let jobs: NonZeroUsize = text.parse().map_err(|e| format!("{e}"))?;
  if jobs.get() > MAX_JOBS {
    return Err(format!("more than {MAX_JOBS} jobs"));
  }
  Ok(jobs)
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

/// the widest verdict, which the lines of a terminal pad the others to
const VERDICT_WIDTH: usize = "likely transcode".len();

/// a file's line where lines are piped: its path, then its readings
fn piped(path: &Path, checked: &Checked) -> String {
  format!("{}: {}", path.display(), said(checked, false))
}

/// a file's line in a terminal: its verdict in a column of its own, what was found, then its
/// path, which comes last so that it can be copied whole. several readings share the line
/// unpadded
fn listed(path: &Path, checked: &Checked, colour: bool) -> String {
  let path = terminal::legible(path);
  let [read] = &checked.readings[..] else {
    return format!("{}  {path}", said(checked, colour));
  };
  let reading = &read.reading;
  let mut found = found(reading, checked.grid.is_some());
  found.extend(kind(checked));
  status_line(
    &terminal::painted(Some(reading.verdict), label(reading.verdict), colour),
    label(reading.verdict).len(),
    &found.join(", "),
    &path,
  )
}

/// a terminal's line for a file not read as audio
fn not_read(path: &Path, why: &str, colour: bool) -> String {
  status_line(
    &terminal::painted(None, NOT_READ, colour),
    NOT_READ.len(),
    why,
    &terminal::legible(path),
  )
}

const NOT_READ: &str = "not read";

/// `status`, `width` columns wide as printed, padded to the verdicts' column, then `found` in
/// parentheses when there is any, then `path`
fn status_line(status: &str, width: usize, found: &str, path: &str) -> String {
  let pad = " ".repeat(VERDICT_WIDTH.saturating_sub(width));
  if found.is_empty() {
    format!("{status}{pad}  {path}")
  } else {
    format!("{status}{pad}  ({found})  {path}")
  }
}

/// a file's readings, each its verdict and what was found in parentheses, named after their
/// profile when there are several, with the verdicts coloured when `colour`
fn said(checked: &Checked, colour: bool) -> String {
  let kind = kind(checked);
  let words = |reading: &Reading, kind: Option<String>| {
    let mut found = found(reading, checked.grid.is_some());
    found.extend(kind);
    let verdict = terminal::painted(Some(reading.verdict), label(reading.verdict), colour);
    if found.is_empty() {
      verdict
    } else {
      format!("{verdict} ({})", found.join(", "))
    }
  };
  match &checked.readings[..] {
    [read] => words(&read.reading, kind),
    readings => readings
      .iter()
      .map(|read| format!("[{}] {}", read.profile, words(&read.reading, None)))
      .chain(kind)
      .collect::<Vec<_>>()
      .join("; "),
  }
}

/// what a file is beside its codec: what an Opus file's packets state, the profile of an HE-AAC
/// file, the rate of a DSD file
fn kind(checked: &Checked) -> Option<String> {
  match (
    OpusUse::describe(&checked.opus),
    checked.he_aac,
    checked.dsd_multiple,
  ) {
    (Some(coding), ..) => Some(format!("Opus {coding}")),
    (None, Some(he_aac), _) => Some(he_aac.to_string()),
    (None, None, Some(multiple)) => Some(format!("DSD{multiple}")),
    (None, None, None) => None,
  }
}

/// a frequency as kHz to one decimal
fn khz(hz: f64) -> String {
  format!("{:.1} kHz", hz / 1000.0)
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
      Evidence::Shifted { .. } => format!("{codec} grid at another level"),
      Evidence::Hybrid { .. } => format!("{codec} hybrid grid"),
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
      if reading.limitations.contains(&Limitation::GridShort) {
        "MP3 grid short of a transcode"
      } else if scanned {
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

/// the encoder family of a grid in a word or two; `None` for the family that names no encoder,
/// and for one this program has no word for
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

fn flag_words(flag: Flag) -> &'static str {
  match flag {
    Flag::Upsampled => "upsampled",
    Flag::BitPadded => "bit padded",
    Flag::DualMono => "dual mono",
  }
}
