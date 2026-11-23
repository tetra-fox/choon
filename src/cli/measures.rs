//! the measures file: a line of JSON for each measuring of a file, which `--save-measures`
//! writes and `--from-measures` reads again

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Split};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use choon::{MeasureThresholds, Measured};
use serde::{Deserialize, Serialize};

/// one measuring of a file as a line holds it. a line read holds each part owned
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Kept<'a> {
  /// the path as `--json` prints it
  pub file: Cow<'a, str>,
  /// the bytes of a path that is not Unicode, as `--json` prints them
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub file_bytes: Option<Cow<'a, [u8]>>,
  /// the version of the program that measured the file
  pub version: Cow<'a, str>,
  /// the thresholds the file was measured with
  pub measure: Cow<'a, MeasureThresholds>,
  pub measured: Cow<'a, Measured>,
}

/// the file a line names, which is all that is read of a line to tell whose it is
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Named {
  file: String,
  #[serde(default)]
  file_bytes: Option<Vec<u8>>,
}

impl Named {
  #[cfg(unix)]
  fn path(self) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    match self.file_bytes {
      Some(bytes) => OsString::from_vec(bytes).into(),
      None => self.file.into(),
    }
  }

  /// off Unix no line is written with a path's bytes, and one read with them names its file
  /// by `file`
  #[cfg(not(unix))]
  fn path(self) -> PathBuf {
    self.file.into()
  }
}

/// the lines of one file: one for each measuring of it, as they follow each other in the
/// measures file
pub struct Group {
  /// the number of its first line in the measures file, from 1
  pub line: usize,
  /// the file the lines name, or why the first line names none
  pub path: Result<PathBuf, String>,
  pub lines: Vec<Vec<u8>>,
}

/// a line as it is read: its number, the file it names or why it names none, and its bytes
type Line = (usize, Result<Named, String>, Vec<u8>);

/// the groups of a measures file in its order. a line that names no file is a group of its
/// own, and the groups end after a line that could not be read from the file
pub struct Groups {
  lines: Split<BufReader<File>>,
  /// the lines read so far
  read: usize,
  /// the line read past the end of the group before it
  ahead: Option<Line>,
  failed: bool,
}

impl Groups {
  pub fn new(file: File) -> Self {
    Self {
      lines: BufReader::new(file).split(b'\n'),
      read: 0,
      ahead: None,
      failed: false,
    }
  }

  /// the groups until `stop` says the run is stopped. `total` is set to how many groups the
  /// file holds once it is read to its end, which is looked for before `stop` is: a stop that
  /// comes after the last group was given leaves a run that read them all
  pub fn until<'a>(
    mut self,
    stop: &'a AtomicBool,
    total: &'a OnceLock<usize>,
  ) -> impl Iterator<Item = Group> + 'a {
    let mut given = 0;
    std::iter::from_fn(move || {
      let Some(group) = self.next() else {
        total.get_or_init(|| given);
        return None;
      };
      if stop.load(Ordering::Relaxed) {
        return None;
      }
      given += 1;
      Some(group)
    })
  }

  /// the next line that is not blank
  fn line(&mut self) -> Option<Line> {
    while !self.failed {
      let line = self.lines.next()?;
      self.read += 1;
      match line {
        Ok(bytes) if bytes.trim_ascii().is_empty() => {}
        Ok(bytes) => {
          let named = serde_json::from_slice(&bytes).map_err(|e| e.to_string());
          return Some((self.read, named, bytes));
        }
        Err(e) => {
          self.failed = true;
          return Some((self.read, Err(e.to_string()), Vec::new()));
        }
      }
    }
    None
  }
}

impl Iterator for Groups {
  type Item = Group;

  fn next(&mut self) -> Option<Group> {
    let (line, named, bytes) = self.ahead.take().or_else(|| self.line())?;
    let mut lines = vec![bytes];
    if let Ok(named) = &named {
      loop {
        match self.line() {
          Some((_, Ok(next), bytes)) if next == *named => lines.push(bytes),
          ahead => {
            self.ahead = ahead;
            break;
          }
        }
      }
    }
    Some(Group {
      line,
      path: named.map(Named::path),
      lines,
    })
  }
}

#[cfg(test)]
mod tests {
  use std::fs::File;
  use std::sync::OnceLock;
  use std::sync::atomic::{AtomicBool, Ordering};

  use super::Groups;

  #[test]
  fn a_stop_after_the_last_group_leaves_the_file_read_to_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let measures = dir.path().join("measures.jsonl");
    // two files, the first with two measurings
    let lines = "{\"file\": \"a\"}\n{\"file\": \"a\"}\n\n{\"file\": \"b\"}\n";
    std::fs::write(&measures, lines).unwrap();
    let groups = || Groups::new(File::open(&measures).unwrap());

    let (stop, total) = (AtomicBool::new(false), OnceLock::new());
    let mut read = groups().until(&stop, &total);
    assert_eq!(read.next().unwrap().lines.len(), 2);
    assert_eq!(read.next().unwrap().line, 4);
    assert_eq!(total.get(), None);
    stop.store(true, Ordering::Relaxed);
    assert!(read.next().is_none());
    assert_eq!(total.get(), Some(&2));

    // a stop before the last group ends the groups with their number unknown
    let (stop, total) = (AtomicBool::new(false), OnceLock::new());
    let mut read = groups().until(&stop, &total);
    assert!(read.next().is_some());
    stop.store(true, Ordering::Relaxed);
    assert!(read.next().is_none());
    assert_eq!(total.get(), None);
  }
}
