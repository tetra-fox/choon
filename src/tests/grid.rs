//! the grid scan on generated audio: decodes of MP3, of a transform coder with AAC's long
//! blocks and of Opus carry their codec's grid, and audio that was never coded carries none

use std::path::Path;

use super::{flac, read, settings, state};
use crate::engine::{measure, measure_bytes};
use crate::fixtures::{self, Audio, Bitrate, LongWindow, music, pink};
use crate::grid::{self, Codec, Evidence};
use crate::limitation::Limitation;
use crate::reading::Verdict;
use crate::thresholds::{ReadingThresholds, StepShare};

#[test]
fn steps_name_a_lame_decode_alike_whole_and_with_its_start_cut() {
  let dir = tempfile::tempdir().unwrap();
  let thresholds = ReadingThresholds::default();
  // the generated noise moves fewer of its middle bands' steps far than music does, so
  // LAME's rule is read with that bound lowered to where the noise at 192 kbit/s sits
  let mut lowered = thresholds.clone();
  for rule in &mut lowered.mp3_family_steps.rules {
    for bound in &mut rule.bounds {
      if rule.family == grid::Family::Lame && bound.share == StepShare::FarIn(1) {
        bound.min = Some(0.15);
      }
    }
  }
  for bitrate in [Bitrate::Kbps128, Bitrate::Kbps192] {
    // the fixture's stream carries no tag, so its decode keeps LAME's delay and the grid
    // sits where an encoder that states none leaves it
    let through = fixtures::through_mp3(&music(1, 44_100, pink), bitrate, dir.path());
    // and a decode whose start was cut carries it at an offset no encoder leaves
    let cut = Audio {
      rate: through.rate,
      channels: through
        .channels
        .iter()
        .map(|c| c[1234..].to_vec())
        .collect(),
    };
    for (name, audio, offset) in [("whole.flac", &through, 1), ("cut.flac", &cut, 495)] {
      let measured = measure(flac(dir.path(), name, audio, 16), &settings()).unwrap();
      let scan = measured.grid.as_ref().expect("scanned");
      assert_eq!(scan.mp3.offset, offset, "{name}: {scan:?}");
      let steps = scan
        .mp3_steps
        .as_ref()
        .expect("the steps are read at 44.1 kHz");
      let neighbours = steps.neighbours.as_ref().expect("neighbours are compared");
      let family = |thresholds| grid::found(scan, thresholds).and_then(|found| found.family);
      // the rules the analyzer ships with leave the noise to its offset, which names the
      // group for the whole decode and nothing for the cut one
      let by_offset = (offset == 1).then_some(grid::Family::Untagged);
      assert_eq!(family(&thresholds), by_offset, "{name}: {neighbours:?}");
      // the lowered rule names both decodes at 192 kbit/s, and neither at 128, whose high
      // bands move too few steps far
      let named = match bitrate {
        Bitrate::Kbps192 => Some(grid::Family::Lame),
        _ => by_offset,
      };
      assert_eq!(family(&lowered), named, "{name}: {neighbours:?}");
    }
  }
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
  for (name, window, codec, carried) in [
    (
      "kaiser-bessel",
      LongWindow::KaiserBessel,
      Codec::Aac,
      "an AAC",
    ),
    ("sine", LongWindow::Sine, Codec::Aac, "an AAC"),
    ("vorbis", LongWindow::Vorbis, Codec::Vorbis, "a Vorbis"),
    ("wma", LongWindow::Wma, Codec::Wma, "a WMA"),
    ("ac3", LongWindow::Ac3, Codec::Ac3, "an AC-3"),
  ] {
    let through = fixtures::through_transform(&source, window, 64);
    // the coder kept the audio where it should be: it differs by its rounding and its cut
    let off = apart(&through.channels[0], &source.channels[0]);
    assert!(off < 0.5, "{name}: {off}");
    let path = flac(dir.path(), &format!("{name}.flac"), &through, 16);
    let measured = measure(path, &settings()).unwrap();
    let scan = measured.grid.as_ref().expect("a lossless file is scanned");
    let sine = scan.aac_sine.as_ref().expect("the sine window is scanned");
    let vorbis = scan.vorbis.as_ref().expect("Vorbis's window is scanned");
    let wma = scan.wma.as_ref().expect("WMA's window is scanned");
    let ac3 = scan.ac3.as_ref().expect("AC-3's window is scanned");
    let (own, others, hop) = match window {
      LongWindow::KaiserBessel => (&scan.aac, [sine, vorbis, wma, ac3], 1024),
      LongWindow::Sine => (sine, [&scan.aac, vorbis, wma, ac3], 1024),
      LongWindow::Vorbis => (vorbis, [&scan.aac, sine, wma, ac3], 1024),
      LongWindow::Wma => (wma, [&scan.aac, sine, vorbis, ac3], 2048),
      LongWindow::Ac3 => (ac3, [&scan.aac, sine, vorbis, wma], 256),
    };
    assert_eq!(own.offset, hop - 64, "{name}: {scan:?}");
    assert_eq!(own.segments_agreeing, 4, "{name}: {scan:?}");
    for other in others {
      assert!(own.z > 1.5 * other.z, "{name}: {scan:?}");
    }
    let found = grid::found(scan, &ReadingThresholds::default());
    let found = found.unwrap_or_else(|| panic!("{name}: {scan:?}"));
    assert_eq!(found.codec, codec, "{name}: {scan:?}");
    assert_eq!(found.evidence, Evidence::Count(own.clone()), "{name}");
    let reading = read(&measured);
    assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{name}");
    let carries = format!("The samples carry {carried} frame grid");
    assert!(reading.summary.starts_with(&carries), "{}", reading.summary);
  }
}

