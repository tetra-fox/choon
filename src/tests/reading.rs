//! the readings of generated audio: honest lossless audio, MP3 transcodes, an upsample, padded
//! bits, mixed sources and honest band-limited audio, which must never read as a transcode or
//! as mixed, and lossy files read against their own bitrate

use std::path::Path;

use super::{a_17_khz_cut, analyzed, explained, flac, read, settings};
use crate::decode::HeAac;
use crate::engine::{Settings, measure, measure_bytes};
use crate::fixtures::{self, Audio, Bitrate, lo_fi, music, music_for, pink, tone};
use crate::flags::Flag;
use crate::grid::{Codec, Evidence};
use crate::limitation::Limitation;
use crate::reading::{Reading, Verdict};
use crate::thresholds::{MeasureThresholds, ReadingThresholds};

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
  assert_eq!(guess.profile.to_string(), profile, "{reading:#?}");
  assert!(reading.cutoff_hz.is_some());
}

#[test]
fn a_128_kbps_transcode_reads_as_a_transcode_of_its_bitrate() {
  reads_as_a_transcode_of(Bitrate::Kbps128, "MP3 ~128 kbps");
}

#[test]
fn a_192_kbps_transcode_reads_as_a_transcode_of_its_bitrate() {
  reads_as_a_transcode_of(Bitrate::Kbps192, "MP3 ~192 kbps");
}

/// LAME 3.100 cuts 224 kbps where it cuts 256, at 19.53 kHz, so the two take one name
#[test]
fn a_224_kbps_transcode_reads_as_the_cut_it_shares_with_256_kbps() {
  reads_as_a_transcode_of(Bitrate::Kbps224, "MP3 ~224-256 kbps");
}

