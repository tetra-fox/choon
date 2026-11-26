//! files made to break the analyzer: headers that state what cannot be, samples at the edge of
//! their type, damaged files of each decoder here and arbitrary bytes, none of which may panic

use super::{read, settings};
use crate::engine::{Error, Measured, measure_bytes};
use crate::fixtures::{self, Audio, music, music_for, pink};
use crate::limitation::Limitation;

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

#[test]
fn float_samples_near_the_largest_an_f32_holds_are_read_without_a_panic() {
  // two seconds of 32-bit float stereo at 48 kHz: a tone of 1e37 over noise of 1e30, so every
  // band is loud and the tone's transform value is past an f32
  let (rate, frames) = (48_000u32, 96_000u32);
  let mut wave = Vec::new();
  wave.extend_from_slice(b"RIFF");
  wave.extend_from_slice(&(36 + frames * 8).to_le_bytes());
  wave.extend_from_slice(b"WAVEfmt ");
  wave.extend_from_slice(&16u32.to_le_bytes());
  wave.extend_from_slice(&3u16.to_le_bytes());
  wave.extend_from_slice(&2u16.to_le_bytes());
  wave.extend_from_slice(&rate.to_le_bytes());
  wave.extend_from_slice(&(rate * 8).to_le_bytes());
  wave.extend_from_slice(&8u16.to_le_bytes());
  wave.extend_from_slice(&32u16.to_le_bytes());
  wave.extend_from_slice(b"data");
  wave.extend_from_slice(&(frames * 8).to_le_bytes());
  let mut state = 1u32;
  for frame in 0..frames {
    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    let noise = f64::from(state) / f64::from(u32::MAX) * 2.0 - 1.0;
    let turn = 2.0 * std::f64::consts::PI * 300.0 * f64::from(frame) / f64::from(rate);
    let sample = (1e37 * turn.sin() + 1e30 * noise) as f32;
    wave.extend_from_slice(&sample.to_le_bytes());
    wave.extend_from_slice(&(sample * 0.9).to_le_bytes());
  }
  let measured = measure_bytes(wave, &settings()).expect("a float WAVE file is read");
  read(&measured);
}

#[test]
fn an_mp4_whose_sample_counts_pass_32_bits_is_read_without_a_panic() {
  let mut bytes = fixtures::TONE.to_vec();
  // the table of sample durations: its name, version and flags, the number of entries, then a
  // sample count and a duration for each. two entries, of 3 samples and of 1
  let table = bytes.windows(4).position(|w| w == b"stts").unwrap();
  let entries = table + 12;
  assert_eq!(bytes[table + 8..entries], 2u32.to_be_bytes());
  bytes[entries..entries + 4].copy_from_slice(&1u32.to_be_bytes());
  bytes[entries + 8..entries + 12].copy_from_slice(&u32::MAX.to_be_bytes());
  let measured = measure_bytes(bytes, &settings()).expect("the first sample is read");
  let reading = read(&measured);
  assert!(
    reading
      .limitations
      .iter()
      .any(|l| matches!(l, Limitation::Decoder(why) if why.contains("reader failed"))),
    "{:?}",
    reading.limitations
  );
}

#[test]
fn an_ogg_file_is_read_whatever_follows_its_last_page() {
  let audio = Audio {
    rate: 48_000,
    ..music(1, 44_100, pink)
  };
  let whole = fixtures::opus(&audio, fixtures::OpusSettings::music(128)).ogg();
  let trailing = |measured: &Measured| {
    read(measured)
      .limitations
      .into_iter()
      .find(|l| matches!(l, Limitation::Decoder(why) if why.contains("last Ogg page")))
  };
  let clean = measure_bytes(whole.clone(), &settings()).unwrap();
  assert_eq!(trailing(&clean), None);
  // under a page's most bytes after the audio, which the reader looks past, and over it,
  // where the reader's search for the last page starts in what follows
  for (after, said) in [(1000, false), (100_000, true)] {
    let mut bytes = whole.clone();
    bytes.extend(std::iter::repeat_n(0xa5u8, after));
    let measured = measure_bytes(bytes, &settings()).unwrap_or_else(|e| panic!("{after}: {e}"));
    assert_eq!(measured.duration, clean.duration, "{after}");
    assert_eq!(measured.claimed.frames, clean.claimed.frames, "{after}");
    let limitation = trailing(&measured).map(|l| l.to_string());
    assert_eq!(limitation.is_some(), said, "{after}: {limitation:?}");
    if let Some(limitation) = limitation {
      assert!(limitation.contains("100000 bytes"), "{limitation}");
    }
  }
}

#[test]
fn a_flac_file_with_tags_after_it_is_read_to_its_last_frame() {
  let whole = fixtures::flac(&music_for(4, 1, 44_100, pink), 16, false);
  let clean = measure_bytes(whole.clone(), &settings()).unwrap();
  // an APE tag of one 9-byte item under a footer, then an ID3v1 tag
  let item = b"\x01\0\0\0\0\0\0\0A\0b";
  let mut ape = item.to_vec();
  ape.extend_from_slice(b"APETAGEX");
  for word in [2000u32, item.len() as u32 + 32, 1, 0] {
    ape.extend_from_slice(&word.to_le_bytes());
  }
  ape.extend_from_slice(&[0; 8]);
  let id3v1 = [&b"TAG"[..], &[0; 125]].concat();
  for tags in [id3v1.clone(), ape.clone(), [ape, id3v1].concat()] {
    let measured = measure_bytes([&whole[..], &tags].concat(), &settings()).unwrap();
    assert_eq!(measured.duration, clean.duration, "{} bytes", tags.len());
    let reading = read(&measured);
    assert!(
      !reading
        .limitations
        .iter()
        .any(|l| matches!(l, Limitation::Decoder(_))),
      "{:?}",
      reading.limitations
    );
  }
}

