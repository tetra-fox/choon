//! gives the reader of a WAVE file the length of its samples where the file's own header does
//! not: a data size of 0 or of 0xFFFFFFFF, which a program writing to a pipe leaves, one that
//! wrapped at 32 bits, and an RF64 file's, which is 64 bits wide in a chunk of its own.
//!
//! symphonia-format-riff 0.6.1 reads the 32 bits of a RIFF file's data chunk as they are, so
//! it takes a size of 0 for a file of no audio and a size that wrapped for a short file, and
//! does not know RF64 (wave/mod.rs, wave/chunks.rs). the source is handed over with the header's sizes
//! written as the file holds them, and an RF64 file's marker as RIFF's. samples whose size
//! 32 bits cannot state, which includes 0xFFFFFFFF itself, are written as of no stated size,
//! which the reader reads to the end of its source, so the source ends where they do, and the
//! end of the source is the end of the audio
//! TODO: remove once a symphonia release reads RF64, and reads a data chunk whose size is 0
//! or wrapped at 32 bits as long as the file holds it

use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

use super::ending::Ended;

const RIFF: &[u8; 4] = b"RIFF";
const RF64: &[u8; 4] = b"RF64";
const WAVE: &[u8; 4] = b"WAVE";
const DS64: &[u8; 4] = b"ds64";
const DATA: &[u8; 4] = b"data";
/// the bytes before a file's first chunk: its marker, its size and its form
const FORM: u64 = 12;
/// the bytes of a chunk's name and size
const CHUNK: u64 = 8;
/// the size a header states of what it does not know, and what one more than it wraps to
const UNSTATED: u32 = u32::MAX;
const WRAP: u64 = 1 << 32;
/// the chunks read before the samples are given up on
const CHUNKS: usize = 64;

/// `source`, with the sizes of a WAVE file's header as the file holds them where the header's
/// own are not, and whether the reader reads its samples to the end of the source, which is
/// where they end. any other source comes back as it is, at its start
pub(super) fn sized(mut source: Box<dyn MediaSource>) -> (Box<dyn MediaSource>, bool) {
  let Some(length) = source.byte_len().filter(|_| source.is_seekable()) else {
    return (source, false);
  };
  // a source that fails here fails the same way when the reader opens it
  let Sizes { patches, end } = sizes(source.as_mut(), length).unwrap_or_default();
  if source.seek(SeekFrom::Start(0)).is_err() {
    return (source, false);
  }
  if !patches.is_empty() {
    source = Box::new(Patched {
      inner: source,
      patches,
      at: 0,
    });
  }
  if let Some(end) = end.filter(|end| *end < length) {
    source = Box::new(Ended::new(source, end));
  }
  (source, end.is_some())
}

/// what the reader of a WAVE file is given in place of the file's own sizes
#[derive(Default)]
struct Sizes {
  /// the bytes in place of the file's own, each with where it goes
  patches: Vec<(u64, [u8; 4])>,
  /// where the samples end, when their size is written as unstated and the file holds all its
  /// header states of them
  end: Option<u64>,
}

/// the sizes the reader of the WAVE file in `source` is given; none for any other file
fn sizes(source: &mut dyn MediaSource, length: u64) -> io::Result<Sizes> {
  let mut form = [0; FORM as usize];
  source.seek(SeekFrom::Start(0))?;
  source.read_exact(&mut form)?;
  let rf64 = form.starts_with(RF64);
  if !(rf64 || form.starts_with(RIFF)) || !form.ends_with(WAVE) {
    return Ok(Sizes::default());
  }
  // the size of the samples an RF64 file states, from its first chunk
  let mut wide = None;
  let mut at = FORM;
  for _ in 0..CHUNKS {
    let mut chunk = [0; CHUNK as usize];
    source.seek(SeekFrom::Start(at))?;
    source.read_exact(&mut chunk)?;
    let stated = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
    let body = at + CHUNK;
    if rf64 && chunk.starts_with(DS64) {
      // the size of the file, then of the samples, each in 64 bits
      let mut sizes = [0; 16];
      source.read_exact(&mut sizes)?;
      let [.., a, b, c, d, e, f, g, h] = sizes;
      wide = Some(u64::from_le_bytes([a, b, c, d, e, f, g, h]));
    }
    if chunk.starts_with(DATA) {
      let held = length.saturating_sub(body);
      let samples = samples(stated, wide, held);
      // a size 32 bits cannot state is written as unstated, as is one of exactly `UNSTATED`
      let stating = |size: u64| u32::try_from(size).ok().filter(|size| *size != UNSTATED);
      let fit = |size: u64| stating(size).unwrap_or(UNSTATED).to_le_bytes();
      let mut patches = Vec::new();
      if rf64 {
        patches.push((0, *RIFF));
      }
      // the reader refuses a data chunk that runs past the file's own size, so that size holds
      // the samples, the ones a file cut short is missing too
      if rf64 || fit(samples) != stated.to_le_bytes() {
        patches.push((4, fit((body + samples).max(length) - CHUNK)));
        patches.push((at + 4, fit(samples)));
      }
      // an RF64 file that holds less than its own chunk states is read to its end as one cut
      let cut = stated == UNSTATED && wide.is_some_and(|wide| wide > held);
      let end = (stating(samples).is_none() && !cut).then_some(body + samples);
      return Ok(Sizes { patches, end });
    }
    // a chunk of an odd size is followed by a byte of padding
    at = body + u64::from(stated) + u64::from(stated % 2);
  }
  Ok(Sizes::default())
}

