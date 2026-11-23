//! the `choon` program: reads the files it is given, several at once, says of each whether it
//! is what its format claims, as a line or a JSON object or, in a terminal, with progress and
//! tables, and exits with a status a script can act on

mod cli {
  pub mod inputs;
  pub mod measures;
  pub mod terminal;
}

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use choon::grid::{Evidence, Family, Scan};
use choon::picture::Shown;
use choon::{
  CancelCheck, Claimed, Coverage, Error, Flag, HeAac, Limitation, MeasureThresholds, Measured,
  OpusUse, Reading, ReadingThresholds, Settings, Verdict,
};
use clap::{Args, ColorChoice, Parser, Subcommand};
use rayon::iter::{ParallelBridge, ParallelIterator};
use serde::{Deserialize, Serialize};

use crate::cli::inputs::{EXTENSIONS, Found, Inputs};
use crate::cli::measures::{self, Group, Groups, Kept};
use crate::cli::terminal::{self, Bars, Counted, Look, Row};

/// every file read is consistent with its format, possibly lossy, or the analyzer cannot tell
const CLEAN: u8 = 0;
/// at least one file reads as lossy audio in a lossless file
const LOSSY: u8 = 1;
/// a file could not be read or an argument was wrong
const FAILED: u8 = 2;
/// an interrupt ended the run before every file was read: 128 and the number of SIGINT, as a
/// shell reports a program the signal killed
const INTERRUPTED: u8 = 130;

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
  130  an interrupt ended the run before every file was read

A file the analyzer cannot open as audio is one that could not be read. An interrupt is
Ctrl-C, or SIGTERM or SIGHUP on Unix. On one the files under way are dropped, the files
already read keep their lines, and stderr says how many were read. A second interrupt ends
the program at once.")]
  Check(Check),
}

#[derive(Args)]
struct Check {
  /// Audio files and directories to read, or - for the paths listed on stdin
  #[arg(
    required_unless_present_any = ["defaults", "from_measures"],
    conflicts_with = "from_measures",
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

  /// Write the measures of each file to FILE, one line of JSON per measuring
  ///
  /// A line holds the file's path, the program's version, the measure thresholds and the
  /// measures without the spectrogram and waveform data, and is written when its file is
  /// read. A file read with profiles that measure differently has a line for each measuring.
  /// A file not read has none. --from-measures reads the lines again.
  #[arg(long, value_name = "FILE", conflicts_with = "from_measures")]
  save_measures: Option<PathBuf>,

  /// Read the measures --save-measures wrote to FILE in place of audio files
  ///
  /// Each file is printed as a check of its audio prints it, under each --profile or the
  /// defaults, with a measureSeconds of 0 in the JSON. No audio is opened or decoded. A
  /// profile reads a file only from measures taken with its own measure thresholds, and a
  /// file without them is not read, so --fast is given again for measures taken with it.
  /// The reading thresholds are free to differ.
  #[arg(
    long,
    value_name = "FILE",
    conflicts_with_all = ["max_full_minutes", "sampled_windows", "timeout", "null", "follow"]
  )]
  from_measures: Option<PathBuf>,

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

  /// Stop measuring a file after SECONDS and read what was measured by then
  ///
  /// Each file has SECONDS from when it is started, for all its measurings together. A file
  /// out of time still prints a reading, which covers the part measured and says so among its
  /// limitations. A file with no window measured by then is not read. Without this option a
  /// file is measured to its end.
  #[arg(long, value_name = "SECONDS", value_parser = seconds)]
  timeout: Option<Duration>,

  /// Skip the scans for WMA and for a decode converted between 44.1 and 48 kHz
  ///
  /// A file then takes about a third less time. A decode from WMA, or one converted between
  /// the two rates, can still read as possibly lossy by its cutoff. Every other reading is
  /// the same. The option applies to every profile.
  #[arg(long)]
  fast: bool,

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

  /// List every file in a terminal, and every flagged file in detail
  ///
  /// A terminal otherwise lists only the flagged files, read as a likely transcode, mixed
  /// sources or possibly lossy, and those not read, and gives a directory whose files all read
  /// alike one line. With this option the list also states each grid's offset. A file checked
  /// alone is always shown in detail, and with its reading's summary under this option. Lines
  /// and JSON always hold every file.
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
#[cfg_attr(test, derive(Clone))]
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