#[test]
fn a_decode_of_aacs_quantizer_shows_its_steps_under_the_window_it_was_coded_with() {
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 44_100, pink);
  let thresholds = ReadingThresholds::default();
  let showing = |steps: &grid::AacSteps| {
    let segments = steps.segment_whole.iter();
    segments
      .filter(|whole| **whole >= thresholds.aac_whole_steps)
      .count()
  };
  for (name, window, family) in [
    (
      "kaiser-bessel",
      LongWindow::KaiserBessel,
      grid::Family::Nero,
    ),
    ("sine", LongWindow::Sine, grid::Family::Faac),
  ] {
    // the delays Nero's decodes and FAAC's leave
    let delay = if window == LongWindow::Sine {
      1024
    } else {
      576
    };
    let through = fixtures::through_stepped_transform(&source, window, delay);
    let path = flac(dir.path(), &format!("{name}.flac"), &through, 16);
    let measured = measure(path, &settings()).unwrap();
    let scan = measured.grid.as_ref().expect("a lossless file is scanned");
    let kbd = scan.aac_steps.as_ref().expect("read at 44.1 kHz");
    let sine = scan.aac_sine_steps.as_ref().expect("read at 44.1 kHz");
    let (own, other) = match window {
      LongWindow::KaiserBessel => (kbd, sine),
      LongWindow::Sine => (sine, kbd),
      LongWindow::Vorbis | LongWindow::Wma | LongWindow::Ac3 => {
        unreachable!("the steps are AAC's, coded under its windows")
      }
    };
    assert_eq!(showing(own), grid::SEGMENTS, "{name}: {scan:?}");
    assert_eq!(showing(other), 0, "{name}: {scan:?}");
    let reading = read(&measured);
    let vorbis = Limitation::AacGridWithoutSteps;
    assert!(!reading.limitations.contains(&vorbis), "{reading:#?}");
    let on_steps = format!("{} scalefactor bands sit on whole steps", own.whole);
    assert!(reading.summary.contains(&on_steps), "{}", reading.summary);
    let found = reading.grid.expect("the decode carries its grid");
    assert_eq!(found.codec, Codec::Aac, "{name}: {scan:?}");
    assert_eq!(found.family, Some(family), "{name}: {scan:?}");
    assert_eq!(found.quantizer.as_ref(), Some(own), "{name}");
    // with no count over its threshold, the steps alone show the grid
    let uncounted = ReadingThresholds {
      aac_grid_z: 1e9,
      vorbis_grid_z: 1e9,
      ..ReadingThresholds::default()
    };
    let found = grid::found(scan, &uncounted).expect("the steps show the grid");
    let evidence = Evidence::AacSteps {
      offset: 1024 - delay,
      steps: own.clone(),
    };
    assert_eq!(found.evidence, evidence, "{name}");
    assert_eq!(found.family, Some(family), "{name}");
  }
  // a quantizer that is not AAC's leaves a grid and no steps under either window
  let through = fixtures::through_transform(&source, LongWindow::KaiserBessel, 64);
  let measured = measure(flac(dir.path(), "even.flac", &through, 16), &settings()).unwrap();
  let scan = measured.grid.as_ref().expect("a lossless file is scanned");
  for steps in [&scan.aac_steps, &scan.aac_sine_steps] {
    let steps = steps.as_ref().expect("read at 44.1 kHz");
    assert_eq!(showing(steps), 0, "{scan:?}");
  }
  // so its reading says what else leaves such a grid
  let reading = read(&measured);
  assert_eq!(reading.grid.as_ref().map(|g| g.codec), Some(Codec::Aac));
  assert!(
    reading
      .limitations
      .contains(&Limitation::AacGridWithoutSteps),
    "{reading:#?}"
  );
  // and audio that was never coded none
  let measured = measure(flac(dir.path(), "source.flac", &source, 16), &settings()).unwrap();
  let scan = measured.grid.as_ref().expect("a lossless file is scanned");
  for steps in [&scan.aac_steps, &scan.aac_sine_steps] {
    let steps = steps.as_ref().expect("read at 44.1 kHz");
    assert_eq!(showing(steps), 0, "{scan:?}");
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
fn an_opus_decode_of_a_steady_pitch_carries_opus_frames_under_the_pitch_filter() {
  let dir = tempfile::tempdir().unwrap();
  let thresholds = ReadingThresholds::default();
  // a voice-like tone at 150 Hz, each channel at its own level: a harmonic in every band,
  // most of the power low, and phases that keep it from peaking once a period. the encoder
  // marks nearly every frame of such a steady pitch for the decoder's pitch post-filter
  let buzz = |level: f32| -> Vec<f32> {
    (0..12 * 48_000)
      .map(|i| {
        let turn = std::f64::consts::TAU * 150.0 * i as f64 / 48_000.0;
        let wave: f64 = (1..=133)
          .map(|harmonic| {
            let h = f64::from(harmonic);
            (turn * h + (h * h * 0.37) % std::f64::consts::TAU).sin() / h.powf(1.5)
          })
          .sum();
        level * (0.1 * wave) as f32
      })
      .collect()
  };
  let source = Audio {
    rate: 48_000,
    channels: vec![buzz(1.0), buzz(0.8)],
  };
  let through = through_opus(&source, 128, dir.path());
  let measured = measure(flac(dir.path(), "buzz.flac", &through, 16), &settings()).unwrap();
  let opus = measured.grid.as_ref().and_then(|scan| scan.opus.clone());
  let opus = opus.expect("the rate is one Opus is looked for at");
  let unfiltered = opus.unfiltered.clone().expect("the filter is searched");
  assert!(
    opus.frames < thresholds.opus_grid_frames && opus.uncut_frames < thresholds.opus_uncut_frames,
    "{opus:?}"
  );
  assert!(
    unfiltered.frames >= 2 * thresholds.opus_unfiltered_frames,
    "{opus:?}"
  );
  let grid = measured.read(&thresholds).grid.expect("a grid is found");
  assert_eq!(grid.codec, Codec::Opus);
  assert!(
    matches!(grid.evidence, Evidence::Unfiltered { .. }),
    "{grid:?}"
  );
  // the same buzz never coded has no frame on the lattice with any filter taken off
  let clean = measure(flac(dir.path(), "clean.flac", &source, 16), &settings()).unwrap();
  let clean = clean.grid.as_ref().and_then(|scan| scan.opus.clone());
  let clean = clean.and_then(|opus| opus.unfiltered).expect("searched");
  assert_eq!(clean.frames, 0, "{clean:?}");
  assert!(clean.searched > 0, "{clean:?}");
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
fn an_opus_decode_of_frames_under_20_ms_carries_frames_of_their_length() {
  let dir = tempfile::tempdir().unwrap();
  let thresholds = ReadingThresholds::default();
  let source = music(1, 48_000, pink);
  // libopus set for music delays by 312 samples whatever the frame's length
  for (at, (length, offset)) in [(480, 468), (240, 108), (120, 48)].into_iter().enumerate() {
    let short = fixtures::OpusSettings {
      frame: length,
      ..fixtures::OpusSettings::music(128)
    };
    let path = dir.path().join(format!("frames-{length}.opus"));
    std::fs::write(&path, fixtures::opus(&source, short).ogg()).unwrap();
    let through = fixtures::decoded(&path);
    for (name, audio) in [
      ("decoded.flac", through.clone()),
      ("at-44100.flac", through.resampled(44_100)),
    ] {
      let measured = measure(flac(dir.path(), name, &audio, 16), &settings()).unwrap();
      let opus = measured.grid.as_ref().and_then(|scan| scan.opus.clone());
      let opus = opus.expect("the rate is one Opus is looked for at");
      let said = format!("{length} samples, {name}: {opus:?}");
      assert!(
        opus.frames < thresholds.opus_grid_frames
          && opus.uncut_frames < thresholds.opus_uncut_frames,
        "{said}"
      );
      for (other, counted) in opus.short.iter().enumerate() {
        let enough = counted.frames >= 4 * thresholds.opus_short_frames
          && counted.uncut_frames >= 4 * thresholds.opus_short_uncut_frames;
        assert_eq!(enough, other == at, "{said}");
        assert!(
          other == at || counted.frames + counted.uncut_frames <= 1,
          "{said}"
        );
      }
      let short = opus.short[at].clone();
      assert_eq!(
        (short.length, short.offset, short.uncut_offset),
        (length, offset, offset),
        "{said}"
      );
      let reading = read(&measured);
      assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
      let found = reading.grid.expect("the short frames make a grid");
      assert_eq!(found.codec, Codec::Opus);
      assert_eq!(found.family, Some(grid::Family::Libopus));
      let frames = format!("frames of {} ms", short.milliseconds());
      assert_eq!(found.evidence, Evidence::Short { short });
      assert!(reading.summary.contains(&frames), "{}", reading.summary);
    }
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
    let short = opus.short.iter();
    assert!(
      short.clone().count() == 3 && short.map(|s| s.frames + s.uncut_frames).all(|n| n <= 1),
      "{name}: {opus:?}"
    );
  }
  let high = flac(dir.path(), "at-96000.flac", &music(1, 96_000, pink), 24);
  let measured = measure(high, &settings()).unwrap();
  assert_eq!(measured.grid.expect("scanned for MP3 and AAC").opus, None);
}

#[test]
fn a_decode_that_states_no_length_is_scanned_once_it_is_read() {
  let dir = tempfile::tempdir().unwrap();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir.path());
  let path = flac(dir.path(), "piped.flac", &through, 16);
  // a total sample count of 0 is none stated, as an encoder writing to a pipe leaves it
  state(&path, 0);
  let measured = measure(&path, &settings()).unwrap();
  assert_eq!(measured.claimed.frames, None);
  let reading = read(&measured);
  assert_eq!(reading.grid.map(|grid| grid.codec), Some(Codec::Mp3));
  assert!(
    !reading.limitations.contains(&Limitation::GridNotScanned),
    "{:?}",
    reading.limitations
  );
}

#[test]
fn a_file_stuck_at_negative_full_scale_carries_no_grid() {
  for rate in [44_100u32, 48_000] {
    // 16-bit stereo PCM in a WAV, every sample the lowest value there is
    let frames = 2_700_001u32;
    let data = frames * 4;
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((36 + data).to_le_bytes());
    bytes.extend(b"WAVEfmt \x10\0\0\0\x01\0\x02\0");
    bytes.extend(rate.to_le_bytes());
    bytes.extend((4 * rate).to_le_bytes());
    bytes.extend(b"\x04\0\x10\0data");
    bytes.extend(data.to_le_bytes());
    bytes.extend(i16::MIN.to_le_bytes().repeat(2 * frames as usize));
    let measured = measure_bytes(bytes, &settings()).unwrap();
    let scan = measured.grid.as_ref().expect("a lossless file is scanned");
    let other = scan.other_rate.as_ref().expect("scanned at the other rate");
    // every offset counts the same in a stretch of one value
    assert_eq!((other.aac.z, other.mp3.z), (0.0, 0.0), "{rate}: {other:?}");
    let reading = read(&measured);
    assert_eq!(reading.grid, None, "{rate}: {other:?}");
    assert_ne!(reading.verdict, Verdict::LikelyTranscode, "{reading:#?}");
  }
}

/// `audio` made `gain` times louder, with what passes full scale clipped
fn clipped(audio: &Audio, gain: f32) -> Audio {
  let mut loud = audio.clone();
  for sample in loud.channels.iter_mut().flatten() {
    *sample = (*sample * gain).clamp(-1.0, 1.0);
  }
  loud
}

#[test]
fn a_decode_clipped_throughout_shows_its_grid_and_says_its_stretches_clip() {
  let dir = tempfile::tempdir().unwrap();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps192, dir.path());
  // nearly half the samples clip, which fills the band the encoder cut and lowers the count
  // of every segment, each still lowest at the grid's offset
  let loud = clipped(&through, 16.0);
  let measured = measure(flac(dir.path(), "loud.flac", &loud, 16), &settings()).unwrap();
  let scan = measured.grid.as_ref().expect("scanned");
  assert_eq!(scan.mp3.segments_agreeing, 4, "{scan:?}");
  let reading = read(&measured);
  assert_eq!(reading.cutoff_verdict, Verdict::Consistent, "{reading:?}");
  assert_eq!(reading.verdict, Verdict::LikelyTranscode, "{reading:?}");
  // every stretch scanned is clipped and the file has none that is not, which the reading says
  let clipped = Limitation::ClippedStretches { stretches: 4 };
  assert!(reading.limitations.contains(&clipped), "{reading:?}");
}

