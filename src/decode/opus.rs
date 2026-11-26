//! what an Opus stream's packets and header state: each packet's coding mode, audio bandwidth
//! and length, and the frames a player drops from the stream's start

use std::fmt;

/// the rate Opus decodes at, whatever rate the encoder was given
pub(crate) const RATE: u32 = 48_000;

/// Coding mode an Opus packet states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum OpusMode {
  /// SILK alone, the linear-prediction coder made for voice.
  Silk,
  /// SILK up to 8 kHz with CELT above it, made for voice at higher bitrates.
  Hybrid,
  /// CELT alone, the transform coder music is encoded with.
  Celt,
}

impl OpusMode {
  /// Returns whether the mode codes voice with SILK, alone or under CELT.
  pub fn is_voice(self) -> bool {
    match self {
      Self::Silk | Self::Hybrid => true,
      Self::Celt => false,
    }
  }
}

impl fmt::Display for OpusMode {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Silk => "SILK",
      Self::Hybrid => "hybrid",
      Self::Celt => "CELT",
    })
  }
}

/// Audio bandwidth an Opus packet states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum OpusBandwidth {
  /// Narrowband, audio up to 4 kHz.
  Narrowband,
  /// Mediumband, audio up to 6 kHz.
  Mediumband,
  /// Wideband, audio up to 8 kHz.
  Wideband,
  /// Super-wideband, audio up to 12 kHz.
  SuperWideband,
  /// Fullband, audio up to 20 kHz.
  Fullband,
}

impl OpusBandwidth {
  /// Returns the top of the band in Hz.
  pub fn hz(self) -> f64 {
    match self {
      Self::Narrowband => 4_000.0,
      Self::Mediumband => 6_000.0,
      Self::Wideband => 8_000.0,
      Self::SuperWideband => 12_000.0,
      Self::Fullband => 20_000.0,
    }
  }
}

impl fmt::Display for OpusBandwidth {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Narrowband => "narrowband",
      Self::Mediumband => "mediumband",
      Self::Wideband => "wideband",
      Self::SuperWideband => "super-wideband",
      Self::Fullband => "fullband",
    })
  }
}

/// Time decoded Opus packets spend in one mode and bandwidth.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpusUse {
  /// Coding mode the packets state.
  pub mode: OpusMode,
  /// Audio bandwidth the packets state.
  pub bandwidth: OpusBandwidth,
  /// Length of the packets decoded in this mode and bandwidth, in seconds.
  pub seconds: f64,
}

impl OpusUse {
  /// Describes the first mode and bandwidth in `uses`.
  ///
  /// The first is the one with the most time when `uses` is ordered as
  /// [`Measured::opus`](crate::Measured::opus) orders it. SILK and hybrid are named as voice
  /// mode, and the share of the time is added when `uses` holds more than one: "CELT
  /// fullband", "SILK wideband, voice mode", "hybrid fullband, voice mode for 71% of the
  /// time". Returns `None` for no uses, as of a file that is not Opus.
  pub fn describe(uses: &[OpusUse]) -> Option<String> {
    let most = uses.first()?;
    let total: f64 = uses.iter().map(|u| u.seconds).sum();
    let voice = if most.mode.is_voice() {
      ", voice mode"
    } else {
      ""
    };
    let share = if uses.len() > 1 && total > 0.0 {
      format!(" for {:.0}% of the time", 100.0 * most.seconds / total)
    } else {
      String::new()
    };
    Some(format!("{} {}{voice}{share}", most.mode, most.bandwidth))
  }
}

/// the first byte of an Opus packet: its mode, bandwidth and frame length, and how its frames
/// are counted
struct Toc(u8);

