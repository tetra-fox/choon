//! the analyzer's readings on generated audio: honest lossless audio, MP3 transcodes, an
//! upsample, padded bits, mixed sources and honest band-limited audio, which must never read
//! as a transcode or as mixed

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::decode::HeAac;
use crate::engine::{self, Coverage, Error, Measured, Settings, measure, measure_bytes};
use crate::fixtures::{self, Audio, Bitrate, LongWindow, lo_fi, music, music_for, pink, tone};
use crate::flags::Flag;
use crate::grid::{self, Codec, Evidence};
use crate::limitation::Limitation;
use crate::reading::{Reading, Verdict};
use crate::thresholds::{MeasureThresholds, ReadingThresholds};

/// a long file at `windows` windows, and one of up to `over_seconds` in full
fn sampled(over_seconds: f64, windows: usize) -> Coverage {
  Coverage::Sampled {
    over_seconds,
    windows: NonZeroUsize::new(windows).unwrap(),
  }
}

fn settings() -> Settings {
  Settings {
    coverage: sampled(900.0, 240),
    deadline: Some(Instant::now() + Duration::from_secs(600)),
    ..Settings::default()
  }
}

fn read(measured: &Measured) -> Reading {
  measured.read(&ReadingThresholds::default())
}

fn analyzed(path: &Path) -> Reading {
  explained(path).0
}

/// the reading of the file at `path`, with what it saw for an assertion to print
fn explained(path: &Path) -> (Reading, String) {
  let measured = measure(path, &settings()).unwrap();
  let reading = read(&measured);
  let why = engine::explain(&measured, &reading);
  (reading, why)
}

fn flac(dir: &Path, name: &str, audio: &Audio, bits: u32) -> PathBuf {
  let path = dir.join(name);
  fixtures::write_flac(audio, bits, false, &path);
  path
}

/// the reading must not call honest audio fake, nor its cutoff read as lossy
fn not_fake(reading: &Reading) {
  assert!(
    !matches!(
      reading.cutoff_verdict,
      Verdict::LikelyTranscode | Verdict::MixedSources
    ) && reading.verdict != Verdict::LikelyTranscode,
    "{reading:#?}"
  );
}

#[test]
fn lossless_music_reads_consistent_with_no_frame_rhythm_or_top_band_gaps() {
  let dir = tempfile::tempdir().unwrap();
  let reading = analyzed(&flac(dir.path(), "a.flac", &music(1, 44_100, pink), 16));
  assert_eq!(reading.verdict, Verdict::Consistent, "{reading:#?}");
  assert!(reading.confidence >= 0.7, "{reading:#?}");
  assert!(reading.flags.is_empty());
  assert_eq!(reading.encoder_guess, None);
  assert_eq!(reading.cutoff_hz, None);
  assert_eq!(reading.measures.bit_depth_effective, Some(16));
  assert!(
    reading.measures.frame_artifact_score.unwrap() < 0.05,
    "{reading:#?}"
  );
  assert!(
    reading.measures.top_band_holes.unwrap() < 0.01,
    "{reading:#?}"
  );
}

/// an MP3 at `bitrate` written out as a FLAC reads as a transcode of that bitrate
fn reads_as_a_transcode_of(bitrate: Bitrate, profile: &str) {
  let dir = tempfile::tempdir().unwrap();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), bitrate, dir.path());
  let reading = analyzed(&flac(dir.path(), "t.flac", &through, 16));
  assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
  assert!(reading.confidence >= 0.6, "{reading:#?}");
  let guess = reading.encoder_guess.as_ref().unwrap();
  assert_eq!(guess.profile, profile, "{reading:#?}");
  assert!(reading.cutoff_hz.is_some());
}

#[test]
fn a_128_kbps_transcode_reads_as_a_transcode_of_its_bitrate() {
  reads_as_a_transcode_of(Bitrate::Kbps128, "mp3 ~128 kbps");
}

#[test]
fn a_192_kbps_transcode_reads_as_a_transcode_of_its_bitrate() {
  reads_as_a_transcode_of(Bitrate::Kbps192, "mp3 ~192 kbps");
}

/// LAME 3.100 cuts 224 kbps where it cuts 256, at 19.53 kHz, so the two take one name
#[test]
fn a_224_kbps_transcode_reads_as_the_cut_it_shares_with_256_kbps() {
  reads_as_a_transcode_of(Bitrate::Kbps224, "mp3 ~224-256 kbps");
}

#[test]
fn a_256_kbps_transcode_reads_as_the_cut_it_shares_with_224_kbps() {
  reads_as_a_transcode_of(Bitrate::Kbps256, "mp3 ~224-256 kbps");
}

#[test]
fn a_320_kbps_transcode_reads_as_a_transcode_by_its_grid() {
  let dir = tempfile::tempdir().unwrap();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps320, dir.path());
  let reading = analyzed(&flac(dir.path(), "t.flac", &through, 16));
  assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
  assert_eq!(reading.grid.as_ref().map(|g| g.codec), Some(Codec::Mp3));
  // the cutoff alone cannot tell where the highest bitrates cut
  assert_eq!(
    reading.cutoff_verdict,
    Verdict::Inconclusive,
    "{reading:#?}"
  );
}

#[test]
fn an_mp3_decode_carries_the_mp3_grid_and_its_source_none() {
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 44_100, pink);
  let through = fixtures::through_mp3(&source, Bitrate::Kbps128, dir.path());
  for (name, audio, grid) in [
    ("decoded.flac", &through, Some(Codec::Mp3)),
    ("source.flac", &source, None),
  ] {
    let measured = measure(flac(dir.path(), name, audio, 16), &settings()).unwrap();
    let scan = measured.grid.as_ref().expect("a lossless file is scanned");
    let found = grid::found(scan, &ReadingThresholds::default());
    assert_eq!(found.as_ref().map(|f| f.codec), grid, "{name}: {scan:?}");
    if grid.is_some() {
      assert_eq!(scan.mp3.segments_agreeing, 4, "{scan:?}");
    } else {
      assert!(scan.aac.z < 5.0 && scan.mp3.z < 5.0, "{scan:?}");
    }
  }
}

