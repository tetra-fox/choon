//! one analysis from file to measures: decodes a window at a time, in full or at windows spread
//! over the file, measures each, and scans a file that claims a lossless format for a frame grid

use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSource;
use tracing::{debug, warn};

use crate::decode::opus::OpusUse;
use crate::decode::{self, Claimed, Decoder, Filled, HeAac, dsd};
use crate::grid::lattice::{self, Uncut};
use crate::grid::segments::Segments;
use crate::grid::{self, Scan};
use crate::limitation::Limitation;
use crate::thresholds::MeasureThresholds;
use crate::totals::Totals;
use crate::window::{Analyzer, FFT_SIZE, Window};

/// Check that says whether an analysis is canceled.
///
/// It is asked between windows and before each read of the file's bytes, so it must answer
/// quickly.
pub type CancelCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// How much of a file an analysis reads.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Coverage {
  /// Every second of the file.
  ///
  /// The time an analysis takes and the measures it holds grow with the file's length, which
  /// only [`Settings::deadline`] bounds.
  #[default]
  Full,
  /// One-second windows spread evenly over a long file, and every second of a shorter one.
  Sampled {
    /// Longest file analyzed in full, in seconds.
    ///
    /// A file that does not state its length, or runs longer than it states, is read no
    /// further than this, so not at all when it is 0 or not a number.
    over_seconds: f64,
    /// Number of windows a longer file is analyzed at.
    ///
    /// A file with no more seconds than this has each of them analyzed once.
    windows: NonZeroUsize,
  },
}

impl Coverage {
  /// the longest file analyzed in full, in seconds
  fn max_full_seconds(self) -> f64 {
    match self {
      Self::Full => f64::INFINITY,
      Self::Sampled { over_seconds, .. } => over_seconds,
    }
  }
}

/// Settings of one analysis.
///
/// `Default` reads every second of the file with the thresholds the analyzer ships with, and
/// has no deadline, progress or cancel check.
#[derive(Clone, Default)]
pub struct Settings {
  /// How much of the file is read.
  pub coverage: Coverage,
  /// Time at which the analysis stops with what it has.
  ///
  /// `None` for an analysis that runs until the file ends.
  ///
  /// On `wasm32-unknown-unknown` [`Instant::now`] panics, so no deadline can be made there.
  pub deadline: Option<Instant>,
  /// Where the analysis reports how far it is, in thousandths of its work.
  ///
  /// The windows come first, then the grid scan of a file that claims a lossless format, each
  /// weighed by about how long it takes.
  pub progress: Option<Arc<AtomicU32>>,
  /// Check that cancels the analysis.
  ///
  /// Once it says so, the analysis stops and reports nothing. `None` for an analysis nothing
  /// cancels.
  pub canceled: Option<CancelCheck>,
  /// Thresholds each window is measured with.
  pub thresholds: MeasureThresholds,
}

impl fmt::Debug for Settings {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Settings")
      .field("coverage", &self.coverage)
      .field("deadline", &self.deadline)
      .field("progress", &self.progress)
      // a closure has nothing to print but whether there is one
      .field("canceled", &self.canceled.is_some())
      .field("thresholds", &self.thresholds)
      .finish()
  }
}

/// whether `canceled` says the analysis is canceled; never without one
fn is_canceled(canceled: Option<&CancelCheck>) -> bool {
  canceled.is_some_and(|canceled| canceled())
}

/// whether `deadline` has passed; never without one
fn is_past(deadline: Option<Instant>) -> bool {
  deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

impl Settings {
  /// says the analysis is `done` of the way through, from 0 to 1
  fn report(&self, done: f64) {
    if let Some(progress) = &self.progress {
      progress.store(thousandths(done), Ordering::Relaxed);
    }
  }
}

/// `done`, from 0 to 1, in the thousandths progress is reported in
fn thousandths(done: f64) -> u32 {
  (done.clamp(0.0, 1.0) * 1000.0) as u32
}

/// windows that take about as long to measure as one job of the grid scan takes, which weighs
/// the two in the progress an analysis reports
const SCAN_JOB_WINDOWS: f64 = 110.0;

/// what measures a file's windows: the analyzer, the buffer each window is filled into, one
/// vector per analyzed channel, and in a file scanned for Opus's frames the count of them on the
/// grid of an uncut decode
struct Measuring<'a> {
  analyzer: Analyzer<'a>,
  buffer: Vec<Vec<f32>>,
  uncut: Option<Uncut>,
}

impl Measuring<'_> {
  /// measures the window in the buffer at `second` into `measured`
  fn window(&mut self, second: u64, rate: usize, measured: &mut Measured) {
    let buffer = &self.buffer;
    let window = self
      .analyzer
      .measure(second as f64, buffer, &mut measured.totals);
    measured.windows.push(window);
    // a short last window has too few frames to be worth resampling
    if let Some(uncut) = &mut self.uncut
      && buffer.first().is_some_and(|channel| channel.len() == rate)
    {
      uncut.take(second * rate as u64, buffer);
    }
  }
}

/// an analysis under way: its settings, and how its progress splits between the windows and the
/// grid scan
struct Run<'a> {
  settings: &'a Settings,
  /// the part of the progress the windows take, from 0 to 1; the grid scan takes what is left
  windows_share: f64,
}

