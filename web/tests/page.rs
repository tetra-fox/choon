//! the page of `web/dist` in a headless Chromium, reading generated audio. `tests/page.mjs`
//! drives the browser and holds what is checked; `just web-test` builds the page first

use std::path::Path;
use std::process::Command;

use choon::fixtures::{self, Bitrate, music, pink};

#[test]
fn the_page_reads_remembers_and_clears() {
  let web = Path::new(env!("CARGO_MANIFEST_DIR"));
  assert!(
    web.join("dist/index.html").exists(),
    "`just web` has built web/dist"
  );
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 44_100, pink);
  let mp3 = dir.path().join("source.mp3");
  fixtures::write_mp3(&source, Bitrate::Kbps128, &mp3);
  let files = [
    ("honest.flac", fixtures::flac(&source, 16, false)),
    (
      "decoded.flac",
      fixtures::flac(&fixtures::decoded(&mp3), 16, false),
    ),
    ("words.txt", b"no audio in these bytes".repeat(64)),
  ];
  for (name, bytes) in files {
    std::fs::write(dir.path().join(name), bytes).unwrap();
  }
  let page = Command::new("node")
    .arg(web.join("tests/page.mjs"))
    .arg(dir.path())
    .output()
    .expect("node runs");
  assert!(
    page.status.success(),
    "{}",
    String::from_utf8_lossy(&page.stderr)
  );
}
