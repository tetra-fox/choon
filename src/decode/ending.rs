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
//! the last frame's checksum fails, the frame is dropped and the read ends as one cut short.
//! the tags that taggers put after a file's audio, ID3v1, Lyrics3 and APE, are left out here.
//! other bytes cannot be told from the frame's own without decoding it, and still cost the
//! frame
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
/// the places with a page's marker and a length that fits whose checksums are read, each a
/// read of up to a page, before the last page is given up on
const TRIED: usize = 64;

/// the marker an ID3v1 tag starts with, and the tag's length
const ID3V1: &[u8] = b"TAG";
const ID3V1_BYTES: usize = 128;
/// the marker both versions of a Lyrics3 tag start with, and the one each ends with. the
/// second version states its length before its end marker, in six digits, and the first holds
/// this many bytes of lyrics at most
const LYRICS_START: &[u8] = b"LYRICSBEGIN";
const LYRICS_END: &[u8] = b"LYRICSEND";
const LYRICS2_END: &[u8] = b"LYRICS200";
const LYRICS2_DIGITS: usize = 6;
const LYRICS_MAX: usize = 5100;
/// the marker an APE tag's footer and header start with, and the length of either. the footer
/// states the tag's length without its header at byte 12, and whether it has one in the top
/// bit of the flags at byte 20
const APE: &[u8] = b"APETAGEX";
const APE_FOOTER: usize = 32;
const APE_HAS_HEADER: u32 = 1 << 31;
/// the kinds of tag looked for after the audio: each comes once at most
const KINDS: usize = 4;
/// the marker a FLAC stream starts with, and the one of an ID3v2 tag that can come before it,
/// whose header is this long and states the tag's length in its last four bytes, seven bits each
const FLAC: &[u8; 4] = b"fLaC";
const ID3V2: &[u8; 3] = b"ID3";
const ID3V2_HEADER: usize = 10;

/// `source`, ended where its audio ends: at an Ogg file's last page when the reader would not
/// find that page, with the bytes left out then, and before the tags after a FLAC file, which
/// are no audio left out. any other source comes back as it is, at its start
pub(super) fn ended(mut source: Box<dyn MediaSource>) -> (Box<dyn MediaSource>, Option<u64>) {
  let Some(length) = source.byte_len().filter(|_| source.is_seekable()) else {
    return (source, None);
  };
  // a source that fails here fails the same way when the reader opens it
  let page = last_page(source.as_mut(), length).ok().flatten();
  let tag = before_tags(source.as_mut(), length).ok().flatten();
  if source.seek(SeekFrom::Start(0)).is_err() {
    return (source, None);
  }
  let Some(end) = page.or(tag) else {
    return (source, None);
  };
  (
    Box::new(Ended::new(source, end)),
    page.map(|end| length - end),
  )
}

/// where the tags after a FLAC file of `length` bytes start; `None` for a file that is not
/// FLAC from its start, or after an ID3v2 tag, and for one with no tag after its audio
fn before_tags(source: &mut dyn MediaSource, length: u64) -> io::Result<Option<u64>> {
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
  // the tags come in any order, each before the ones already found
  let mut end = length;
  for _ in 0..KINDS {
    let Some(start) = tag_start(source, end)? else {
      break;
    };
    end = start;
  }
  Ok((end < length).then_some(end))
}

/// where the tag that ends at byte `end` of the file starts; `None` when none ends there
fn tag_start(source: &mut dyn MediaSource, end: u64) -> io::Result<Option<u64>> {
  if let Some(tag) = before::<ID3V1_BYTES>(source, end)?
    && tag.starts_with(ID3V1)
  {
    return Ok(Some(end - ID3V1_BYTES as u64));
  }
  if let Some(footer) = before::<APE_FOOTER>(source, end)?
    && footer.starts_with(APE)
  {
    let word =
      |at: usize| u32::from_le_bytes([footer[at], footer[at + 1], footer[at + 2], footer[at + 3]]);
    let header = word(20) & APE_HAS_HEADER != 0;
    let length = u64::from(word(12)) + if header { APE_FOOTER as u64 } else { 0 };
    // the stated length holds the footer, and a header starts with the marker too
    let start = end
      .checked_sub(length)
      .filter(|_| word(12) as usize >= APE_FOOTER);
    let Some(start) = start else {
      return Ok(None);
    };
    let marked = !header
      || before::<APE_FOOTER>(source, start + APE_FOOTER as u64)?
        .is_some_and(|h| h.starts_with(APE));
    return Ok(marked.then_some(start));
  }
  if let Some(tail) = before::<{ LYRICS2_DIGITS + LYRICS2_END.len() }>(source, end)?
    && tail.ends_with(LYRICS2_END)
  {
    let digits = std::str::from_utf8(&tail[..LYRICS2_DIGITS]).ok();
    let stated = digits.and_then(|digits| digits.parse::<u64>().ok());
    let start = stated.and_then(|stated| (end - tail.len() as u64).checked_sub(stated));
    let Some(start) = start else {
      return Ok(None);
    };
    let marked = before::<{ LYRICS_START.len() }>(source, start + LYRICS_START.len() as u64)?
      .is_some_and(|marker| marker == LYRICS_START);
    return Ok(marked.then_some(start));
  }
  if let Some(tail) = before::<{ LYRICS_END.len() }>(source, end)?
    && tail == LYRICS_END
  {
    // the first version states no length: its start is looked for as far back as it can be
    let reach = (LYRICS_START.len() + LYRICS_MAX + LYRICS_END.len()) as u64;
    let from = end.saturating_sub(reach);
    let mut bytes = vec![0; (end - from) as usize];
    source.seek(SeekFrom::Start(from))?;
    source.read_exact(&mut bytes)?;
    let at = bytes
      .windows(LYRICS_START.len())
      .rposition(|window| window == LYRICS_START);
    return Ok(at.map(|at| from + at as u64));
  }
  Ok(None)
}

