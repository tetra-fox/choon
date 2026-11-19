//! how a file is measured: in full or at windows spread over it, with the grid's stretches
//! read either way, under the thresholds, the progress and the cancel check it is given, and
//! no further than the length its bytes hold

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use super::{a_17_khz_cut, flac, read, sampled, settings};
use crate::engine::{Error, Measured, Settings, measure, measure_bytes};
use crate::fixtures::{self, Audio, Bitrate, music, music_for, pink};
use crate::grid;
use crate::limitation::Limitation;
use crate::reading::Verdict;
use crate::thresholds::MeasureThresholds;

#[test]
fn progress_goes_on_through_the_grid_scan() {
  let dir = tempfile::tempdir().unwrap();
  let path = flac(dir.path(), "a.flac", &music(1, 44_100, pink), 16);
  let progress = Arc::new(std::sync::atomic::AtomicU32::new(0));
  let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
  // the cancel check is asked between windows, before each job of the scan and before each
  // segment's quantizer steps, which are read once the last job has ended
  let (at, record) = (progress.clone(), seen.clone());
  let settings = Settings {
    progress: Some(progress.clone()),
    canceled: Some(Arc::new(move || {
      let now = at.load(std::sync::atomic::Ordering::Relaxed);
      record.lock().unwrap().push(now);
      false
    })),
    ..Settings::default()
  };
  // on one thread a job of the scan starts once the one before it has ended
  let pool = rayon::ThreadPoolBuilder::new()
    .num_threads(1)
    .build()
    .unwrap();
  let measured = pool.install(|| measure(&path, &settings)).unwrap();
  assert!(measured.grid.is_some());
  let seen = seen.lock().unwrap();
  assert!(seen.is_sorted(), "{seen:?}");
  let jobs_ended = seen
    .windows(2)
    .filter(|pair| pair[1] - pair[0] >= 30)
    .count();
  assert_eq!(jobs_ended, grid::jobs(true, true), "{seen:?}");
  assert_eq!(progress.load(std::sync::atomic::Ordering::Relaxed), 1000);
}

#[test]
fn a_long_file_is_sampled_at_windows_spread_over_it() {
  let dir = tempfile::tempdir().unwrap();
  let path = flac(dir.path(), "a.flac", &music(1, 44_100, pink), 16);
  let progress = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
  let settings = Settings {
    coverage: sampled(10.0, 20),
    progress: Some(progress.clone()),
    ..settings()
  };
  let measured = measure(&path, &settings).unwrap();
  assert!(measured.sampled);
  assert_eq!(measured.windows.len(), 20);
  assert_eq!(progress.load(std::sync::atomic::Ordering::Relaxed), 1000);
  let starts: Vec<f64> = measured.windows.iter().map(|w| w.start).collect();
  assert_eq!(starts.first(), Some(&0.0));
  assert_eq!(starts.last(), Some(&32.0), "the last whole second");
  let reading = read(&measured);
  assert!(
    reading
      .limitations
      .contains(&Limitation::Sampled { windows: 20 }),
    "{reading:#?}"
  );
}

#[test]
fn a_sampled_window_holds_the_same_second_as_one_read_in_full() {
  let dir = tempfile::tempdir().unwrap();
  let path = flac(dir.path(), "a.flac", &music(1, 44_100, pink), 16);
  let full = measure(&path, &settings()).unwrap();
  // no second but the first starts where a packet does
  let sampled = Settings {
    coverage: sampled(10.0, 5),
    ..settings()
  };
  let sampled = measure(&path, &sampled).unwrap();
  assert_eq!(sampled.windows.len(), 5);
  for window in &sampled.windows {
    let same = full.windows.iter().find(|w| w.start == window.start);
    assert!(Some(window) == same, "the window at {} s", window.start);
  }
}