impl Run<'_> {
  /// says `done` of the windows are measured, from 0 to 1
  fn windows(&self, done: f64) {
    self
      .settings
      .report(done.clamp(0.0, 1.0) * self.windows_share);
  }

  /// says every window is measured and `done` of the grid scan's jobs have ended, from 0 to 1
  fn scan(&self, done: f64) {
    let done = self.windows_share + (1.0 - self.windows_share) * done;
    // the jobs end on several threads, and the report of an earlier one can come after a
    // later one's: the furthest stays
    if let Some(progress) = &self.settings.progress {
      progress.fetch_max(thousandths(done), Ordering::Relaxed);
    }
  }
}

/// Measures an analysis took of a file.
///
/// With the `serde` feature they can be kept and read again later under other thresholds.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Measured {
  /// Format the file claims.
  pub claimed: Claimed,
  /// Length of the audio, in seconds.
  pub duration: f64,
  /// Lossy file's bitrate in kbit/s, from its decoded packets.
  pub bitrate_kbps: Option<u32>,
  /// Modes and bandwidths an Opus file's packets state.
  ///
  /// The one with the most time comes first. Each packet states its own, so the analyzer reads
  /// them without measuring. Only the packets decoded count, which in a sampled file are those
  /// around its windows. Empty for other codecs.
  #[cfg_attr(feature = "serde", serde(default))]
  pub opus: Vec<OpusUse>,
  /// What an HE-AAC file's decoder rebuilt beside its core, `None` for any other file.
  #[cfg_attr(feature = "serde", serde(default))]
  pub he_aac: Option<HeAac>,
  /// One-bit sample rate of a DSD file, in Hz, `None` for any other file.
  ///
  /// The file is measured as PCM, at the rate [`Claimed::sample_rate`] states.
  #[cfg_attr(feature = "serde", serde(default))]
  pub dsd_rate: Option<u32>,
  /// Every analyzed window, in time order.
  pub windows: Vec<Window>,
  /// the figures over every analyzed sample
  pub(crate) totals: Totals,
  /// the bits per sample the integer samples use
  pub(crate) bits_used: Option<u32>,
  /// Whether only windows spread over the file were analyzed.
  pub sampled: bool,
  /// Scan of the samples for a codec's frame grid.
  ///
  /// `None` for a file that claims a lossy codec, a DSD file, one too short for the scan, or one
  /// whose scan could not run, which [`Reading::limitations`](crate::Reading::limitations) says.
  pub grid: Option<Scan>,
  /// the channels left out of a file with more than two
  pub(crate) channels_left_out: u32,
  /// what made the analysis stop early or skip audio
  pub(crate) problems: Vec<Limitation>,
}

impl Measured {
  /// Returns the multiple of 44.1 or 48 kHz a DSD file's one-bit rate is, 64 for DSD64.
  pub fn dsd_multiple(&self) -> Option<u32> {
    self.dsd_rate.map(dsd::multiple_of)
  }
}

/// whether a codec's frame grid is looked for in a file: a lossy file carries its own codec's
/// grid, which says nothing, and a codec's decode made into one-bit samples was resampled on
/// the way, which leaves no grid
pub(crate) fn grid_looked_for(claimed: &Claimed, dsd_rate: Option<u32>) -> bool {
  claimed.lossless && dsd_rate.is_none()
}

/// Error of an analysis that measured nothing.
#[derive(Debug)]
pub enum Error {
  /// Analysis that was canceled.
  ///
  /// Nothing is reported of it.
  Canceled,
  /// Deadline that passed before the audio was opened.
  OutOfTime,
  /// File that could not be opened.
  Open(io::Error),
  /// Bytes not recognized as audio, with why in words.
  ///
  /// The container is not recognized or holds no audio track.
  NotAudio(String),
  /// Audio the analyzer does not read, with why in words.
  ///
  /// The track does not state its codec parameters, sample rate or channels, the codec is not
  /// one the analyzer decodes, its decoder cannot start, or the sample rate is above what the
  /// analyzer reads.
  Unsupported(String),
  /// Analysis that panicked, which is a bug in the analyzer or in a decoder.
  ///
  /// The panic's own message goes where the panic hook writes it. A build with
  /// `panic = "abort"` aborts instead. `wasm32-unknown-unknown` is such a build: a panic there
  /// is a trap, and this error is never returned.
  Panicked,
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Canceled => f.write_str("the analysis was canceled"),
      Self::OutOfTime => f.write_str("the analysis ran out of time while opening the file"),
      Self::Open(_) => f.write_str("the file could not be opened"),
      Self::NotAudio(why) | Self::Unsupported(why) => f.write_str(why),
      Self::Panicked => f.write_str("the analyzer failed on this file, which is a bug in choon"),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Open(e) => Some(e),
      Self::Canceled
      | Self::OutOfTime
      | Self::NotAudio(_)
      | Self::Unsupported(_)
      | Self::Panicked => None,
    }
  }
}

/// a source of audio bytes that stops giving them once the analysis is canceled or its time is
/// up, which ends whatever the decoder was doing with an error
struct Watched {
  inner: Box<dyn MediaSource>,
  canceled: Option<CancelCheck>,
  deadline: Option<Instant>,
}