#[test]
fn a_decode_converted_between_44_1_and_48_khz_carries_its_grid_at_the_rate_it_was_made_at() {
  let dir = tempfile::tempdir().unwrap();
  let thresholds = ReadingThresholds::default();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir.path());
  let coded = fixtures::through_transform(&music(2, 48_000, pink), LongWindow::KaiserBessel, 64);
  for (name, audio, codec, rate) in [
    (
      "mp3-to-48000.flac",
      through.resampled(48_000),
      Codec::Mp3,
      44_100,
    ),
    (
      "coded-to-44100.flac",
      coded.resampled(44_100),
      Codec::Aac,
      48_000,
    ),
  ] {
    let measured = measure(flac(dir.path(), name, &audio, 16), &settings()).unwrap();
    let scan = measured.grid.as_ref().expect("a lossless file is scanned");
    let other = scan.other_rate.as_ref().expect("the other rate is scanned");
    assert_eq!(other.rate, rate, "{name}");
    // nothing at the file's own rate
    let sine = scan.aac_sine.as_ref().expect("the sine window is scanned");
    assert!(
      scan.mp3.z < thresholds.mp3_grid_z
        && scan.aac.z < thresholds.aac_grid_z
        && sine.z < thresholds.aac_grid_z,
      "{name}: {scan:?}"
    );
    let own = match codec {
      Codec::Mp3 => &other.mp3,
      _ => &other.aac,
    };
    let reading = read(&measured);
    assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
    let found = reading.grid.expect("the grid at the other rate is one");
    assert_eq!(found.codec, codec, "{name}: {scan:?}");
    let evidence = Evidence::CountAt {
      rate,
      scan: own.clone(),
    };
    assert_eq!(found.evidence, evidence, "{name}");
    let brought = format!("Brought to {} kHz", f64::from(rate) / 1000.0);
    assert!(reading.summary.starts_with(&brought), "{}", reading.summary);
  }
  // honest audio converted the same way carries none
  let honest = music(1, 44_100, pink).resampled(48_000);
  let measured = measure(flac(dir.path(), "honest.flac", &honest, 16), &settings()).unwrap();
  let scan = measured.grid.as_ref().expect("a lossless file is scanned");
  assert_eq!(grid::found(scan, &thresholds), None, "{scan:?}");
  let other = scan.other_rate.as_ref().expect("the other rate is scanned");
  assert!(other.mp3.z < 5.0 && other.aac.z < 5.0, "{scan:?}");
  // and a rate that is neither is scanned at its own alone
  let high = flac(dir.path(), "at-96000.flac", &music(1, 96_000, pink), 24);
  let scan = measure(high, &settings()).unwrap().grid;
  assert_eq!(scan.expect("scanned for MP3 and AAC").other_rate, None);
}

#[test]
fn a_transform_decode_carries_the_aac_grid_under_the_window_it_was_coded_with() {
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 44_100, pink);
  let apart = |a: &[f32], b: &[f32]| -> f64 {
    let squares = |x: &mut dyn Iterator<Item = f64>| x.map(|v| v * v).sum::<f64>();
    let difference = squares(&mut a.iter().zip(b).map(|(a, b)| f64::from(a - b)));
    (difference / squares(&mut b.iter().map(|b| f64::from(*b)))).sqrt()
  };
  for (name, window) in [
    ("kaiser-bessel", LongWindow::KaiserBessel),
    ("sine", LongWindow::Sine),
  ] {
    let through = fixtures::through_transform(&source, window, 64);
    // the coder kept the audio where it should be: it differs by its rounding and its cut
    let off = apart(&through.channels[0], &source.channels[0]);
    assert!(off < 0.5, "{name}: {off}");
    let path = flac(dir.path(), &format!("{name}.flac"), &through, 16);
    let measured = measure(path, &settings()).unwrap();
    let scan = measured.grid.as_ref().expect("a lossless file is scanned");
    let sine = scan.aac_sine.as_ref().expect("the sine window is scanned");
    let (own, other) = match window {
      LongWindow::KaiserBessel => (&scan.aac, sine),
      LongWindow::Sine => (sine, &scan.aac),
    };
    assert_eq!(own.offset, 1024 - 64, "{name}: {scan:?}");
    assert_eq!(own.segments_agreeing, 4, "{name}: {scan:?}");
    assert!(own.z > 3.0 * other.z, "{name}: {scan:?}");
    let found = grid::found(scan, &ReadingThresholds::default());
    let found = found.unwrap_or_else(|| panic!("{name}: {scan:?}"));
    assert_eq!(found.codec, Codec::Aac, "{name}: {scan:?}");
    assert_eq!(found.evidence, Evidence::Count(own.clone()), "{name}");
    assert_eq!(read(&measured).verdict, Verdict::LikelyTranscode, "{name}");
  }
}

/// `source`, at 48 kHz, through Opus at `kbps` kbit/s and decoded again
fn through_opus(source: &Audio, kbps: u32, dir: &Path) -> Audio {
  let path = dir.join(format!("through-{kbps}.opus"));
  let stream = fixtures::opus(source, fixtures::OpusSettings::music(kbps));
  std::fs::write(&path, stream.ogg()).unwrap();
  fixtures::decoded(&path)
}

#[test]
fn an_opus_decode_carries_opus_frames_at_48_khz_at_44_1_khz_and_with_its_start_cut() {
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 48_000, pink);
  let thresholds = ReadingThresholds::default();
  for kbps in [64, 160] {
    let through = through_opus(&source, kbps, dir.path());
    let cut = Audio {
      rate: through.rate,
      channels: through
        .channels
        .iter()
        .map(|c| c[1000..].to_vec())
        .collect(),
    };
    // a cut of 1000 samples moves the frames of an uncut decode by as many
    let moved = (grid::OPUS_UNCUT_OFFSET + 960 * 2 - 1000) % 960;
    for (name, audio, offset, uncut) in [
      ("decoded.flac", &through, grid::OPUS_UNCUT_OFFSET, true),
      (
        "at-44100.flac",
        &through.resampled(44_100),
        grid::OPUS_UNCUT_OFFSET,
        true,
      ),
      ("cut.flac", &cut, moved, false),
    ] {
      let measured = measure(flac(dir.path(), name, audio, 16), &settings()).unwrap();
      let scan = measured.grid.as_ref().expect("a lossless file is scanned");
      let opus = scan
        .opus
        .as_ref()
        .expect("the rate is one Opus is looked for at");
      let said = format!("{kbps} kbit/s, {name}: {scan:?}");
      assert_eq!(opus.offset, offset, "{said}");
      assert!(opus.frames >= 4 * thresholds.opus_grid_frames, "{said}");
      assert_eq!(opus.frames, opus.segment_frames.iter().sum::<usize>());
      assert_eq!(
        opus.uncut_frames >= 4 * thresholds.opus_uncut_frames,
        uncut,
        "{said}"
      );
      let reading = read(&measured);
      assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
      assert_eq!(reading.grid.map(|found| found.codec), Some(Codec::Opus));
      assert!(reading.summary.contains("Opus"), "{}", reading.summary);
    }
  }
}