/// the bytes of samples in a data chunk that states `stated` of them, in a file that holds
/// `held` bytes from the chunk's start on, and whose RF64 chunk states `wide`
fn samples(stated: u32, wide: Option<u64>, held: u64) -> u64 {
  match (stated, wide) {
    (UNSTATED, Some(wide)) => wide.min(held),
    (0 | UNSTATED, _) => held,
    // a size that wrapped leaves whole turns of 32 bits after what it states
    (stated, _) => u64::from(stated) + held.saturating_sub(u64::from(stated)) / WRAP * WRAP,
  }
}

/// a source that gives `patches` in place of its file's bytes where they go
struct Patched {
  inner: Box<dyn MediaSource>,
  patches: Vec<(u64, [u8; 4])>,
  /// where the next read starts
  at: u64,
}

impl Read for Patched {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    let read = self.inner.read(buf)?;
    for (start, bytes) in &self.patches {
      for (offset, byte) in bytes.iter().enumerate() {
        // a patch's bytes that fall inside what was read
        let place = (start + offset as u64).checked_sub(self.at);
        if let Some(slot) =
          place.and_then(|place| buf[..read].get_mut(usize::try_from(place).ok()?))
        {
          *slot = *byte;
        }
      }
    }
    self.at += read as u64;
    Ok(read)
  }
}

impl Seek for Patched {
  fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
    self.at = self.inner.seek(to)?;
    Ok(self.at)
  }
}

impl MediaSource for Patched {
  fn is_seekable(&self) -> bool {
    self.inner.is_seekable()
  }