impl Watched {
  fn new(inner: Box<dyn MediaSource>, settings: &Settings) -> Self {
    Self {
      inner,
      canceled: settings.canceled.clone(),
      deadline: settings.deadline,
    }
  }
}

impl Read for Watched {
  fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
    if is_canceled(self.canceled.as_ref()) || is_past(self.deadline) {
      // not `Interrupted`, which readers try again
      return Err(std::io::Error::other("the analysis stopped"));
    }
    self.inner.read(buf)
  }
}

impl Seek for Watched {
  fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
    self.inner.seek(to)
  }
}

impl MediaSource for Watched {
  fn is_seekable(&self) -> bool {
    self.inner.is_seekable()
  }

  fn byte_len(&self) -> Option<u64> {
    self.inner.byte_len()
  }
}

/// Measures the audio of the file at `path`.
///
/// The file is decoded a window at a time, in full or, for a long file, at windows spread
/// evenly over it ([`Settings::coverage`]), and each window is measured. Only the first two
/// channels of a file with more are measured. The analysis blocks the thread it runs on, stops
/// promptly when canceled, and stops with what it has when its time is up. The file's bytes
/// are read through a source that stops giving them at either, so a container that is slow to
/// open or to seek in holds the thread no longer than a window does.
///
/// A FIFO waits for its writer when it is opened. That wait ends too when the analysis is
/// canceled or its time is up.
///
/// A file that claims a lossless format other than DSD is then scanned for a codec's frame grid
/// in four stretches of its samples ([`grid`]). The scan's jobs, one for each codec looked for in each
/// stretch, run on rayon's thread pool: the global one, or the one the analysis is called from
/// inside.
/// Cancellation and the deadline are checked before each job.
///
/// # Errors
///
/// - [`Error::Open`] if the file cannot be opened or is a directory.
/// - [`Error::NotAudio`] if its bytes are not recognized as audio.
/// - [`Error::Unsupported`] if the audio is not one the analyzer reads.
/// - [`Error::Canceled`] if [`Settings::canceled`] says so before the analysis ends.
/// - [`Error::OutOfTime`] if [`Settings::deadline`] passes before the audio is opened.
/// - [`Error::Panicked`] if the analysis panics.
///
/// # Examples
///
/// A file measured once under a deadline, sampled when long, and read under several thresholds:
///
/// ```no_run
/// use std::num::NonZeroUsize;
/// use std::time::{Duration, Instant};
///
/// use choon::{Coverage, ReadingThresholds, Settings};
///
/// # fn main() -> Result<(), choon::Error> {
/// let settings = Settings {
///   coverage: Coverage::Sampled {
///     over_seconds: 900.0,
///     windows: NonZeroUsize::new(240).expect("240 is not zero"),
///   },
///   deadline: Some(Instant::now() + Duration::from_secs(120)),
///   ..Settings::default()
/// };
/// let measured = choon::measure("track.flac", &settings)?;
/// for sharp_shelf_db in [20.0, 25.0, 30.0] {
///   let thresholds = ReadingThresholds {
///     sharp_shelf_db,
///     ..ReadingThresholds::default()
///   };
///   let reading = measured.read(&thresholds);
///   println!("{sharp_shelf_db} dB: {:?}", reading.verdict);
/// }
/// # Ok(())
/// # }
/// ```
pub fn measure(path: impl AsRef<Path>, settings: &Settings) -> Result<Measured, Error> {
  let path = path.as_ref();
  let file = open(path, settings)?;
  // a directory opens on some systems and only fails when read
  if file.metadata().map_err(Error::Open)?.is_dir() {
    return Err(Error::Open(io::ErrorKind::IsADirectory.into()));
  }
  let source = Watched::new(Box::new(file), settings);
  guarded(|| {
    let opened = Decoder::from_source(Box::new(source), &decode::hint_of(path));
    if let Err(problem) = &opened {
      debug!(path = %path.display(), %problem, "audio not opened");
    }
    measure_opened(opened, settings)
  })
}

/// opens the file at `path`. opening a FIFO waits for a writer and nothing cuts that wait
/// short, so under a cancel check or a deadline a FIFO is opened on a thread of its own, which
/// is left waiting when the analysis stops first and ends once a writer comes
#[cfg(unix)]
fn open(path: &Path, settings: &Settings) -> Result<File, Error> {
  use std::os::unix::fs::FileTypeExt;
  use std::sync::mpsc::{RecvTimeoutError, channel};
  use std::time::Duration;

  let stoppable = settings.canceled.is_some() || settings.deadline.is_some();
  let fifo = std::fs::metadata(path).is_ok_and(|file| file.file_type().is_fifo());
  if stoppable && fifo {
    // one message at most is ever sent
    let (opened, waiting) = channel();
    let path = path.to_path_buf();
    std::thread::spawn(move || {
      // nothing receives once the analysis has stopped, and the file is closed here
      let _ = opened.send(File::open(path));
    });
    loop {
      match waiting.recv_timeout(Duration::from_millis(20)) {
        Ok(file) => return file.map_err(Error::Open),
        Err(RecvTimeoutError::Timeout) if is_canceled(settings.canceled.as_ref()) => {
          return Err(Error::Canceled);
        }
        Err(RecvTimeoutError::Timeout) if is_past(settings.deadline) => {
          return Err(Error::OutOfTime);
        }
        Err(RecvTimeoutError::Timeout) => {}
        Err(RecvTimeoutError::Disconnected) => return Err(Error::Panicked),
      }
    }
  }
  File::open(path).map_err(Error::Open)
}

