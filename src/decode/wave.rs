//! gives the reader of a WAVE file the length of its samples where the file's own header does
//! not: a data size of 0 or of 0xFFFFFFFF, which a program writing to a pipe leaves, one that
//! wrapped at 32 bits, and an RF64 file's, which is 64 bits wide in a chunk of its own.
//!
//! symphonia-format-riff 0.6.1 reads the 32 bits of a RIFF file's data chunk as they are, so
//! it takes the first for a file of no audio and the third for a short one, and does not know
//! RF64 (wave/mod.rs, wave/chunks.rs). the source is handed over with the header's sizes
//! written as the file holds them, and an RF64 file's marker as RIFF's
//! TODO: remove once a symphonia release reads RF64 and reads a data chunk of no stated size
//! to the end of its file

use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

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
/// own are not. any other source comes back as it is, at its start
pub(super) fn sized(mut source: Box<dyn MediaSource>) -> Box<dyn MediaSource> {
  let Some(length) = source.byte_len().filter(|_| source.is_seekable()) else {
    return source;
  };
  // a source that fails here fails the same way when the reader opens it
  let patches = patches(source.as_mut(), length).unwrap_or_default();
  if source.seek(SeekFrom::Start(0)).is_err() || patches.is_empty() {
    return source;
  }
  Box::new(Patched {
    inner: source,
    patches,
    at: 0,
  })
}

/// the bytes to give the reader in place of the file's own, each with where it goes
fn patches(source: &mut dyn MediaSource, length: u64) -> io::Result<Vec<(u64, [u8; 4])>> {
  let mut form = [0; FORM as usize];
  source.seek(SeekFrom::Start(0))?;
  source.read_exact(&mut form)?;
  let rf64 = form.starts_with(RF64);
  if !(rf64 || form.starts_with(RIFF)) || !form.ends_with(WAVE) {
    return Ok(Vec::new());
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
      // more than 32 bits hold is written as not stated, which the reader reads to the end
      let fit = |size: u64| u32::try_from(size).unwrap_or(UNSTATED).to_le_bytes();
      let mut patches = Vec::new();
      if rf64 {
        patches.push((0, *RIFF));
      }
      // the file's own size goes with its samples', which the reader holds it to
      if rf64 || fit(samples) != stated.to_le_bytes() {
        patches.push((4, fit(length - CHUNK)));
        patches.push((at + 4, fit(samples)));
      }
      return Ok(patches);
    }
    // a chunk of an odd size is followed by a byte of padding
    at = body + u64::from(stated) + u64::from(stated % 2);
  }
  Ok(Vec::new())
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
    patches(&mut io::Cursor::new(bytes), length).unwrap_or_default()
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
    let mut source = sized(Box::new(io::Cursor::new(bytes.clone())));
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