impl Checked {
  /// the worst verdict a profile flags the file with, which a terminal counts and lists it by
  fn flag(&self) -> Option<Verdict> {
    let reads = |verdict| {
      let read = |read: &Read| read.reading.verdict == verdict;
      self.readings.iter().any(read)
    };
    [
      Verdict::LikelyTranscode,
      Verdict::MixedSources,
      Verdict::PossiblyLossy,
    ]
    .into_iter()
    .find(|verdict| reads(*verdict))
  }
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
#[cfg_attr(test, derive(Clone))]
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
  error: &'a str,
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
  /// the measures file could not be opened, made or written
  Measures(PathBuf, io::Error),
  Signals(ctrlc::Error),
}

/// what the threads reading files tell the one that prints
enum Event {
  /// the file numbered `file` is being read, and says here how far each measuring of it is, in
  /// thousandths
  Started {
    file: usize,
    name: String,
    progress: Vec<Arc<AtomicU32>>,
  },
  /// the file numbered `file` is read or could not be, with the lines the measures file gets
  /// for it. `root` is which of the directories named it was found under
  Done {
    file: usize,
    path: PathBuf,
    root: Option<usize>,
    result: Result<Box<Checked>, Unread>,
    kept: Vec<String>,
  },
}

/// why a file has no reading
enum Unread {
  /// the file could not be opened
  Open(io::Error),
  /// it was opened and is not audio the analyzer reads, its analysis failed, or its lines in
  /// the measures file could not be read, with why in words
  NotRead(String),
  /// the measures file holds no measuring of it under the measure thresholds of the profile
  /// named
  Unmeasured(String),
}