#[test]
fn a_256_kbps_transcode_reads_as_the_cut_it_shares_with_224_kbps() {
  reads_as_a_transcode_of(Bitrate::Kbps256, "MP3 ~224-256 kbps");
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
fn a_file_decoded_from_mp3_in_half_reads_as_mixed_sources_by_its_grid() {
  let dir = tempfile::tempdir().unwrap();
  let stem = fixtures::through_mp3(&music(3, 44_100, pink), Bitrate::Kbps320, dir.path());
  // the first seventeen seconds from the MP3: two of the four stretches the grid scan reads
  // are in the MP3 part, too few to agree on a grid and each far over the grid's threshold
  let half = stem.spliced(&music(1, 44_100, pink), 17);
  let reading = analyzed(&flac(dir.path(), "half.flac", &half, 16));
  assert_eq!(reading.verdict, Verdict::MixedSources, "{reading:#?}");
  let grid = reading.grid.as_ref().expect("a grid in part");
  assert_eq!(grid.codec, Codec::Mp3);
  assert!(
    matches!(grid.evidence, Evidence::Part { segments: 2, .. }),
    "{grid:?}"
  );
  assert!(
    reading.summary.contains("in 2 of the 4 stretches"),
    "{}",
    reading.summary
  );
}

#[test]
fn a_lossy_part_before_lossless_audio_reads_as_mixed_sources_by_one_stretch() {
  let dir = tempfile::tempdir().unwrap();
  let stem = fixtures::through_mp3(&music(3, 44_100, pink), Bitrate::Kbps128, dir.path());
  // the first twelve seconds from the MP3, the rest lossless: one of the four stretches the
  // grid scan reads is in the MP3 part, far out at the offset an uncut decode has
  let intro = stem.spliced(&music(1, 44_100, pink), 12);
  let reading = analyzed(&flac(dir.path(), "intro.flac", &intro, 16));
  assert_eq!(reading.verdict, Verdict::MixedSources, "{reading:#?}");
  let grid = reading.grid.as_ref().expect("a grid in part");
  assert_eq!(grid.codec, Codec::Mp3);
  assert!(
    matches!(grid.evidence, Evidence::Part { segments: 1, .. }),
    "{grid:?}"
  );
  assert!(
    reading.summary.contains("in 1 of the 4 stretches"),
    "{}",
    reading.summary
  );
  assert!(
    reading.summary.contains("65% of the time is full-band"),
    "{}",
    reading.summary
  );
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
    (guess.profile.to_string().as_str(), guess.confidence),
    ("MP3 ~320 kbps or AAC", 0.4)
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
fn a_320_kbps_stem_under_a_bed_12_db_down_reads_as_that_cut() {
  a_320_kbps_stem_under_a_bed_reads_as_that_cut(0.25, None);
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
fn too_little_audio_cut_where_an_encoder_and_a_lower_rate_both_stop_is_inconclusive() {
  let dir = tempfile::tempdir().unwrap();
  // 16 kHz is the top of a 32 kHz recording and a 128 kbps MP3's cutoff
  let audio = music_for(5, 1, 44_100, fixtures::cut_at(16_000.0));
  let (reading, why) = explained(&flac(dir.path(), "short.flac", &audio, 16));
  assert_eq!(reading.verdict, Verdict::Inconclusive, "{why}");
  assert!(reading.confidence <= 0.2, "{why}");
  assert_eq!(reading.encoder_guess, None, "{why}");
  assert!(reading.summary.starts_with("Only 4.9 s"), "{why}");
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
      .measures
      .cutoff_median_hz
      .unwrap_or_else(|| panic!("{name}: {reading:#?}"));
    assert!((15_000..17_500).contains(&cutoff), "{name}: {cutoff}");
    // three seconds are too few for a reading, whatever edge the band stops at
    assert!(
      reading.summary.starts_with("Only 3."),
      "{name}: {reading:#?}"
    );
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

#[test]
fn a_17_khz_cut_whose_grid_was_not_scanned_is_possibly_lossy_too() {
  let dir = tempfile::tempdir().unwrap();
  let audio = music_for(60, 1, 44_100, fixtures::cut_at(17_000.0));
  let path = flac(dir.path(), "cut.flac", &audio, 16);
  // cut short before the last of the scan's stretches, with enough left for a reading
  let bytes = std::fs::read(&path).unwrap();
  std::fs::write(&path, &bytes[..bytes.len() * 6 / 10]).unwrap();
  let (reading, why) = explained(&path);
  assert!(
    reading
      .limitations
      .contains(&Limitation::GridScanIncomplete),
    "{why}"
  );
  assert_eq!(reading.cutoff_verdict, Verdict::LikelyTranscode, "{why}");
  assert_eq!(reading.verdict, Verdict::PossiblyLossy, "{why}");
  assert!(reading.summary.contains("were not scanned"), "{why}");
}

#[test]
fn a_cut_without_a_grid_names_only_the_grids_looked_for() {
  let dir = tempfile::tempdir().unwrap();
  let path = a_17_khz_cut(dir.path());
  let all = analyzed(&path);
  assert_eq!(all.verdict, Verdict::PossiblyLossy, "{all:#?}");
  assert!(
    all
      .summary
      .contains("No MP3, MP2, AAC, Vorbis, WMA, AC-3 or Opus frame grid shows"),
    "{}",
    all.summary
  );
  let no_wma = Settings {
    thresholds: MeasureThresholds {
      scan_wma: false,
      ..MeasureThresholds::default()
    },
    ..settings()
  };
  let reading = read(&measure(&path, &no_wma).unwrap());
  assert_eq!(reading.verdict, Verdict::PossiblyLossy, "{reading:#?}");
  assert!(
    reading
      .summary
      .contains("No MP3, MP2, AAC, Vorbis, AC-3 or Opus frame grid shows"),
    "{}",
    reading.summary
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
    reading.encoder_guess.as_ref().unwrap().profile.to_string(),
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
    "MP3 ~128 kbps",
    "32 kHz",
  );
}

#[test]
fn audio_made_at_32_khz_and_upsampled_to_44_1_khz_is_not_read_as_a_transcode() {
  a_cutoff_at_a_lower_rates_band_edge_is_not_read_as_a_transcode(
    "32 to 44.1",
    &music(1, 32_000, pink).resampled(44_100),
    "MP3 ~128 kbps",
    "32 kHz",
  );
}

#[test]
fn audio_made_at_22_05_khz_is_not_read_as_a_transcode() {
  // it stops at 11.025 kHz, where a 64 kbps MP3 cuts
  a_cutoff_at_a_lower_rates_band_edge_is_not_read_as_a_transcode(
    "22.05 to 44.1",
    &music(1, 22_050, pink).resampled(44_100),
    "MP3 ~64 kbps",
    "22.05 kHz",
  );
}

#[test]
fn a_cut_at_16_khz_is_not_read_as_a_transcode() {
  // the old encoders' own cut, which a 32 kHz source shows too
  a_cutoff_at_a_lower_rates_band_edge_is_not_read_as_a_transcode(
    "cut at 16 kHz",
    &music(1, 44_100, fixtures::cut_at(16_000.0)),
    "MP3 ~128 kbps",
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
fn a_gentle_roll_off_at_an_old_equipment_rate_is_not_called_its_cliff() {
  let dir = tempfile::tempdir().unwrap();
  // falling 24 dB an octave from 5.3 kHz, so it fades out near 15 kHz, the top of 30 kHz
  let audio = music(1, 44_100, |hz| pink(hz) / (1.0 + (hz / 5_300.0).powi(4)));
  let (reading, why) = explained(&flac(dir.path(), "soft.flac", &audio, 16));
  assert!(reading.cutoff_hz.is_some(), "{why}");
  assert!(
    !reading
      .limitations
      .iter()
      .any(|l| matches!(l, Limitation::OtherRate { .. })),
    "{why}"
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
fn digital_silence_holds_zero_seconds_of_audio_not_minus_zero() {
  let silence = Audio {
    rate: 44_100,
    channels: vec![vec![0.0; 2 * 44_100]; 2],
  };
  let measured = measure_bytes(fixtures::flac(&silence, 16, false), &settings()).unwrap();
  let reading = read(&measured);
  assert!(
    reading.limitations.iter().any(|limitation| limitation
      .to_string()
      .starts_with("Only 0.0 s of the audio")),
    "{:?}",
    reading.limitations
  );
}

#[test]
fn quiet_audio_under_its_quantization_noise_has_no_cutoff() {
  // 8-bit samples at -65 dBFS: louder than silence, but nowhere over the quantization noise and
  // the margin above it
  let mut audio = music_for(40, 1, 44_100, pink);
  audio.set_level(-65.0);
  let measured = measure_bytes(fixtures::flac(&audio, 8, false), &settings()).unwrap();
  assert!(!measured.windows.is_empty());
  assert!(
    measured.windows.iter().all(|w| w.cutoff_hz.is_none()),
    "{:?}",
    measured
      .windows
      .iter()
      .map(|w| w.cutoff_hz)
      .collect::<Vec<_>>()
  );
  let reading = read(&measured);
  assert_eq!(reading.measures.cutoff_median_hz, None, "{reading:#?}");
  assert!(
    !reading.limitations.contains(&Limitation::EarlyRollOff),
    "{reading:#?}"
  );
}