/// the settings bound the wait of a FIFO, which only Unix has
#[cfg(not(unix))]
fn open(path: &Path, _: &Settings) -> Result<File, Error> {
  File::open(path).map_err(Error::Open)
}

/// `analysis`, with a panic in it made an error. the decoders are other crates' and young,
/// and a caller reads files a stranger made on a thread it wants back. the analysis owns what
/// it changes, so a panic leaves nothing of the caller's half changed. the grid scan's jobs
/// run on rayon's pool, which hands their panic to the thread that waits for them
fn guarded(analysis: impl FnOnce() -> Result<Measured, Error>) -> Result<Measured, Error> {
  catch_unwind(AssertUnwindSafe(analysis)).unwrap_or_else(|_| {
    warn!("the analysis panicked, the file not read");
    Err(Error::Panicked)
  })
}

/// Measures audio held in memory, as [`measure`] does a file's.
///
/// # Errors
///
/// - [`Error::NotAudio`] if the bytes are not recognized as audio.
/// - [`Error::Unsupported`] if the audio is not one the analyzer reads.
/// - [`Error::Canceled`] if [`Settings::canceled`] says so before the analysis ends.
/// - [`Error::OutOfTime`] if [`Settings::deadline`] passes before the audio is opened.
/// - [`Error::Panicked`] if the analysis panics.
pub fn measure_bytes(bytes: Vec<u8>, settings: &Settings) -> Result<Measured, Error> {
  let source = Watched::new(Box::new(io::Cursor::new(bytes)), settings);
  guarded(|| {
    measure_opened(
      Decoder::from_source(Box::new(source), &Hint::new()),
      settings,
    )
  })
}

/// measures the audio of `opened`, or says why it did not open: the analysis's own stop when
/// that cut opening short
fn measure_opened(opened: Result<Decoder, Error>, settings: &Settings) -> Result<Measured, Error> {
  match opened {
    Ok(decoder) => measure_decoded(decoder, settings),
    Err(_) if is_canceled(settings.canceled.as_ref()) => Err(Error::Canceled),
    Err(_) if is_past(settings.deadline) => Err(Error::OutOfTime),
    Err(problem) => Err(problem),
  }
}

