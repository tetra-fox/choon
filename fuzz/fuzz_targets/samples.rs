#![no_main]

use choon::{ReadingThresholds, Settings};
use libfuzzer_sys::fuzz_target;

/// two seconds, which the grid scan's four stretches fit in
const FRAMES: u32 = 88_200;

// the input as samples behind a valid WAV header, so that the transforms, the step tests and
// the lattice meet arbitrary values with no container reader in the way. the first byte
// chooses 16-bit or float samples
fuzz_target!(|data: &[u8]| {
  choon_fuzz::pass_caught_panics();
  let Some((&kind, samples)) = data.split_first() else {
    return;
  };
  if samples.is_empty() {
    return;
  }
  let wave = choon_fuzz::wave(samples, FRAMES, kind % 2 == 1);
  choon_fuzz::on_one_thread(|| {
    let measured =
      choon::measure_bytes(wave, &Settings::default()).expect("a valid WAV file is read");
    measured.read(&ReadingThresholds::default());
  });
});