impl std::fmt::Display for Unread {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Open(e) => write!(f, "{e}"),
      Self::NotRead(why) => f.write_str(why),
      Self::Unmeasured(profile) => write!(
        f,
        "profile {profile}: its measure thresholds are not those the file was measured with"
      ),
    }
  }
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
      Self::Measures(path, e) => write!(f, "{}: {e}", path.display()),
      Self::Signals(e) => write!(f, "setting the interrupt handler: {e}"),
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
  let profiles = profiles(check)?;
  let coverage = match (check.max_full_minutes, check.sampled_windows) {
    (Some(minutes), Some(windows)) => Coverage::Sampled {
      over_seconds: minutes * 60.0,
      windows,
    },
    // clap lets neither through without the other
    _ => Coverage::Full,
  };
  let mut code = CLEAN;
  // the directories named, which a terminal lists their files under
  let mut roots = Vec::new();
  let source = match &check.from_measures {
    Some(path) => {
      let file = File::open(path).map_err(|e| Failed::Measures(path.clone(), e))?;
      Source::Kept(path, Groups::new(file))
    }
    None => {
      let inputs = Inputs::gather(
        &check.paths,
        &mut io::stdin().lock(),
        check.null,
        check.follow,
      )
      .map_err(Failed::Stdin)?;
      for problem in &inputs.problems {
        eprintln!("{}", complaint(&problem.path, &problem.why));
        if problem.unread {
          code = FAILED;
        }
      }
      if inputs.files.is_empty() {
        eprintln!("choon: no audio files found");
        return Ok(code);
      }
      roots = inputs.roots;
      Source::Files(inputs.files)
    }
  };
  // how many files there are. a measures file says only once it is read to its end
  let total = match &source {
    Source::Files(files) => OnceLock::from(files.len()),
    Source::Kept(..) => OnceLock::new(),
  };
  let mut keep = match &check.save_measures {
    Some(path) => Some(Keep {
      path,
      file: File::create(path).map_err(|e| Failed::Measures(path.clone(), e))?,
    }),
    None => None,
  };
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
  // the first interrupt stops the run as an output that ended early does, and a second ends
  // the program where it is
  let interrupted = Arc::new(AtomicBool::new(false));
  ctrlc::set_handler({
    let (stop, interrupted) = (stop.clone(), interrupted.clone());
    move || {
      if interrupted.swap(true, Ordering::Relaxed) {
        std::process::exit(INTERRUPTED.into());
      }
      stop.store(true, Ordering::Relaxed);
    }
  })
  .map_err(Failed::Signals)?;
  let plan = Plan::new(&profiles, coverage, check.timeout, keep.is_some(), {
    let stop = stop.clone();
    Arc::new(move || stop.load(Ordering::Relaxed))
  });
  let (events, received) = mpsc::sync_channel(jobs);
  thread::scope(|scope| {
    let (pool, plan, stop, total) = (&pool, &plan, &*stop, &total);
    // the sender moves to this thread and is dropped with it, which ends the events
    scope.spawn(move || {
      pool.install(|| match source {
        // the files start in the order given, each thread taking the next when it is free
        Source::Files(files) => files
          .iter()
          .enumerate()
          .par_bridge()
          .for_each(|(file, found)| check_file(file, found, plan, &events)),
        // a run that is stopped reads no more of the measures file
        Source::Kept(measures, groups) => groups
          .until(stop, total)
          .enumerate()
          .par_bridge()
          .for_each(|(file, group)| check_group(measures, file, group, plan, &events)),
      });
    });
    let mut tally = Tally {
      code,
      finished: 0,
      read: 0,
    };
    let ended = if !check.json && io::stdout().is_terminal() {
      let run = Run {
        total,
        roots: &roots,
        profiled: profiles.len() > 1,
        started,
        interrupted: &interrupted,
      };
      shown(received, check, &run, &mut keep, &mut tally)
    } else {
      printed(received, check.json, &mut keep, &mut tally)
    };
    // an output that ended early stops the files under way, and the scope waits for them
    stop.store(true, Ordering::Relaxed);
    ended?;
    let total = total.get().copied();
    if !cut_short(&interrupted, total, tally.finished) {
      return Ok(tally.code);
    }
    match total {
      Some(total) => eprintln!("choon: interrupted: {} of {total} files read", tally.read),
      None => eprintln!("choon: interrupted: {} files read", tally.read),
    }
    Ok(INTERRUPTED)
  })
}

/// the profiles a check reads each file under, with what `--fast` leaves out of every one
fn profiles(check: &Check) -> Result<Vec<(String, Thresholds)>, Failed> {
  let mut profiles = if check.profile.is_empty() {
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
  if check.fast {
    for (_, thresholds) in &mut profiles {
      thresholds.measure.scan_wma = false;
      thresholds.measure.scan_other_rate = false;
    }
  }
  Ok(profiles)
}

/// how every file of a check is measured and read
struct Plan<'a> {
  profiles: &'a [(String, Thresholds)],
  /// the measure thresholds of the profiles, each once: profiles that measure alike share one
  /// measuring of a file
  measures: Vec<&'a MeasureThresholds>,
  coverage: Coverage,
  /// how long the measurings of a file may take together
  timeout: Option<Duration>,
  /// whether a file's lines for the measures file are made
  save: bool,
  /// whether the run is stopped: no file is begun then, and those under way are canceled
  canceled: CancelCheck,
}

impl<'a> Plan<'a> {
  fn new(
    profiles: &'a [(String, Thresholds)],
    coverage: Coverage,
    timeout: Option<Duration>,
    save: bool,
    canceled: CancelCheck,
  ) -> Self {
    let mut measures = Vec::new();
    for (_, thresholds) in profiles {
      if !measures.contains(&&thresholds.measure) {
        measures.push(&thresholds.measure);
      }
    }
    Self {
      profiles,
      measures,
      coverage,
      timeout,
      save,
      canceled,
    }
  }
}

