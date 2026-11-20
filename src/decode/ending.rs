//! ends a file's source where its audio ends, for the two readers of symphonia that trip on
//! what follows it.
//!
//! symphonia-format-ogg 0.6.1 looks for a stream's last page from a page's most bytes before
//! the end of the file, and fails the open when no page starts from there on (physical.rs,
//! `probe_stream_end`: the first `next_page` is not allowed to fail). a file with that much
//! after its audio, or damaged over its end, is then not read at all
//! TODO: remove once a symphonia release searches back for the last page
//!
//! symphonia-bundle-flac 0.6.1 takes a frame to run to the next frame's header, and the last
//! one to the end of the file (parser.rs, `try_read_fragment`). with anything after the audio
//! the last frame's checksum fails, the frame is dropped and the read ends as one cut short. an
//! ID3v1 tag, which old taggers put after a FLAC file, is left out here. other bytes cannot
//! be told from the frame's own without decoding it, and still cost the frame
//! TODO: remove once a symphonia release ends a FLAC stream's last frame by its checksum

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

/// the marker of an ID3v1 tag, which is the last bytes of its file, and the tag's length
const ID3V1: &[u8; 3] = b"TAG";
const ID3V1_BYTES: u64 = 128;
/// the marker a FLAC stream starts with, and the one of an ID3v2 tag that can come before it,
/// whose header is this long and states the tag's length in its last four bytes, seven bits each
const FLAC: &[u8; 4] = b"fLaC";
const ID3V2: &[u8; 3] = b"ID3";
const ID3V2_HEADER: usize = 10;

/// `source`, ended where its audio ends: at an Ogg file's last page when the reader would not
/// find that page, with the bytes left out then, and before the ID3v1 tag after a FLAC file,
/// which is no audio left out. any other source comes back as it is, at its start
pub(super) fn ended(mut source: Box<dyn MediaSource>) -> (Box<dyn MediaSource>, Option<u64>) {
  let Some(length) = source.byte_len().filter(|_| source.is_seekable()) else {
    return (source, None);
  };
  // a source that fails here fails the same way when the reader opens it
  let page = last_page(source.as_mut(), length).ok().flatten();
  let tag = before_tag(source.as_mut(), length).ok().flatten();
  if source.seek(SeekFrom::Start(0)).is_err() {
    return (source, None);
  }
  let Some(end) = page.or(tag) else {
    return (source, None);
  };
  let ended = Ended {
    inner: source,
    length: end,
    at: 0,
  };
  (Box::new(ended), page.map(|end| length - end))
}

/// where the ID3v1 tag after a FLAC file of `length` bytes starts; `None` for a file that is
/// not FLAC from its start, or after an ID3v2 tag, and for one without the tag
fn before_tag(source: &mut dyn MediaSource, length: u64) -> io::Result<Option<u64>> {
  let Some(end) = length.checked_sub(ID3V1_BYTES) else {
    return Ok(None);
  };
  let mut header = [0; ID3V2_HEADER];
  source.seek(SeekFrom::Start(0))?;
  source.read_exact(&mut header)?;
  let mut marker = [0; FLAC.len()];
  if header.starts_with(ID3V2) {
    let stated = header[6..]
      .iter()
      .fold(0, |size, byte| size << 7 | u64::from(byte & 0x7f));
    source.seek(SeekFrom::Start(ID3V2_HEADER as u64 + stated))?;
    source.read_exact(&mut marker)?;
  } else {
    marker.copy_from_slice(&header[..FLAC.len()]);
  }
  if marker != *FLAC {
    return Ok(None);
  }
  let mut tag = [0; ID3V1.len()];
  source.seek(SeekFrom::Start(end))?;
  source.read_exact(&mut tag)?;
  Ok((tag == *ID3V1).then_some(end))
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
  fn a_tag_is_found_after_flac_alone() {
    let tag = [&b"TAG"[..], &[0; 125]].concat();
    let before = |start: &[u8]| {
      let bytes = [start, &[7; 500], &tag].concat();
      let length = bytes.len() as u64;
      before_tag(&mut io::Cursor::new(bytes), length).unwrap()
    };
    assert_eq!(before(b"fLaC"), Some(504));
    // an ID3v2 tag of 300 bytes after its header, then the stream
    let tagged = [&b"ID3\x04\0\0\0\0\x02\x2c"[..], &[0; 300], b"fLaC"].concat();
    assert_eq!(before(&tagged), Some(tagged.len() as u64 + 500));
    assert_eq!(before(b"RIFF"), None);
    let untagged = [&b"fLaC"[..], &[7; 500]].concat();
    assert_eq!(
      before_tag(&mut io::Cursor::new(untagged), 504).unwrap(),
      None
    );
    assert_eq!(
      before_tag(&mut io::Cursor::new(b"fLaC".to_vec()), 4).unwrap(),
      None
    );
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