  fn byte_len(&self) -> Option<u64> {
    self.inner.byte_len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::decode::{CUT_SHORT, Decoder};
  use crate::fixtures;
  use symphonia::core::formats::probe::Hint;

  /// a WAVE file of `samples` bytes of samples that states `stated` of them, with a chunk of
  /// odd size before them and `after` bytes of another after them
  fn wave(marker: &[u8; 4], stated: u32, samples: usize, after: usize) -> Vec<u8> {
    let mut bytes = [
      &marker[..],
      &[0; 4],
      WAVE,
      b"fmt ",
      &16u32.to_le_bytes(),
      &[1; 16],
    ]
    .concat();
    bytes.extend_from_slice(b"LIST\x03\0\0\0abc\0");
    bytes.extend_from_slice(DATA);
    bytes.extend_from_slice(&stated.to_le_bytes());
    bytes.extend(std::iter::repeat_n(5, samples));
    bytes.extend(std::iter::repeat_n(6, after));
    bytes
  }

  fn patched(bytes: Vec<u8>) -> Vec<(u64, [u8; 4])> {
    let length = bytes.len() as u64;
    // a file too short to hold a chunk is none to patch, as in `sized`
    sizes(&mut io::Cursor::new(bytes), length)
      .unwrap_or_default()
      .patches
  }

  #[test]
  fn a_data_size_the_header_does_not_state_is_what_the_file_holds() {
    // the size is 4 bytes into the data chunk, after 12 of form, 24 of format and 12 of list
    let size = 12 + 24 + 12 + 4;
    assert_eq!(patched(wave(RIFF, 1000, 1000, 0)), []);
    assert_eq!(patched(wave(RIFF, 1000, 1000, 30)), [], "a chunk follows");
    for unstated in [0, UNSTATED] {
      let file = wave(RIFF, unstated, 1000, 0);
      let whole = (file.len() as u32 - 8).to_le_bytes();
      let found = patched(file);
      assert_eq!(
        found,
        [(4, whole), (size, 1000u32.to_le_bytes())],
        "{unstated}"
      );
    }
    assert_eq!(patched(b"RIFF\0\0\0\0AVI LIST".to_vec()), []);
    assert_eq!(patched(b"fLaC".to_vec()), []);
  }

  #[test]
  fn an_rf64_file_is_given_as_riff_with_the_size_its_own_chunk_states() {
    let mut bytes = [&RF64[..], &[0xff; 4], WAVE, DS64, &28u32.to_le_bytes()].concat();
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&1000u64.to_le_bytes());
    bytes.extend_from_slice(&[0; 12]);
    bytes.extend_from_slice(DATA);
    bytes.extend_from_slice(&UNSTATED.to_le_bytes());
    bytes.extend(std::iter::repeat_n(5, 1000));
    bytes.extend_from_slice(b"LIST\x02\0\0\0ab");
    let length = bytes.len() as u32;
    assert_eq!(
      patched(bytes.clone()),
      [
        (0, *RIFF),
        (4, (length - 8).to_le_bytes()),
        (12 + 36 + 4, 1000u32.to_le_bytes())
      ]
    );
    // the source reads as the file with those bytes in it, however it is read
    let (mut source, read_to_end) = sized(Box::new(io::Cursor::new(bytes.clone())));
    assert!(!read_to_end);
    let mut whole = Vec::new();
    source.read_to_end(&mut whole).unwrap();
    assert_eq!(whole[..4], *RIFF);
    assert_eq!(whole[52..56], 1000u32.to_le_bytes());
    assert_eq!(whole[56..], bytes[56..]);
    source.seek(SeekFrom::Start(2)).unwrap();
    let mut some = [0; 4];
    source.read_exact(&mut some).unwrap();
    assert_eq!(some, [b'F', b'F', whole[4], whole[5]]);
  }

  /// the format chunk of `channels` of `bits` at 44.1 kHz
  fn format(channels: u16, bits: u16) -> Vec<u8> {
    let block = channels * bits / 8;
    [
      &b"fmt "[..],
      &16u32.to_le_bytes(),
      &1u16.to_le_bytes(),
      &channels.to_le_bytes(),
      &44_100u32.to_le_bytes(),
      &(44_100 * u32::from(block)).to_le_bytes(),
      &block.to_le_bytes(),
      &bits.to_le_bytes(),
    ]
    .concat()
  }