/// measures and reads the file numbered `file` and tells `events` how it went. a file not
/// begun or canceled, because the run is stopped, is not told of
fn check_file(file: usize, found: &Found, plan: &Plan<'_>, events: &SyncSender<Event>) {
  if (plan.canceled)() {
    return;
  }
  let path = &found.path;
  let progress: Vec<Arc<AtomicU32>> = plan.measures.iter().map(|_| Arc::default()).collect();
  let name = path.file_name().unwrap_or(path.as_os_str());
  let event = Event::Started {
    file,
    name: terminal::legible(Path::new(name)),
    progress: progress.clone(),
  };
  // nothing receives once the output is closed, which stops the run for the files not begun
  if events.send(event).is_err() {
    return;
  }
  // the library makes a panic while it measures an error. one in a reading, or in this
  // program's own part, is caught here, so that the run goes on to the other files either
  // way. the panic's own message is on stderr by then. nothing the analysis borrows is left
  // half changed, as it only reads what it is given
  let run = AssertUnwindSafe(|| analyzed(path, plan, &progress));
  let outcome = match catch_unwind(run) {
    // the time ran out after the audio was opened and before its first window
    Ok(Ok((checked, _))) if checked.windows == 0 && out_of_time(&checked) => Err(Unread::NotRead(
      "the analysis ran out of time before a window was measured".to_string(),
    )),
    Ok(Ok(read)) => Ok(read),
    Ok(Err(Error::Canceled)) => return,
    Ok(Err(Error::Open(e))) => Err(Unread::Open(e)),
    Ok(Err(Error::NotAudio(why) | Error::Unsupported(why))) => Err(Unread::NotRead(why)),
    Ok(Err(failed @ (Error::OutOfTime | Error::Panicked))) => {
      Err(Unread::NotRead(failed.to_string()))
    }
    Err(_) => Err(Unread::NotRead(Error::Panicked.to_string())),
  };
  let (result, kept) = match outcome {
    Ok((checked, kept)) => (Ok(Box::new(checked)), kept),
    Err(unread) => (Err(unread), Vec::new()),
  };
  let done = Event::Done {
    file,
    path: path.clone(),
    root: found.root,
    result,
    kept,
  };
  // as above: a closed output has no use for the result
  let _ = events.send(done);
}

/// reads the file numbered `file` again from `group`, its lines of the measures file at
/// `measures`, and tells `events` how it went, unless the run is stopped
fn check_group(
  measures: &Path,
  file: usize,
  group: Group,
  plan: &Plan<'_>,
  events: &SyncSender<Event>,
) {
  if (plan.canceled)() {
    return;
  }
  let (path, result) = match group.path {
    Ok(path) => {
      // as with a file measured, a panic in a reading is this file's failure alone
      let run = AssertUnwindSafe(|| read_again(&group.lines, plan.profiles));
      let read =
        catch_unwind(run).unwrap_or_else(|_| Err(Unread::NotRead(Error::Panicked.to_string())));
      (path, read.map(Box::new))
    }
    // a line that names no file is named by where it is
    Err(why) => (
      PathBuf::from(format!("{}:{}", measures.display(), group.line)),
      Err(Unread::NotRead(why)),
    ),
  };
  let done = Event::Done {
    file,
    path,
    root: None,
    result,
    kept: Vec::new(),
  };
  // a closed output has no use for the result
  let _ = events.send(done);
}

/// what the files finished so far come to
struct Tally {
  /// the exit status they ask for
  code: u8,
  /// how many there are, read or not
  finished: usize,
  /// how many of them have a reading
  read: usize,
}

impl Tally {
  fn count(&mut self, result: &Result<Box<Checked>, Unread>) {
    self.code = self.code.max(status(result));
    self.finished += 1;
    self.read += usize::from(result.is_ok());
  }
}

/// what a terminal's summary says of the run beside its files
struct Run<'a> {
  /// how many files there are to read, once that is known: from the start for files named,
  /// and for a measures file when it is read to its end
  total: &'a OnceLock<usize>,
  /// the directories named
  roots: &'a [PathBuf],
  /// whether the files are read under several profiles
  profiled: bool,
  started: Instant,
  interrupted: &'a AtomicBool,
}

