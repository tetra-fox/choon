//! One analysis from file to measures.
//!
//! The file is decoded a window at a time, in full or, for a long file, at windows spread evenly
//! over it, and each window is measured. An analysis blocks the thread it runs on, stops
//! promptly when canceled, and stops with what it has when its time is up. The file's bytes are
//! read through a source that stops giving them at either, so a container that is slow to open
//! or to seek in holds the thread no longer than a window does.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSource;
use tracing::debug;

use crate::decode::{Claimed, Decoder, Filled};
use crate::measure::{Analyzer, FFT_SIZE, Window};
use crate::thresholds::MeasureThresholds;
use crate::totals::Totals;

/// Check that says whether an analysis is canceled.
///
/// It is asked between windows and before each read of the file's bytes, so it must answer
/// quickly.
pub type CancelCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Settings of one analysis.
#[derive(Clone)]
pub struct Settings {
  /// Longest file analyzed in full, in seconds.
  pub max_full_seconds: f64,
  /// Number of windows a longer file is analyzed at.
  pub sampled_windows: usize,
  /// Time at which the analysis stops with what it has.
  pub deadline: Instant,
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

/// Whether `canceled` says the analysis is canceled; never without one.
fn is_canceled(canceled: Option<&CancelCheck>) -> bool {
  canceled.is_some_and(|canceled| canceled())
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
#[derive(Debug)]
pub struct Measured {
  /// Format the file claims, `None` when it could not be opened as audio.
  pub claimed: Option<Claimed>,
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
  /// the channels left out of a file with more than two
  pub(crate) channels_left_out: u32,
  /// what made the analysis stop early or skip audio, in words for the report
  pub(crate) problems: Vec<String>,
}

/// Error of an analysis that was canceled.
///
/// Nothing is reported of it.
#[derive(Debug, PartialEq, Eq)]
pub struct Canceled;

/// What an analysis that ran out of time says of it.
const OUT_OF_TIME: &str = "The analysis ran out of time and covers only the part read by then";

/// A source of audio bytes that stops giving them once the analysis is canceled or its time is
/// up, which ends whatever the decoder was doing with an error.
struct Watched {
  inner: Box<dyn MediaSource>,
  canceled: Option<CancelCheck>,
  deadline: Instant,
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
    if is_canceled(self.canceled.as_ref()) || Instant::now() >= self.deadline {
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

/// Analyzes the file at `path`.
///
/// # Errors
///
/// Returns [`Canceled`] if [`Settings::canceled`] says so before the analysis ends.
pub fn run(path: &Path, settings: &Settings) -> Result<Measured, Canceled> {
  let opened = Decoder::open_through(path, |file| {
    Box::new(Watched::new(Box::new(file), settings))
  });
  if let Err(problem) = &opened {
    debug!(path = %path.display(), problem, "audio not opened");
  }
  measure_opened(opened, settings)
}

/// Analyzes audio held in memory, as a file's would be.
///
/// # Errors
///
/// Returns [`Canceled`] if [`Settings::canceled`] says so before the analysis ends.
pub fn read_bytes(bytes: Vec<u8>, settings: &Settings) -> Result<Measured, Canceled> {
  let source = Watched::new(Box::new(std::io::Cursor::new(bytes)), settings);
  measure_opened(
    Decoder::from_source(Box::new(source), &Hint::new()),
    settings,
  )
}

/// Measures the audio of `opened`, or reports why it did not open: the analysis's own stop when
/// that cut opening short.
fn measure_opened(
  opened: Result<Decoder, String>,
  settings: &Settings,
) -> Result<Measured, Canceled> {
  match opened {
    Ok(decoder) => measure(decoder, settings),
    Err(_) if is_canceled(settings.canceled.as_ref()) => Err(Canceled),
    Err(_) if Instant::now() >= settings.deadline => Ok(unreadable(
      "The analysis ran out of time while opening the file".to_string(),
    )),
    Err(problem) => Ok(unreadable(problem)),
  }
}

/// What is reported for audio that could not be opened.
fn unreadable(problem: String) -> Measured {
  Measured {
    claimed: None,
    duration: 0.0,
    bitrate_kbps: None,
    windows: Vec::new(),
    totals: Totals::default(),
    bits_used: None,
    sampled: false,
    channels_left_out: 0,
    problems: vec![problem],
  }
}

/// Measures the audio `decoder` reads.
fn measure(mut decoder: Decoder, settings: &Settings) -> Result<Measured, Canceled> {
  let claimed = decoder.claimed.clone();
  let rate = claimed.sample_rate as usize;
  let stated = claimed.frames.map(|frames| frames as f64 / rate as f64);
  let mut analyzer = Analyzer::new(claimed.sample_rate, claimed.bit_depth, &settings.thresholds);
  let mut buffer = vec![Vec::with_capacity(rate); decoder.channels];
  let mut measured = Measured {
    duration: stated.unwrap_or(0.0),
    bitrate_kbps: None,
    channels_left_out: claimed.channels.saturating_sub(decoder.channels as u32),
    claimed: Some(claimed),
    windows: Vec::new(),
    totals: Totals::default(),
    bits_used: None,
    sampled: false,
    problems: Vec::new(),
  };
  let picks = stated
    .filter(|seconds| *seconds > settings.max_full_seconds)
    .map(|seconds| picks(seconds, settings.sampled_windows));
  let reached = match picks {
    Some(picks) => {
      measured.sampled = true;
      sampled(
        &mut decoder,
        &mut analyzer,
        &mut buffer,
        &picks,
        settings,
        &mut measured,
      )?
    }
    None => {
      // reading in full stops at max_full_seconds, whatever length the file states, if any
      let longest = settings.max_full_seconds.ceil() as u64;
      whole(
        &mut decoder,
        &mut analyzer,
        &mut buffer,
        Windows::Upto(longest),
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
      let read = measured.windows.last().map_or(0.0, |w| w.t + 1.0);
      if let Some(held) = past_bytes(&decoder, measured.duration, read, &mut measured) {
        measured.duration = held;
      }
    }
  }
  settings.report(1.0);
  measured.bits_used = decoder.bits_used();
  if measured.claimed.as_ref().is_some_and(|c| !c.lossless) {
    measured.bitrate_kbps = decoder.bitrate_kbps();
  }
  if decoder.skipped > 0 {
    measured.problems.push(format!(
      "{} packets could not be decoded and were skipped",
      decoder.skipped
    ));
  }
  if let Some(stopped) = decoder.stopped.take() {
    measured.problems.push(stopped);
  }
  debug!(
    windows = measured.windows.len(),
    sampled = measured.sampled,
    skipped = decoder.skipped,
    "audio measured"
  );
  Ok(measured)
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
fn stop(settings: &Settings, measured: &mut Measured) -> Result<bool, Canceled> {
  if is_canceled(settings.canceled.as_ref()) {
    return Err(Canceled);
  }
  if Instant::now() >= settings.deadline {
    debug!(windows = measured.windows.len(), "analysis out of time");
    measured.problems.push(OUT_OF_TIME.to_string());
    return Ok(true);
  }
  Ok(false)
}

/// Whether decoding that just ended, or a seek that just failed, was the analysis's own stop
/// rather than the file's doing: [`stop`], dropping the reason the decoder gave, which is only
/// that its source stopped giving bytes.
fn stopped_by_us(
  decoder: &mut Decoder,
  settings: &Settings,
  measured: &mut Measured,
) -> Result<bool, Canceled> {
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

/// What a file states of its length beyond what its bytes can hold.
const PAST_BYTES: &str =
  "The file states a longer length than its bytes can hold, so the length they hold is used";

/// The length the file's bytes can hold, when audio up to `seconds` needs more bytes than the
/// file has: the length taken in place of the one stated, never short of `read`, the furthest
/// the analysis read, and noted in `measured`.
fn past_bytes(decoder: &Decoder, seconds: f64, read: f64, measured: &mut Measured) -> Option<f64> {
  let most = decoder.most_seconds().filter(|most| seconds > *most)?;
  debug!(
    seconds,
    most, "the stated length is past what the file's bytes hold"
  );
  measured.problems.push(PAST_BYTES.to_string());
  Some(most.max(read))
}

/// `seconds` in words, as minutes when they are whole minutes.
fn minutes(seconds: u64) -> String {
  match seconds {
    60 => "a minute".to_string(),
    s if s % 60 == 0 => format!("{} minutes", s / 60),
    s => format!("{s} seconds"),
  }
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
  settings: &Settings,
  measured: &mut Measured,
) -> Result<Reached, Canceled> {
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
        None => format!(
          "The file does not state its length, so the analysis stopped after {}",
          minutes(second)
        ),
        Some(_) => format!(
          "The file runs longer than it states, so the analysis stopped after {}",
          minutes(second)
        ),
      });
      return Ok(Reached::End(second as f64));
    }
    let length = match filled {
      Filled::Full => rate,
      Filled::End(length) => length,
    };
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
  settings: &Settings,
  measured: &mut Measured,
) -> Result<Reached, Canceled> {
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
        measured.problems.push(problem);
        return Ok(Reached::End(furthest));
      }
      debug!(problem, "the file cannot be sought in, reading it through");
      return match whole(
        decoder,
        analyzer,
        buffer,
        Windows::Only(picks),
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
      window.t,
      window.rms_dbfs,
      window.cutoff_hz.map(f64::round),
      window.shelf_db.map(f64::round),
      step.unwrap_or_default()
    );
  }
  text
}
