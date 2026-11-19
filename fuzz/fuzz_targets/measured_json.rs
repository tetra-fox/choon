#![no_main]

use choon::{Measured, ReadingThresholds};
use libfuzzer_sys::fuzz_target;

// measures kept as JSON and read back after an edit or a change of shape
fuzz_target!(|data: &[u8]| {
  choon_fuzz::pass_caught_panics();
  if let Ok(measured) = serde_json::from_slice::<Measured>(data) {
    measured.read(&ReadingThresholds::default());
  }
});