  /// the header of an RF64 file of `format` whose own chunk states `wide` bytes of samples, up
  /// to the size of its data chunk, which states `stated`
  fn rf64_header(format: Vec<u8>, wide: u64, stated: u32) -> Vec<u8> {
    let mut bytes = [&RF64[..], &[0xff; 4], WAVE, DS64, &28u32.to_le_bytes()].concat();
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&wide.to_le_bytes());
    bytes.extend_from_slice(&[0; 12]);
    bytes.extend(format);
    bytes.extend_from_slice(DATA);
    bytes.extend_from_slice(&stated.to_le_bytes());
    bytes
  }

  #[test]
  fn an_rf64_file_cut_short_opens_as_the_same_riff_file_does() {
    let mut bytes = rf64_header(format(2, 16), 400_000, 400_000);
    let body = bytes.len() as u32;
    bytes.extend(std::iter::repeat_n(0, 200_000));
    let size = u64::from(body) - 4;
    assert_eq!(
      patched(bytes.clone()),
      [
        (0, *RIFF),
        (4, (body + 400_000 - 8).to_le_bytes()),
        (size, 400_000u32.to_le_bytes())
      ]
    );
    let mut decoder = Decoder::from_source(Box::new(io::Cursor::new(bytes)), &Hint::new()).unwrap();
    assert_eq!(decoder.claimed.frames, Some(100_000));
    let read = fixtures::rest(&mut decoder);
    assert_eq!(read[0].len(), 50_000);
    assert_eq!(decoder.stopped.as_deref(), Some(CUT_SHORT));
  }

  /// a file of `head`, `zeros` bytes of zeros that are counted and not held, and `tail`
  struct Sparse {
    head: Vec<u8>,
    zeros: u64,
    tail: Vec<u8>,
    at: u64,
  }

  impl Sparse {
    fn new(head: Vec<u8>, zeros: u64, tail: Vec<u8>) -> Self {
      Self {
        head,
        zeros,
        tail,
        at: 0,
      }
    }
  }

  impl Read for Sparse {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
      let head = self.head.len() as u64;
      let zeros_end = head + self.zeros;
      let read = if self.at < zeros_end && self.at >= head {
        let read =
          usize::try_from(zeros_end - self.at).map_or(buf.len(), |left| left.min(buf.len()));
        buf[..read].fill(0);
        read
      } else {
        let from = if self.at < head {
          &self.head[self.at as usize..]
        } else {
          self
            .tail
            .get((self.at - zeros_end) as usize..)
            .unwrap_or(&[])
        };
        let read = from.len().min(buf.len());
        buf[..read].copy_from_slice(&from[..read]);
        read
      };
      self.at += read as u64;
      Ok(read)
    }
  }

  impl Seek for Sparse {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
      self.at = match to {
        SeekFrom::Start(at) => at,
        SeekFrom::End(by) => self.byte_len().unwrap().checked_add_signed(by).unwrap(),
        SeekFrom::Current(by) => self.at.checked_add_signed(by).unwrap(),
      };
      Ok(self.at)
    }
  }

  impl MediaSource for Sparse {
    fn is_seekable(&self) -> bool {
      true
    }

    fn byte_len(&self) -> Option<u64> {
      Some(self.head.len() as u64 + self.zeros + self.tail.len() as u64)
    }
  }

  /// the frames read of `file` past its `samples` bytes in blocks of `block` bytes at 44.1 kHz,
  /// read from 3 s before their end, and why reading stopped
  fn read_past_the_end(file: Sparse, samples: u64, block: u64) -> (i64, Option<String>) {
    let mut decoder = Decoder::from_source(Box::new(file), &Hint::new()).unwrap();
    let frames = samples / block;
    let from = frames / 44_100 - 3;
    decoder.seek(from as f64).unwrap();
    let read = fixtures::rest(&mut decoder);
    let past = read[0].len() as i64 - (frames - from * 44_100) as i64;
    (past, decoder.stopped)
  }

  #[test]
  fn samples_32_bits_cannot_state_end_where_the_header_says() {
    // 4 GiB and 10 s of samples, then a chunk of bytes that are no audio
    let samples = WRAP + 1_764_000;
    let list = [&b"LIST"[..], &400_000u32.to_le_bytes(), &[0x55; 400_000]].concat();
    let rf64 = Sparse::new(
      rf64_header(format(2, 16), samples, UNSTATED),
      samples,
      list.clone(),
    );
    assert_eq!(read_past_the_end(rf64, samples, 4), (0, None), "RF64");
    let mut riff = [&RIFF[..], &[0; 4], WAVE].concat();
    riff.extend(format(2, 16));
    riff.extend_from_slice(DATA);
    riff.extend_from_slice(&(samples as u32).to_le_bytes());
    let wrapped = Sparse::new(riff, samples, list.clone());
    assert_eq!(read_past_the_end(wrapped, samples, 4), (0, None), "wrapped");
    // a size of exactly 0xFFFFFFFF is one the reader takes as unstated
    let whole = u64::from(UNSTATED);
    let mono = Sparse::new(rf64_header(format(1, 24), whole, UNSTATED), whole, list);
    assert_eq!(read_past_the_end(mono, whole, 3), (0, None), "0xFFFFFFFF");
    // an RF64 file that holds less than its own chunk states is still one cut short
    let cut = Sparse::new(
      rf64_header(format(2, 16), samples + 4000, UNSTATED),
      samples,
      Vec::new(),
    );
    let (past, stopped) = read_past_the_end(cut, samples, 4);
    assert_eq!(past, 0);
    assert_eq!(stopped.as_deref(), Some(CUT_SHORT), "cut");
  }

  #[test]
  fn a_size_that_wrapped_is_what_the_file_holds_in_whole_turns() {
    // 1000 stated of samples that are one turn and two turns of 32 bits longer, with a chunk
    // of 30 bytes after them, and of samples that are as long as stated
    assert_eq!(samples(1000, None, WRAP + 1030), WRAP + 1000);
    assert_eq!(samples(1000, None, 2 * WRAP + 1030), 2 * WRAP + 1000);
    assert_eq!(samples(1000, None, 1030), 1000);
    assert_eq!(
      samples(1000, None, 400),
      1000,
      "a file cut short states what it did"
    );
    assert_eq!(samples(UNSTATED, Some(5 * WRAP), 6 * WRAP), 5 * WRAP);
  }
}
