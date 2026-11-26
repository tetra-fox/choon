//! how a file is measured: in full or at windows spread over it, with the grid's stretches
//! read either way, under the thresholds, the progress and the cancel check it is given, and
//! no further than the length its bytes hold

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use super::{a_17_khz_cut, flac, read, sampled, settings, state};
use crate::engine::{Error, Measured, Settings, measure, measure_bytes};
use crate::fixtures::{self, Audio, Bitrate, music, music_for, pink};
use crate::grid;
use crate::limitation::Limitation;
use crate::reading::{Reading, Verdict};
use crate::thresholds::MeasureThresholds;

#[test]
fn progress_goes_on_through_the_grid_scan() {
  let dir = tempfile::tempdir().unwrap();
  let path = flac(dir.path(), "a.flac", &music(1, 44_100, pink), 16);
  let progress = Arc::new(std::sync::atomic::AtomicU32::new(0));
  let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
  // the cancel check is asked between windows and before each job of the scan
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
  let mut seen = seen.lock().unwrap().clone();
  seen.push(progress.load(std::sync::atomic::Ordering::Relaxed));
  assert_eq!(seen.last(), Some(&1000));
  assert!(seen.is_sorted(), "{seen:?}");
  // a scan that reported only once it ended would move the progress most of the way at once
  let furthest = seen.windows(2).map(|pair| pair[1] - pair[0]).max();
  assert!(furthest.is_some_and(|step| step <= 100), "{seen:?}");
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
fn an_analysis_out_of_time_before_it_opens_reports_nothing() {
  let dir = tempfile::tempdir().unwrap();
  let path = flac(dir.path(), "a.flac", &music(1, 44_100, pink), 16);
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

/// the seconds of audio a reading says its file states and does not hold
fn missing(measured: &Measured) -> Option<f64> {
  read(measured)
    .limitations
    .into_iter()
    .find_map(|limitation| match limitation {
      Limitation::AudioMissing { seconds } => Some(seconds),
      _ => None,
    })
}

#[test]
fn a_flac_with_a_damaged_frame_says_how_much_audio_is_missing() {
  let whole = fixtures::flac(&music(1, 44_100, pink), 16, false);
  assert_eq!(
    missing(&measure_bytes(whole.clone(), &settings()).unwrap()),
    None
  );
  // one bit of the audio: the frame's checksum fails and its reader drops the frame unsaid
  let mut damaged = whole;
  let middle = damaged.len() / 2;
  damaged[middle] ^= 1;
  let measured = measure_bytes(damaged, &settings()).unwrap();
  let short = missing(&measured).expect("a frame is missing");
  let block = fixtures::FLAC_BLOCK as f64 / 44_100.0;
  assert!((short - block).abs() < 0.001, "{short}");
}

#[test]
fn an_mp3_whose_header_counts_more_frames_than_its_bytes_hold_says_so() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("a.mp3");
  fixtures::write_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, &path);
  let audio = std::fs::read(&path).unwrap();
  // a first frame of 417 bytes that holds a Xing header in place of audio, after the 4 bytes
  // of the frame's own header and the 32 of a stereo frame's side information: its marker,
  // the flag that a count of frames follows, and a count of 4,000,000
  let mut counted = audio[..4].to_vec();
  counted[2] &= !0b10;
  counted.resize(417, 0);
  counted[36..40].copy_from_slice(b"Xing");
  counted[40..44].copy_from_slice(&1u32.to_be_bytes());
  counted[44..48].copy_from_slice(&4_000_000u32.to_be_bytes());
  counted.extend_from_slice(&audio);
  let measured = measure_bytes(counted, &Settings::default()).unwrap();
  let stated = measured.claimed.frames.unwrap() as f64 / 44_100.0;
  assert!(stated > 100_000.0, "{stated}");
  assert!(
    (measured.duration - fixtures::SECONDS as f64).abs() < 0.2,
    "{}",
    measured.duration
  );
  let reading = read(&measured);
  assert_eq!(past_bytes(&reading), 1, "{:?}", reading.limitations);
}

#[test]
fn a_wave_file_that_states_no_size_of_its_samples_is_as_long_as_its_bytes() {
  // two seconds of 16-bit mono PCM at 8 kHz that state 0 bytes, then 0xFFFFFFFF
  let samples: Vec<u8> = (0..16_000i32)
    .flat_map(|n| ((n * 37 % 2000 - 1000) as i16).to_le_bytes())
    .collect();
  for stated in [0u32, u32::MAX] {
    let mut bytes = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0".to_vec();
    bytes.extend(8000u32.to_le_bytes());
    bytes.extend(16_000u32.to_le_bytes());
    bytes.extend(b"\x02\0\x10\0data");
    bytes.extend(stated.to_le_bytes());
    bytes.extend(&samples);
    let measured = measure_bytes(bytes, &Settings::default()).unwrap();
    assert_eq!(measured.claimed.frames, Some(16_000), "{stated}");
    assert_eq!(measured.duration, 2.0, "{stated}");
  }
}

