//! writes the seeds that are made, not copied, under the directory named: the measures of the
//! sample files as JSON, and the default thresholds

use std::path::Path;

use choon::Settings;
use choon_fuzz::Thresholds;

const FILES: [(&str, &[u8]); 3] = [
  ("tone", include_bytes!("../../src/fixtures/tone.m4a")),
  ("he-aac", include_bytes!("../../src/fixtures/he-aac.m4a")),
  ("wavpack", include_bytes!("../../src/fixtures/clip.wv")),
];

fn main() {
  let corpus = std::env::args()
    .nth(1)
    .expect("the corpus directory is named");
  let corpus = Path::new(&corpus);
  for (name, bytes) in FILES {
    let measured =
      choon::measure_bytes(bytes.to_vec(), &Settings::default()).expect("a sample file is read");
    let json = serde_json::to_vec(&measured).expect("measures serialize");
    std::fs::write(corpus.join(format!("measured_json/{name}.json")), json)
      .expect("the seed is written");
  }
  let noise = choon::measure_bytes(choon_fuzz::noise(), &Settings::default())
    .expect("a valid WAV file is read");
  let json = serde_json::to_vec(&noise).expect("measures serialize");
  std::fs::write(corpus.join("measured_json/noise.json"), json).expect("the seed is written");
  let defaults = serde_json::to_vec(&Thresholds::default()).expect("thresholds serialize");
  std::fs::write(corpus.join("thresholds_json/defaults.json"), defaults)
    .expect("the seed is written");
}