/// whether an interrupt ended the run early: one came before all of `total` files had finished
fn cut_short(interrupted: &AtomicBool, total: Option<usize>, finished: usize) -> bool {
  interrupted.load(Ordering::Relaxed) && total != Some(finished)
}

/// what a check reads
enum Source<'a> {
  /// audio files, to measure
  Files(Vec<Found>),
  /// the measures file at this path, by the groups of its lines
  Kept(&'a Path, Groups),
}

/// the measures file a check writes
struct Keep<'a> {
  path: &'a Path,
  file: File,
}

impl Keep<'_> {
  /// writes the lines of one file, each of which ends in its newline
  fn write(&mut self, lines: &[String]) -> Result<(), Failed> {
    for line in lines {
      self
        .file
        .write_all(line.as_bytes())
        .map_err(|e| Failed::Measures(self.path.to_path_buf(), e))?;
    }
    Ok(())
  }
}

/// whether a reading of `checked` says its measuring ran out of time
fn out_of_time(checked: &Checked) -> bool {
  checked
    .readings
    .iter()
    .any(|read| read.reading.limitations.contains(&Limitation::OutOfTime))
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

/// prints each file as it is done, a line or a JSON object, and counts it in `tally`
fn printed(
  received: Receiver<Event>,
  json: bool,
  keep: &mut Option<Keep<'_>>,
  tally: &mut Tally,
) -> Result<(), Failed> {
  let mut out = io::stdout().lock();
  for event in received {
    let Event::Done {
      path: file,
      result,
      kept,
      ..
    } = event
    else {
      // only a terminal shows the files under way
      continue;
    };
    let file = &file;
    tally.count(&result);
    if let Some(keep) = keep {
      keep.write(&kept)?;
    }
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
          piped(file.display(), &checked, false)
        }
      }
      Err(unread @ (Unread::Open(_) | Unread::Unmeasured(_))) => {
        eprintln!("{}", complaint(file, unread));
        continue;
      }
      Err(Unread::NotRead(why)) => {
        if json {
          serde_json::to_string(&NotAudio {
            file: file.to_string_lossy(),
            file_bytes: raw(file),
            error: &why,
          })
          .expect("strings and bytes serialize")
        } else {
          piped_unread(file.display(), &why, false)
        }
      }
    };
    if !print(&mut out, &line)? {
      // the files printed so far decide the status
      return Ok(());
    }
  }
  Ok(())
}

