//! what scripts act on: the program's exit status, its lines and its JSON

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use choon::fixtures::{self, Bitrate, music, pink};

/// the paths of an honest FLAC and of one made from a 128 kbps MP3, written in `dir`
fn honest_and_fake(dir: &Path) -> (String, String) {
  let honest = dir.join("honest.flac");
  fixtures::write_flac(&music(1, 44_100, pink), 16, false, &honest);
  let fake = dir.join("fake.flac");
  let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir);
  fixtures::write_flac(&through, 16, false, &fake);
  (path(&honest), path(&fake))
}

/// the path of an honest FLAC filtered at 20 kHz, where the highest bitrates cut, written in
/// `dir`
fn cut_20(dir: &Path) -> String {
  let cut = dir.join("cut-20.flac");
  fixtures::write_flac(
    &music(1, 44_100, fixtures::cut_at(20_000.0)),
    16,
    false,
    &cut,
  );
  path(&cut)
}

/// `path` as an argument; a temporary directory's path is UTF-8
fn path(path: &Path) -> String {
  path.to_str().unwrap().to_string()
}

fn choon(args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_choon"))
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

  let output = choon(&[&honest]);
  assert_eq!(status(&output), 0, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  assert!(
    lines.starts_with(&format!("{honest}: consistent, confidence ")),
    "{lines}"
  );
  assert_eq!(lines.lines().count(), 1);

  let output = choon(&[&honest, &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  let fake_line = lines.lines().nth(1).unwrap();
  assert!(
    fake_line.starts_with(&format!("{fake}: likely transcode: MP3 grid at offset "))
      && fake_line.contains("), cut at 16.")
      && fake_line.ends_with("kHz, where mp3 ~128 kbps cuts"),
    "{lines}"
  );

  // a file that cannot be read outranks one read as lossy, wherever it comes, and the others
  // are still read
  for files in [[&fake, &missing], [&missing, &fake]] {
    let output = choon(&files.map(String::as_str));
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

  let output = choon(&["--json", &honest, &fake]);
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
  // the file's grid scan, and the grid its reading found
  assert_eq!(lines[1]["grid"]["mp3"]["segmentsAgreeing"], 4);
  assert_eq!(
    lines[1]["readings"][0]["grid"]["offset"],
    lines[1]["grid"]["mp3"]["offset"]
  );
  assert_eq!(lines[1]["readings"][0]["grid"]["codec"], "mp3");
  assert_eq!(lines[0]["readings"][0]["grid"], serde_json::Value::Null);

  // a profile takes the defaults' place; too little audio for its cutoff leaves the grid
  let output = choon(&["--json", "--profile", &strict, &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  let readings = line["readings"].as_array().unwrap();
  assert_eq!(readings.len(), 1);
  assert_eq!(readings[0]["profile"], "strict");
  assert_eq!(readings[0]["cutoffVerdict"], "inconclusive");
  assert_eq!(readings[0]["verdict"], "likely_transcode");

  // a grid threshold no scan reaches leaves the cutoff, which alone only doubts
  let blind = path(&dir.path().join("blind.json"));
  std::fs::write(&blind, r#"{"reading": {"mp3GridZ": 1e9}}"#).unwrap();
  let output = choon(&["--json", "--profile", &blind, &fake]);
  assert_eq!(status(&output), 0, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(line["readings"][0]["cutoffVerdict"], "likely_transcode");
  assert_eq!(line["readings"][0]["verdict"], "possibly_lossy");
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
      "0",
      "--sampled-windows",
      "240",
      &audio,
    ],
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
    let output = choon(&args);
    assert_eq!(status(&output), 2, "{args:?}: {output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
  }

  let output = choon(&["--defaults"]);
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
    let output = choon(&[bad]);
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
    let output = choon(&files.map(String::as_str));
    assert_eq!(status(&output), 2, "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap().lines().count(), 2);
  }

  let output = choon(&["--json", &text]);
  assert_eq!(status(&output), 2, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(line["file"], text);
  let error = line["error"].as_str().unwrap();
  assert!(
    error.starts_with("the container was not recognized"),
    "{error}"
  );

  // a directory is a file that could not be opened, which goes to stderr
  let output = choon(&[&path(dir.path())]);
  assert_eq!(status(&output), 2, "{output:?}");
  assert!(output.stdout.is_empty(), "{output:?}");
  let errors = String::from_utf8(output.stderr).unwrap();
  assert!(errors.contains("is a directory"), "{errors}");
}

#[test]
fn a_closed_pipe_ends_the_run_with_the_status_of_the_files_read() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let missing = path(&dir.path().join("missing.flac"));
  let mut child = Command::new(env!("CARGO_BIN_EXE_choon"))
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
  assert!(
    output.stderr.is_empty(),
    "the run stops at the closed pipe: {output:?}"
  );
}

#[test]
fn several_profiles_share_a_line_and_only_a_lossy_verdict_names_an_encoder() {
  let dir = tempfile::tempdir().unwrap();
  let (_, fake) = honest_and_fake(dir.path());
  let cut_20 = cut_20(dir.path());
  let shipped = path(&dir.path().join("shipped.json"));
  std::fs::write(&shipped, "{}").unwrap();
  let strict = path(&dir.path().join("strict.json"));
  std::fs::write(&strict, r#"{"reading": {"minSeconds": 1000.0}}"#).unwrap();

  let output = choon(&["--profile", &shipped, "--profile", &strict, &fake, &cut_20]);
  assert_eq!(status(&output), 1, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  let lines: Vec<&str> = lines.lines().collect();
  let (shipped_says, strict_says) = lines[0]
    .strip_prefix(&format!("{fake}: [shipped] "))
    .and_then(|rest| rest.split_once("; [strict] "))
    .unwrap_or_else(|| panic!("{lines:?}"));
  assert!(
    shipped_says.starts_with("likely transcode: MP3 grid")
      && shipped_says.contains("mp3 ~128 kbps"),
    "{shipped_says}"
  );
  assert!(
    strict_says.starts_with("likely transcode: MP3 grid"),
    "{strict_says}"
  );
  // a 20 kHz cut reads as something the analyzer cannot tell, with no encoder named
  assert!(
    lines[1].starts_with(&format!("{cut_20}: [shipped] inconclusive")),
    "{}",
    lines[1]
  );
  assert!(!lines[1].contains("kbps"), "{}", lines[1]);
}
