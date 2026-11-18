//! the files a check reads: those named, those a named directory holds, and those listed on stdin

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

/// the extensions a walk takes for audio, in any case: the codecs and containers the analyzer is
/// given to decode
pub const EXTENSIONS: [&str; 20] = [
  "flac", "wav", "aif", "aiff", "aifc", "caf", "m4a", "mp4", "alac", "mp3", "aac", "ogg", "oga",
  "opus", "mka", "webm", "wv", "ape", "dsf", "dff",
];

/// something a walk could not read
pub struct Problem {
  pub path: PathBuf,
  pub why: String,
  /// whether files may have gone unread because of it. a link that loops leads only to files
  /// the walk reads anyway
  pub unread: bool,
}

/// the files to read, in the order given, and what the walks could not read
#[derive(Default)]
pub struct Inputs {
  pub files: Vec<PathBuf>,
  pub problems: Vec<Problem>,
}

impl Inputs {
  /// the files `paths` name: a directory is walked, `-` stands for the paths `stdin` lists, and
  /// anything else is a file to read, whatever its name
  pub fn gather(
    paths: &[PathBuf],
    stdin: &mut impl Read,
    null: bool,
    follow: bool,
  ) -> io::Result<Self> {
    let mut inputs = Self::default();
    for path in paths {
      if path.as_os_str() == "-" {
        for listed in listed(stdin, null)? {
          inputs.add(&listed, follow);
        }
      } else {
        inputs.add(path, follow);
      }
    }
    Ok(inputs)
  }

  fn add(&mut self, path: &Path, follow: bool) {
    // a path that cannot be looked at is left for the analysis to fail on, which reports it
    if !path.is_dir() {
      self.files.push(path.to_path_buf());
      return;
    }
    let walk = WalkDir::new(path)
      .follow_links(follow)
      .sort_by_file_name()
      .into_iter()
      // the directory named is walked even when hidden
      .filter_entry(|entry| entry.depth() == 0 || !hidden(entry));
    for entry in walk {
      match entry {
        // a link not followed is neither a file nor a directory
        Ok(entry) if entry.file_type().is_file() && audio(entry.path()) => {
          self.files.push(entry.into_path());
        }
        Ok(_) => {}
        Err(e) => self.problems.push(Problem {
          path: e.path().unwrap_or(path).to_path_buf(),
          why: match (e.loop_ancestor(), e.io_error()) {
            (Some(ancestor), _) => format!("the link loops back to {}", ancestor.display()),
            (None, Some(io)) => io.to_string(),
            (None, None) => e.to_string(),
          },
          unread: e.loop_ancestor().is_none(),
        }),
      }
    }
  }
}

fn hidden(entry: &DirEntry) -> bool {
  entry.file_name().as_encoded_bytes().first() == Some(&b'.')
}

fn audio(path: &Path) -> bool {
  path.extension().is_some_and(|extension| {
    EXTENSIONS
      .iter()
      .any(|known| extension.eq_ignore_ascii_case(known))
  })
}

/// the paths `stdin` lists, one per line, or each ended by a NUL when `null`
fn listed(stdin: &mut impl Read, null: bool) -> io::Result<Vec<PathBuf>> {
  let mut bytes = Vec::new();
  stdin.read_to_end(&mut bytes)?;
  bytes
    .split(|byte| *byte == if null { 0 } else { b'\n' })
    .map(|path| if null { path } else { without_return(path) })
    .filter(|path| !path.is_empty())
    .map(|path| os_string(path).map(PathBuf::from))
    .collect()
}

/// a line without the carriage return Windows programs end it with, which no file name there
/// can hold
#[cfg(windows)]
fn without_return(line: &[u8]) -> &[u8] {
  line.strip_suffix(b"\r").unwrap_or(line)
}

/// a line as it is: a carriage return can be part of a file's name
#[cfg(not(windows))]
fn without_return(line: &[u8]) -> &[u8] {
  line
}

/// a path's bytes as the system names it: any bytes on Unix
#[cfg(unix)]
fn os_string(bytes: &[u8]) -> io::Result<OsString> {
  use std::os::unix::ffi::OsStringExt;
  Ok(OsString::from_vec(bytes.to_vec()))
}

/// a path's bytes as the system names it: UTF-8 where paths are not bytes
#[cfg(not(unix))]
fn os_string(bytes: &[u8]) -> io::Result<OsString> {
  String::from_utf8(bytes.to_vec())
    .map(OsString::from)
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