#[test]
fn a_second_stream_chained_after_the_first_is_named_as_left_out() {
  let audio = Audio {
    rate: 48_000,
    ..music(1, 44_100, pink)
  };
  let stream = fixtures::opus(&audio, fixtures::OpusSettings::music(128)).ogg();
  let alone = measure_bytes(stream.clone(), &settings()).unwrap();
  let measured = measure_bytes([stream.clone(), stream].concat(), &settings()).unwrap();
  assert_eq!(measured.duration, alone.duration);
  let reading = read(&measured);
  assert!(
    reading
      .limitations
      .iter()
      .any(|l| l.to_string() == "Another stream follows the first, which alone was read"),
    "{:?}",
    reading.limitations
  );
}

#[test]
fn a_wavpack_file_whose_first_block_is_damaged_is_refused_as_damaged() {
  let mut wavpack = fixtures::wavpack(&music(1, 44_100, pink), 16);
  // a byte in the middle of the first block's audio fails the block's checksum
  let block = u32::from_le_bytes(wavpack[4..8].try_into().unwrap()) as usize + 8;
  wavpack[block / 2] ^= 0x55;
  let refused = measure_bytes(wavpack, &settings());
  assert!(
    matches!(
      &refused,
      Err(Error::Unsupported(why)) if why == "wavpack: the file's first block could not be decoded"
    ),
    "{refused:?}"
  );
}

/// measures and reads `input`, failing on a panic in either, which `measure_bytes` gives as
/// [`Error::Panicked`]. any other error is a refusal of the bytes
fn survives(input: Vec<u8>) {
  match measure_bytes(input, &settings()) {
    Ok(measured) => {
      read(&measured);
    }
    Err(Error::Panicked) => panic!("the analysis panicked"),
    Err(_) => {}
  }
}

proptest::proptest! {
  #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

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
    survives(input);
  }

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
    survives(input);
  }

  /// ADTS and MP4 have no checksums, so the damage reaches the AAC decoder
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
    survives(input);
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
    survives(input);
  }

  /// WebM has no checksums, so the damage reaches the Opus decoder
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
    survives(input);
  }
}

/// `audio` as a WAVE file of 32-bit float samples
fn float_wave(audio: &Audio) -> Vec<u8> {
  let channels = audio.channels.len() as u32;
  let frames = audio.channels[0].len() as u32;
  let bytes = frames * channels * 4;
  let mut wave = b"RIFF".to_vec();
  wave.extend_from_slice(&(36 + bytes).to_le_bytes());
  wave.extend_from_slice(b"WAVEfmt ");
  wave.extend_from_slice(&16u32.to_le_bytes());
  wave.extend_from_slice(&3u16.to_le_bytes());
  wave.extend_from_slice(&(channels as u16).to_le_bytes());
  wave.extend_from_slice(&audio.rate.to_le_bytes());
  wave.extend_from_slice(&(audio.rate * channels * 4).to_le_bytes());
  wave.extend_from_slice(&(channels as u16 * 4).to_le_bytes());
  wave.extend_from_slice(&32u16.to_le_bytes());
  wave.extend_from_slice(b"data");
  wave.extend_from_slice(&bytes.to_le_bytes());
  for frame in 0..frames as usize {
    for channel in &audio.channels {
      wave.extend_from_slice(&channel[frame].to_le_bytes());
    }
  }
  wave
}

#[test]
fn float_samples_that_are_no_number_or_far_past_full_scale_give_finite_measures() {
  for odd in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30] {
    let mut audio = music_for(3, 1, 44_100, pink);
    audio.channels[0][44_100 + 500] = odd;
    let measured = measure_bytes(float_wave(&audio), &settings()).unwrap();
    for window in &measured.windows {
      assert!(
        window.rms_dbfs.is_finite() && window.power.is_finite() && window.upper_power.is_finite(),
        "{odd}: {} dBFS, power {}, upper {}",
        window.rms_dbfs,
        window.power,
        window.upper_power
      );
    }
    let totals = &measured.totals;
    assert!(totals.dc_offset().is_some_and(f64::is_finite), "{odd}");
    assert!(totals.peak_dbfs().is_some_and(f64::is_finite), "{odd}");
    let kept = serde_json::to_string(&measured).unwrap();
    let back: Measured = serde_json::from_str(&kept).unwrap();
    assert_eq!(serde_json::to_string(&back).unwrap(), kept, "{odd}");
    assert_eq!(read(&back), read(&measured), "{odd}");
    let noted = |l: &Limitation| matches!(l, Limitation::Decoder(why) if why.contains("1 sample"));
    assert!(measured.problems.iter().any(noted), "{odd}");
  }
}