#[test]
fn an_opus_decode_at_another_level_carries_opus_frames_once_the_level_is_taken_off() {
  let dir = tempfile::tempdir().unwrap();
  let through = through_opus(&music(1, 48_000, pink), 128, dir.path());
  let thresholds = ReadingThresholds::default();
  // 7.3 dB quieter, and the channels 1 and 2.5 dB quieter
  for (name, gains) in [
    ("quieter.flac", [-7.3, -7.3]),
    ("balance.flac", [-1.0, -2.5]),
  ] {
    let mut changed = through.clone();
    for (channel, db) in changed.channels.iter_mut().zip(gains) {
      let gain = 10f32.powf(db / 20.0);
      for sample in channel {
        *sample *= gain;
      }
    }
    let measured = measure(flac(dir.path(), name, &changed, 16), &settings()).unwrap();
    let opus = measured.grid.as_ref().and_then(|scan| scan.opus.clone());
    let opus = opus.expect("the rate is one Opus is looked for at");
    let shifted = opus.shifted.clone().expect("a level is searched for");
    assert!(
      opus.frames < thresholds.opus_grid_frames && opus.uncut_frames < thresholds.opus_uncut_frames,
      "{name}: {opus:?}"
    );
    assert_eq!(shifted.offset, grid::OPUS_UNCUT_OFFSET, "{name}: {opus:?}");
    assert!(
      shifted.frames >= 4 * thresholds.opus_shifted_frames
        && shifted.uncut_frames >= 4 * thresholds.opus_shifted_uncut_frames,
      "{name}: {opus:?}"
    );
    let reading = read(&measured);
    assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
    let found = reading.grid.expect("the shifted count makes a grid");
    assert_eq!(found.codec, Codec::Opus);
    assert_eq!(found.evidence, Evidence::Shifted { shifted });
    assert!(
      reading
        .summary
        .contains("once one change of level is taken off"),
      "{}",
      reading.summary
    );
  }
}

#[test]
fn a_decode_of_opus_in_its_hybrid_mode_carries_hybrid_frames() {
  let dir = tempfile::tempdir().unwrap();
  let thresholds = ReadingThresholds::default();
  // voice at 16 kbit/s over the full band is coded as a voice coder under CELT
  let voice = fixtures::OpusSettings {
    voice: true,
    ..fixtures::OpusSettings::music(16)
  };
  let path = dir.path().join("voice.opus");
  std::fs::write(&path, fixtures::opus(&music(1, 48_000, pink), voice).ogg()).unwrap();
  let packets = measure(&path, &settings()).unwrap().opus;
  let most = packets.first().expect("an Opus file's packets are counted");
  assert_eq!(most.mode, crate::OpusMode::Hybrid, "{packets:?}");
  let through = fixtures::decoded(&path);
  for (name, audio) in [
    ("decoded.flac", through.clone()),
    ("at-44100.flac", through.resampled(44_100)),
  ] {
    let measured = measure(flac(dir.path(), name, &audio, 16), &settings()).unwrap();
    let opus = measured.grid.as_ref().and_then(|scan| scan.opus.clone());
    let opus = opus.expect("the rate is one Opus is looked for at");
    let hybrid = opus.hybrid.clone().expect("hybrid frames are counted");
    assert!(
      opus.frames < thresholds.opus_grid_frames && opus.uncut_frames < thresholds.opus_uncut_frames,
      "{name}: {opus:?}"
    );
    assert!(
      hybrid.uncut_steps >= 2 * thresholds.opus_hybrid_steps
        && hybrid.uncut_frames as f64
          >= 2.0 * thresholds.opus_hybrid_share * hybrid.uncut_tested as f64,
      "{name}: {opus:?}"
    );
    let reading = read(&measured);
    assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
    let found = reading.grid.expect("the hybrid count makes a grid");
    assert_eq!(found.codec, Codec::Opus);
    assert_eq!(found.evidence, Evidence::Hybrid { hybrid });
    assert!(
      reading.summary.contains("hybrid mode"),
      "{}",
      reading.summary
    );
  }
}

#[test]
fn audio_that_was_never_opus_carries_no_opus_frames() {
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 44_100, pink);
  let through = fixtures::through_mp3(&source, Bitrate::Kbps128, dir.path());
  for (name, audio) in [
    ("honest-44100.flac", &source),
    ("honest-48000.flac", &music(2, 48_000, pink)),
    ("mp3.flac", &through),
  ] {
    let measured = measure(flac(dir.path(), name, audio, 16), &settings()).unwrap();
    let opus = measured.grid.unwrap().opus.expect("the rate is one read");
    assert!(
      opus.frames <= 1 && opus.uncut_frames == 0,
      "{name}: {opus:?}"
    );
    let shifted = opus.shifted.as_ref().expect("a level is searched for");
    assert!(
      shifted.frames <= 1 && shifted.uncut_frames <= 1,
      "{name}: {opus:?}"
    );
    // two bands land on the lattice by chance in a few frames of a hundred
    let hybrid = opus.hybrid.as_ref().expect("hybrid frames are counted");
    assert!(
      (hybrid.uncut_frames as f64) < 0.05 * hybrid.uncut_tested as f64,
      "{name}: {opus:?}"
    );
  }
  let high = flac(dir.path(), "at-96000.flac", &music(1, 96_000, pink), 24);
  let measured = measure(high, &settings()).unwrap();
  assert_eq!(measured.grid.expect("scanned for MP3 and AAC").opus, None);
}

#[test]
fn an_opus_file_reads_by_its_cutoff_with_what_its_packets_state() {
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 48_000, pink);
  let path = dir.path().join("music.opus");
  let stream = fixtures::opus(&source, fixtures::OpusSettings::music(128));
  std::fs::write(&path, stream.ogg()).unwrap();
  let measured = measure(&path, &settings()).unwrap();
  assert_eq!(measured.claimed.codec, "opus");
  assert!((measured.duration - fixtures::SECONDS as f64).abs() < 1e-9);
  let kbps = measured.bitrate_kbps.expect("a lossy file has a bitrate");
  assert!((110..=140).contains(&kbps), "{kbps}");
  assert_eq!(measured.grid, None, "a lossy file is not scanned");
  let [most] = &measured.opus[..] else {
    panic!("one mode and bandwidth: {:?}", measured.opus)
  };
  assert_eq!(
    (most.mode, most.bandwidth),
    (crate::OpusMode::Celt, crate::OpusBandwidth::Fullband)
  );
  let reading = read(&measured);
  assert_eq!(reading.verdict, Verdict::Consistent, "{reading:#?}");
  assert!(
    reading
      .summary
      .ends_with("Its Opus packets are CELT fullband."),
    "{}",
    reading.summary
  );
  // voice at a low bitrate is coded with SILK, which the packets say
  let voice = fixtures::OpusSettings {
    bitrate: 16_000,
    voice: true,
    ..fixtures::OpusSettings::music(16)
  };
  let mono = Audio {
    rate: 48_000,
    channels: vec![source.channels[0].clone()],
  };
  std::fs::write(&path, fixtures::opus(&mono, voice).ogg()).unwrap();
  let measured = measure(&path, &settings()).unwrap();
  let most = measured
    .opus
    .first()
    .expect("an Opus file's packets are counted");
  assert!(most.mode.is_voice(), "{:?}", measured.opus);
  let reading = read(&measured);
  assert!(
    reading.summary.contains("voice mode"),
    "{}",
    reading.summary
  );
  assert_ne!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
}