impl Toc {
  /// the mode and bandwidth of the packet's configuration number, its top five bits
  fn coding(&self) -> (OpusMode, OpusBandwidth) {
    use OpusBandwidth::{Fullband, Mediumband, Narrowband, SuperWideband, Wideband};
    match self.0 >> 3 {
      0..=3 => (OpusMode::Silk, Narrowband),
      4..=7 => (OpusMode::Silk, Mediumband),
      8..=11 => (OpusMode::Silk, Wideband),
      12..=13 => (OpusMode::Hybrid, SuperWideband),
      14..=15 => (OpusMode::Hybrid, Fullband),
      16..=19 => (OpusMode::Celt, Narrowband),
      20..=23 => (OpusMode::Celt, Wideband),
      24..=27 => (OpusMode::Celt, SuperWideband),
      _ => (OpusMode::Celt, Fullband),
    }
  }

  /// the length of one frame at 48 kHz, in sample frames
  fn frame_length(&self) -> u32 {
    let config = self.0 >> 3;
    match config {
      // SILK: 10, 20, 40 and 60 ms
      0..=11 => [480, 960, 1920, 2880][usize::from(config % 4)],
      // hybrid: 10 and 20 ms
      12..=15 => [480, 960][usize::from(config % 2)],
      // CELT: 2.5, 5, 10 and 20 ms
      _ => [120, 240, 480, 960][usize::from(config % 4)],
    }
  }
}

/// the mode, bandwidth and length at 48 kHz in sample frames of `packet`; `None` for a packet
/// without the bytes that say
fn coding_of(packet: &[u8]) -> Option<(OpusMode, OpusBandwidth, u32)> {
  let toc = Toc(*packet.first()?);
  let frames = match toc.0 & 3 {
    0 => 1,
    1 | 2 => 2,
    // the count is the low six bits of the second byte
    _ => u32::from(*packet.get(1)? & 0x3f),
  };
  let (mode, bandwidth) = toc.coding();
  Some((mode, bandwidth, frames * toc.frame_length()))
}

/// the time decoded packets spend in each mode and bandwidth, in sample frames at 48 kHz
#[derive(Clone, Debug, Default)]
pub(crate) struct Tally(Vec<(OpusMode, OpusBandwidth, u64)>);

impl Tally {
  pub(crate) fn count(&mut self, packet: &[u8]) {
    let Some((mode, bandwidth, frames)) = coding_of(packet) else {
      return;
    };
    match self
      .0
      .iter_mut()
      .find(|(m, b, _)| (*m, *b) == (mode, bandwidth))
    {
      Some((_, _, total)) => *total += u64::from(frames),
      None => self.0.push((mode, bandwidth, u64::from(frames))),
    }
  }

  /// the modes and bandwidths counted, the one with the most time first
  pub(crate) fn uses(&self) -> Vec<OpusUse> {
    let mut counted = self.0.clone();
    counted.sort_by_key(|(_, _, frames)| std::cmp::Reverse(*frames));
    counted
      .into_iter()
      .map(|(mode, bandwidth, frames)| OpusUse {
        mode,
        bandwidth,
        seconds: frames as f64 / f64::from(RATE),
      })
      .collect()
  }
}

/// the identification header an Ogg stream would carry, from `head`, the signature followed by
/// what an MP4 stores: the same fields with a version of 0 and its numbers big-endian. a header
/// too short for a field keeps that field's bytes as they are
pub(crate) fn head_from_mp4(head: &[u8]) -> Vec<u8> {
  let mut head = head.to_vec();
  if let Some(version) = head.get_mut(8) {
    *version = 1;
  }
  // the pre-skip, the input rate and the output gain
  for field in [10..12, 12..16, 16..18] {
    if let Some(bytes) = head.get_mut(field) {
      bytes.reverse();
    }
  }
  head
}