fn measure_decoded(mut decoder: Decoder, settings: &Settings) -> Result<Measured, Error> {
  let claimed = decoder.claimed.clone();
  let rate = claimed.sample_rate as usize;
  let stated = claimed.frames.map(|frames| frames as f64 / rate as f64);
  // samples without a stated depth are floats, which round no coarser than 24 bits
  let bits = claimed.bit_depth.unwrap_or(24);
  let scanned = grid_looked_for(&claimed, decoder.dsd_rate);
  let opus_rate = lattice::Rate::of(claimed.sample_rate).filter(|_| scanned);
  let mut measuring = Measuring {
    analyzer: Analyzer::new(claimed.sample_rate, claimed.bit_depth, &settings.thresholds),
    buffer: (0..decoder.channels)
      .map(|_| Vec::with_capacity(rate))
      .collect(),
    uncut: opus_rate.map(|rate| Uncut::new(rate, bits)),
  };
  let mut segments = claimed.frames.filter(|_| scanned).and_then(Segments::of);
  let unscanned = scanned && claimed.frames.is_none();
  let mut measured = Measured {
    duration: stated.unwrap_or(0.0),
    bitrate_kbps: None,
    opus: Vec::new(),
    he_aac: decoder.he_aac,
    dsd_rate: decoder.dsd_rate,
    channels_left_out: claimed.channels.saturating_sub(decoder.channels as u32),
    claimed,
    windows: Vec::new(),
    totals: Totals::default(),
    bits_used: None,
    sampled: false,
    grid: None,
    problems: Vec::new(),
  };
  let picks = match settings.coverage {
    Coverage::Sampled {
      over_seconds,
      windows,
    } => stated
      .filter(|seconds| *seconds > over_seconds)
      .map(|seconds| picks(seconds, windows.get())),
    Coverage::Full => None,
  };
  // a read in full stops at the longest length read in full, whatever length the file states
  let longest = settings.coverage.max_full_seconds().ceil();
  let windows = picks.as_ref().map_or_else(
    || stated.map_or(longest, |seconds| seconds.min(longest)),
    |picks| picks.len() as f64,
  );
  let run = Run {
    settings,
    // a file with segments states its length, so its count of windows is finite
    windows_share: if segments.is_some() {
      let jobs = grid::jobs(
        opus_rate.is_some(),
        settings.thresholds.scan_other_rate,
        settings.thresholds.scan_wma,
      );
      windows / (windows + jobs as f64 * SCAN_JOB_WINDOWS)
    } else {
      1.0
    },
  };
  // a file whose packets are so long that seeking to each window decodes more than reading
  // through to the last one is read through from its start, its stretches taken on the way
  let reads_through = picks
    .as_ref()
    .is_some_and(|picks| seeking_decodes_more(picks, decoder.seek_decodes, rate as u64));
  // a sampled read's stretches are read before its windows where the source can be sought in.
  // symphonia-format-caf 0.6.1 refuses every seek once its last packet is read (demuxer.rs,
  // `seek`: "invalid packet index"), which the last window can do
  // TODO: read them after the windows again once a symphonia release seeks from the end
  let stretches_first = picks.is_some() && decoder.seekable && !reads_through;
  let mut moved = false;
  // whether the time ran out while the segments were read, which leaves the scan no time either
  let mut scan_halted = false;
  if stretches_first && let Some(segments) = &mut segments {
    let read = read_segments(&mut decoder, segments, settings, &mut measured)?;
    (moved, scan_halted) = (read.moved, read.halted);
  }
  let reached = match picks {
    Some(picks) if reads_through => {
      measured.sampled = true;
      debug!("seeking to the windows decodes more than reading through to them");
      through(
        &mut decoder,
        &mut measuring,
        &picks,
        &mut segments,
        &run,
        &mut measured,
      )?
    }
    Some(picks) => {
      measured.sampled = true;
      sampled(
        &mut decoder,
        &mut measuring,
        &picks,
        !moved,
        &mut segments,
        &run,
        &mut measured,
      )?
    }
    None => whole(
      &mut decoder,
      &mut measuring,
      Windows::Upto(longest as u64),
      &mut segments,
      &run,
      &mut measured,
    )?,
  };
  // the length is what was read when the read came to an end the file set, or when the file
  // states none; a read whose time ran out says nothing against the stated length
  match reached {
    Reached::End(seconds) => {
      measured.duration = seconds;
      if let Some(stated) = stated
        && stated - seconds > WHOLE_WITHIN
      {
        // a length the bytes could not hold is the header's mistake, whatever kind of length
        // it is. short of one they could hold, audio is missing where the length is a count,
        // and a decoder that stopped says why itself
        if held_by_bytes(&decoder, stated, seconds).is_some() {
          measured.problems.push(Limitation::LengthPastBytes);
        } else if decoder.length_is_counted() && decoder.stopped.is_none() {
          debug!(
            stated,
            read = seconds,
            "the file holds less audio than it states"
          );
          measured.problems.push(Limitation::AudioMissing {
            seconds: stated - seconds,
          });
        }
      }
    }
    Reached::Short(seconds) if stated.is_none() => measured.duration = seconds,
    Reached::Short(_) | Reached::Sampled => {
      let read = measured.windows.last().map_or(0.0, |w| w.start + 1.0);
      if let Some(held) = held_by_bytes(&decoder, measured.duration, read) {
        measured.problems.push(Limitation::LengthPastBytes);
        measured.duration = held;
      }
    }
  }
  // a file that states no length has shown it by the end of a read in full. where the source
  // can be sought in and a timestamp places a sample, the stretches of the scan are read then,
  // as a sampled read's are. Matroska counts in milliseconds, and symphonia 0.6.1's reader of it
  // does not seek once it has read to the end, so a lossless file in it stays unscanned
  let ended = !measured
    .problems
    .iter()
    .any(|problem| matches!(problem, Limitation::LengthNotStated { .. }));
  if unscanned
    && decoder.seekable
    && decoder.ticks_fit_frames()
    && ended
    && let Reached::End(seconds) = reached
  {
    segments = Segments::of((seconds * rate as f64).round() as u64);
  }
  if let Some(segments) = &mut segments
    && !segments.complete()
    && (measured.sampled || unscanned)
    && !stretches_first
  {
    scan_halted = read_segments(&mut decoder, segments, settings, &mut measured)?.halted;
  }
  measured.bits_used = decoder.bits_used();
  measured.opus = decoder.opus_uses();
  if !measured.claimed.lossless {
    measured.bitrate_kbps = decoder.bitrate_kbps();
  }
  if decoder.skipped > 0 {
    measured.problems.push(Limitation::PacketsSkipped {
      count: decoder.skipped,
    });
  }
  if let Some(stopped) = decoder.stopped.take() {
    measured.problems.push(Limitation::Decoder(stopped));
  }
  match decoder.unbounded {
    0 => {}
    1 => measured.problems.push(Limitation::Decoder(
      "1 sample was not a number or far past full scale, and was set to 0 or limited".to_string(),
    )),
    count => measured.problems.push(Limitation::Decoder(format!(
      "{count} samples were not numbers or far past full scale, and were set to 0 or limited"
    ))),
  }
  if decoder.other_tracks > 0 {
    measured.problems.push(Limitation::OtherTracks {
      tracks: decoder.other_tracks + 1,
    });
  }
  if let Some(bytes) = decoder.left_out {
    measured.problems.push(Limitation::Decoder(format!(
      "reading stopped at the last Ogg page, {bytes} bytes before the end of the file"
    )));
  }
  if unscanned && segments.is_none() {
    measured.problems.push(Limitation::GridNotScanned);
  }
  if scan_halted {
    debug!("grid scan out of time while its segments were read");
    measured.problems.push(Limitation::GridScanOutOfTime);
  } else if let Some(segments) = &mut segments {
    let opus = opus_rate
      .zip(measuring.uncut.as_ref())
      .map(|(rate, uncut)| grid::Opus {
        rate,
        bits,
        uncut: uncut.counted(),
        uncut_short: uncut.short_counted(),
        unfiltered: uncut.unfiltered(),
      });
    measured.grid = scan_grid(segments, opus, &run, &mut measured)?;
  }
  settings.report(1.0);
  debug!(
    windows = measured.windows.len(),
    sampled = measured.sampled,
    skipped = decoder.skipped,
    "audio measured"
  );
  Ok(measured)
}