#[test]
fn an_honest_mp3_is_judged_against_its_own_bitrate() {
  let dir = tempfile::tempdir().unwrap();
  let honest = dir.path().join("honest.mp3");
  fixtures::write_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, &honest);
  let reading = analyzed(&honest);
  assert_eq!(reading.verdict, Verdict::Consistent, "{reading:#?}");
}

/// content that stops at 16 kHz, where older encoders and 32 kHz sources stop too, as an MP3 at
/// `bitrate`, reads as `verdict`
fn a_16_khz_cut_as_an_mp3_reads(bitrate: Bitrate, verdict: Verdict) {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("cut16.mp3");
  fixtures::write_mp3(
    &music(1, 44_100, fixtures::cut_at(16_000.0)),
    bitrate,
    &path,
  );
  let (reading, why) = explained(&path);
  assert_eq!(reading.verdict, verdict, "{}: {why}", bitrate as u32);
}

#[test]
fn a_16_khz_cut_is_a_128_kbps_mp3s_own_cutoff() {
  a_16_khz_cut_as_an_mp3_reads(Bitrate::Kbps128, Verdict::Consistent);
}

#[test]
fn a_16_khz_cut_in_a_320_kbps_mp3_fits_either_explanation() {
  a_16_khz_cut_as_an_mp3_reads(Bitrate::Kbps320, Verdict::Inconclusive);
}

#[test]
fn a_cover_larger_than_the_audio_does_not_raise_an_mp3s_bitrate() {
  let dir = tempfile::tempdir().unwrap();
  let honest = dir.path().join("honest.mp3");
  fixtures::write_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, &honest);
  fixtures::add_cover(&honest, 2_000_000);
  let measured = measure(&honest, &settings()).unwrap();
  let kbps = measured.bitrate_kbps.unwrap();
  assert!((120..=136).contains(&kbps), "{kbps} kbps");
  let reading = read(&measured);
  assert_eq!(reading.verdict, Verdict::Consistent, "{reading:#?}");
}

#[test]
fn a_128_kbps_mp3_encoded_again_at_320_kbps_keeps_the_lower_cutoff() {
  let dir = tempfile::tempdir().unwrap();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir.path());
  let inflated = dir.path().join("inflated.mp3");
  fixtures::write_mp3(&through, Bitrate::Kbps320, &inflated);
  let reading = analyzed(&inflated);
  assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
}

#[test]
fn an_upsample_is_flagged_beside_the_verdict() {
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 44_100, pink);
  let reading = analyzed(&flac(dir.path(), "up.flac", &source.resampled(96_000), 24));
  assert_eq!(reading.flags, [Flag::Upsampled], "{reading:#?}");
  assert_eq!(reading.verdict, Verdict::Consistent, "{reading:#?}");
  assert!(reading.measures.upsample_ratio.unwrap() < 0.001);
}

#[test]
fn padded_bits_are_flagged_beside_the_verdict() {
  let dir = tempfile::tempdir().unwrap();
  let padded = dir.path().join("padded.flac");
  fixtures::write_flac(&music(1, 44_100, pink), 24, true, &padded);
  let reading = analyzed(&padded);
  assert_eq!(reading.flags, [Flag::BitPadded], "{reading:#?}");
  assert_eq!(reading.measures.bit_depth_effective, Some(16));
}

#[test]
fn the_peak_is_the_loudest_sample_of_either_channel() {
  let dir = tempfile::tempdir().unwrap();
  let mut audio = music(1, 44_100, pink);
  // the loudest sample is in the second channel
  audio.channels[0].iter_mut().for_each(|s| *s *= 0.5);
  let path = flac(dir.path(), "peak.flac", &audio, 16);
  let loudest = fixtures::decoded(&path)
    .channels
    .iter()
    .flatten()
    .fold(0f64, |peak, s| peak.max(f64::from(s.abs())));
  let reading = analyzed(&path);
  let peak = reading.measures.peak_dbfs.unwrap();
  assert!(
    (peak - 20.0 * loudest.log10()).abs() < 1e-6,
    "{peak} dBFS against {loudest}"
  );
}

#[test]
fn a_mono_source_copied_to_stereo_is_flagged_and_shows_in_the_measures() {
  let dir = tempfile::tempdir().unwrap();
  let mono = music(1, 44_100, pink).mono_to_stereo();
  let reading = analyzed(&flac(dir.path(), "m.flac", &mono, 16));
  assert!(reading.measures.stereo_correlation.unwrap() > 0.9999);
  assert_eq!(reading.measures.side_energy_db, Some(-120.0));
  assert!(reading.summary.contains("mono"), "{}", reading.summary);
  assert_eq!(reading.flags, [Flag::DualMono]);
}

#[test]
fn a_lossy_layer_under_a_full_band_bed_reads_as_mixed_sources_and_shows_its_grid() {
  let dir = tempfile::tempdir().unwrap();
  let stem = fixtures::through_mp3(&music(3, 44_100, pink), Bitrate::Kbps128, dir.path());
  // a full-band bed 23 dB under a stem from a 128 kbps MP3, all the way through
  let layered = stem.mixed(&music(4, 44_100, pink), 0.071);
  let reading = analyzed(&flac(dir.path(), "layered.flac", &layered, 16));
  assert_eq!(
    reading.cutoff_verdict,
    Verdict::MixedSources,
    "{reading:#?}"
  );
  assert!(reading.summary.contains("step"), "{}", reading.summary);
  assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
  assert_eq!(reading.grid.as_ref().map(|g| g.codec), Some(Codec::Mp3));
}

#[test]
fn a_lossy_part_before_lossless_audio_reads_as_mixed_sources_and_possibly_lossy() {
  let dir = tempfile::tempdir().unwrap();
  let stem = fixtures::through_mp3(&music(3, 44_100, pink), Bitrate::Kbps128, dir.path());
  // the first twelve seconds from the MP3, the rest lossless: one of the four stretches the
  // grid scan reads is in the MP3 part, too few to agree on a grid
  let intro = stem.spliced(&music(1, 44_100, pink), 12);
  let reading = analyzed(&flac(dir.path(), "intro.flac", &intro, 16));
  assert_eq!(
    reading.cutoff_verdict,
    Verdict::MixedSources,
    "{reading:#?}"
  );
  assert!(
    reading.summary.starts_with("65% of the time is full-band"),
    "{}",
    reading.summary
  );
  assert_eq!(reading.verdict, Verdict::PossiblyLossy, "{reading:#?}");
  assert_eq!(reading.grid, None);
}

