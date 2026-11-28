//! the wasm of `web/page/wasm`, run by deno over generated audio and compared with the same
//! function called natively. `just web-test` builds the wasm first

use std::path::Path;
use std::process::Command;

use choon::fixtures::{self, Audio, Bitrate, OpusSettings, music, music_for, pink};
use serde_json::Value;

/// how far a float of the wasm's reading may be from the native one, as a fraction of the
/// larger of the two and of 1
const FLOATS_APART: f64 = 1e-3;

/// `audio` as interleaved 16-bit samples
fn pcm(audio: &Audio) -> Vec<i16> {
  let frames = audio.channels[0].len();
  (0..frames)
    .flat_map(|frame| audio.channels.iter().map(move |channel| channel[frame]))
    .map(|sample| (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
    .collect()
}

fn little_endian(samples: &[i16]) -> Vec<u8> {
  samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// the `fmt ` chunk of 16-bit PCM
fn wave_format(audio: &Audio) -> Vec<u8> {
  let channels = audio.channels.len() as u16;
  let mut chunk = b"fmt ".to_vec();
  chunk.extend_from_slice(&16u32.to_le_bytes());
  chunk.extend_from_slice(&1u16.to_le_bytes());
  chunk.extend_from_slice(&channels.to_le_bytes());
  chunk.extend_from_slice(&audio.rate.to_le_bytes());
  chunk.extend_from_slice(&(audio.rate * u32::from(channels) * 2).to_le_bytes());
  chunk.extend_from_slice(&(channels * 2).to_le_bytes());
  chunk.extend_from_slice(&16u16.to_le_bytes());
  chunk
}

fn wav(audio: &Audio) -> Vec<u8> {
  let data = little_endian(&pcm(audio));
  let mut file = b"RIFF".to_vec();
  file.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
  file.extend_from_slice(b"WAVE");
  file.extend_from_slice(&wave_format(audio));
  file.extend_from_slice(b"data");
  file.extend_from_slice(&(data.len() as u32).to_le_bytes());
  file.extend_from_slice(&data);
  file
}

/// a WAVE file whose sizes are 64 bits wide, in a chunk of their own
fn rf64(audio: &Audio) -> Vec<u8> {
  let data = little_endian(&pcm(audio));
  let frames = audio.channels[0].len() as u64;
  let mut file = b"RF64".to_vec();
  file.extend_from_slice(&u32::MAX.to_le_bytes());
  file.extend_from_slice(b"WAVEds64");
  file.extend_from_slice(&28u32.to_le_bytes());
  file.extend_from_slice(&(72 + data.len() as u64).to_le_bytes());
  file.extend_from_slice(&(data.len() as u64).to_le_bytes());
  file.extend_from_slice(&frames.to_le_bytes());
  file.extend_from_slice(&0u32.to_le_bytes());
  file.extend_from_slice(&wave_format(audio));
  file.extend_from_slice(b"data");
  file.extend_from_slice(&u32::MAX.to_le_bytes());
  file.extend_from_slice(&data);
  file
}

fn aiff(audio: &Audio) -> Vec<u8> {
  let data: Vec<u8> = pcm(audio).iter().flat_map(|s| s.to_be_bytes()).collect();
  let mut file = b"FORM".to_vec();
  file.extend_from_slice(&(46 + data.len() as u32).to_be_bytes());
  file.extend_from_slice(b"AIFFCOMM");
  file.extend_from_slice(&18u32.to_be_bytes());
  file.extend_from_slice(&(audio.channels.len() as u16).to_be_bytes());
  file.extend_from_slice(&(audio.channels[0].len() as u32).to_be_bytes());
  file.extend_from_slice(&16u16.to_be_bytes());
  // the rate as an 80-bit float: an exponent, then the rate with its top bit first
  let exponent = 16383 + 31 - audio.rate.leading_zeros() as u16;
  file.extend_from_slice(&exponent.to_be_bytes());
  file
    .extend_from_slice(&(u64::from(audio.rate) << (32 + audio.rate.leading_zeros())).to_be_bytes());
  file.extend_from_slice(b"SSND");
  file.extend_from_slice(&(8 + data.len() as u32).to_be_bytes());
  file.extend_from_slice(&[0; 8]);
  file.extend_from_slice(&data);
  file
}

fn caf(audio: &Audio) -> Vec<u8> {
  let data = little_endian(&pcm(audio));
  let channels = audio.channels.len() as u32;
  let mut file = b"caff".to_vec();
  file.extend_from_slice(&[0, 1, 0, 0]);
  file.extend_from_slice(b"desc");
  file.extend_from_slice(&32i64.to_be_bytes());
  file.extend_from_slice(&f64::from(audio.rate).to_be_bytes());
  file.extend_from_slice(b"lpcm");
  // flags: little-endian integers
  for word in [2, channels * 2, 1, channels, 16] {
    file.extend_from_slice(&word.to_be_bytes());
  }
  file.extend_from_slice(b"data");
  file.extend_from_slice(&(4 + data.len() as i64).to_be_bytes());
  file.extend_from_slice(&[0; 4]);
  file.extend_from_slice(&data);
  file
}

/// a file of each format the analyzer reads that can be generated here, by name. the first
/// three are long enough for a reading, the rest are there to be opened
fn files(dir: &Path) -> Vec<(&'static str, Vec<u8>)> {
  let source = music(1, 44_100, pink);
  let source_48 = music(1, 48_000, pink);
  let short = music_for(5, 2, 44_100, pink);
  let mp3 = dir.join("source.mp3");
  fixtures::write_mp3(&source, Bitrate::Kbps128, &mp3);
  let opus = fixtures::opus(&source_48, OpusSettings::music(128));
  let opus_ogg = dir.join("source.opus");
  std::fs::write(&opus_ogg, opus.ogg()).unwrap();
  let tones = fixtures::one_bit_tones(&[1000.0, 3000.0], 3);
  vec![
    ("flac", fixtures::flac(&source, 16, false)),
    (
      "flac-from-mp3",
      fixtures::flac(&fixtures::decoded(&mp3), 16, false),
    ),
    (
      "flac-from-opus",
      fixtures::flac(&fixtures::decoded(&opus_ogg), 16, false),
    ),
    ("mp3", std::fs::read(&mp3).unwrap()),
    ("opus-ogg", opus.ogg()),
    ("opus-webm", opus.webm()),
    ("opus-mp4", opus.mp4()),
    ("flac-ogg", fixtures::ogg_flac(&short, 16)),
    ("wav", wav(&short)),
    ("rf64", rf64(&short)),
    ("aiff", aiff(&short)),
    ("caf", caf(&short)),
    ("wavpack", fixtures::wavpack(&short, 16)),
    ("ape", fixtures::ape(&short, 16, false)),
    ("dsf", tones.dsf()),
    ("dff", tones.dff()),
    ("alac", fixtures::TONE.to_vec()),
    ("he-aac", fixtures::HE_AAC.to_vec()),
    ("he-aac-v2", fixtures::HE_AAC_V2.to_vec()),
    ("he-aac-adts", fixtures::HE_AAC_ADTS.to_vec()),
    ("mp2", fixtures::MP2.to_vec()),
    ("ape-insane", fixtures::APE_INSANE.to_vec()),
  ]
}

/// what the wasm makes of the file at `path`: `check`'s JSON, or the message it throws
fn wasm(path: &Path) -> Result<String, String> {
  let web = Path::new(env!("CARGO_MANIFEST_DIR"));
  assert!(
    web.join("page/wasm/choon_web_bg.wasm").exists(),
    "`just web` has built web/page/wasm"
  );
  let deno = Command::new("deno")
    .args(["run", "--allow-read"])
    .arg(web.join("tests/run.js"))
    .arg(path)
    .output()
    .expect("deno runs");
  match deno.status.code() {
    Some(0) => Ok(String::from_utf8(deno.stdout).unwrap()),
    Some(2) => Err(String::from_utf8(deno.stderr).unwrap()),
    _ => panic!(
      "deno failed on {}: {}",
      path.display(),
      String::from_utf8_lossy(&deno.stderr)
    ),
  }
}

/// how a native reading and the wasm's differ
#[derive(Default)]
struct Compared {
  /// the largest distance between a float and its counterpart, as a fraction of the larger of
  /// the two and of 1
  apart: f64,
  /// where that float is
  furthest: String,
  /// everything else that is not the same, in words
  unlike: Vec<String>,
}

impl Compared {
  fn compare(&mut self, native: &Value, wasm: &Value, at: &str) {
    match (native, wasm) {
      (Value::Number(a), Value::Number(b)) if a.is_f64() && b.is_f64() => {
        let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
        let apart = (a - b).abs() / a.abs().max(b.abs()).max(1.0);
        if apart > self.apart {
          self.apart = apart;
          self.furthest = format!("{at}: {a} natively, {b} in wasm");
        }
      }
      (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
        for (i, (a, b)) in a.iter().zip(b).enumerate() {
          self.compare(a, b, &format!("{at}[{i}]"));
        }
      }
      (Value::Object(a), Value::Object(b)) if a.keys().eq(b.keys()) => {
        for (key, value) in a {
          self.compare(value, &b[key], &format!("{at}.{key}"));
        }
      }
      _ if native == wasm => {}
      _ => self
        .unlike
        .push(format!("{at}: {native} natively, {wasm} in wasm")),
    }
  }
}

#[test]
fn the_wasm_reads_each_format_as_the_native_library_does() {
  let dir = tempfile::tempdir().unwrap();
  let files = files(dir.path());
  let natives: Vec<Value> = files
    .iter()
    .map(|(name, bytes)| {
      let json = choon_web::read(bytes.clone(), "").unwrap_or_else(|e| panic!("{name}: {e}"));
      serde_json::from_str(&json).unwrap()
    })
    .collect();
  // the grid scan of a lossless file is slow in wasm, so every file is read at once
  let wasms: Vec<Value> = std::thread::scope(|scope| {
    let reads: Vec<_> = files
      .iter()
      .map(|(name, bytes)| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        scope.spawn(move || wasm(&path).unwrap_or_else(|e| panic!("{name}: {e}")))
      })
      .collect();
    reads
      .into_iter()
      .map(|read| serde_json::from_str(&read.join().unwrap()).unwrap())
      .collect()
  });

  let mut compared = Compared::default();
  for (((name, _), native), wasm) in files.iter().zip(&natives).zip(&wasms) {
    compared.compare(native, wasm, name);
  }
  println!(
    "floats at most {:e} apart, at {}",
    compared.apart, compared.furthest
  );
  assert!(compared.unlike.is_empty(), "{:#?}", compared.unlike);
  assert!(compared.apart <= FLOATS_APART, "{}", compared.furthest);

  // the comparison says nothing of a grid's codec, family and offset unless a grid is found
  let verdicts: Vec<_> = natives.iter().map(|n| &n["reading"]["verdict"]).collect();
  assert_eq!(
    verdicts[..3],
    ["consistent", "likely_transcode", "likely_transcode"]
  );
  assert_eq!(natives[1]["reading"]["grid"]["codec"], "mp3");
  assert_eq!(natives[2]["reading"]["grid"]["codec"], "opus");
}

/// the program in the main crate's release build, which `just web-test` builds
fn program() -> std::path::PathBuf {
  let program = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/release/choon");
  assert!(program.exists(), "`just web-test` has built the program");
  program
}

#[test]
fn the_words_are_the_ones_the_program_prints() {
  let dir = tempfile::tempdir().unwrap();
  let files = files(dir.path());
  let paths: Vec<_> = files
    .iter()
    .map(|(name, bytes)| {
      let path = dir.path().join(name);
      std::fs::write(&path, bytes).unwrap();
      path
    })
    .collect();
  // its status says whether a file was flagged, which the lines say too
  let printed = Command::new(program())
    .arg("check")
    .args(&paths)
    .output()
    .expect("the program runs");
  let printed = String::from_utf8(printed.stdout).unwrap();
  let mut lines: Vec<&str> = printed.lines().collect();
  lines.sort_unstable();

  let mut worded: Vec<String> = files
    .iter()
    .zip(&paths)
    .map(|((name, bytes), path)| {
      let json = choon_web::read(bytes.clone(), "").unwrap_or_else(|e| panic!("{name}: {e}"));
      let words = &serde_json::from_str::<Value>(&json).unwrap()["words"];
      let found: Vec<&str> = words["found"]
        .as_array()
        .unwrap()
        .iter()
        .map(|found| found.as_str().unwrap())
        .collect();
      let verdict = words["verdict"].as_str().unwrap();
      if found.is_empty() {
        format!("{}: {verdict}", path.display())
      } else {
        format!("{}: {verdict} ({})", path.display(), found.join(", "))
      }
    })
    .collect();
  worded.sort_unstable();
  assert_eq!(lines, worded);
}

#[test]
fn an_error_comes_back_as_the_message_the_library_gives() {
  let dir = tempfile::tempdir().unwrap();
  let refused = [
    ("words", b"no audio in these bytes".repeat(64)),
    ("wavpack-hybrid", fixtures::WAVPACK_HYBRID.to_vec()),
  ];
  for (name, bytes) in refused {
    let path = dir.path().join(name);
    std::fs::write(&path, &bytes).unwrap();
    let native = choon_web::read(bytes, "").unwrap_err();
    assert_eq!(wasm(&path).unwrap_err(), native, "{name}");
  }
}