/// how a read of the grid segments ended
struct Stretches {
  /// whether it sought, which moves the decoder
  moved: bool,
  /// whether the analysis's time ran out before the segments were read
  halted: bool,
}

/// reads the grid segments that sampled windows did not fill, seeking once to each and placing
/// its samples by their packets' timestamps. a segment that cannot be read whole stays short
fn read_segments(
  decoder: &mut Decoder,
  segments: &mut Segments,
  settings: &Settings,
  measured: &mut Measured,
) -> Result<Stretches, Error> {
  let mut ended = Stretches {
    moved: false,
    halted: false,
  };
  // without whole frames per tick, a packet's timestamp does not place its first sample
  if !decoder.ticks_fit_frames() {
    return Ok(ended);
  }
  let channels = decoder.claimed.channels as usize;
  let mut samples = Vec::new();
  for index in segments.missing() {
    if segments.untouched(index)
      && let Some(first) = segments.needs(index)
    {
      if halted(settings)? {
        ended.halted = true;
        return Ok(ended);
      }
      ended.moved = true;
      let seek = decoder.seek_frame(first);
      // a seek the analysis's own stop cut short only says its source gave no more bytes,
      // which a reader can take for a frame out of range
      if !matches!(seek, Ok(true)) && halted(settings)? {
        ended.halted = true;
        return Ok(ended);
      }
      match seek {
        Ok(true) => {}
        Ok(false) => {
          debug!(
            frame = first,
            "a grid segment is not among the packets indexed"
          );
          return Ok(ended);
        }
        Err(problem) => {
          debug!(problem, frame = first, "seeking to a grid segment failed");
          measured.problems.push(Limitation::Decoder(problem));
          return Ok(ended);
        }
      }
    }
    // a seek can give a packet that ends at or before the frame needed; nothing is taken of it
    // and the next is read
    while let Some(next) = segments.needs(index) {
      if halted(settings)? {
        ended.halted = true;
        return Ok(ended);
      }
      let Some(frames) = decoder.next_interleaved(&mut samples) else {
        // as with the seek, the decoder's reason for the analysis's own stop says nothing
        if halted(settings)? {
          decoder.stopped = None;
          ended.halted = true;
        }
        return Ok(ended);
      };
      // a packet past the frame needed leaves a gap no later packet fills
      if frames.start > next {
        debug!(frame = next, "a grid segment's packets leave a gap");
        return Ok(ended);
      }
      segments.take_interleaved(frames.start, &samples, channels);
    }
  }
  Ok(ended)
}

/// scans full `segments` for a codec's frame grid; `None`, noted in `measured`, when they are
/// not full or the analysis runs out of time first
fn scan_grid(
  segments: &mut Segments,
  opus: Option<grid::Opus>,
  run: &Run<'_>,
  measured: &mut Measured,
) -> Result<Option<Scan>, Error> {
  let settings = run.settings;
  if !segments.complete() {
    debug!("the grid segments were not all read");
    measured.problems.push(Limitation::GridScanIncomplete);
    return Ok(None);
  }
  let replaced = segments.settle();
  if replaced > 0 {
    debug!(
      replaced,
      "clipped or quiet grid segments gave way to louder stretches"
    );
  }
  let clipped = segments.clipped();
  if clipped > 0 {
    measured
      .problems
      .push(Limitation::ClippedStretches { stretches: clipped });
  }
  let ended = AtomicUsize::new(0);
  let other_rate = settings.thresholds.scan_other_rate;
  let wma = settings.thresholds.scan_wma;
  let jobs = grid::jobs(opus.is_some(), other_rate, wma);
  let scan = grid::scan(
    segments,
    measured.claimed.sample_rate,
    opus,
    other_rate,
    wma,
    || !is_canceled(settings.canceled.as_ref()) && !is_past(settings.deadline),
    || {
      let ended = ended.fetch_add(1, Ordering::Relaxed) + 1;
      run.scan(ended as f64 / jobs as f64);
    },
  );
  if scan.is_none() && is_canceled(settings.canceled.as_ref()) {
    return Err(Error::Canceled);
  }
  if scan.is_none() {
    debug!("grid scan out of time");
    measured.problems.push(Limitation::GridScanOutOfTime);
  }
  Ok(scan)
}

/// the whole seconds `count` windows spread evenly over `seconds` of audio start at: each
/// second once when there are no more seconds than windows
fn picks(seconds: f64, count: usize) -> BTreeSet<u64> {
  let last = (seconds - 1.0).max(0.0);
  let starts = last as u64 + 1;
  if count as u64 >= starts {
    return (0..starts).collect();
  }
  (0..count)
    .map(|i| {
      let at = last * i as f64 / (count.max(2) - 1) as f64;
      at.floor() as u64
    })
    .collect()
}