/// a 320 kbps stem under a bed at `gain`: a step where the highest bitrates cut, which a
/// recording filtered at 20 kHz with something above it also shows. the stem's grid shows
/// through a bed quiet enough, as `grid`.
fn a_320_kbps_stem_under_a_bed_reads_as_that_cut(gain: f32, grid: Option<Codec>) {
  let dir = tempfile::tempdir().unwrap();
  let stem = fixtures::through_mp3(&music(3, 44_100, pink), Bitrate::Kbps320, dir.path());
  let layered = stem.mixed(&music(4, 44_100, pink), gain);
  let (reading, why) = explained(&flac(dir.path(), "layered320.flac", &layered, 16));
  assert_eq!(
    reading.cutoff_verdict,
    Verdict::Inconclusive,
    "{gain}: {why}"
  );
  assert_eq!(
    reading.grid.as_ref().map(|g| g.codec),
    grid,
    "{gain}: {why}"
  );
  let guess = reading.encoder_guess.as_ref().unwrap();
  assert_eq!(
    (guess.profile.as_str(), guess.confidence),
    ("mp3 ~320 kbps or aac", 0.4)
  );
  assert!(
    !reading.summary.contains("no encoder shelf"),
    "{}",
    reading.summary
  );
}

#[test]
fn a_320_kbps_stem_under_a_bed_23_db_down_reads_as_that_cut() {
  a_320_kbps_stem_under_a_bed_reads_as_that_cut(0.071, Some(Codec::Mp3));
}

#[test]
fn a_320_kbps_stem_under_a_bed_15_db_down_reads_as_that_cut() {
  a_320_kbps_stem_under_a_bed_reads_as_that_cut(0.18, None);
}

#[test]
fn honest_lo_fi_audio_is_not_read_as_fake() {
  let dir = tempfile::tempdir().unwrap();
  let reading = analyzed(&flac(dir.path(), "lofi.flac", &music(5, 44_100, lo_fi), 16));
  not_fake(&reading);
  assert!(reading.confidence <= 0.5, "{reading:#?}");
}

#[test]
fn a_pure_tone_is_inconclusive_not_fake() {
  let dir = tempfile::tempdir().unwrap();
  let reading = analyzed(&flac(
    dir.path(),
    "tone.flac",
    &tone(1000.0, fixtures::SECONDS),
    16,
  ));
  not_fake(&reading);
  assert_eq!(reading.verdict, Verdict::Inconclusive, "{reading:#?}");
}

/// a synth line at `hz` low-passed at 20 kHz: a line spectrum, with gaps between its harmonics,
/// which stops where the highest bitrates cut
fn a_synth_line_filtered_at_20_khz_reads_as_that_cut(hz: f64) {
  let dir = tempfile::tempdir().unwrap();
  let saw = fixtures::harmonics(hz, 20_000.0, 1.0);
  let reading = analyzed(&flac(dir.path(), "saw.flac", &saw, 16));
  assert_eq!(reading.verdict, Verdict::Inconclusive, "{hz}: {reading:#?}");
  assert!(reading.encoder_guess.is_some(), "{hz}");
}

#[test]
fn a_110_hz_synth_line_filtered_at_20_khz_reads_as_that_cut() {
  a_synth_line_filtered_at_20_khz_reads_as_that_cut(110.0);
}

#[test]
fn a_220_hz_synth_line_filtered_at_20_khz_reads_as_that_cut() {
  a_synth_line_filtered_at_20_khz_reads_as_that_cut(220.0);
}

#[test]
fn a_clipped_pulse_filtered_at_20_khz_reads_as_that_cut() {
  let dir = tempfile::tempdir().unwrap();
  // a pulse of equal harmonics every 100 Hz, which clips when written: its clipping puts lines
  // above the 20 kHz cut, as a recording filtered there with something above it
  let pulse = fixtures::harmonics(100.0, 20_000.0, 0.0);
  let reading = analyzed(&flac(dir.path(), "pulse.flac", &pulse, 16));
  assert!(
    reading.measures.clipped_samples_pct.unwrap() > 0.0,
    "{reading:#?}"
  );
  assert_eq!(reading.verdict, Verdict::Inconclusive, "{reading:#?}");
}

/// white noise at `dbfs`, a stand-in for tape hiss
fn hiss(dbfs: f64) -> Audio {
  let mut floor = music(9, 44_100, |_| 1.0);
  floor.set_level(dbfs);
  floor
}

/// the reading of `audio` written as a 16-bit FLAC, which must be `verdict`
fn reads(dir: &Path, name: &str, audio: &Audio, verdict: Verdict) -> Reading {
  let (reading, why) = explained(&flac(dir, "floor.flac", audio, 16));
  assert_eq!(reading.verdict, verdict, "{name}: {why}");
  reading
}

/// a recording filtered at 20 kHz with hiss `db` under full scale over it stops where the highest
/// bitrates cut, which is all the analyzer can say
fn hiss_over_a_20_khz_cut_neither_hides_nor_fakes_it(db: f64) {
  let dir = tempfile::tempdir().unwrap();
  let hissy = music(1, 44_100, fixtures::cut_at(20_000.0)).mixed(&hiss(db), 1.0);
  let reading = reads(
    dir.path(),
    &format!("hiss {db}"),
    &hissy,
    Verdict::Inconclusive,
  );
  assert!(reading.encoder_guess.is_some());
}

#[test]
fn hiss_55_db_down_over_a_20_khz_cut_neither_hides_nor_fakes_it() {
  hiss_over_a_20_khz_cut_neither_hides_nor_fakes_it(-55.0);
}

#[test]
fn hiss_60_db_down_over_a_20_khz_cut_neither_hides_nor_fakes_it() {
  hiss_over_a_20_khz_cut_neither_hides_nor_fakes_it(-60.0);
}

/// a recording filtered at 20 kHz at `level` dBFS, so quiet that 16-bit dither is its floor,
/// still reads as that cut
fn a_quiet_20_khz_cut_reads_as_that_cut(level: f64) {
  let dir = tempfile::tempdir().unwrap();
  let mut quiet = music(1, 44_100, fixtures::cut_at(20_000.0));
  quiet.set_level(level);
  reads(
    dir.path(),
    &format!("quiet {level}"),
    &quiet,
    Verdict::Inconclusive,
  );
}

#[test]
fn a_20_khz_cut_50_db_down_reads_as_that_cut() {
  a_quiet_20_khz_cut_reads_as_that_cut(-50.0);
}

#[test]
fn a_20_khz_cut_55_db_down_reads_as_that_cut() {
  a_quiet_20_khz_cut_reads_as_that_cut(-55.0);
}

#[test]
fn a_128_kbps_transcode_under_hiss_is_still_a_transcode() {
  let dir = tempfile::tempdir().unwrap();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir.path());
  let hissy = through.mixed(&hiss(-56.0), 1.0);
  reads(
    dir.path(),
    "transcode under hiss",
    &hissy,
    Verdict::LikelyTranscode,
  );
}

