//! ends an Ogg file's source at its last page when more than a page's most bytes follow it.
//!
//! symphonia-format-ogg 0.6.1 looks for a stream's last page from a page's most bytes before
//! the end of the file, and fails the open when no page starts from there on (physical.rs,
//! `probe_stream_end`: the first `next_page` is not allowed to fail). a file with that much
//! after its audio, or damaged over its end, is then not read at all
//! TODO: remove once a symphonia release searches back for the last page

use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

const MARKER: &[u8; 4] = b"OggS";
/// the bytes of a page's header before its segment table, and the most segments in the table
const HEADER: usize = 27;
const MAX_SEGMENTS: usize = 255;
/// the most bytes of a page: its header, its segment table and segments of 255 bytes each
const PAGE_MAX: u64 = (HEADER + MAX_SEGMENTS + MAX_SEGMENTS * 255) as u64;
/// the bytes read at a time on the way back from the end of the file, more than a page's most,
/// so a file with a page at its end is settled in one read
const STEP: usize = 1 << 16;
/// how far back from the end of the file the last page is looked for
const SEARCHED: u64 = 16 << 20;

/// `source`, ended where its last Ogg page ends when the reader would not find that page, and
/// the bytes left out then. any other source comes back as it is, at its start
pub(super) fn ended(mut source: Box<dyn MediaSource>) -> (Box<dyn MediaSource>, Option<u64>) {
  let Some(length) = source.byte_len().filter(|_| source.is_seekable()) else {
    return (source, None);
  };
  // a source that fails here fails the same way when the reader opens it
  let end = last_page(source.as_mut(), length).ok().flatten();
  if source.seek(SeekFrom::Start(0)).is_err() {
    return (source, None);
  }
  match end {
    Some(end) => {
      let ended = Ended {
        inner: source,
        length: end,
        at: 0,
      };
      (Box::new(ended), Some(length - end))
    }
    None => (source, None),
  }
}

/// where the last page of an Ogg file of `length` bytes ends, when it starts more than a page's
/// most bytes before the end of the file. `None` for a file that does not start with a page,
/// one whose last page the reader finds itself, and one with no page in the bytes searched.
///
/// a page is taken by its marker, its version and a length that fits the file. its checksum is
/// not read: a marker met by chance in what follows the audio gives a file the reader fails
/// on, as it does without this.
fn last_page(source: &mut dyn MediaSource, length: u64) -> io::Result<Option<u64>> {
  let mut marker = [0; MARKER.len()];
  source.seek(SeekFrom::Start(0))?;
  source.read_exact(&mut marker)?;
  if marker != *MARKER {
    return Ok(None);
  }
  // each step is read with the header of a page that starts at its last byte
  let overlap = HEADER + MAX_SEGMENTS;
  let mut buffer = vec![0; STEP + overlap];
  let floor = length.saturating_sub(SEARCHED);
  let mut end = length;
  while end > floor {
    let start = end.saturating_sub(STEP as u64).max(floor);
    let read_to = (end + overlap as u64).min(length);
    // at most a step and the overlap
    let bytes = &mut buffer[..(read_to - start) as usize];
    source.seek(SeekFrom::Start(start))?;
    source.read_exact(bytes)?;
    let found = (0..(end - start) as usize).rev().find_map(|at| {
      let page_start = start + at as u64;
      let page_end = page_start + page_length(&bytes[at..])?;
      (page_end <= length).then_some((page_start, page_end))
    });
    if let Some((page_start, page_end)) = found {
      return Ok((length - page_start > PAGE_MAX).then_some(page_end));
    }
    end = start;
  }
  Ok(None)
}

/// the length of the page `bytes` start with, from its header; `None` when they do not start
/// with one whose header they hold
fn page_length(bytes: &[u8]) -> Option<u64> {
  let (header, rest) = bytes.split_first_chunk::<HEADER>()?;
  // the marker, then the version, which is 0
  if !header.starts_with(MARKER) || header[4] != 0 {
    return None;
  }
  let segments = usize::from(header[HEADER - 1]);
  let table = rest.get(..segments)?;
  let data: u64 = table.iter().map(|length| u64::from(*length)).sum();
  Some((HEADER + segments) as u64 + data)
}

/// a source that ends `length` bytes in, whatever its file holds after them
struct Ended {
  inner: Box<dyn MediaSource>,
  length: u64,
  /// where the next read starts
  at: u64,
}

impl Read for Ended {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    let left = self.length.saturating_sub(self.at);
    let most = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
    let read = self.inner.read(&mut buf[..most])?;
    self.at += read as u64;
    Ok(read)
  }
}

impl Seek for Ended {
  fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
    let target = match to {
      SeekFrom::Start(at) => Some(at),
      SeekFrom::End(by) => self.length.checked_add_signed(by),
      SeekFrom::Current(by) => self.at.checked_add_signed(by),
    };
    let target = target.ok_or(io::ErrorKind::InvalidInput)?;
    self.at = self.inner.seek(SeekFrom::Start(target))?;
    Ok(self.at)
  }
}

impl MediaSource for Ended {
  fn is_seekable(&self) -> bool {
    true
  }

  fn byte_len(&self) -> Option<u64> {
    Some(self.length)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// a page of `segments` segments of 255 bytes each
  fn page(segments: u8) -> Vec<u8> {
    let mut page = MARKER.to_vec();
    page.resize(HEADER - 1, 0);
    page.push(segments);
    page.extend(std::iter::repeat_n(255, usize::from(segments)));
    page.extend(std::iter::repeat_n(7, usize::from(segments) * 255));
    page
  }

  fn end_of(bytes: Vec<u8>) -> Option<u64> {
    let length = bytes.len() as u64;
    last_page(&mut io::Cursor::new(bytes), length).unwrap()
  }

  #[test]
  fn the_last_page_is_found_only_where_the_reader_would_miss_it() {
    let pages: Vec<u8> = [page(3), page(255), page(1)].concat();
    let audio = pages.len() as u64;
    assert_eq!(end_of(pages.clone()), None, "a page ends the file");
    let followed = |by: usize| [pages.clone(), vec![0xa5; by]].concat();
    // the last page starts 27 + 1 + 255 bytes before what follows
    let near = PAGE_MAX as usize - 283;
    assert_eq!(end_of(followed(near)), None, "the reader still finds it");
    assert_eq!(end_of(followed(near + 1)), Some(audio));
    // past one step back, and with a marker in what follows whose page would pass the end
    assert_eq!(end_of(followed(3 * STEP + 5)), Some(audio));
    let mut marked = followed(3 * STEP);
    marked.extend_from_slice(b"OggS\0");
    marked.extend(vec![0xff; 1000]);
    assert_eq!(end_of(marked), Some(audio));
    assert_eq!(end_of(vec![0; 100_000]), None, "no Ogg file");
  }

  #[test]
  fn an_ended_source_reads_and_seeks_as_a_file_of_its_length() {
    let inner = Box::new(io::Cursor::new((0..=99u8).collect::<Vec<u8>>()));
    let mut source = Ended {
      inner,
      length: 10,
      at: 0,
    };
    let mut all = Vec::new();
    source.read_to_end(&mut all).unwrap();
    assert_eq!(all, (0..10).collect::<Vec<u8>>());
    assert_eq!(source.seek(SeekFrom::End(-2)).unwrap(), 8);
    let mut two = [0; 4];
    assert_eq!(source.read(&mut two).unwrap(), 2);
    assert_eq!(two[..2], [8, 9]);
    assert!(source.seek(SeekFrom::Current(-11)).is_err());
  }
}