/// the sample frames a player drops from the start of the stream whose identification header is
/// `head`, at 48 kHz: the header's pre-skip. `None` when the header is too short to hold it
pub(crate) fn pre_skip(head: &[u8]) -> Option<u64> {
  // eight bytes of signature, the version and the channel count come first
  let bytes = head.get(10..12)?;
  Some(u64::from(u16::from_le_bytes([bytes[0], bytes[1]])))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_packets_first_byte_states_its_mode_bandwidth_and_length() {
    // configuration 31 is CELT fullband at 20 ms; code 0 holds one frame
    assert_eq!(
      coding_of(&[31 << 3]),
      Some((OpusMode::Celt, OpusBandwidth::Fullband, 960))
    );
    // configuration 9 is SILK wideband at 20 ms; code 1 holds two frames
    assert_eq!(
      coding_of(&[(9 << 3) | 1, 0]),
      Some((OpusMode::Silk, OpusBandwidth::Wideband, 1920))
    );
    // configuration 12 is hybrid super-wideband at 10 ms; code 3 counts three frames in the
    // second byte, under its padding and variable-size flags
    assert_eq!(
      coding_of(&[(12 << 3) | 3, 0xc3]),
      Some((OpusMode::Hybrid, OpusBandwidth::SuperWideband, 1440))
    );
    // configuration 16 is CELT narrowband at 2.5 ms, 3 is SILK narrowband at 60 ms
    assert_eq!(
      coding_of(&[16 << 3]),
      Some((OpusMode::Celt, OpusBandwidth::Narrowband, 120))
    );
    assert_eq!(
      coding_of(&[3 << 3]),
      Some((OpusMode::Silk, OpusBandwidth::Narrowband, 2880))
    );
    assert_eq!(coding_of(&[]), None);
    assert_eq!(coding_of(&[3]), None, "a count that is not there");
  }

  #[test]
  fn the_use_with_the_most_time_comes_first() {
    let mut tally = Tally::default();
    tally.count(&[14 << 3]);
    for _ in 0..3 {
      tally.count(&[31 << 3]);
    }
    tally.count(&[14 << 3]);
    let uses = tally.uses();
    assert_eq!(
      uses,
      [
        OpusUse {
          mode: OpusMode::Celt,
          bandwidth: OpusBandwidth::Fullband,
          seconds: 0.06
        },
        OpusUse {
          mode: OpusMode::Hybrid,
          bandwidth: OpusBandwidth::Fullband,
          seconds: 0.02
        },
      ]
    );
    assert_eq!(
      OpusUse::describe(&uses).as_deref(),
      Some("CELT fullband for 75% of the time")
    );
    assert_eq!(
      OpusUse::describe(&uses[1..]).as_deref(),
      Some("hybrid fullband, voice mode")
    );
    assert_eq!(OpusUse::describe(&[]), None);
  }

  #[test]
  fn an_mp4s_header_is_turned_into_the_one_the_decoder_reads() {
    // version 0, two channels, a pre-skip of 312, 48 kHz, a gain of -256, mapping family 0
    let mut stored = b"OpusHead\x00\x02".to_vec();
    stored.extend_from_slice(&312u16.to_be_bytes());
    stored.extend_from_slice(&48_000u32.to_be_bytes());
    stored.extend_from_slice(&(-256i16).to_be_bytes());
    stored.push(0);
    let mut wanted = b"OpusHead\x01\x02".to_vec();
    wanted.extend_from_slice(&312u16.to_le_bytes());
    wanted.extend_from_slice(&48_000u32.to_le_bytes());
    wanted.extend_from_slice(&(-256i16).to_le_bytes());
    wanted.push(0);
    assert_eq!(head_from_mp4(&stored), wanted);
    // a header that ends inside a field keeps the bytes it has of it
    let short = head_from_mp4(&stored[..11]);
    assert_eq!((short[8], short[10]), (1, stored[10]));
  }

  #[test]
  fn the_pre_skip_is_read_from_the_header() {
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&312u16.to_le_bytes());
    assert_eq!(pre_skip(&head), Some(312));
    assert_eq!(pre_skip(&head[..11]), None);
  }
}