#[test]
fn a_128_kbps_stem_over_a_bed_too_quiet_to_count_is_still_a_transcode() {
  let dir = tempfile::tempdir().unwrap();
  // a full-band bed 30 dB down
  let stem = fixtures::through_mp3(&music(3, 44_100, pink), Bitrate::Kbps128, dir.path());
  let bedded = stem.mixed(&music(4, 44_100, pink), 0.0316);
  reads(
    dir.path(),
    "stem over a quiet bed",
    &bedded,
    Verdict::LikelyTranscode,
  );
}

#[test]
fn too_little_audio_is_inconclusive() {
  let dir = tempfile::tempdir().unwrap();
  let reading = analyzed(&flac(
    dir.path(),
    "short.flac",
    &music_for(12, 1, 44_100, pink),
    16,
  ));
  assert_eq!(reading.verdict, Verdict::Inconclusive, "{reading:#?}");
  assert!(reading.confidence <= 0.5);
  assert!(
    reading.summary.starts_with("Only 12.0 s"),
    "{}",
    reading.summary
  );
}

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
  let seen = seen.lock().unwrap();
  assert!(seen.is_sorted(), "{seen:?}");
  let jobs_ended = seen
    .windows(2)
    .filter(|pair| pair[1] - pair[0] >= 30)
    .count();
  assert_eq!(jobs_ended, grid::jobs(true, true) - 1, "{seen:?}");
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

/// content cut where an encoder cuts, which the cutoff alone reads as a transcode with the
/// thresholds the analyzer ships with
fn a_17_khz_cut(dir: &Path) -> PathBuf {
  flac(
    dir,
    "cut.flac",
    &music(1, 44_100, fixtures::cut_at(17_000.0)),
    16,
  )
}