/// the scan of a file's stretches: its grid scan without the counts of the windows measured,
/// which a sampled read takes elsewhere than a read in full does
fn of_stretches(measured: &Measured) -> Option<grid::Scan> {
  let mut scan = measured.grid.clone()?;
  if let Some(opus) = &mut scan.opus {
    opus.uncut_frames = 0;
    if let Some(shifted) = &mut opus.shifted {
      shifted.uncut_frames = 0;
    }
    if let Some(hybrid) = &mut opus.hybrid {
      (hybrid.uncut_frames, hybrid.uncut_tested, hybrid.uncut_steps) = (0, 0, 0);
    }
  }
  Some(scan)
}

#[test]
fn a_sampled_file_scans_the_same_samples_for_a_grid_as_one_read_in_full() {
  let dir = tempfile::tempdir().unwrap();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir.path());
  let path = flac(dir.path(), "t.flac", &through, 16);
  let full = measure(&path, &settings()).unwrap();
  // the sampled windows miss the grid's stretches, which are sought and placed by their
  // packets' timestamps
  let sampled = Settings {
    coverage: sampled(10.0, 3),
    ..settings()
  };
  let sampled = measure(&path, &sampled).unwrap();
  assert!(sampled.sampled);
  assert!(full.grid.is_some());
  assert_eq!(of_stretches(&sampled), of_stretches(&full));
}

#[test]
fn a_grid_stretch_starting_where_a_packet_ends_is_read_from_the_next_packet() {
  // 400 blocks put the first stretch, a fifth of the way in, at the start of block 80, and
  // an Ogg seek to a packet's first frame gives the packet that ends there
  let mut audio = music_for(40, 1, 44_100, pink);
  for channel in &mut audio.channels {
    channel.truncate(400 * fixtures::FLAC_BLOCK);
  }
  let sampled = Settings {
    coverage: sampled(10.0, 3),
    ..settings()
  };
  let sampled = measure_bytes(fixtures::ogg_flac(&audio, 16), &sampled).unwrap();
  assert!(sampled.sampled);
  let full = measure_bytes(fixtures::flac(&audio, 16, false), &settings()).unwrap();
  assert!(full.grid.is_some());
  assert_eq!(of_stretches(&sampled), of_stretches(&full));
}

#[test]
fn a_canceled_analysis_and_one_out_of_time_before_it_opens_report_nothing() {
  let dir = tempfile::tempdir().unwrap();
  let path = flac(dir.path(), "a.flac", &music(1, 44_100, pink), 16);
  let canceled = Settings {
    canceled: Some(Arc::new(|| true)),
    ..settings()
  };
  assert!(matches!(measure(&path, &canceled), Err(Error::Canceled)));
  let late = Settings {
    deadline: Some(Instant::now()),
    ..settings()
  };
  assert!(matches!(measure(&path, &late), Err(Error::OutOfTime)));
}

#[test]
fn an_analysis_canceled_at_any_ask_reports_nothing() {
  use std::sync::atomic::{AtomicUsize, Ordering};
  // sampled, so that the grid's stretches are sought to, in a container whose reader says a
  // seek cut short by the cancel is out of range
  let bytes = fixtures::ogg_flac(&music_for(8, 1, 44_100, pink), 16);
  let asked_until = |until: usize| {
    let asked = Arc::new(AtomicUsize::new(0));
    let canceled = {
      let asked = asked.clone();
      Arc::new(move || asked.fetch_add(1, Ordering::Relaxed) >= until)
    };
    let settings = Settings {
      coverage: sampled(4.0, 3),
      canceled: Some(canceled),
      ..settings()
    };
    let result = measure_bytes(bytes.clone(), &settings);
    (result, asked.load(Ordering::Relaxed))
  };
  let (whole, asks) = asked_until(usize::MAX);
  assert!(whole.unwrap().grid.is_some());
  for until in 0..asks {
    let (result, _) = asked_until(until);
    assert!(
      matches!(result, Err(Error::Canceled)),
      "canceled at ask {until} of {asks}: {:?}",
      result.map(|measured| measured.problems)
    );
  }
}

