//! What scripts act on: the program's exit status, its lines and its JSON.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};

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

/// The path of a FLAC made from a 320 kbps MP3, written in `dir`.
fn fake_320(dir: &Path) -> String {
  let fake = dir.join("fake-320.flac");
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps320, dir);
  fixtures::write_flac(&through, 16, false, &fake);
  path(&fake)
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

#[test]
fn a_file_not_read_as_audio_fails_the_run_and_says_so() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let text = path(&dir.path().join("text.flac"));
  std::fs::write(&text, "not audio\n").unwrap();
  let empty = path(&dir.path().join("empty.flac"));
  std::fs::write(&empty, "").unwrap();

  for bad in [&text, &empty] {
    let output = earwitness(&[bad]);
    assert_eq!(status(&output), 2, "{output:?}");
    let line = String::from_utf8(output.stdout).unwrap();
    assert!(
      line.starts_with(&format!(
        "{bad}: not read as audio: the container was not recognized"
      )),
      "{line}"
    );
  }

  // the other files are still read, and a lossy one does not hide the failure
  for files in [[&honest, &text], [&text, &fake]] {
    let output = earwitness(&files.map(String::as_str));
    assert_eq!(status(&output), 2, "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap().lines().count(), 2);
  }

  let output = earwitness(&["--json", &text]);
  assert_eq!(status(&output), 2, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(line["claimed"], serde_json::Value::Null);
  assert_eq!(line["readings"][0]["verdict"], "inconclusive");
}

#[test]
fn a_closed_pipe_ends_the_run_with_the_status_of_the_files_read() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let missing = path(&dir.path().join("missing.flac"));
  let mut child = Command::new(env!("CARGO_BIN_EXE_earwitness"))
    .args(["check", &fake, &honest, &missing])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  // read the first line and close the pipe, as `head -1` does, while the second file is read
  let mut first = String::new();
  BufReader::new(child.stdout.take().unwrap())
    .read_line(&mut first)
    .unwrap();
  assert!(first.contains("likely transcode"), "{first}");
  let output = child.wait_with_output().unwrap();
  assert_eq!(status(&output), 1, "{output:?}");
  assert!(output.stderr.is_empty(), "the run went on: {output:?}");
}

#[test]
fn several_profiles_share_a_line_and_only_a_lossy_verdict_names_an_encoder() {
  let dir = tempfile::tempdir().unwrap();
  let (_, fake) = honest_and_fake(dir.path());
  let fake_320 = fake_320(dir.path());
  let shipped = path(&dir.path().join("shipped.json"));
  std::fs::write(&shipped, "{}").unwrap();
  let strict = path(&dir.path().join("strict.json"));
  std::fs::write(&strict, r#"{"reading": {"minSeconds": 1000.0}}"#).unwrap();

  let output = earwitness(&[
    "--profile",
    &shipped,
    "--profile",
    &strict,
    &fake,
    &fake_320,
  ]);
  assert_eq!(status(&output), 1, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  let lines: Vec<&str> = lines.lines().collect();
  let (shipped_says, strict_says) = lines[0]
    .strip_prefix(&format!("{fake}: [shipped] "))
    .and_then(|rest| rest.split_once("; [strict] "))
    .unwrap_or_else(|| panic!("{lines:?}"));
  assert!(
    shipped_says.starts_with("likely transcode, cutoff 16.")
      && shipped_says.contains("mp3 ~128 kbps"),
    "{shipped_says}"
  );
  assert!(strict_says.starts_with("inconclusive"), "{strict_says}");
  // a 320 kbps cut reads as something the analyzer cannot tell, with no encoder named
  assert!(
    lines[1].starts_with(&format!("{fake_320}: [shipped] inconclusive")),
    "{}",
    lines[1]
  );
  assert!(!lines[1].contains("kbps"), "{}", lines[1]);
}