/// whether the analysis is to stop: an error when it was canceled, true when its time is up,
/// which is noted once in `measured`
fn stop(settings: &Settings, measured: &mut Measured) -> Result<bool, Error> {
  let halted = halted(settings)?;
  if halted {
    debug!(windows = measured.windows.len(), "analysis out of time");
    measured.problems.push(Limitation::OutOfTime);
  }
  Ok(halted)
}

/// whether the analysis is out of time; an error when it was canceled
fn halted(settings: &Settings) -> Result<bool, Error> {
  if is_canceled(settings.canceled.as_ref()) {
    return Err(Error::Canceled);
  }
  Ok(is_past(settings.deadline))
}

/// whether decoding that just ended, or a seek that just failed, was the analysis's own stop:
/// [`stop`], dropping the decoder's reason, which is only that its source stopped giving bytes
fn stopped_by_us(
  decoder: &mut Decoder,
  settings: &Settings,
  measured: &mut Measured,
) -> Result<bool, Error> {
  let ours = stop(settings, measured)?;
  if ours {
    decoder.stopped = None;
  }
  Ok(ours)
}

/// a read in full that ends within this many seconds of the stated length holds all of it. a
/// DSD file's filter ends it a few samples early, and a lost packet of MP3, AAC or Opus as
/// they are mostly made takes 0.02 s or more
const WHOLE_WITHIN: f64 = 0.01;

/// how far a read went
enum Reached {
  /// to an end the file set: its last sample, or the longest a read in full goes, this many
  /// seconds in
  End(f64),
  /// this many seconds in, when its time ran out
  Short(f64),
  /// windows spread over the stated length, which says nothing against it
  Sampled,
}

/// the length the file's bytes can hold, when audio up to `seconds` needs more bytes than the
/// file has; never short of `read`, the furthest the analysis read
fn held_by_bytes(decoder: &Decoder, seconds: f64, read: f64) -> Option<f64> {
  let most = decoder.most_seconds().filter(|most| seconds > *most)?;
  debug!(
    seconds,
    most, "the stated length is past what the file's bytes hold"
  );
  Some(most.max(read))
}

/// whether a window filled as `filled` is worth measuring: a whole second, or a last part of at
/// least half a second and one spectrum frame
fn usable(filled: &Filled, rate: usize) -> bool {
  match filled {
    Filled::Full => true,
    Filled::End(length) => *length >= FFT_SIZE.min(rate) && length * 2 >= rate,
  }
}

/// which windows [`whole`] measures
enum Windows<'a> {
  /// every window, up to this many
  Upto(u64),
  /// the windows starting at these seconds
  Only(&'a BTreeSet<u64>),
}

/// reads windows one after another from the start of the file, where the decoder must be, and
/// measures those `read` asks for
fn whole(
  decoder: &mut Decoder,
  measuring: &mut Measuring<'_>,
  read: Windows<'_>,
  segments: &mut Option<Segments>,
  run: &Run<'_>,
  measured: &mut Measured,
) -> Result<Reached, Error> {
  let settings = run.settings;
  let rate = decoder.claimed.sample_rate as usize;
  let stated = decoder
    .claimed
    .frames
    .map(|frames| frames as f64 / rate as f64);
  let expected = match read {
    Windows::Upto(longest) => stated.map_or(longest as f64, |s| s.min(longest as f64)),
    Windows::Only(picks) => picks.last().map_or(1.0, |last| (last + 1) as f64),
  };
  let mut second = 0u64;
  loop {
    run.windows(second as f64 / expected.max(1.0));
    if stop(settings, measured)? {
      return Ok(Reached::Short(second as f64));
    }
    let filled = decoder.fill(&mut measuring.buffer, rate);
    if filled != Filled::Full && stopped_by_us(decoder, settings, measured)? {
      return Ok(Reached::Short(second as f64));
    }
    if let Windows::Upto(longest) = read
      && second >= longest
    {
      if filled == Filled::End(0) {
        return Ok(Reached::End(second as f64));
      }
      debug!(
        seconds = second,
        "reading in full stopped at the longest length read in full"
      );
      measured.problems.push(match decoder.claimed.frames {
        None => Limitation::LengthNotStated { seconds: second },
        Some(_) => Limitation::LongerThanStated { seconds: second },
      });
      return Ok(Reached::End(second as f64));
    }
    let length = match filled {
      Filled::Full => rate,
      Filled::End(length) => length,
    };
    if let Some(segments) = segments {
      segments.take(second * rate as u64, &measuring.buffer);
    }
    let wanted = match read {
      Windows::Upto(_) => true,
      Windows::Only(picks) => picks.contains(&second),
    };
    if usable(&filled, rate) && wanted {
      measuring.window(second, rate, measured);
    }
    if filled != Filled::Full {
      return Ok(Reached::End(second as f64 + length as f64 / rate as f64));
    }
    second += 1;
  }
}