#[test]
fn measuring_uses_the_thresholds_it_is_given() {
  let dir = tempfile::tempdir().unwrap();
  // every window is quieter than full scale, so every one is silent
  let deaf = Settings {
    thresholds: MeasureThresholds {
      silence_dbfs: 0.0,
      ..MeasureThresholds::default()
    },
    ..settings()
  };
  let measured = measure(a_17_khz_cut(dir.path()), &deaf).unwrap();
  assert!(!measured.windows.is_empty());
  assert!(measured.windows.iter().all(|w| w.cutoff_hz.is_none()));
  assert_eq!(read(&measured).verdict, Verdict::Inconclusive);
}

#[test]
fn measures_kept_as_json_read_the_same_when_read_back() {
  let through = {
    let dir = tempfile::tempdir().unwrap();
    fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir.path())
  };
  let measured = measure_bytes(fixtures::flac(&through, 16, false), &settings()).unwrap();
  let kept = serde_json::to_string(&measured).unwrap();
  let back: Measured = serde_json::from_str(&kept).unwrap();
  assert_eq!(back.windows, measured.windows);
  assert_eq!(back.grid, measured.grid);
  let reading = read(&back);
  assert_eq!(reading.verdict, Verdict::LikelyTranscode);
  assert_eq!(reading, read(&measured));
}

#[test]
fn a_step_search_from_far_below_the_spectrum_ends() {
  let far = Settings {
    thresholds: MeasureThresholds {
      inner_shelf_from_hz: -1e18,
      ..MeasureThresholds::default()
    },
    ..settings()
  };
  let bytes = fixtures::flac(&music_for(2, 1, 44_100, pink), 16, false);
  let measured = measure_bytes(bytes, &far).unwrap();
  assert!(measured.windows.iter().any(|w| w.inner_shelf.is_some()));
}

/// writes a FLAC of `seconds` of music at `path` that states it holds `frames` sample frames
fn stating(path: &Path, seconds: usize, frames: u64) {
  fixtures::write_flac(&music_for(seconds, 1, 44_100, pink), 16, false, path);
  state(path, frames);
}

/// makes the FLAC at `path` state that it holds `frames` sample frames
fn state(path: &Path, frames: u64) {
  // the stream information's total sample count is the low 36 bits of bytes 21 to 25
  let mut bytes = std::fs::read(path).unwrap();
  bytes[21] = (bytes[21] & 0xf0) | ((frames >> 32) as u8 & 0x0f);
  bytes[22..26].copy_from_slice(&(frames as u32).to_be_bytes());
  std::fs::write(path, &bytes).unwrap();
}

#[test]
fn bytes_that_are_no_audio_and_audio_the_analyzer_does_not_read_are_told_apart() {
  let none = measure_bytes(vec![0; 4096], &settings());
  assert!(matches!(none, Err(Error::NotAudio(_))), "{none:?}");
  // a WAV header of 16-bit mono PCM at 1 MHz, then a second of silence
  let rate = 1_000_000u32;
  let data = vec![0u8; 2 * rate as usize];
  let mut bytes = b"RIFF".to_vec();
  bytes.extend((36 + data.len() as u32).to_le_bytes());
  bytes.extend(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
  bytes.extend(rate.to_le_bytes());
  bytes.extend((2 * rate).to_le_bytes());
  bytes.extend(b"\x02\0\x10\0data");
  bytes.extend((data.len() as u32).to_le_bytes());
  bytes.extend(data);
  let fast = measure_bytes(bytes, &settings());
  assert!(
    matches!(&fast, Err(Error::Unsupported(why)) if why.contains("1000000 Hz")),
    "{fast:?}"
  );
}

#[test]
fn a_file_without_a_stated_length_is_read_only_so_far() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("a.flac");
  // a total sample count of 0 is none stated
  stating(&path, fixtures::SECONDS, 0);
  let settings = Settings {
    coverage: sampled(10.0, 240),
    ..settings()
  };
  let measured = measure(&path, &settings).unwrap();
  assert_eq!(measured.claimed.frames, None);
  assert!(!measured.sampled);
  assert_eq!(measured.windows.len(), 10);
  assert_eq!(measured.duration, 10.0);
  let reading = read(&measured);
  assert!(
    reading
      .limitations
      .contains(&Limitation::LengthNotStated { seconds: 10 }),
    "{reading:#?}"
  );
}