#[test]
fn loud_audio_that_was_never_coded_stays_consistent() {
  let dir = tempfile::tempdir().unwrap();
  let loud = clipped(&music(1, 44_100, pink), 16.0);
  let measured = measure(flac(dir.path(), "loud.flac", &loud, 16), &settings()).unwrap();
  let reading = read(&measured);
  let clipped = reading.measures.clipped_samples_pct.expect("counted");
  assert!(clipped > 40.0, "{clipped}");
  assert_eq!(reading.verdict, Verdict::Consistent, "{reading:?}");
  let clipped = Limitation::ClippedStretches { stretches: 4 };
  assert!(reading.limitations.contains(&clipped), "{reading:?}");
}

#[test]
fn a_clipped_decode_is_scanned_where_it_does_not_clip() {
  let dir = tempfile::tempdir().unwrap();
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps192, dir.path());
  let mut loud = clipped(&through, 16.0);
  // five seconds the gain left alone, between the places of two segments
  let spared = 15 * 44_100..20 * 44_100 + 11_025;
  for (channel, source) in loud.channels.iter_mut().zip(&through.channels) {
    channel[spared.clone()].copy_from_slice(&source[spared.clone()]);
  }
  let measured = measure(flac(dir.path(), "loud.flac", &loud, 16), &settings()).unwrap();
  let scan = measured.grid.as_ref().expect("scanned");
  let found = grid::found(scan, &ReadingThresholds::default());
  assert_eq!(found.map(|f| f.codec), Some(Codec::Mp3), "{scan:?}");
  assert_eq!(scan.mp3.segments_agreeing, 4, "{scan:?}");
  assert_eq!(read(&measured).verdict, Verdict::LikelyTranscode);
}

