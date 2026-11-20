//! what the fuzz targets share. no target sets a deadline: a slow input would pass or fail by
//! the machine's load, so libFuzzer's `-timeout` bounds a run and a timeout is a hang

use std::num::NonZeroUsize;
use std::sync::{LazyLock, Once};

use choon::{Coverage, MeasureThresholds, ReadingThresholds, Settings};
use serde::{Deserialize, Serialize};

/// lets a panic the library catches pass, as it does in a program. libFuzzer's hook aborts on
/// any panic, caught or not. with the default hook back, a panic nothing catches still ends
/// the run when it unwinds out of the target
pub fn pass_caught_panics() {
  static DEFAULT_HOOK: Once = Once::new();
  DEFAULT_HOOK.call_once(|| drop(std::panic::take_hook()));
}

/// runs `work` in a rayon pool of one thread, where the grid scan it starts stays. on the
/// global pool the scan's threads share the fuzzer's coverage counters, and a scan of half a
/// second takes minutes
pub fn on_one_thread<T: Send>(work: impl FnOnce() -> T + Send) -> T {
  static POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
      .num_threads(1)
      .build()
      .expect("a pool of one thread starts")
  });
  POOL.install(work)
}

/// measures `data` as a file's bytes and reads the measures. the first byte chooses the
/// coverage, in full or sampled, so that the seeking code runs too
pub fn analyze(data: &[u8]) {
  pass_caught_panics();
  let Some((&coverage, bytes)) = data.split_first() else {
    return;
  };
  let settings = Settings {
    coverage: if coverage % 2 == 0 {
      Coverage::Full
    } else {
      Coverage::Sampled {
        over_seconds: 2.0,
        windows: NonZeroUsize::new(4).expect("4 is not zero"),
      }
    },
    ..Settings::default()
  };
  on_one_thread(|| match choon::measure_bytes(bytes.to_vec(), &settings) {
    Ok(measured) => {
      measured.read(&ReadingThresholds::default());
    }
    // the library makes a panic of its own or of a decoder an error. here it is the find
    Err(choon::Error::Panicked) => panic!("the analysis panicked"),
    Err(_) => {}
  });
}

/// both sets of thresholds, as a profile file holds them
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
  pub measure: MeasureThresholds,
  pub reading: ReadingThresholds,
}

/// the sample rate of [`wave`]
const RATE: u32 = 44_100;

/// a stereo WAV file of `frames` sample frames whose samples are `samples` over and over, as
/// 32-bit floats when `float` and as 16-bit integers otherwise
pub fn wave(samples: &[u8], frames: u32, float: bool) -> Vec<u8> {
  let (format, bytes): (u16, u16) = if float { (3, 4) } else { (1, 2) };
  let block = 2 * bytes;
  let length = frames * u32::from(block);
  let mut wave = Vec::with_capacity(44 + length as usize);
  wave.extend_from_slice(b"RIFF");
  wave.extend_from_slice(&(36 + length).to_le_bytes());
  wave.extend_from_slice(b"WAVEfmt ");
  wave.extend_from_slice(&16u32.to_le_bytes());
  wave.extend_from_slice(&format.to_le_bytes());
  wave.extend_from_slice(&2u16.to_le_bytes());
  wave.extend_from_slice(&RATE.to_le_bytes());
  wave.extend_from_slice(&(RATE * u32::from(block)).to_le_bytes());
  wave.extend_from_slice(&block.to_le_bytes());
  wave.extend_from_slice(&(bytes * 8).to_le_bytes());
  wave.extend_from_slice(b"data");
  wave.extend_from_slice(&length.to_le_bytes());
  wave.extend(samples.iter().cycle().take(length as usize));
  wave
}

/// three seconds of noise from a fixed seed as a 16-bit WAV file, long enough for the grid scan
pub fn noise() -> Vec<u8> {
  let mut state = 1u32;
  let samples: Vec<u8> = (0..3 * RATE * 4)
    .map(|_| {
      state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      (state >> 24) as u8
    })
    .collect();
  wave(&samples, 3 * RATE, false)
}
