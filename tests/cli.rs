//! What scripts act on: the program's exit status, its lines and its JSON.

use std::path::Path;
use std::process::{Command, Output};

use earwitness::fixtures::{self, music, pink};
use mp3lame_encoder::Bitrate;

/// The paths of an honest FLAC and of one made from a 128 kbps MP3, written in `dir`.
fn honest_and_fake(dir: &Path) -> (String, String) {
  let honest = dir.join("honest.flac");
  fixtures::write_flac(&music(1, 44_100, pink), 16, false, &honest);
  let fake = dir.join("fake.flac");
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir);
  fixtures::write_flac(&through, 16, false, &fake);
  (path(&honest), path(&fake))
}

/// `path` as an argument; a temporary directory's path is UTF-8.
fn path(path: &Path) -> String {
  path.to_str().unwrap().to_string()
}

fn earwitness(args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_earwitness"))
    .arg("check")
    .args(args)
    .output()
    .unwrap()
}

fn status(output: &Output) -> i32 {
  output.status.code().unwrap()
}

#[test]
fn the_exit_status_says_whether_a_file_read_as_lossy_or_could_not_be_read() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let missing = path(&dir.path().join("missing.flac"));

  let output = earwitness(&[&honest]);
  assert_eq!(status(&output), 0, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  assert!(
    lines.starts_with(&format!("{honest}: consistent, confidence ")),
    "{lines}"
  );
  assert_eq!(lines.lines().count(), 1);

  let output = earwitness(&[&honest, &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  let fake_line = lines.lines().nth(1).unwrap();
  assert!(
    fake_line.starts_with(&format!("{fake}: likely transcode, cutoff 16."))
      && fake_line.contains("kHz, mp3 ~128 kbps, confidence"),
    "{lines}"
  );

  // a file that cannot be read outranks one read as lossy, wherever it comes, and the others
  // are still read
  for files in [[&fake, &missing], [&missing, &fake]] {
    let output = earwitness(&files.map(String::as_str));
    assert_eq!(status(&output), 2, "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap().lines().count(), 1);
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(errors.contains("missing.flac"), "{errors}");
  }
}

#[test]
fn json_is_a_line_per_file_with_a_reading_per_profile() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let strict = path(&dir.path().join("strict.json"));
  std::fs::write(&strict, r#"{"reading": {"minSeconds": 1000.0}}"#).unwrap();

  let output = earwitness(&["--json", &honest, &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  assert_eq!(lines.len(), 2);
  assert_eq!(lines[0]["file"], honest);
  assert_eq!(lines[0]["claimed"]["codec"], "flac");
  assert_eq!(lines[0]["readings"][0]["profile"], "default");
  assert_eq!(lines[0]["readings"][0]["verdict"], "consistent");
  assert_eq!(lines[1]["readings"][0]["verdict"], "likely_transcode");
  assert_eq!(
    lines[1]["readings"][0]["encoderGuess"]["profile"],
    "mp3 ~128 kbps"
  );

  // a profile takes the defaults' place, and too little audio for it is no lossy reading
  let output = earwitness(&["--json", "--profile", &strict, &fake]);
  assert_eq!(status(&output), 0, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  let readings = line["readings"].as_array().unwrap();
  assert_eq!(readings.len(), 1);
  assert_eq!(readings[0]["profile"], "strict");
  assert_eq!(readings[0]["verdict"], "inconclusive");
}

#[test]
fn a_wrong_argument_is_refused_before_any_file_is_read() {
  let dir = tempfile::tempdir().unwrap();
  let unknown = path(&dir.path().join("unknown.json"));
  std::fs::write(&unknown, r#"{"reading": {"sharpShelf": 40.0}}"#).unwrap();
  let audio = path(&dir.path().join("tone.m4a"));
  std::fs::write(&audio, fixtures::TONE).unwrap();

  for args in [
    vec!["--profile", &unknown, &audio],
    vec!["--max-full-minutes", "15", &audio],
    vec!["--sampled-windows", "240", &audio],
    vec!["--max-full-minutes=-1", "--sampled-windows", "240", &audio],
    vec![
      "--max-full-minutes",
      "inf",
      "--sampled-windows",
      "240",
      &audio,
    ],
    vec!["--max-full-minutes", "15", "--sampled-windows", "0", &audio],
    vec![],
  ] {
    let output = earwitness(&args);
    assert_eq!(status(&output), 2, "{args:?}: {output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
  }

  let output = earwitness(&["--defaults"]);
  assert_eq!(status(&output), 0, "{output:?}");
  let defaults: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(defaults["reading"]["sharpShelfDb"], 30.0);
}