#[test]
fn the_reading_uses_the_thresholds_it_is_given() {
  let dir = tempfile::tempdir().unwrap();
  let measured = measure(a_17_khz_cut(dir.path()), &settings()).unwrap();
  assert_eq!(read(&measured).cutoff_verdict, Verdict::LikelyTranscode);
  // no shelf is this sharp, so no window stops with an encoder's low-pass
  let blunt = ReadingThresholds {
    sharp_shelf_db: 1000.0,
    ..ReadingThresholds::default()
  };
  let reading = measured.read(&blunt);
  assert_ne!(
    reading.cutoff_verdict,
    Verdict::LikelyTranscode,
    "{reading:#?}"
  );
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
fn no_reading_is_surer_than_the_confidence_cap() {
  let dir = tempfile::tempdir().unwrap();
  let capped = ReadingThresholds {
    confidence_cap: 0.1,
    ..ReadingThresholds::default()
  };
  let short = measure_bytes(fixtures::TONE.to_vec(), &settings()).unwrap();
  let reading = short.read(&capped);
  assert_eq!(reading.verdict, Verdict::Inconclusive);
  assert_eq!(reading.confidence, 0.1);
  let honest = flac(dir.path(), "a.flac", &music(1, 44_100, pink), 16);
  let honest = measure(&honest, &settings()).unwrap();
  let reading = honest.read(&capped);
  assert_eq!(reading.verdict, Verdict::Consistent);
  assert_eq!(reading.confidence, 0.1);
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

#[test]
fn he_aac_is_read_at_its_output_rate_however_its_band_replication_is_signalled() {
  for (name, bytes, parametric_stereo) in [
    ("after the core", fixtures::HE_AAC, false),
    ("first", fixtures::HE_AAC_HIERARCHICAL, false),
    ("not in an MP4", fixtures::HE_AAC_IMPLICIT, false),
    ("not in ADTS", fixtures::HE_AAC_ADTS, false),
    ("with parametric stereo", fixtures::HE_AAC_V2, true),
  ] {
    let measured = measure_bytes(bytes.to_vec(), &settings()).unwrap();
    assert_eq!(measured.claimed.sample_rate, 44_100, "{name}");
    assert_eq!(measured.claimed.channels, 2, "{name}");
    assert!(!measured.claimed.lossless, "{name}");
    assert_eq!(
      measured.he_aac,
      Some(HeAac {
        core_rate: 22_050,
        parametric_stereo
      }),
      "{name}"
    );
    // three seconds, and the encoder's delay and padding, which no container here trims
    assert!(
      (3.0..3.3).contains(&measured.duration),
      "{name}: {}",
      measured.duration
    );
    // the core alone stops under 11.025 kHz: a higher cutoff is the rebuilt band's
    let reading = read(&measured);
    let cutoff = reading
      .cutoff_hz
      .unwrap_or_else(|| panic!("{name}: {reading:#?}"));
    assert!((15_000.0..17_500.0).contains(&cutoff), "{name}: {cutoff}");
    assert_eq!(reading.encoder_guess, None, "{name}");
    assert!(
      reading
        .limitations
        .contains(&Limitation::RebuiltBand { hz: 11_025.0 }),
      "{name}: {reading:#?}"
    );
    let profile = if parametric_stereo {
      "HE-AAC v2"
    } else {
      "HE-AAC"
    };
    assert!(
      reading
        .summary
        .ends_with(&format!("The file is {profile}.")),
      "{name}: {}",
      reading.summary
    );
  }
}

#[test]
fn a_wave_header_that_states_thousands_of_channels_is_refused_without_a_panic() {
  let mut wave = Vec::new();
  wave.extend_from_slice(b"RIFF");
  wave.extend_from_slice(&1036u32.to_le_bytes());
  wave.extend_from_slice(b"WAVEfmt ");
  wave.extend_from_slice(&16u32.to_le_bytes());
  // PCM, 65535 channels of 16 bits at 44.1 kHz: the channels times the bytes a sample do not
  // fit the 16 bits the reader multiplies them in
  wave.extend_from_slice(&1u16.to_le_bytes());
  wave.extend_from_slice(&u16::MAX.to_le_bytes());
  wave.extend_from_slice(&44_100u32.to_le_bytes());
  wave.extend_from_slice(&88_200u32.to_le_bytes());
  wave.extend_from_slice(&2u16.to_le_bytes());
  wave.extend_from_slice(&16u16.to_le_bytes());
  wave.extend_from_slice(b"data");
  wave.extend_from_slice(&1000u32.to_le_bytes());
  wave.extend_from_slice(&[0; 1000]);
  assert!(matches!(
    measure_bytes(wave, &settings()),
    Err(Error::NotAudio(_) | Error::Unsupported(_))
  ));
}

proptest::proptest! {
  #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

  /// a WavPack or Monkey's Audio file with bytes overwritten and its end cut off is read
  /// without a panic
  #[test]
  fn a_damaged_wavpack_or_ape_file_is_read_without_panicking(
    damage in proptest::collection::vec((0.0f64..1.0, proptest::prelude::any::<u8>()), 0..64),
    kept in 0.2f64..1.0,
    file in 0usize..3,
  ) {
    let mut input = [fixtures::WAVPACK, fixtures::APE_NORMAL, fixtures::APE_INSANE][file].to_vec();
    let length = input.len();
    for (at, byte) in damage {
      input[(at * length as f64) as usize] = byte;
    }
    input.truncate((kept * length as f64) as usize);
    if let Ok(measured) = measure_bytes(input, &settings()) {
      read(&measured);
    }
  }

  /// a DSD file with bytes overwritten and its end cut off is read without a panic
  #[test]
  fn a_damaged_dsd_file_is_read_without_panicking(
    damage in proptest::collection::vec((0.0f64..0.01, proptest::prelude::any::<u8>()), 0..64),
    kept in 0.0f64..1.0,
    dsf in proptest::prelude::any::<bool>(),
  ) {
    static TONES: std::sync::LazyLock<fixtures::OneBit> =
      std::sync::LazyLock::new(|| fixtures::one_bit_tones(&[1000.0, 3000.0], 1));
    // the damage is put where the headers are
    let mut input = if dsf { TONES.dsf() } else { TONES.dff() };
    let length = input.len();
    for (at, byte) in damage {
      input[(at * length as f64) as usize] = byte;
    }
    input.truncate((kept * length as f64) as usize);
    if let Ok(measured) = measure_bytes(input, &settings()) {
      read(&measured);
    }
  }

  /// an HE-AAC file with bytes overwritten and its end cut off is read without a panic. ADTS
  /// and MP4 have no checksums, so the damage reaches the AAC decoder
  #[test]
  fn a_damaged_he_aac_file_is_read_without_panicking(
    damage in proptest::collection::vec((0.0f64..1.0, proptest::prelude::any::<u8>()), 0..64),
    kept in 0.2f64..1.0,
    file in 0usize..3,
  ) {
    let mut input = [fixtures::HE_AAC, fixtures::HE_AAC_ADTS, fixtures::HE_AAC_V2][file].to_vec();
    let length = input.len();
    for (at, byte) in damage {
      input[(at * length as f64) as usize] = byte;
    }
    input.truncate((kept * length as f64) as usize);
    if let Ok(measured) = measure_bytes(input, &settings()) {
      read(&measured);
    }
  }

  /// arbitrary bytes, and a real file's header followed by arbitrary bytes, are read without a
  /// panic, as a reading of some kind
  #[test]
  fn any_bytes_are_read_without_panicking(
    bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4096),
    header in 0usize..4,
  ) {
    let headers: [&[u8]; 4] = [
      b"",
      b"fLaC\x00\x00\x00\x22\x10\x00\x10\x00",
      b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x02\x00\x44\xac\x00\x00",
      b"ID3\x04\x00\x00\x00\x00\x00\x00\xff\xfb\x90\x64",
    ];
    let mut input = headers[header].to_vec();
    input.extend_from_slice(&bytes);
    if let Ok(measured) = measure_bytes(input, &settings()) {
      read(&measured);
    }
  }

  /// an Opus file with bytes overwritten and its end cut off is read without a panic. WebM has
  /// no checksums, so the damage reaches the Opus decoder
  #[test]
  fn a_damaged_opus_file_is_read_without_panicking(
    damage in proptest::collection::vec((0.0f64..1.0, proptest::prelude::any::<u8>()), 0..64),
    kept in 0.2f64..1.0,
    ogg in proptest::prelude::any::<bool>(),
  ) {
    static STREAM: std::sync::LazyLock<fixtures::OpusStream> = std::sync::LazyLock::new(|| {
      let audio = music_for(3, 1, 48_000, pink);
      fixtures::opus(&audio, fixtures::OpusSettings::music(64))
    });
    let mut input = if ogg { STREAM.ogg() } else { STREAM.webm() };
    let length = input.len();
    for (at, byte) in damage {
      input[(at * length as f64) as usize] = byte;
    }
    input.truncate((kept * length as f64) as usize);
    if let Ok(measured) = measure_bytes(input, &settings()) {
      read(&measured);
    }
  }
}

/// a cut at `hz`, where the highest bitrates cut, reads as that cut at the sample `rate`
fn a_cut_where_the_highest_bitrates_cut_reads_as_that_cut(rate: u32, hz: f64) {
  let dir = tempfile::tempdir().unwrap();
  let audio = music(1, rate, fixtures::cut_at(hz));
  let reading = analyzed(&flac(dir.path(), "cut.flac", &audio, 16));
  assert_eq!(
    reading.verdict,
    Verdict::Inconclusive,
    "{rate} {hz}: {reading:#?}"
  );
  assert!(reading.encoder_guess.is_some(), "{rate} {hz}");
}

#[test]
fn a_20_khz_cut_at_44_1_khz_reads_as_that_cut() {
  a_cut_where_the_highest_bitrates_cut_reads_as_that_cut(44_100, 20_000.0);
}

#[test]
fn a_20_5_khz_cut_at_44_1_khz_reads_as_that_cut() {
  a_cut_where_the_highest_bitrates_cut_reads_as_that_cut(44_100, 20_500.0);
}

#[test]
fn a_20_khz_cut_at_48_khz_reads_as_that_cut() {
  a_cut_where_the_highest_bitrates_cut_reads_as_that_cut(48_000, 20_000.0);
}

#[test]
fn a_20_5_khz_cut_at_48_khz_reads_as_that_cut() {
  a_cut_where_the_highest_bitrates_cut_reads_as_that_cut(48_000, 20_500.0);
}

/// a cut where a lower bitrate cuts reads as that encoder's cutoff at the sample `rate`, and
/// without a codec grid as possibly lossy
fn a_17_khz_cut_reads_as_an_encoders_cutoff(rate: u32) {
  let dir = tempfile::tempdir().unwrap();
  let audio = music(1, rate, fixtures::cut_at(17_000.0));
  let reading = analyzed(&flac(dir.path(), "cut.flac", &audio, 16));
  assert_eq!(
    reading.cutoff_verdict,
    Verdict::LikelyTranscode,
    "{rate}: {reading:#?}"
  );
  assert_eq!(
    reading.verdict,
    Verdict::PossiblyLossy,
    "{rate}: {reading:#?}"
  );
}

#[test]
fn a_17_khz_cut_at_44_1_khz_reads_as_an_encoders_cutoff() {
  a_17_khz_cut_reads_as_an_encoders_cutoff(44_100);
}

#[test]
fn a_17_khz_cut_at_48_khz_reads_as_an_encoders_cutoff() {
  a_17_khz_cut_reads_as_an_encoders_cutoff(48_000);
}

/// writes a FLAC of `seconds` of music at `path` that states it holds `frames` sample frames
fn stating(path: &Path, seconds: usize, frames: u64) {
  fixtures::write_flac(&music_for(seconds, 1, 44_100, pink), 16, false, path);
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
  let frames: u64 = 2000 * 44_100;
  let mut bytes = std::fs::read(&path).unwrap();
  bytes[21] = (bytes[21] & 0xf0) | ((frames >> 32) as u8 & 0x0f);
  bytes[22..26].copy_from_slice(&(frames as u32).to_be_bytes());
  std::fs::write(&path, &bytes).unwrap();
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
fn a_frame_rhythm_is_measured() {
  let dir = tempfile::tempdir().unwrap();
  // a burst at every 1152-sample frame edge
  let rhythm = music(1, 44_100, pink).with_frame_rhythm(1152);
  let reading = analyzed(&flac(dir.path(), "rhythm.flac", &rhythm, 16));
  assert!(
    reading.measures.frame_artifact_score.unwrap() >= 0.2,
    "{reading:#?}"
  );
}

#[test]
fn gaps_in_the_top_band_are_measured() {
  let dir = tempfile::tempdir().unwrap();
  // gaps 40 Hz wide every 80 Hz in 16-20 kHz
  let combed = music(1, 44_100, fixtures::combed);
  let reading = analyzed(&flac(dir.path(), "combed.flac", &combed, 16));
  assert!(
    reading.measures.top_band_holes.unwrap() >= 0.1,
    "{reading:#?}"
  );
}

#[test]
fn hi_res_with_a_natural_roll_off_is_not_flagged_as_upsampled() {
  let dir = tempfile::tempdir().unwrap();
  // falling 24 dB an octave from 8 kHz: almost no energy above 24 kHz, and no cliff anywhere
  let audio = music(1, 96_000, |hz| pink(hz) / (1.0 + (hz / 8_000.0).powi(4)));
  let reading = analyzed(&flac(dir.path(), "hires.flac", &audio, 24));
  assert!(
    reading.measures.upsample_ratio.unwrap() < 0.001,
    "{reading:#?}"
  );
  assert!(reading.flags.is_empty(), "{reading:#?}");
  not_fake(&reading);
}

/// a cutoff at a lower sample rate's band edge, where an MP3 at `profile` cuts too, reads as
/// either explanation and claims neither
fn a_cutoff_at_a_lower_rates_band_edge_is_not_read_as_a_transcode(
  name: &str,
  audio: &Audio,
  profile: &str,
  rate: &str,
) {
  let dir = tempfile::tempdir().unwrap();
  let (reading, why) = explained(&flac(dir.path(), "up.flac", audio, 24));
  assert_eq!(reading.verdict, Verdict::Inconclusive, "{name}: {why}");
  assert!(reading.flags.is_empty(), "{name}: {reading:#?}");
  assert_eq!(
    reading.encoder_guess.as_ref().unwrap().profile,
    profile,
    "{name}"
  );
  assert!(
    reading.summary.contains(rate) && reading.summary.contains(profile),
    "{name}: {}",
    reading.summary
  );
}

#[test]
fn audio_made_at_32_khz_and_upsampled_to_96_khz_is_not_read_as_a_transcode() {
  // it stops at 16 kHz, where older 128 kbps MP3 encoders cut too
  a_cutoff_at_a_lower_rates_band_edge_is_not_read_as_a_transcode(
    "32 to 96",
    &music(1, 32_000, pink).resampled(96_000),
    "mp3 ~128 kbps",
    "32 kHz",
  );
}

#[test]
fn audio_made_at_32_khz_and_upsampled_to_44_1_khz_is_not_read_as_a_transcode() {
  a_cutoff_at_a_lower_rates_band_edge_is_not_read_as_a_transcode(
    "32 to 44.1",
    &music(1, 32_000, pink).resampled(44_100),
    "mp3 ~128 kbps",
    "32 kHz",
  );
}

#[test]
fn audio_made_at_22_05_khz_is_not_read_as_a_transcode() {
  // it stops at 11.025 kHz, where a 64 kbps MP3 cuts
  a_cutoff_at_a_lower_rates_band_edge_is_not_read_as_a_transcode(
    "22.05 to 44.1",
    &music(1, 22_050, pink).resampled(44_100),
    "mp3 ~64 kbps",
    "22.05 kHz",
  );
}

#[test]
fn a_cut_at_16_khz_is_not_read_as_a_transcode() {
  // the old encoders' own cut, which a 32 kHz source shows too
  a_cutoff_at_a_lower_rates_band_edge_is_not_read_as_a_transcode(
    "cut at 16 kHz",
    &music(1, 44_100, fixtures::cut_at(16_000.0)),
    "mp3 ~128 kbps",
    "32 kHz",
  );
}

#[test]
fn quiet_tails_of_honest_dynamic_music_read_consistent() {
  let dir = tempfile::tempdir().unwrap();
  let honest = fixtures::classical(90);
  let reading = analyzed(&flac(dir.path(), "classical.flac", &honest, 16));
  assert_eq!(reading.verdict, Verdict::Consistent, "{reading:#?}");
}

#[test]
fn quiet_tails_of_transcoded_dynamic_music_do_not_reach_the_quantization_noise() {
  let dir = tempfile::tempdir().unwrap();
  // its quiet tails would otherwise reach the 16-bit noise and read as full-band windows
  let through = fixtures::through_mp3(&fixtures::classical(90), Bitrate::Kbps192, dir.path());
  let reading = analyzed(&flac(dir.path(), "classical192.flac", &through, 16));
  assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
}

#[test]
fn a_cliff_at_an_old_equipment_rate_reads_as_its_encoder_and_says_so() {
  let dir = tempfile::tempdir().unwrap();
  // made at 37.8 kHz, as PlayStation and CD-i audio was: its cliff at 18.9 kHz is where a
  // 192 kbps MP3 cuts, and the reading cannot tell them apart
  let audio = music(1, 37_800, pink).resampled(44_100);
  let (reading, why) = explained(&flac(dir.path(), "xa.flac", &audio, 16));
  assert_eq!(reading.cutoff_verdict, Verdict::LikelyTranscode, "{why}");
  assert_eq!(reading.verdict, Verdict::PossiblyLossy, "{why}");
  assert!(
    reading
      .limitations
      .iter()
      .any(|l| matches!(l, Limitation::OtherRate { hz, .. } if *hz == 18_900.0)),
    "{reading:#?}"
  );
}

#[test]
fn a_reading_needs_thirty_whole_seconds_of_audio() {
  let dir = tempfile::tempdir().unwrap();
  let mut audio = music_for(30, 1, 44_100, pink);
  for channel in &mut audio.channels {
    channel.truncate(44_100 * 299 / 10);
  }
  let reading = analyzed(&flac(dir.path(), "short.flac", &audio, 16));
  assert_eq!(reading.verdict, Verdict::Inconclusive, "{reading:#?}");
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
