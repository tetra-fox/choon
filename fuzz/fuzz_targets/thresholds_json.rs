#![no_main]

use std::sync::LazyLock;

use choon::Settings;
use choon_fuzz::Thresholds;
use libfuzzer_sys::fuzz_target;

static NOISE: LazyLock<Vec<u8>> = LazyLock::new(choon_fuzz::noise);

// a profile file's thresholds, whatever numbers it holds, measuring and reading a short file
fuzz_target!(|data: &[u8]| {
  choon_fuzz::pass_caught_panics();
  let Ok(thresholds) = serde_json::from_slice::<Thresholds>(data) else {
    return;
  };
  let settings = Settings {
    thresholds: thresholds.measure,
    ..Settings::default()
  };
  choon_fuzz::on_one_thread(|| {
    let measured =
      choon::measure_bytes(NOISE.clone(), &settings).expect("a valid WAV file is read");
    measured.read(&thresholds.reading);
  });
});