#[test]
fn a_decode_quiet_where_it_is_scanned_is_scanned_where_it_is_loud() {
  let dir = tempfile::tempdir().unwrap();
  // a quiet recording gives an encoder at a high bitrate little to quantize
  let quiet = |audio: &Audio, but: std::ops::Range<usize>| {
    let mut quiet = audio.clone();
    for channel in &mut quiet.channels {
      for (at, sample) in channel.iter_mut().enumerate() {
        if !but.contains(&at) {
          *sample *= 0.003;
        }
      }
    }
    quiet
  };
  let found = |audio: &Audio, name: &str| {
    let through = fixtures::through_mp3(audio, Bitrate::Kbps320, dir.path());
    let measured = measure(flac(dir.path(), name, &through, 16), &settings()).unwrap();
    let scan = measured.grid.as_ref().expect("scanned");
    grid::found(scan, &ReadingThresholds::default()).map(|f| f.codec)
  };
  let source = fixtures::classical(fixtures::SECONDS);
  assert_eq!(found(&quiet(&source, 0..0), "quiet.flac"), None);
  // five loud seconds between the places of two segments
  let loud = 15 * 44_100..20 * 44_100 + 11_025;
  assert_eq!(found(&quiet(&source, loud), "loud.flac"), Some(Codec::Mp3));
}