#[cfg(unix)]
#[test]
fn a_fifo_no_writer_opens_holds_the_analysis_no_longer_than_it_may_run() {
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::time::Duration;

  let dir = tempfile::tempdir().unwrap();
  let fifo = dir.path().join("audio.fifo");
  let made = std::process::Command::new("mkfifo").arg(&fifo).status();
  assert!(made.is_ok_and(|status| status.success()), "mkfifo ran");
  let timed = Settings {
    deadline: Some(Instant::now() + Duration::from_millis(50)),
    ..Settings::default()
  };
  let out_of_time = measure(&fifo, &timed);
  assert!(
    matches!(out_of_time, Err(Error::OutOfTime)),
    "{out_of_time:?}"
  );
  let stop = Arc::new(AtomicBool::new(false));
  let flag = stop.clone();
  let canceled = Settings {
    canceled: Some(Arc::new(move || flag.load(Ordering::Relaxed))),
    ..Settings::default()
  };
  stop.store(true, Ordering::Relaxed);
  let stopped = measure(&fifo, &canceled);
  assert!(matches!(stopped, Err(Error::Canceled)), "{stopped:?}");
  // a writer and the analysis open a FIFO together, and what the writer gives is read. one
  // of its own, as the first has two threads left waiting on it
  let fifo = dir.path().join("written.fifo");
  let made = std::process::Command::new("mkfifo").arg(&fifo).status();
  assert!(made.is_ok_and(|status| status.success()), "mkfifo ran");
  let writer = std::thread::spawn({
    let fifo = fifo.clone();
    // FLAC is read without seeking
    move || {
      std::fs::write(
        fifo,
        fixtures::flac(&music_for(2, 1, 44_100, pink), 16, false),
      )
    }
  });
  let read = measure(&fifo, &settings());
  writer.join().unwrap().unwrap();
  assert!(read.is_ok(), "{read:?}");
}

#[test]
fn an_mp3_without_a_length_header_is_read_to_its_end() {
  let dir = tempfile::tempdir().unwrap();
  // 4 s at 320 kbps, then 30 s at 128 kbps: a reader that takes the length from the first
  // frames' bitrate and the file's size makes it 16 s
  let loud = dir.path().join("320.mp3");
  let rest = dir.path().join("128.mp3");
  fixtures::write_mp3(&music_for(4, 1, 44_100, pink), Bitrate::Kbps320, &loud);
  fixtures::write_mp3(&music_for(30, 2, 44_100, pink), Bitrate::Kbps128, &rest);
  let mut bytes = std::fs::read(&loud).unwrap();
  bytes.extend(std::fs::read(&rest).unwrap());
  let measured = measure_bytes(bytes, &settings()).unwrap();
  let stated = measured.claimed.frames.expect("a length is guessed") as f64 / 44_100.0;
  assert!(stated < 20.0, "{stated}");
  assert!(
    (measured.duration - 34.0).abs() < 0.2,
    "{}",
    measured.duration
  );
  assert_eq!(measured.windows.len(), 34);
  // a guessed length is none to fall short of
  assert_eq!(missing(&measured), None);
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
  assert_eq!(past_bytes(&reading), 1, "{reading:#?}");
}

/// how many times a reading says its file states more than its bytes hold
fn past_bytes(reading: &Reading) -> usize {
  reading
    .limitations
    .iter()
    .filter(|l| **l == Limitation::LengthPastBytes)
    .count()
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
  assert_eq!(past_bytes(&reading), 1, "{reading:#?}");
}

#[test]
fn a_dsf_file_cut_inside_a_block_states_its_whole_blocks() {
  let whole = fixtures::one_bit_tones(&[1000.0, 3000.0], 2).dsf();
  // a block pair is 8192 bytes, and a file cut anywhere in its last one holds the blocks before
  // it: 172 blocks of 4096 bytes a channel, 32768 one-bit samples each, a sixteenth of them PCM
  for cut in [1000, 5096, 8192] {
    let measured = measure_bytes(whole[..whole.len() - cut].to_vec(), &settings()).unwrap();
    assert_eq!(
      measured.claimed.frames,
      Some(172 * 32_768 / 16),
      "cut {cut}"
    );
    let reading = read(&measured);
    let stopped = |l: &Limitation| {
      matches!(l, Limitation::Decoder(why)
        if why.ends_with("dsd: the samples stop before the length the file states"))
    };
    assert!(
      reading.limitations.iter().any(stopped),
      "cut {cut}: {reading:#?}"
    );
  }
}

#[test]
fn a_sampled_file_of_long_packets_holds_the_same_windows_as_one_read_in_full() {
  // Monkey's Audio's frames at its highest level are long enough that seeking to windows this
  // close together decodes more than reading through to them
  let bytes = fixtures::ape(&music_for(16, 1, 44_100, pink), 16, true);
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("a.ape");
  std::fs::write(&path, &bytes).unwrap();
  let full = measure(&path, &settings()).unwrap();
  let sampled = Settings {
    coverage: sampled(5.0, 12),
    ..settings()
  };
  let sampled = measure(&path, &sampled).unwrap();
  assert!(sampled.sampled);
  assert_eq!(sampled.windows.len(), 12);
  for window in &sampled.windows {
    let same = full.windows.iter().find(|w| w.start == window.start);
    assert!(Some(window) == same, "the window at {} s", window.start);
  }
  assert_eq!(sampled.duration, full.duration);
  assert_eq!(missing(&sampled), None);
  assert!(full.grid.is_some());
  assert_eq!(of_stretches(&sampled), of_stretches(&full));
}
