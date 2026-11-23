//! what scripts act on: the program's exit status, its lines and its JSON

use std::io::{BufRead, BufReader, Write};
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

/// `choon check` with `input` on its stdin
fn choon_fed(args: &[&str], input: &[u8]) -> Output {
  let mut child = Command::new(env!("CARGO_BIN_EXE_choon"))
    .arg("check")
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child.stdin.take().unwrap().write_all(input).unwrap();
  child.wait_with_output().unwrap()
}

/// the lines of `output`, sorted: files are printed in the order they finish
fn sorted_lines(output: &Output) -> Vec<String> {
  let mut lines: Vec<String> = String::from_utf8(output.stdout.clone())
    .unwrap()
    .lines()
    .map(str::to_string)
    .collect();
  lines.sort();
  lines
}

/// the JSON objects of `output` without the times each took, sorted by file
fn sorted_json(output: &Output) -> Vec<serde_json::Value> {
  let mut objects: Vec<serde_json::Value> = sorted_lines(output)
    .iter()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  for object in &mut objects {
    for reading in object["readings"].as_array_mut().into_iter().flatten() {
      let reading = reading.as_object_mut().unwrap();
      reading.remove("measureSeconds").unwrap();
      reading.remove("readSeconds").unwrap();
    }
  }
  objects.sort_by_key(|object| object["file"].as_str().unwrap().to_string());
  objects
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
    lines.starts_with(&format!("{honest}: consistent (confidence ")),
    "{lines}"
  );
  assert_eq!(lines.lines().count(), 1);

  let output = choon(&[&honest, &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  assert_eq!(lines.lines().count(), 2);
  let fake_line = lines.lines().find(|line| line.starts_with(&fake)).unwrap();
  assert!(
    fake_line.starts_with(&format!("{fake}: likely transcode (MP3 grid, cutoff 16."))
      && fake_line.ends_with("kHz, MP3 ~128 kbps)"),
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

  // one job reads the files in the order given
  let output = choon(&["--json", "--jobs", "1", &honest, &fake]);
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
  let profile = &lines[1]["readings"][0]["encoderGuess"]["profile"];
  assert_eq!(profile["codec"], "MP3");
  assert_eq!(profile["setting"], "~128 kbps");
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

  // grid thresholds no scan reaches leave the cutoff, which alone only doubts
  let blind = path(&dir.path().join("blind.json"));
  let unreachable = r#"{"reading": {"mp3GridZ": 1e9, "partGridZ": 1e9, "loneGridZ": 1e9,
    "mp3WholeSteps": 1000000}}"#;
  std::fs::write(&blind, unreachable).unwrap();
  let output = choon(&["--json", "--profile", &blind, &fake]);
  assert_eq!(status(&output), 0, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(line["readings"][0]["cutoffVerdict"], "likely_transcode");
  assert_eq!(line["readings"][0]["verdict"], "possibly_lossy");
}

#[test]
fn fast_leaves_out_the_wma_and_other_rate_scans_and_still_finds_an_mp3_decode() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let thorough = sorted_json(&choon(&["--json", &honest, &fake]));
  let output = choon(&["--json", "--fast", &honest, &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let fast = sorted_json(&output);
  for (fast, thorough) in fast.iter().zip(&thorough) {
    assert!(!thorough["grid"]["wma"].is_null() && !thorough["grid"]["otherRate"].is_null());
    assert!(fast["grid"]["wma"].is_null() && fast["grid"]["otherRate"].is_null());
    assert_eq!(fast["grid"]["mp3"], thorough["grid"]["mp3"]);
    assert_eq!(
      fast["readings"][0]["verdict"],
      thorough["readings"][0]["verdict"]
    );
  }
}

#[test]
fn a_wrong_argument_is_refused_before_any_file_is_read() {
  let dir = tempfile::tempdir().unwrap();
  let unknown = path(&dir.path().join("unknown.json"));
  std::fs::write(&unknown, r#"{"reading": {"sharpShelf": 40.0}}"#).unwrap();
  let audio = path(&dir.path().join("tone.m4a"));
  std::fs::write(&audio, fixtures::TONE).unwrap();
  // two profiles whose files share a name would share one in the output
  let same = path(&dir.path().join("same.json"));
  std::fs::write(&same, "{}").unwrap();
  std::fs::create_dir(dir.path().join("elsewhere")).unwrap();
  let same_elsewhere = path(&dir.path().join("elsewhere").join("same.json"));
  std::fs::write(&same_elsewhere, "{}").unwrap();

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
    vec!["--jobs", "0", &audio],
    vec!["--jobs", "1025", &audio],
    vec!["--profile", &same, "--profile", &same_elsewhere, &audio],
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

#[cfg(unix)]
#[test]
fn json_holds_the_bytes_of_a_path_that_is_not_unicode() {
  use std::ffi::OsStr;
  use std::os::unix::ffi::OsStrExt;

  let dir = tempfile::tempdir().unwrap();
  // two names that differ where neither is Unicode, one audio and one not
  let names: [&[u8]; 2] = [b"bad\xff\xfe.m4a", b"bad\xff\xfd.m4a"];
  let [tone, text] = names.map(|name| dir.path().join(OsStr::from_bytes(name)));
  std::fs::write(&tone, fixtures::TONE).unwrap();
  std::fs::write(&text, "not audio\n").unwrap();
  let plain = dir.path().join("plain.m4a");
  std::fs::write(&plain, fixtures::TONE).unwrap();
  let output = Command::new(env!("CARGO_BIN_EXE_choon"))
    .args(["check", "--json"])
    .args([&tone, &text, &plain])
    .output()
    .unwrap();
  assert_eq!(status(&output), 2, "{output:?}");
  let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  let bytes = |path: &Path| -> serde_json::Value { path.as_os_str().as_bytes().into() };
  let of = |wanted: serde_json::Value| lines.iter().find(|line| line["fileBytes"] == wanted);
  let read = of(bytes(&tone)).expect("the file read is named by its bytes");
  assert!(read["readings"].is_array(), "{read}");
  let unread = of(bytes(&text)).expect("the file not read is named by its bytes");
  assert!(unread["error"].is_string(), "{unread}");
  assert_eq!(
    read["file"], unread["file"],
    "the names differ in their bytes only"
  );
  let plain = of(serde_json::Value::Null).expect("a Unicode path has no bytes beside it");
  assert_eq!(plain["file"], path(&dir.path().join("plain.m4a")));
}

#[test]
fn a_file_named_twice_or_inside_a_directory_named_is_read_once() {
  let dir = tempfile::tempdir().unwrap();
  let tone = path(&dir.path().join("tone.m4a"));
  std::fs::write(&tone, fixtures::TONE).unwrap();
  let output = choon(&[&tone, &path(dir.path()), &tone]);
  assert_eq!(status(&output), 0, "{output:?}");
  assert_eq!(sorted_lines(&output).len(), 1, "{output:?}");
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
        "{bad}: not read (the container was not recognized"
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
}

#[test]
fn an_opus_file_and_a_flac_decoded_from_it_say_opus() {
  let dir = tempfile::tempdir().unwrap();
  let source = music(1, 48_000, pink);
  let opus = dir.path().join("music.opus");
  let stream = fixtures::opus(&source, fixtures::OpusSettings::music(128));
  std::fs::write(&opus, stream.ogg()).unwrap();
  let webm = dir.path().join("music.webm");
  std::fs::write(&webm, stream.webm()).unwrap();
  let fake = dir.path().join("fake.flac");
  fixtures::write_flac(&fixtures::decoded(&opus), 16, false, &fake);
  let (opus, webm, fake) = (path(&opus), path(&webm), path(&fake));

  // an Opus file is read by its cutoff, and its line ends with what its packets state
  for file in [&opus, &webm] {
    let output = choon(&[file]);
    assert_eq!(status(&output), 0, "{output:?}");
    let line = String::from_utf8(output.stdout).unwrap();
    assert!(
      line.starts_with(&format!("{file}: consistent (cutoff 20.")),
      "{line}"
    );
    assert!(line.trim_end().ends_with(", Opus CELT fullband)"), "{line}");
  }

  let output = choon(&[&fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let line = String::from_utf8(output.stdout).unwrap();
  assert!(
    line.starts_with(&format!("{fake}: likely transcode (Opus grid, cutoff 20.")),
    "{line}"
  );
  assert!(line.trim_end().ends_with(", Opus (libopus))"), "{line}");

  let output = choon(&["--json", &opus, &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let [fake, opus] = &sorted_json(&output)[..] else {
    panic!("two files: {output:?}")
  };
  assert_eq!(opus["claimed"]["codec"], "opus");
  assert_eq!(opus["opus"][0]["mode"], "celt");
  assert_eq!(opus["opus"][0]["bandwidth"], "fullband");
  // the packets run a frame past the audio, which starts after the pre-skip
  assert_eq!(opus["opus"][0]["seconds"], 34.02);
  assert_eq!(opus["grid"], serde_json::Value::Null);
  // a file that is not Opus has no packets to state anything
  assert_eq!(fake.get("opus"), None);
  assert_eq!(fake["grid"]["opus"]["offset"], 228);
  let found = &fake["readings"][0]["grid"];
  assert_eq!(found["codec"], "opus");
  assert_eq!(found["offset"], 228);
  assert_eq!(found["frames"], fake["grid"]["opus"]["frames"]);
  assert!(found["uncutFrames"].as_u64().unwrap() >= 100, "{found}");
}

#[test]
fn a_flac_decoded_from_opus_and_made_quieter_says_opus_at_another_level() {
  let dir = tempfile::tempdir().unwrap();
  let opus = dir.path().join("music.opus");
  let stream = fixtures::opus(&music(1, 48_000, pink), fixtures::OpusSettings::music(128));
  std::fs::write(&opus, stream.ogg()).unwrap();
  let mut quieter = fixtures::decoded(&opus);
  // 7.3 dB down
  for sample in quieter.channels.iter_mut().flatten() {
    *sample *= 0.4315;
  }
  let fake = dir.path().join("quieter.flac");
  fixtures::write_flac(&quieter, 16, false, &fake);
  let fake = path(&fake);

  let output = choon(&[&fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let line = String::from_utf8(output.stdout).unwrap();
  assert!(
    line.starts_with(&format!(
      "{fake}: likely transcode (Opus grid at another level, cutoff 20."
    )),
    "{line}"
  );
  assert!(line.trim_end().ends_with(", Opus (libopus))"), "{line}");

  let output = choon(&["--json", &fake]);
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  let scan = &line["grid"]["opus"];
  // nothing at the decoder's level
  assert_eq!(scan["uncutFrames"], 0, "{scan}");
  let found = &line["readings"][0]["grid"];
  assert_eq!(found["codec"], "opus");
  assert_eq!(found["shifted"], scan["shifted"]);
  assert_eq!(found["shifted"]["offset"], 228);
  assert!(
    found["shifted"]["uncutFrames"].as_u64().unwrap() >= 100,
    "{found}"
  );
}

#[cfg(unix)]
#[test]
fn a_directory_is_read_through_for_its_audio_files() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let at = |name: &str| dir.path().join(name);
  for made in ["music/album", "music/.hidden", "aside"] {
    std::fs::create_dir_all(at(made)).unwrap();
  }
  std::fs::rename(&honest, at("music/honest.flac")).unwrap();
  // an extension counts in upper or lower case
  std::fs::copy(&fake, at("music/album/fake.FLAC")).unwrap();
  std::fs::copy(&fake, at("music/.hidden/fake.flac")).unwrap();
  std::fs::copy(&fake, at("music/.fake.flac")).unwrap();
  std::fs::write(at("music/album/cover.jpg"), "not audio").unwrap();
  std::fs::write(at("music/album/rip.log"), "not audio").unwrap();
  // audio under a name no walk takes
  std::fs::rename(&fake, at("aside/fake.bin")).unwrap();
  std::os::unix::fs::symlink(at("music"), at("music/album/back")).unwrap();
  std::os::unix::fs::symlink(at("aside/fake.bin"), at("music/linked.flac")).unwrap();
  let music = path(&at("music"));
  let named = path(&at("aside/fake.bin"));
  let read = |output: &Output| -> Vec<String> {
    sorted_lines(output)
      .iter()
      .map(|line| line.split_once(": ").unwrap().0.to_string())
      .collect()
  };

  // a file and a directory together: the file is read whatever its name
  let output = choon(&[&named, &music]);
  assert_eq!(status(&output), 1, "{output:?}");
  assert!(output.stderr.is_empty(), "{output:?}");
  assert_eq!(
    read(&output),
    [
      named.clone(),
      format!("{music}/album/fake.FLAC"),
      format!("{music}/honest.flac")
    ]
  );

  // with links followed, the linked file is read and the link back to the directory is
  // reported once and leaves the status alone
  let output = choon(&["--follow", &music]);
  assert_eq!(status(&output), 1, "{output:?}");
  assert_eq!(
    read(&output),
    [
      format!("{music}/album/fake.FLAC"),
      format!("{music}/honest.flac"),
      format!("{music}/linked.flac")
    ]
  );
  let errors = String::from_utf8(output.stderr).unwrap();
  assert_eq!(errors.lines().count(), 1, "{errors}");
  assert!(
    errors.starts_with(&format!(
      "choon: {music}/album/back: the link loops back to {music}"
    )),
    "{errors}"
  );

  // a directory named is read even when hidden
  let output = choon(&[&path(&at("music/.hidden"))]);
  assert_eq!(read(&output), [format!("{music}/.hidden/fake.flac")]);

  // a directory without audio reads nothing and says so
  let output = choon(&[&path(&at("aside"))]);
  assert_eq!(status(&output), 0, "{output:?}");
  assert!(output.stdout.is_empty(), "{output:?}");
  assert_eq!(
    String::from_utf8(output.stderr).unwrap(),
    "choon: no audio files found\n"
  );
}

#[test]
fn paths_are_read_from_stdin_by_line_or_by_nul() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let named = choon(&[&honest, &fake]);

  let output = choon_fed(&["-"], format!("{honest}\n\n{fake}\n").as_bytes());
  assert_eq!(status(&output), 1, "{output:?}");
  assert_eq!(sorted_lines(&output), sorted_lines(&named));

  // a name with a newline in it comes through whole when NUL ends each path
  let odd = path(&dir.path().join("a\nb.flac"));
  std::fs::copy(&honest, &odd).unwrap();
  let output = choon_fed(&["-0", "-"], format!("{odd}\0{fake}\0").as_bytes());
  assert_eq!(status(&output), 1, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  assert!(lines.contains(&format!("{odd}: consistent")), "{lines}");
  assert!(
    lines.contains(&format!("{fake}: likely transcode")),
    "{lines}"
  );
}

#[test]
fn many_files_at_once_print_what_each_prints_alone() {
  let dir = tempfile::tempdir().unwrap();
  // the two FLACs, the MP3 the fake was made through, a cut one and one that is not audio
  honest_and_fake(dir.path());
  cut_20(dir.path());
  std::fs::write(dir.path().join("text.flac"), "not audio\n").unwrap();
  let files: Vec<String> = std::fs::read_dir(dir.path())
    .unwrap()
    .map(|entry| path(&entry.unwrap().path()))
    .collect();
  assert_eq!(files.len(), 5);
  let all = path(dir.path());

  let mut alone: Vec<String> = files
    .iter()
    .flat_map(|file| sorted_lines(&choon(&[file])))
    .collect();
  alone.sort();
  for jobs in ["1", "3"] {
    let output = choon(&["--jobs", jobs, &all]);
    assert_eq!(status(&output), 2, "{output:?}");
    assert_eq!(sorted_lines(&output), alone, "{jobs} jobs");
  }

  let mut alone: Vec<serde_json::Value> = files
    .iter()
    .flat_map(|file| sorted_json(&choon(&["--json", file])))
    .collect();
  alone.sort_by_key(|object| object["file"].as_str().unwrap().to_string());
  let output = choon(&["--json", &all]);
  assert_eq!(status(&output), 2, "{output:?}");
  assert_eq!(sorted_json(&output), alone);
}

#[test]
fn a_closed_pipe_ends_the_run_with_the_status_of_the_files_read() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let missing = path(&dir.path().join("missing.flac"));
  let mut child = Command::new(env!("CARGO_BIN_EXE_choon"))
    // one job reads the files in the order given
    .args(["check", "--jobs", "1", &fake, &honest, &missing])
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

/// a FIFO in `dir`, which the program waits at until a writer opens it
#[cfg(unix)]
fn fifo(dir: &Path, name: &str) -> std::path::PathBuf {
  let fifo = dir.join(name);
  let made = Command::new("mkfifo").arg(&fifo).status();
  assert!(made.is_ok_and(|status| status.success()), "mkfifo ran");
  fifo
}

#[cfg(unix)]
#[test]
fn a_files_line_is_written_whole_while_other_files_are_under_way() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  // the last file is a FIFO no writer has opened, so the run cannot end before it is given one
  let last = fifo(dir.path(), "last.flac");
  let mut child = Command::new(env!("CARGO_BIN_EXE_choon"))
    .args(["check", "--json", "--jobs", "2", &honest, &fake])
    .arg(&last)
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
  let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
  let mut next = || -> serde_json::Value {
    let line = lines.next().expect("a line for each file").unwrap();
    serde_json::from_str(&line).unwrap()
  };
  // in the order they finish, which the two may do either way round
  let mut read = [next(), next()].map(|line| line["file"].as_str().unwrap().to_string());
  read.sort();
  let mut given = [honest, fake];
  given.sort();
  assert_eq!(read, given);
  // FLAC is read without seeking
  std::fs::write(&last, fixtures::flac(&music(1, 44_100, pink), 16, false)).unwrap();
  assert_eq!(next()["file"], path(&last));
  assert_eq!(child.wait().unwrap().code(), Some(1));
}

#[test]
fn measures_saved_are_read_again_as_a_check_prints_them() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let text = path(&dir.path().join("text.flac"));
  std::fs::write(&text, "not audio\n").unwrap();
  let measures = path(&dir.path().join("measures.jsonl"));

  // a file not read has no line in the measures file
  let output = choon(&[
    "--json",
    "--save-measures",
    &measures,
    &honest,
    &fake,
    &text,
  ]);
  assert_eq!(status(&output), 2, "{output:?}");
  let kept = std::fs::read_to_string(&measures).unwrap();
  assert_eq!(kept.lines().count(), 2, "{kept}");
  for line in kept.lines() {
    let line: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(line["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(line["measure"]["scanWma"], true);
    // every window's measures, without the pictures no reading is made from
    let windows = line["measured"]["windows"].as_array().unwrap();
    assert_eq!(windows.len(), fixtures::SECONDS);
    assert_eq!(windows[0]["shown"]["columns"], serde_json::json!([]));
  }
  let checked = sorted_json(&choon(&["--json", &honest, &fake]));
  let again = choon(&["--json", "--from-measures", &measures]);
  assert_eq!(status(&again), 1, "{again:?}");
  assert_eq!(sorted_json(&again), checked);
  let again = choon(&["--from-measures", &measures]);
  assert_eq!(status(&again), 1, "{again:?}");
  assert_eq!(
    sorted_lines(&again),
    sorted_lines(&choon(&[&honest, &fake]))
  );

  // other reading thresholds read the same measures: grid thresholds no scan reaches
  let blind = path(&dir.path().join("blind.json"));
  let unreachable = r#"{"reading": {"mp3GridZ": 1e9, "partGridZ": 1e9, "loneGridZ": 1e9,
    "mp3WholeSteps": 1000000}}"#;
  std::fs::write(&blind, unreachable).unwrap();
  let again = choon(&["--json", "--profile", &blind, "--from-measures", &measures]);
  assert_eq!(status(&again), 0, "{again:?}");
  let checked = choon(&["--json", "--profile", &blind, &honest, &fake]);
  assert_eq!(sorted_json(&again), sorted_json(&checked));

  // a profile that measures differently has no measures to read, and says so of each file
  let deaf = path(&dir.path().join("deaf.json"));
  std::fs::write(&deaf, r#"{"measure": {"silenceDbfs": -50.0}}"#).unwrap();
  let again = choon(&["--json", "--profile", &deaf, "--from-measures", &measures]);
  assert_eq!(status(&again), 2, "{again:?}");
  assert!(again.stdout.is_empty(), "{again:?}");
  let errors = String::from_utf8(again.stderr).unwrap();
  assert_eq!(errors.lines().count(), 2, "{errors}");
  assert!(
    errors.contains(&format!("choon: {fake}: profile deaf: ")),
    "{errors}"
  );
  // as do measures taken with --fast under profiles read without it
  let fast = path(&dir.path().join("fast.jsonl"));
  let output = choon(&["--fast", "--save-measures", &fast, &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  assert_eq!(status(&choon(&["--from-measures", &fast])), 2);
  let again = choon(&["--fast", "--from-measures", &fast]);
  assert_eq!(status(&again), 1, "{again:?}");
  assert_eq!(again.stdout, output.stdout);

  // measures are read in place of files, so both at once are refused
  let output = choon(&["--from-measures", &measures, &honest]);
  assert_eq!(status(&output), 2, "{output:?}");
  assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn profiles_that_measure_differently_keep_a_line_each_and_read_theirs_again() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  let shipped = path(&dir.path().join("shipped.json"));
  std::fs::write(&shipped, "{}").unwrap();
  let deaf = path(&dir.path().join("deaf.json"));
  std::fs::write(&deaf, r#"{"measure": {"silenceDbfs": -50.0}}"#).unwrap();
  let measures = path(&dir.path().join("measures.jsonl"));
  let profiles = ["--profile", &shipped, "--profile", &deaf];

  let mut args = vec!["--json", "--save-measures", &measures, &honest, &fake];
  args.extend(profiles);
  let checked = choon(&args);
  assert_eq!(status(&checked), 1, "{checked:?}");
  let kept = std::fs::read_to_string(&measures).unwrap();
  let silences: Vec<f64> = kept
    .lines()
    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
    .map(|line| line["measure"]["silenceDbfs"].as_f64().unwrap())
    .collect();
  // a file's lines follow each other, in the order of the profiles
  assert_eq!(silences, [-70.0, -50.0, -70.0, -50.0]);

  // a file's two lines make its one line again, whichever order the profiles come in
  let mut args = vec!["--json", "--from-measures", &measures];
  args.extend(profiles);
  let again = choon(&args);
  assert_eq!(status(&again), 1, "{again:?}");
  assert_eq!(sorted_json(&again), sorted_json(&checked));
  let turned = ["--json", "--from-measures", &measures, "--profile", &deaf];
  let again = sorted_json(&choon(&[&turned[..], &["--profile", &shipped]].concat()));
  assert_eq!(again.len(), 2);
  for (again, checked) in again.iter().zip(sorted_json(&checked)) {
    assert_eq!(again["readings"][0], checked["readings"][1]);
    assert_eq!(again["readings"][1], checked["readings"][0]);
  }
  // one of the profiles alone reads its own line of the two
  let again = choon(&["--json", "--from-measures", &measures, "--profile", &deaf]);
  assert_eq!(status(&again), 1, "{again:?}");
  assert_eq!(sorted_json(&again).len(), 2);
}

#[test]
fn a_line_of_a_measures_file_that_does_not_parse_is_a_file_not_read() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, _) = honest_and_fake(dir.path());
  let measures = path(&dir.path().join("measures.jsonl"));
  let output = choon(&["--save-measures", &measures, &honest]);
  assert_eq!(status(&output), 0, "{output:?}");
  let kept = std::fs::read_to_string(&measures).unwrap();
  // a line that is no JSON, a blank one, the file's own, and one that names a file and holds
  // no measures
  let lines = format!("not json\n\n{kept}{{\"file\": \"cut.flac\", \"version\": \"0.1.0\"}}\n");
  std::fs::write(&measures, lines).unwrap();

  let again = choon(&["--from-measures", &measures]);
  assert_eq!(status(&again), 2, "{again:?}");
  let lines = sorted_lines(&again);
  assert_eq!(lines.len(), 3, "{lines:?}");
  let has = |start: &str| lines.iter().any(|line| line.starts_with(start));
  assert!(
    has("cut.flac: not read (missing field `measure`"),
    "{lines:?}"
  );
  assert!(
    has(String::from_utf8(output.stdout).unwrap().trim_end()),
    "{lines:?}"
  );
  // a line that names no file is named by its place in the measures file
  assert!(
    has(&format!("{measures}:1: not read (expected ")),
    "{lines:?}"
  );

  let again = choon(&["--json", "--from-measures", &measures]);
  assert_eq!(status(&again), 2, "{again:?}");
  let unread: Vec<serde_json::Value> = sorted_lines(&again)
    .iter()
    .map(|line| serde_json::from_str(line).unwrap())
    .filter(|line: &serde_json::Value| line["error"].is_string())
    .collect();
  assert_eq!(unread.len(), 2, "{unread:?}");

  let missing = path(&dir.path().join("missing.jsonl"));
  let output = choon(&["--from-measures", &missing]);
  assert_eq!(status(&output), 2, "{output:?}");
  assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn a_file_out_of_time_before_its_first_window_is_not_read() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, _) = honest_and_fake(dir.path());
  // a millionth of a second is over before the audio is opened
  let output = choon(&["--timeout", "0.000001", &honest]);
  assert_eq!(status(&output), 2, "{output:?}");
  let line = String::from_utf8(output.stdout).unwrap();
  assert!(
    line.starts_with(&format!(
      "{honest}: not read (the analysis ran out of time "
    )),
    "{line}"
  );
  // time enough reads the file as without a timeout
  let output = choon(&["--timeout", "3600", &honest]);
  assert_eq!(status(&output), 0, "{output:?}");
  assert_eq!(output.stdout, choon(&[&honest]).stdout);

  for none in ["0", "-1", "nan", "inf", "soon"] {
    let output = choon(&[&format!("--timeout={none}"), &honest]);
    assert_eq!(status(&output), 2, "{none}: {output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
  }
}

/// what `choon check --json --timeout 3` prints of a FLAC whose writer gives its first `given`
/// bytes at once and holds the rest back until the file's time is up, which leaves the
/// analysis waiting that far in. the program sets the file's time before it opens the file,
/// and the writer's own open returns no sooner, so the time counted from there ends later
#[cfg(unix)]
fn held_back(given: impl Fn(usize) -> usize) -> Output {
  use std::time::{Duration, Instant};

  let dir = tempfile::tempdir().unwrap();
  let slow = fifo(dir.path(), "slow.flac");
  let flac = fixtures::flac(&music(1, 44_100, pink), 16, false);
  let timeout = Duration::from_secs(3);
  let child = Command::new(env!("CARGO_BIN_EXE_choon"))
    .args(["check", "--json", "--timeout", "3"])
    .arg(&slow)
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
  let mut writer = std::fs::File::create(&slow).unwrap();
  let opened = Instant::now();
  writer.write_all(&flac[..given(flac.len())]).unwrap();
  std::thread::sleep((opened + timeout).saturating_duration_since(Instant::now()));
  drop(writer);
  child.wait_with_output().unwrap()
}

#[cfg(unix)]
#[test]
fn a_file_out_of_time_part_way_prints_a_reading_of_the_part_measured() {
  let output = held_back(|all| all * 3 / 4);
  // a part reading counts as any reading does
  assert_eq!(status(&output), 0, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  let windows = line["windows"].as_u64().unwrap();
  assert!(
    windows > 0 && windows < fixtures::SECONDS as u64,
    "{windows} windows"
  );
  let limitations = line["readings"][0]["limitations"].as_array().unwrap();
  let out_of_time = "The analysis ran out of time and covers only the part read by then";
  assert!(
    limitations
      .iter()
      .any(|limitation| limitation == out_of_time),
    "{limitations:?}"
  );
}

#[cfg(unix)]
#[test]
fn a_file_opened_and_out_of_time_before_a_window_is_not_read() {
  // under a second of audio
  let output = held_back(|_| 32_768);
  assert_eq!(status(&output), 2, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(
    line["error"],
    "the analysis ran out of time before a window was measured"
  );
}

#[cfg(unix)]
#[test]
fn an_interrupt_ends_the_run_with_the_lines_of_the_files_read() {
  let dir = tempfile::tempdir().unwrap();
  let (honest, fake) = honest_and_fake(dir.path());
  // a FIFO no writer opens is under way for as long as the run lasts
  let waiting = fifo(dir.path(), "waiting.flac");
  let measures = dir.path().join("measures.jsonl");
  let mut child = Command::new(env!("CARGO_BIN_EXE_choon"))
    // one job reads the files in the order given
    .args(["check", "--json", "--jobs", "1", "--save-measures"])
    .arg(&measures)
    .arg(&fake)
    .arg(&waiting)
    .arg(&honest)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut lines = BufReader::new(child.stdout.take().unwrap());
  let mut first = String::new();
  lines.read_line(&mut first).unwrap();
  let read: serde_json::Value = serde_json::from_str(&first).unwrap();
  assert_eq!(read["file"], fake);
  let sent = Command::new("kill")
    .args(["-INT", &child.id().to_string()])
    .status();
  assert!(sent.is_ok_and(|status| status.success()), "kill ran");

  // the file under way prints nothing and the file after it is never started
  let mut rest = String::new();
  std::io::Read::read_to_string(&mut lines, &mut rest).unwrap();
  assert_eq!(rest, "");
  let output = child.wait_with_output().unwrap();
  assert_eq!(status(&output), 130, "{output:?}");
  assert_eq!(
    String::from_utf8(output.stderr).unwrap(),
    "choon: interrupted: 1 of 3 files read\n"
  );
  // the measures file holds the whole line of the file read and no part of another
  let kept = std::fs::read_to_string(&measures).unwrap();
  assert!(kept.ends_with('\n'), "{kept}");
  let kept: Vec<serde_json::Value> = kept
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  assert_eq!(kept.len(), 1);
  assert_eq!(kept[0]["file"], fake);
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

  // one job reads the files in the order given
  let output = choon(&[
    "--jobs",
    "1",
    "--profile",
    &shipped,
    "--profile",
    &strict,
    &fake,
    &cut_20,
  ]);
  assert_eq!(status(&output), 1, "{output:?}");
  let lines = String::from_utf8(output.stdout).unwrap();
  let lines: Vec<&str> = lines.lines().collect();
  let (shipped_says, strict_says) = lines[0]
    .strip_prefix(&format!("{fake}: [shipped] "))
    .and_then(|rest| rest.split_once("; [strict] "))
    .unwrap_or_else(|| panic!("{lines:?}"));
  assert!(
    shipped_says.starts_with("likely transcode (MP3 grid")
      && shipped_says.contains("MP3 ~128 kbps"),
    "{shipped_says}"
  );
  assert!(
    strict_says.starts_with("likely transcode (MP3 grid"),
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

#[test]
fn he_aac_wavpack_ape_and_dsd_files_are_read_and_name_what_they_are() {
  let dir = tempfile::tempdir().unwrap();
  let write = |name: &str, bytes: &[u8]| {
    let file = dir.path().join(name);
    std::fs::write(&file, bytes).unwrap();
    path(&file)
  };
  let he_aac = write("he.m4a", fixtures::HE_AAC_V2);
  let wavpack = write("clip.wv", fixtures::WAVPACK);
  let ape = write("clip.ape", fixtures::APE_NORMAL);
  let tones = fixtures::one_bit_tones(&[1000.0, 3000.0], 2);
  let dsf = write("tones.dsf", &tones.dsf());
  let dff = write("tones.dff", &tones.dff());
  // the same samples stated at 64 times 48 kHz; a DSF header's rate is its bytes 56 to 59
  let mut at_48k = tones.dsf();
  at_48k[56..60].copy_from_slice(&(64 * 48_000u32).to_le_bytes());
  let at_48k = write("tones-48k.dsf", &at_48k);

  // a walk takes each by its extension, and each line ends with what a file is beside its codec
  let output = choon(&[&path(dir.path())]);
  assert_eq!(status(&output), 0, "{output:?}");
  let lines = sorted_lines(&output);
  let line = |file: &str| -> &str {
    let found = lines.iter().find(|line| line.starts_with(file));
    found.unwrap_or_else(|| panic!("{file}: {lines:?}"))
  };
  assert_eq!(lines.len(), 6, "{lines:?}");
  assert!(line(&he_aac).ends_with(", HE-AAC v2)"), "{lines:?}");
  for file in [&dsf, &dff, &at_48k] {
    assert!(line(file).ends_with(" (DSD64)"), "{lines:?}");
  }
  for file in [&wavpack, &ape] {
    assert_eq!(line(file), format!("{file}: inconclusive"), "{lines:?}");
  }

  let output = choon(&["--json", &he_aac, &wavpack, &ape, &dsf]);
  assert_eq!(status(&output), 0, "{output:?}");
  let [ape, wavpack, he_aac, dsf] = &sorted_json(&output)[..] else {
    panic!("four files: {output:?}")
  };
  assert_eq!(he_aac["claimed"]["sampleRate"], 44_100);
  assert_eq!(he_aac["claimed"]["channels"], 2);
  assert_eq!(he_aac["heAac"]["coreRate"], 22_050);
  assert_eq!(he_aac["heAac"]["parametricStereo"], true);
  assert_eq!(wavpack["claimed"]["codec"], "wavpack");
  assert_eq!(wavpack["claimed"]["bitDepth"], 16);
  assert_eq!(wavpack["claimed"]["lossless"], true);
  assert_eq!(ape["claimed"]["codec"], "ape");
  assert_eq!(ape["claimed"]["frames"], 11_025);
  assert_eq!(dsf["claimed"]["codec"], "dsd");
  assert_eq!(dsf["claimed"]["sampleRate"], 176_400);
  assert_eq!(dsf["dsdRate"], 2_822_400);
  // only a file of its kind states either
  assert_eq!(wavpack.get("heAac"), None);
  assert_eq!(wavpack.get("dsdRate"), None);
  // a DSD file's samples were resampled on the way from any codec, so no grid is looked for
  assert_eq!(dsf["grid"], serde_json::Value::Null);
}

#[test]
fn a_file_a_reader_knows_and_does_not_decode_says_why() {
  let dir = tempfile::tempdir().unwrap();
  let hybrid = dir.path().join("hybrid.wv");
  std::fs::write(&hybrid, fixtures::WAVPACK_HYBRID).unwrap();
  let hybrid = path(&hybrid);
  let output = choon(&[&hybrid]);
  assert_eq!(status(&output), 2, "{output:?}");
  assert_eq!(
    String::from_utf8(output.stdout).unwrap(),
    format!(
      "{hybrid}: not read (wavpack: hybrid WavPack without its correction file is lossy and is \
       not read)\n"
    )
  );
}

#[test]
fn a_decode_of_a_sine_window_coder_names_aac_and_the_scan_states_both_windows() {
  let dir = tempfile::tempdir().unwrap();
  let through =
    fixtures::through_transform(&music(1, 44_100, pink), fixtures::LongWindow::Sine, 64);
  let fake = dir.path().join("fake.flac");
  fixtures::write_flac(&through, 16, false, &fake);
  let fake = path(&fake);
  let output = choon(&["--json", &fake]);
  assert_eq!(status(&output), 1, "{output:?}");
  let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  let found = &line["readings"][0]["grid"];
  assert_eq!(found["codec"], "aac");
  assert_eq!(found["offset"], 960);
  assert_eq!(found["z"], line["grid"]["aacSine"]["z"]);
  assert!(
    line["grid"]["aac"]["z"].as_f64().unwrap() < found["z"].as_f64().unwrap(),
    "{line}"
  );
}