/// measures one window at each of `picks`, seeking to each, or reading from the start when the
/// file cannot be sought in and the decoder is still `at_start`. a file that gives out early
/// ends where the furthest window read ends
fn sampled(
  decoder: &mut Decoder,
  measuring: &mut Measuring<'_>,
  picks: &BTreeSet<u64>,
  at_start: bool,
  segments: &mut Option<Segments>,
  run: &Run<'_>,
  measured: &mut Measured,
) -> Result<Reached, Error> {
  let settings = run.settings;
  let rate = decoder.claimed.sample_rate as usize;
  // where the furthest window read ends
  let mut furthest = 0.0;
  for (index, &second) in picks.iter().enumerate() {
    run.windows(index as f64 / picks.len() as f64);
    if stop(settings, measured)? {
      return Ok(Reached::Sampled);
    }
    // the picks are spread over the stated length, which tables pointing many packets at the
    // same bytes can make longer than the file holds. the end of the read notes it
    if let Some(held) = held_by_bytes(decoder, (second + 1) as f64, furthest) {
      return Ok(Reached::End(held));
    }
    if let Err(problem) = decoder.seek(second as f64) {
      if stopped_by_us(decoder, settings, measured)? {
        return Ok(Reached::Sampled);
      }
      if index > 0 {
        debug!(problem, second, "seeking failed part way, sampling stopped");
        measured.problems.push(Limitation::Decoder(problem));
        return Ok(Reached::End(furthest));
      }
      // a read through from where the stretches left the decoder would place its windows wrong
      if !at_start {
        debug!(
          problem,
          "seeking to the start failed after the stretches were read"
        );
        measured.problems.push(Limitation::Decoder(problem));
        return Ok(Reached::Sampled);
      }
      debug!(problem, "the file cannot be sought in, reading it through");
      return through(decoder, measuring, picks, segments, run, measured);
    }
    let filled = decoder.fill(&mut measuring.buffer, rate);
    let length = match filled {
      Filled::Full => rate,
      Filled::End(length) => length,
    };
    if filled != Filled::Full {
      if stopped_by_us(decoder, settings, measured)? {
        return Ok(Reached::Sampled);
      }
      if length > 0 {
        furthest = second as f64 + length as f64 / rate as f64;
      }
      if usable(&filled, rate) {
        measuring.window(second, rate, measured);
      }
      debug!(second, "the file ended before its last pick");
      return Ok(Reached::End(furthest));
    }
    furthest = (second + 1) as f64;
    measuring.window(second, rate, measured);
  }
  Ok(Reached::Sampled)
}

/// measures the windows at `picks` on a read from the start of the file, where the decoder must
/// be, to the last of them
fn through(
  decoder: &mut Decoder,
  measuring: &mut Measuring<'_>,
  picks: &BTreeSet<u64>,
  segments: &mut Option<Segments>,
  run: &Run<'_>,
  measured: &mut Measured,
) -> Result<Reached, Error> {
  let read = Windows::Only(picks);
  match whole(decoder, measuring, read, segments, run, measured)? {
    Reached::Short(_) => Ok(Reached::Sampled),
    reached => Ok(reached),
  }
}

/// whether seeking to each of `picks` decodes more audio than one read through the file does.
/// a seek lands where a packet starts, up to `unit` sample frames long at `rate`, and what is
/// decoded from there to the window is dropped, once for every window in the packet
fn seeking_decodes_more(picks: &BTreeSet<u64>, unit: u64, rate: u64) -> bool {
  if unit == 0 {
    return false;
  }
  // each window's own second, and the frames from its packet's start to it
  let seeking: u128 = picks
    .iter()
    .map(|second| u128::from(rate) + u128::from(second.saturating_mul(rate) % unit))
    .sum();
  let through = picks
    .last()
    .map_or(0, |last| u128::from(last + 1) * u128::from(rate));
  seeking > through
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_panic_in_an_analysis_is_an_error() {
    let failed = guarded(|| panic!("a decoder failed"));
    assert!(matches!(failed, Err(Error::Panicked)), "{failed:?}");
  }

  #[test]
  fn a_file_of_long_packets_is_read_through_when_seeking_would_decode_more() {
    // the frames of Monkey's Audio at its highest level, 26.7 s at 44.1 kHz
    let (frame, rate) = (1_179_648, 44_100);
    assert!(seeking_decodes_more(&picks(34.0, 20), frame, rate));
    assert!(!seeking_decodes_more(
      &picks(6.0 * 3600.0, 240),
      frame,
      rate
    ));
    // its first levels' frames, 1.7 s
    assert!(!seeking_decodes_more(&picks(600.0, 20), 73_728, rate));
    assert!(!seeking_decodes_more(&picks(34.0, 20), 0, rate));
  }

  #[test]
  fn more_windows_than_a_file_has_seconds_pick_each_second_once() {
    assert_eq!(picks(100.0, 3), BTreeSet::from([0, 49, 99]));
    assert_eq!(picks(100.0, 1), BTreeSet::from([0]));
    assert_eq!(picks(34.5, 34), (0..34).collect());
    assert_eq!(picks(34.5, usize::MAX), (0..34).collect());
    assert_eq!(picks(0.4, usize::MAX), BTreeSet::from([0]));
  }
}