/// shows the files under way in a terminal over a count of what was found, then one file's
/// reading in detail or the list and the counts of many, and counts each file in `tally`
fn shown(
  received: Receiver<Event>,
  check: &Check,
  run: &Run<'_>,
  keep: &mut Option<Keep<'_>>,
  tally: &mut Tally,
) -> Result<(), Failed> {
  let Run {
    total,
    roots,
    profiled,
    started,
    interrupted,
  } = *run;
  let colour = match check.color {
    ColorChoice::Always => true,
    ColorChoice::Never => false,
    ColorChoice::Auto => {
      anstream::AutoStream::choice(&io::stdout()) != anstream::ColorChoice::Never
    }
  };
  let known = total.get().copied();
  let one = known == Some(1);
  let mut bars = Bars::new(known);
  let mut rows = Vec::with_capacity(known.unwrap_or(0));
  // of the one file of a check, which no list holds, the line that says why it was not read
  let mut alone = None;
  loop {
    match received.recv_timeout(TICK) {
      Ok(Event::Started {
        file,
        name,
        progress,
      }) => bars.started(file, name, progress),
      Ok(Event::Done {
        file,
        path,
        root,
        result,
        kept,
      }) => {
        tally.count(&result);
        if let Some(keep) = keep {
          keep.write(&kept)?;
        }
        let outcome = match result {
          Ok(checked) => {
            // a file under several profiles counts once, as the worst any of them reads it
            bars.ended(file, checked.flag().map(Counted::Flagged));
            Ok(checked)
          }
          Err(unread) => {
            bars.ended(file, Some(Counted::NotRead));
            match &unread {
              Unread::Open(_) | Unread::Unmeasured(_) => {
                bars.complain(&complaint(&path, &unread));
              }
              Unread::NotRead(why) => alone = Some(not_read(&path, why, colour)),
            }
            Err(unread.to_string())
          }
        };
        rows.push(Row {
          path,
          root: root.map(|root| roots[root].as_path()),
          outcome,
        });
      }
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => break,
    }
    bars.tick().map_err(Failed::Output)?;
  }
  bars.finish();
  rows.sort_by(|a, b| a.path.cmp(&b.path));
  let look = Look {
    profiled,
    verbose: check.verbose,
    colour,
    width: console::Term::stdout().size().1,
  };
  let total = total.get().copied();
  let left = cut_short(interrupted, total, tally.finished).then(|| match total {
    Some(total) => format!("{} of {total} files left", total - tally.finished),
    None => "measures left unread".to_string(),
  });
  let text = match &rows[..] {
    [
      Row {
        path,
        outcome: Ok(checked),
        ..
      },
    ] if one => terminal::readout(path, checked, look),
    // stderr has said why it could not be opened, or that an interrupt came before it was read
    [] | [_] if one => alone.unwrap_or_default(),
    _ => terminal::tables(
      &rows,
      look,
      started.elapsed(),
      check.from_measures.is_none(),
      left.as_deref(),
    ),
  };
  if !text.is_empty() {
    print(&mut io::stdout().lock(), text.trim_end())?;
  }
  Ok(())
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

/// the time `text` names in seconds, refusing no time and one a `Duration` cannot hold
fn seconds(text: &str) -> Result<Duration, String> {
  let seconds: f64 = text.parse().map_err(|e| format!("{e}"))?;
  Duration::try_from_secs_f64(seconds)
    .ok()
    .filter(|time| !time.is_zero())
    .ok_or_else(|| "not a time in seconds".to_string())
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

/// one measuring of a file
struct Measuring {
  /// the thresholds it was made with
  thresholds: MeasureThresholds,
  measured: Measured,
  /// how long it took, in seconds; 0 for one read from a measures file
  seconds: f64,
}

/// what the file at `path` measured, with its readings under each of the plan's profiles, and
/// its lines for the measures file when the plan saves them. the file is decoded once for
/// each of the plan's measure thresholds, and every measuring stops when the file's time is
/// up. each says how far it is in the next of `progress`, which holds one for each.
fn analyzed(
  path: &Path,
  plan: &Plan<'_>,
  progress: &[Arc<AtomicU32>],
) -> Result<(Checked, Vec<String>), Error> {
  // a time too far off for the clock to hold never comes
  let deadline = plan
    .timeout
    .and_then(|timeout| Instant::now().checked_add(timeout));
  let mut measurings = Vec::with_capacity(plan.measures.len());
  for (thresholds, progress) in plan.measures.iter().zip(progress) {
    let settings = Settings {
      coverage: plan.coverage,
      deadline,
      thresholds: (*thresholds).clone(),
      progress: Some(progress.clone()),
      canceled: Some(plan.canceled.clone()),
    };
    let started = Instant::now();
    let measured = choon::measure(path, &settings)?;
    measurings.push(Measuring {
      thresholds: settings.thresholds,
      measured,
      seconds: started.elapsed().as_secs_f64(),
    });
  }
  if plan.save {
    // a line holds no pictures: no reading is made from them, and they are nearly all of a
    // window's bytes
    let windows = measurings
      .iter_mut()
      .flat_map(|measuring| &mut measuring.measured.windows);
    for window in windows {
      window.shown = Shown {
        spectrum: Vec::new(),
        columns: Vec::new(),
        peaks: Vec::new(),
      };
    }
  }
  let kept = measurings
    .iter()
    .filter(|_| plan.save)
    .map(|measuring| {
      let kept = Kept {
        file: path.to_string_lossy(),
        file_bytes: raw(path).map(Cow::Borrowed),
        version: Cow::Borrowed(env!("CARGO_PKG_VERSION")),
        measure: Cow::Borrowed(&measuring.thresholds),
        measured: Cow::Borrowed(&measuring.measured),
      };
      // the measures are numbers, strings and lists of them, which always serialize
      let mut line = serde_json::to_string(&kept).expect("measures serialize");
      line.push('\n');
      line
    })
    .collect();
  let checked = checked(plan.profiles, measurings)
    .unwrap_or_else(|profile| unreachable!("the file was measured for profile {profile}"));
  Ok((checked, kept))
}

/// a file's readings under each of `profiles` from the lines a measures file holds for it
fn read_again(
  lines: &[(usize, Vec<u8>)],
  profiles: &[(String, Thresholds)],
) -> Result<Checked, Unread> {
  let measurings = lines
    .iter()
    .map(|(number, line)| {
      let kept: Kept<'_> = serde_json::from_slice(line)
        .map_err(|e| Unread::NotRead(measures::unparsed(*number, &e)))?;
      Ok(Measuring {
        thresholds: kept.measure.into_owned(),
        measured: kept.measured.into_owned(),
        seconds: 0.0,
      })
    })
    .collect::<Result<_, Unread>>()?;
  checked(profiles, measurings).map_err(|profile| Unread::Unmeasured(profile.to_string()))
}

/// what a file measured and its reading under each of `profiles`, each from the measuring made
/// with the profile's measure thresholds. `Err` names the first profile that has none
fn checked(
  profiles: &[(String, Thresholds)],
  mut measurings: Vec<Measuring>,
) -> Result<Checked, &str> {
  let mut readings = Vec::with_capacity(profiles.len());
  let mut of_first = None;
  for (name, thresholds) in profiles {
    let at = measurings
      .iter()
      .position(|alike| alike.thresholds == thresholds.measure)
      .ok_or(name.as_str())?;
    of_first.get_or_insert(at);
    let measuring = &measurings[at];
    let started = Instant::now();
    let reading = measuring.measured.read(&thresholds.reading);
    readings.push(Read {
      profile: name.clone(),
      measure_seconds: measuring.seconds,
      read_seconds: started.elapsed().as_secs_f64(),
      reading,
    });
  }
  // every profile reads the same windows of the file; what differs is what is made of them
  let first = measurings
    .swap_remove(of_first.expect("a check has at least one profile"))
    .measured;
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
fn piped(path: impl std::fmt::Display, checked: &Checked, colour: bool) -> String {
  format!("{path}: {}", said(checked, colour))
}

/// the line of a file not read as audio where lines are piped: its path, then why
fn piped_unread(path: impl std::fmt::Display, why: &str, colour: bool) -> String {
  let status = terminal::painted(None, NOT_READ, colour);
  format!("{path}: {status} ({why})")
}

/// a terminal's line for the one file of a check when it is not read as audio: the status
/// padded to the verdicts' width, why in parentheses, then its path
fn not_read(path: &Path, why: &str, colour: bool) -> String {
  let pad = " ".repeat(VERDICT_WIDTH - NOT_READ.len());
  format!(
    "{}{pad}  ({why})  {}",
    terminal::painted(None, NOT_READ, colour),
    terminal::legible(path)
  )
}

const NOT_READ: &str = "not read";

/// a file's readings, each its verdict and what was found in parentheses, named after their
/// profile when there are several, with the verdicts coloured when `colour`
fn said(checked: &Checked, colour: bool) -> String {
  let kind = kind(checked);
  let words = |reading: &Reading, kind: Option<String>| {
    let mut found = found(reading, checked.grid.is_some());
    found.extend(kind);
    detailed(terminal::worded(reading.verdict, colour), &found)
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

/// `status` with what was found in parentheses after it, when anything was
fn detailed(status: String, found: &[String]) -> String {
  if found.is_empty() {
    status
  } else {
    format!("{status} ({})", found.join(", "))
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