/// the `N` bytes before byte `end` of the file; `None` when fewer come before it
fn before<const N: usize>(source: &mut dyn MediaSource, end: u64) -> io::Result<Option<[u8; N]>> {
  let Some(start) = end.checked_sub(N as u64) else {
    return Ok(None);
  };
  let mut bytes = [0; N];
  source.seek(SeekFrom::Start(start))?;
  source.read_exact(&mut bytes)?;
  Ok(Some(bytes))
}

/// where the last page of an Ogg file of `length` bytes ends, when it starts more than a page's
/// most bytes before the end of the file. `None` for a file that does not start with a page,
/// one whose last page the reader finds itself, and one with no page in the bytes searched.
///
/// a page is one by its marker, its version, a length that fits the file and its checksum, so
/// a marker met by chance in what follows the audio is passed over.
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
  let mut tried_left = TRIED;
  while end > floor && tried_left > 0 {
    let start = end.saturating_sub(STEP as u64).max(floor);
    let read_to = (end + overlap as u64).min(length);
    // at most a step and the overlap
    let bytes = &mut buffer[..(read_to - start) as usize];
    source.seek(SeekFrom::Start(start))?;
    source.read_exact(bytes)?;
    // the places a page could start at, from the last: few, as each holds the marker
    let starts: Vec<(u64, u64)> = (0..(end - start) as usize)
      .rev()
      .filter_map(|at| Some((start + at as u64, page_length(&bytes[at..])?)))
      .filter(|(page_start, page_length)| page_start + page_length <= length)
      .collect();
    for (page_start, page_length) in starts.into_iter().take(tried_left) {
      tried_left -= 1;
      // at most a page's most bytes
      let mut page = vec![0; page_length as usize];
      source.seek(SeekFrom::Start(page_start))?;
      source.read_exact(&mut page)?;
      if sums(&page) {
        return Ok((length - page_start > PAGE_MAX).then_some(page_start + page_length));
      }
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

/// where a page's header holds its checksum, which is counted as zeros
const CHECKSUM: std::ops::Range<usize> = 22..26;

/// whether `page`, a whole page, holds the checksum of its own bytes: a CRC of 32 bits with
/// the polynomial 0x04c11db7, from zero, the high bit first
fn sums(page: &[u8]) -> bool {
  let crc = page.iter().enumerate().fold(0u32, |crc, (at, byte)| {
    let byte = if CHECKSUM.contains(&at) { 0 } else { *byte };
    (0..8).fold(crc ^ u32::from(byte) << 24, |crc, _| {
      if crc & 0x8000_0000 == 0 {
        crc << 1
      } else {
        crc << 1 ^ 0x04c1_1db7
      }
    })
  });
  page
    .get(CHECKSUM)
    .is_some_and(|stated| stated == crc.to_le_bytes())
}

/// a source that ends `length` bytes in, whatever its file holds after them
pub(super) struct Ended {
  inner: Box<dyn MediaSource>,
  length: u64,
  /// where the next read starts
  at: u64,
}

impl Ended {
  /// `inner`, which is at its start, ended `length` bytes in
  pub(super) fn new(inner: Box<dyn MediaSource>, length: u64) -> Self {
    Self {
      inner,
      length,
      at: 0,
    }
  }
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
    // the checksum of the page with zeros where it goes
    let crc = page.iter().fold(0u32, |crc, byte| {
      (0..8).fold(crc ^ u32::from(*byte) << 24, |crc, _| {
        if crc & 0x8000_0000 == 0 {
          crc << 1
        } else {
          crc << 1 ^ 0x04c1_1db7
        }
      })
    });
    page[CHECKSUM].copy_from_slice(&crc.to_le_bytes());
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
    // past one step back, and with markers in what follows: one whose page would pass the
    // end, and one whose page fits and does not sum
    assert_eq!(end_of(followed(3 * STEP + 5)), Some(audio));
    let mut marked = followed(3 * STEP);
    marked.extend_from_slice(b"OggS\0");
    marked.extend(vec![0xff; 1000]);
    assert_eq!(end_of(marked.clone()), Some(audio));
    marked.extend_from_slice(b"OggS\0");
    marked.extend(vec![0xff; 100_000]);
    assert_eq!(end_of(marked), Some(audio));
    assert_eq!(end_of(vec![0; 100_000]), None, "no Ogg file");
  }

  fn id3v1() -> Vec<u8> {
    [ID3V1, &[0; ID3V1_BYTES - 3]].concat()
  }

  /// an APE tag of 100 bytes of items, with a header when `header`
  fn ape(header: bool) -> Vec<u8> {
    let block = |flags: u32| -> Vec<u8> {
      let words = [2000, 100 + APE_FOOTER as u32, 1, flags];
      let words = words.iter().flat_map(|word| word.to_le_bytes());
      APE.iter().copied().chain(words).chain([0; 8]).collect()
    };
    let flags = if header { APE_HAS_HEADER } else { 0 };
    let start = if header {
      block(flags | 1 << 29)
    } else {
      Vec::new()
    };
    [start, vec![9; 100], block(flags)].concat()
  }

  fn lyrics2() -> Vec<u8> {
    let body = [LYRICS_START, &[b'x'; 40]].concat();
    [
      &body[..],
      format!("{:06}", body.len()).as_bytes(),
      LYRICS2_END,
    ]
    .concat()
  }

  fn lyrics1() -> Vec<u8> {
    [LYRICS_START, &[b'x'; 40], LYRICS_END].concat()
  }

  /// where the tags start in a file of `start`, 500 bytes and `tags`
  fn tags_start(start: &[u8], tags: &[Vec<u8>]) -> Option<u64> {
    let bytes = [start, &[7; 500], &tags.concat()].concat();
    let length = bytes.len() as u64;
    before_tags(&mut io::Cursor::new(bytes), length).unwrap()
  }

  #[test]
  fn tags_are_found_after_flac_alone() {
    assert_eq!(tags_start(b"fLaC", &[id3v1()]), Some(504));
    // an ID3v2 tag of 300 bytes after its header, then the stream
    let tagged = [&b"ID3\x04\0\0\0\0\x02\x2c"[..], &[0; 300], b"fLaC"].concat();
    assert_eq!(
      tags_start(&tagged, &[id3v1()]),
      Some(tagged.len() as u64 + 500)
    );
    assert_eq!(tags_start(b"RIFF", &[id3v1()]), None);
    assert_eq!(tags_start(b"fLaC", &[]), None);
    let short = before_tags(&mut io::Cursor::new(b"fLaC\0\0\0\0\0\0".to_vec()), 10);
    assert_eq!(short.unwrap(), None);
  }

  #[test]
  fn each_kind_of_tag_is_found_alone_and_before_the_others() {
    for tags in [
      vec![ape(true)],
      vec![ape(false)],
      vec![lyrics2()],
      vec![lyrics1()],
      vec![ape(true), id3v1()],
      vec![ape(false), lyrics2(), id3v1()],
      vec![lyrics1(), ape(true), id3v1()],
    ] {
      assert_eq!(tags_start(b"fLaC", &tags), Some(504), "{tags:?}");
    }
  }

  #[test]
  fn a_tag_that_states_more_than_the_file_holds_is_not_one() {
    // an APE footer that states 4 GB, one that states less than itself, and a Lyrics3 length
    // that leads to no start marker
    let mut long = ape(false);
    let size = long.len() - APE_FOOTER + 12;
    long[size..size + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut short = ape(false);
    short[size..size + 4].copy_from_slice(&8u32.to_le_bytes());
    let mut lyrics = lyrics2();
    lyrics[..LYRICS_START.len()].fill(b'x');
    let huge = [&[b'x'; 40][..], b"999999", LYRICS2_END].concat();
    for tag in [long, short, lyrics, huge] {
      assert_eq!(
        tags_start(b"fLaC", std::slice::from_ref(&tag)),
        None,
        "{tag:?}"
      );
    }
  }

  #[test]
  fn an_ended_source_reads_and_seeks_as_a_file_of_its_length() {
    let inner = Box::new(io::Cursor::new((0..=99u8).collect::<Vec<u8>>()));
    let mut source = Ended::new(inner, 10);
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
