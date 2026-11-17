//! One analysis from file to measures: the file is decoded a window at a time, in full or at
//! windows spread over it, each window is measured, and a file that claims a lossless format is
//! scanned for a codec's frame grid.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSource;
use tracing::debug;

use crate::decode::{self, Claimed, Decoder, Filled};
use crate::grid::{self, Scan, Segments};
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
    /// further than this, so not at all when it is 0.
    over_seconds: f64,
    /// Number of windows a longer file is analyzed at.
    windows: NonZeroUsize,
  },
}

impl Coverage {
  /// The longest file analyzed in full, in seconds.
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
  pub deadline: Option<Instant>,
  /// Where the analysis reports how far it is, in thousandths of the file.
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

/// Whether `canceled` says the analysis is canceled; never without one.
fn is_canceled(canceled: Option<&CancelCheck>) -> bool {
  canceled.is_some_and(|canceled| canceled())
}

/// Whether `deadline` has passed; never without one.
fn is_past(deadline: Option<Instant>) -> bool {
  deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

impl Settings {
  /// Says the analysis is `done` of the way through, from 0 to 1.
  fn report(&self, done: f64) {
    if let Some(progress) = &self.progress {
      progress.store((done.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
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
  /// `None` for a file that claims a lossy codec, one too short for the scan, or one whose
  /// scan could not run, which [`Reading::limitations`](crate::Reading::limitations) says.
  pub grid: Option<Scan>,
  /// the channels left out of a file with more than two
  pub(crate) channels_left_out: u32,
  /// what made the analysis stop early or skip audio
  pub(crate) problems: Vec<Limitation>,
}

/// Error of an analysis that measured nothing.
#[derive(Debug)]
pub enum Error {
  /// The analysis was canceled.
  ///
  /// Nothing is reported of it.
  Canceled,
  /// The deadline passed before the audio was opened.
  OutOfTime,
  /// The file could not be opened.
  Open(io::Error),
  /// The bytes were not opened as audio, with why in words.
  ///
  /// The container is not recognized, holds no audio track or does not state its sample rate
  /// or channels, the codec is not one the analyzer decodes, or the sample rate is above what
  /// the analyzer reads.
  NotAudio(String),
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Canceled => f.write_str("the analysis was canceled"),
      Self::OutOfTime => f.write_str("the analysis ran out of time while opening the file"),
      Self::Open(_) => f.write_str("the file could not be opened"),
      Self::NotAudio(why) => f.write_str(why),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Open(e) => Some(e),
      Self::Canceled | Self::OutOfTime | Self::NotAudio(_) => None,
    }
  }
}

/// A source of audio bytes that stops giving them once the analysis is canceled or its time is
/// up, which ends whatever the decoder was doing with an error.
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
/// A file that claims a lossless format is then scanned for a codec's frame grid in four
/// stretches of its samples ([`grid`]). The scan's eight jobs, each codec in each stretch, run
/// on rayon's thread pool: the global one, or the one the analysis is called from inside.
/// Cancellation and the deadline are checked before each job.
///
/// # Errors
///
/// Returns [`Error::Open`] if the file cannot be opened or is a directory,
/// [`Error::NotAudio`] if its bytes are not opened as audio, [`Error::Canceled`] if
/// [`Settings::canceled`] says so before the analysis ends, and [`Error::OutOfTime`] if
/// [`Settings::deadline`] passes before the audio is opened.
///
/// # Examples
///
/// Measuring is the slow step, so a file measured once can be read under several thresholds.
/// A long file is sampled here, and the analysis given a deadline:
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
  let file = File::open(path).map_err(Error::Open)?;
  // a directory opens on some systems and only fails when read
  if file.metadata().map_err(Error::Open)?.is_dir() {
    return Err(Error::Open(io::ErrorKind::IsADirectory.into()));
  }
  let source = Watched::new(Box::new(file), settings);
  let opened = Decoder::from_source(Box::new(source), &decode::hint_of(path));
  if let Err(problem) = &opened {
    debug!(path = %path.display(), problem, "audio not opened");
  }
  measure_opened(opened, settings)
}

/// Measures audio held in memory, as [`measure`] does a file's.
///
/// # Errors
///
/// Returns [`Error::NotAudio`] if the bytes are not opened as audio, [`Error::Canceled`] if
/// [`Settings::canceled`] says so before the analysis ends, and [`Error::OutOfTime`] if
/// [`Settings::deadline`] passes before the audio is opened.
pub fn measure_bytes(bytes: Vec<u8>, settings: &Settings) -> Result<Measured, Error> {
  let source = Watched::new(Box::new(io::Cursor::new(bytes)), settings);
  measure_opened(
    Decoder::from_source(Box::new(source), &Hint::new()),
    settings,
  )
}

/// Measures the audio of `opened`, or says why it did not open: the analysis's own stop when
/// that cut opening short.
fn measure_opened(opened: Result<Decoder, String>, settings: &Settings) -> Result<Measured, Error> {
  match opened {
    Ok(decoder) => measure_decoded(decoder, settings),
    Err(_) if is_canceled(settings.canceled.as_ref()) => Err(Error::Canceled),
    Err(_) if is_past(settings.deadline) => Err(Error::OutOfTime),
    Err(problem) => Err(Error::NotAudio(problem)),
  }
}

/// Measures the audio `decoder` reads.
fn measure_decoded(mut decoder: Decoder, settings: &Settings) -> Result<Measured, Error> {
  let claimed = decoder.claimed.clone();
  let rate = claimed.sample_rate as usize;
  let stated = claimed.frames.map(|frames| frames as f64 / rate as f64);
  let mut analyzer = Analyzer::new(claimed.sample_rate, claimed.bit_depth, &settings.thresholds);
  let mut buffer: Vec<Vec<f32>> = (0..decoder.channels)
    .map(|_| Vec::with_capacity(rate))
    .collect();
  // a lossy file carries its own codec's grid, which says nothing
  let mut segments = claimed
    .frames
    .filter(|_| claimed.lossless)
    .and_then(Segments::of);
  let unscanned = claimed.lossless && claimed.frames.is_none();
  let mut measured = Measured {
    duration: stated.unwrap_or(0.0),
    bitrate_kbps: None,
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
  let reached = match picks {
    Some(picks) => {
      measured.sampled = true;
      sampled(
        &mut decoder,
        &mut analyzer,
        &mut buffer,
        &picks,
        &mut segments,
        settings,
        &mut measured,
      )?
    }
    None => {
      // reading in full stops at the longest file read so, whatever length the file states, if
      // any
      let longest = settings.coverage.max_full_seconds().ceil() as u64;
      whole(
        &mut decoder,
        &mut analyzer,
        &mut buffer,
        Windows::Upto(longest),
        &mut segments,
        settings,
        &mut measured,
      )?
    }
  };
  // the length is what was read when the read came to an end the file set, or when the file
  // states none; a read whose time ran out says nothing against the stated length
  match reached {
    Reached::End(seconds) => measured.duration = seconds,
    Reached::Short(seconds) if stated.is_none() => measured.duration = seconds,
    Reached::Short(_) | Reached::Sampled => {
      let read = measured.windows.last().map_or(0.0, |w| w.start + 1.0);
      if let Some(held) = past_bytes(&decoder, measured.duration, read, &mut measured) {
        measured.duration = held;
      }
    }
  }
  if let Some(segments) = &mut segments
    && !segments.complete()
    && measured.sampled
  {
    read_segments(&mut decoder, segments, settings, &mut measured)?;
  }
  measured.bits_used = decoder.bits_used();
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
  if unscanned {
    measured.problems.push(Limitation::GridNotScanned);
  }
  if let Some(segments) = &segments {
    measured.grid = scan_grid(segments, settings, &mut measured)?;
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

/// Reads the segments of the grid scan that sampled windows did not, seeking once to each and
/// placing its samples by the timestamps of their packets. A segment that cannot be read whole
/// stays short: a seek that fails is noted in `measured`, and a read that fails leaves its
/// reason in the decoder.
fn read_segments(
  decoder: &mut Decoder,
  segments: &mut Segments,
  settings: &Settings,
  measured: &mut Measured,
) -> Result<(), Error> {
  // without whole frames per tick, a packet's timestamp does not place its first sample
  if !decoder.ticks_fit_frames() {
    return Ok(());
  }
  let channels = decoder.claimed.channels as usize;
  let mut samples = Vec::new();
  for index in segments.missing() {
    if segments.untouched(index)
      && let Some(first) = segments.needs(index)
    {
      if halted(settings)? {
        return Ok(());
      }
      match decoder.seek_frame(first) {
        Ok(true) => {}
        Ok(false) => {
          debug!(
            frame = first,
            "a grid segment is not among the packets indexed"
          );
          return Ok(());
        }
        Err(problem) => {
          // a seek the analysis's own stop cut short only says its source gave no more bytes
          if !halted(settings)? {
            debug!(problem, frame = first, "seeking to a grid segment failed");
            measured.problems.push(Limitation::Decoder(problem));
          }
          return Ok(());
        }
      }
    }
    // a seek can give a packet that ends at or before the frame needed; nothing is taken of it
    // and the next is read
    while let Some(next) = segments.needs(index) {
      if halted(settings)? {
        return Ok(());
      }
      let Some(frames) = decoder.next_interleaved(&mut samples) else {
        // as with the seek, the decoder's reason for the analysis's own stop says nothing
        if halted(settings)? {
          decoder.stopped = None;
        }
        return Ok(());
      };
      // a packet past the frame needed leaves a gap no later packet fills
      if frames.start > next {
        debug!(frame = next, "a grid segment's packets leave a gap");
        return Ok(());
      }
      segments.take_interleaved(frames.start, &samples, channels);
    }
  }
  Ok(())
}

/// Scans full `segments` for a codec's frame grid; `None`, noted in `measured`, when they are
/// not full or the analysis runs out of time first.
fn scan_grid(
  segments: &Segments,
  settings: &Settings,
  measured: &mut Measured,
) -> Result<Option<Scan>, Error> {
  if !segments.complete() {
    debug!("the grid segments were not all read");
    measured.problems.push(Limitation::GridScanIncomplete);
    return Ok(None);
  }
  let scan = grid::scan(segments, || {
    !is_canceled(settings.canceled.as_ref()) && !is_past(settings.deadline)
  });
  if scan.is_none() && is_canceled(settings.canceled.as_ref()) {
    return Err(Error::Canceled);
  }
  if scan.is_none() {
    debug!("grid scan out of time");
    measured.problems.push(Limitation::GridScanOutOfTime);
  }
  Ok(scan)
}

/// The whole seconds `count` windows spread evenly over `seconds` of audio start at.
fn picks(seconds: f64, count: usize) -> BTreeSet<u64> {
  let last = (seconds - 1.0).max(0.0);
  (0..count)
    .map(|i| {
      let at = last * i as f64 / (count.max(2) - 1) as f64;
      at.floor() as u64
    })
    .collect()
}

/// Whether the analysis is to stop: an error when it was canceled, true when its time is up,
/// which is noted once in `measured`.
fn stop(settings: &Settings, measured: &mut Measured) -> Result<bool, Error> {
  let halted = halted(settings)?;
  if halted {
    debug!(windows = measured.windows.len(), "analysis out of time");
    measured.problems.push(Limitation::OutOfTime);
  }
  Ok(halted)
}

/// Whether the analysis is out of time; an error when it was canceled.
fn halted(settings: &Settings) -> Result<bool, Error> {
  if is_canceled(settings.canceled.as_ref()) {
    return Err(Error::Canceled);
  }
  Ok(is_past(settings.deadline))
}

/// Whether decoding that just ended, or a seek that just failed, was the analysis's own stop
/// rather than the file's doing: [`stop`], dropping the reason the decoder gave, which is only
/// that its source stopped giving bytes.
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

/// How far a read went.
enum Reached {
  /// to an end the file set: its last sample, or the longest a read in full goes, this many
  /// seconds in
  End(f64),
  /// this many seconds in, when its time ran out
  Short(f64),
  /// windows spread over the stated length, which says nothing against it
  Sampled,
}

/// The length the file's bytes can hold, when audio up to `seconds` needs more bytes than the
/// file has: the length taken in place of the one stated, never short of `read`, the furthest
/// the analysis read, and noted in `measured`.
fn past_bytes(decoder: &Decoder, seconds: f64, read: f64, measured: &mut Measured) -> Option<f64> {
  let most = decoder.most_seconds().filter(|most| seconds > *most)?;
  debug!(
    seconds,
    most, "the stated length is past what the file's bytes hold"
  );
  measured.problems.push(Limitation::LengthPastBytes);
  Some(most.max(read))
}

/// Whether a window filled as `filled` is worth measuring: a whole second, or a last part of at
/// least half a second and one spectrum frame.
fn usable(filled: &Filled, rate: usize) -> bool {
  match filled {
    Filled::Full => true,
    Filled::End(length) => *length >= FFT_SIZE.min(rate) && length * 2 >= rate,
  }
}

/// Which windows [`whole`] measures.
enum Windows<'a> {
  /// every window, up to this many
  Upto(u64),
  /// the windows starting at these seconds
  Only(&'a BTreeSet<u64>),
}

/// Reads windows one after another from where the decoder is and measures those `read` asks
/// for.
fn whole(
  decoder: &mut Decoder,
  analyzer: &mut Analyzer,
  buffer: &mut [Vec<f32>],
  read: Windows<'_>,
  segments: &mut Option<Segments>,
  settings: &Settings,
  measured: &mut Measured,
) -> Result<Reached, Error> {
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
    settings.report(second as f64 / expected.max(1.0));
    if stop(settings, measured)? {
      return Ok(Reached::Short(second as f64));
    }
    let filled = decoder.fill(buffer, rate);
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
        "reading in full stopped at the longest file read so"
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
      segments.take(second * rate as u64, buffer);
    }
    let wanted = match read {
      Windows::Upto(_) => true,
      Windows::Only(picks) => picks.contains(&second),
    };
    if usable(&filled, rate) && wanted {
      let window = analyzer.measure(second as f64, buffer, &mut measured.totals);
      measured.windows.push(window);
    }
    if filled != Filled::Full {
      return Ok(Reached::End(second as f64 + length as f64 / rate as f64));
    }
    second += 1;
  }
}

/// Measures one window at each of `picks`, seeking to each; when the file cannot be sought
/// in, reads it from the start and measures the picked seconds as they come. A file that gives
/// out before the last pick, its end reached or a seek past what it holds failing, has ended
/// where the furthest window read ends.
fn sampled(
  decoder: &mut Decoder,
  analyzer: &mut Analyzer,
  buffer: &mut [Vec<f32>],
  picks: &BTreeSet<u64>,
  segments: &mut Option<Segments>,
  settings: &Settings,
  measured: &mut Measured,
) -> Result<Reached, Error> {
  let rate = decoder.claimed.sample_rate as usize;
  // where the furthest window read ends
  let mut furthest = 0.0;
  for (index, &second) in picks.iter().enumerate() {
    settings.report(index as f64 / picks.len() as f64);
    if stop(settings, measured)? {
      return Ok(Reached::Sampled);
    }
    // the picks are spread over the stated length, which tables pointing many packets at the
    // same bytes can make longer than the file holds
    if let Some(held) = past_bytes(decoder, (second + 1) as f64, furthest, measured) {
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
      debug!(problem, "the file cannot be sought in, reading it through");
      return match whole(
        decoder,
        analyzer,
        buffer,
        Windows::Only(picks),
        segments,
        settings,
        measured,
      )? {
        Reached::Short(_) => Ok(Reached::Sampled),
        reached => Ok(reached),
      };
    }
    let filled = decoder.fill(buffer, rate);
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
        let window = analyzer.measure(second as f64, buffer, &mut measured.totals);
        measured.windows.push(window);
      }
      debug!(second, "the file ended before its last pick");
      return Ok(Reached::End(furthest));
    }
    furthest = (second + 1) as f64;
    let window = analyzer.measure(second as f64, buffer, &mut measured.totals);
    measured.windows.push(window);
  }
  Ok(Reached::Sampled)
}

/// What a reading saw, for a failing test to print: the reading, then each window's start,
/// level, cutoff, shelf and steepest inner step.
#[cfg(test)]
pub(crate) fn explain(measured: &Measured, reading: &crate::reading::Reading) -> String {
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