#[test]
fn a_file_shorter_than_it_states_reports_the_length_read() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("short.flac");
  stating(&path, fixtures::SECONDS, 600 * 44_100);
  let measured = measure(&path, &settings()).unwrap();
  assert!(!measured.sampled);
  assert_eq!(measured.windows.len(), fixtures::SECONDS);
  assert!(
    (measured.duration - fixtures::SECONDS as f64).abs() < 0.1,
    "{}",
    measured.duration
  );
}

#[test]
fn a_file_stating_a_huge_length_reports_only_what_was_read() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("huge.flac");
  stating(&path, 4, (1 << 36) - 1);
  let measured = measure(&path, &settings()).unwrap();
  assert!(measured.sampled);
  assert!(measured.duration <= 4.0, "{}", measured.duration);
  assert!(!measured.windows.is_empty());
  let reading = read(&measured);
  assert!(
    reading.limitations.contains(&Limitation::LengthPastBytes),
    "{reading:#?}"
  );
}

#[test]
fn a_seek_past_what_a_file_holds_ends_the_length_where_reading_stopped() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("short.flac");
  // digital silence takes a few bytes a frame; read first, it says these bytes could hold far
  // more than 2000 s, so the picks go on until a seek past the end fails
  let audio = music_for(4, 1, 44_100, pink);
  let silence = Audio {
    rate: audio.rate,
    channels: vec![vec![0.0; 8 * 44_100]; 2],
  };
  let padded = Audio {
    rate: audio.rate,
    channels: audio
      .channels
      .iter()
      .zip(&silence.channels)
      .map(|(music, quiet)| quiet.iter().chain(music).copied().collect())
      .collect(),
  };
  fixtures::write_flac(&padded, 16, false, &path);
  state(&path, 2000 * 44_100);
  let measured = measure(&path, &settings()).unwrap();
  assert!(measured.sampled);
  assert!(measured.duration <= 12.0, "{}", measured.duration);
  let reading = read(&measured);
  assert!(
    reading
      .limitations
      .iter()
      .any(|l| matches!(l, Limitation::Decoder(why) if why.starts_with("seeking failed"))),
    "{reading:#?}"
  );
}

#[test]
fn a_file_stating_more_than_its_bytes_hold_is_as_long_as_they_hold() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("repeating.m4a");
  // every chunk points at the same second, and each sample lasts 2^32 - 1 ticks
  fixtures::write_repeating_mp4(&music(1, 44_100, pink), 20, u32::MAX, &path);
  let bytes = std::fs::metadata(&path).unwrap().len();
  let settings = Settings {
    coverage: sampled(10.0, 20),
    ..settings()
  };
  let measured = measure(&path, &settings).unwrap();
  let stated = measured.claimed.frames.unwrap() as f64 / 44_100.0;
  assert!(stated > 1e10, "the file states {stated} s");
  // two bytes a sample frame
  let held = bytes as f64 / 2.0 / 44_100.0;
  assert!(
    measured.duration <= held,
    "{} s from {bytes} bytes",
    measured.duration
  );
  assert!(!measured.windows.is_empty());
  assert!(
    measured.windows.iter().all(|w| w.start < measured.duration),
    "every window is inside the length"
  );
  let reading = read(&measured);
  assert!(
    reading.limitations.contains(&Limitation::LengthPastBytes),
    "{reading:#?}"
  );
}