#[test]
fn an_mp2_decode_carries_its_grid_on_the_filterbanks_phases() {
  let dir = tempfile::tempdir().unwrap();
  let mp2 = dir.path().join("clip.mp2");
  std::fs::write(&mp2, fixtures::MP2).unwrap();
  let through = fixtures::decoded(&mp2);
  let measured = measure(flac(dir.path(), "decoded.flac", &through, 16), &settings()).unwrap();
  let scan = measured.grid.as_ref().expect("a lossless file is scanned");
  let count = scan.mp2.as_ref().expect("the subband samples are counted");
  assert_eq!(count.segments_agreeing, 4, "{scan:?}");
  let found = grid::found(scan, &ReadingThresholds::default());
  let found = found.unwrap_or_else(|| panic!("{scan:?}"));
  assert_eq!(found.codec, Codec::Mp2, "{scan:?}");
  // audio that was never coded and a decode of MP3 show none
  let source = music(1, 44_100, pink);
  let mp3 = fixtures::through_mp3(&source, Bitrate::Kbps192, dir.path());
  for (name, audio) in [("source", &source), ("mp3", &mp3)] {
    let measured = measure(flac(dir.path(), "other.flac", audio, 16), &settings()).unwrap();
    let scan = measured.grid.as_ref().expect("a lossless file is scanned");
    let count = scan.mp2.as_ref().expect("the subband samples are counted");
    assert!(
      count.z < ReadingThresholds::default().mp2_grid_z,
      "{name}: {count:?}"
    );
  }
}
