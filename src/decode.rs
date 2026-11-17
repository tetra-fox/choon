//! Audio files read as samples, a little at a time.
//!
//! The container and codec are found by probing, packets are decoded one by one, and the
//! analyzer takes the first two channels as floats in one-second windows. Integer samples are
//! also looked at as they come, for the bits they actually use.

use std::ops::Range;
use std::path::Path;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::audio::sample::{i24, u24};
use symphonia::core::audio::{Audio, AudioBuffer};
use symphonia::core::codecs::audio::well_known::{
  CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW,
};
use symphonia::core::codecs::audio::{
  AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions,
};
use symphonia::core::errors::{Error, SeekErrorKind};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase, Timestamp};
use symphonia_common::apple::audio::alac::MagicCookie;
use tracing::{debug, trace};

use crate::engine;

/// The most channels the analyzer reads; the rest of a surround file is left out.
const ANALYZED_CHANNELS: usize = 2;

/// The highest sample rate the analyzer reads; a window holds a second of samples, so a header
/// claiming more is not trusted with the memory.
const MAX_RATE: u32 = 768_000;

/// How many undecodable packets are skipped before reading stops.
const MAX_SKIPPED: u32 = 1000;

/// Format a file's container and codec claim.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Claimed {
  /// Codec's short name, such as `flac` or `mp3`.
  pub codec: String,
  /// Samples per second.
  pub sample_rate: u32,
  /// Bits per sample, `None` for a codec without integer samples.
  pub bit_depth: Option<u32>,
  /// Number of channels in the file, analyzed or not.
  pub channels: u32,
  /// Whether the codec keeps every bit of the audio.
  pub lossless: bool,
  /// Length the container states, in sample frames, when it states one.
  pub frames: Option<u64>,
}

/// An audio file open for decoding.
pub(crate) struct Decoder {
  format: Box<dyn FormatReader>,
  decoder: Box<dyn AudioDecoder>,
  track: u32,
  /// what the track's timestamps count, when the container says
  time_base: Option<TimeBase>,
  /// the format the file claims
  pub(crate) claimed: Claimed,
  /// the channels analyzed: one for mono, else the first two
  pub(crate) channels: usize,
  /// decoded samples not taken yet, one vector per analyzed channel
  pending: Vec<Vec<f32>>,
  /// where the next sample of `pending` starts in `pending`
  taken: usize,
  /// a scratch buffer each packet is converted into
  scratch: Vec<Vec<f32>>,
  /// the bits set in any integer sample decoded, and the width of the sample type
  bits: Option<(u32, u32)>,
  /// the bytes and the sample frames of the packets decoded, for the stream's bitrate
  decoded: (u64, u64),
  /// how many bytes the file holds, when its source says
  file_bytes: Option<u64>,
  /// the bytes and the sample frames of the packet decoded so far that carried the most frames
  /// per byte
  densest: Option<(u64, u64)>,
  /// packets the decoder could not read and skipped
  pub(crate) skipped: u32,
  /// why decoding stopped before the end of the file, when it did
  pub(crate) stopped: Option<String>,
}

/// A hint of what the file at `path` holds: its extension.
pub(crate) fn hint_of(path: &Path) -> Hint {
  let mut hint = Hint::new();
  if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
    hint.with_extension(extension);
  }
  hint
}

/// What filling a window gave.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Filled {
  /// the window is full
  Full,
  /// the file ended first; the window holds this many frames
  End(usize),
}

impl Decoder {
  /// Opens audio from any source of bytes, with `hint` naming what it might be. The error says
  /// why, in words. Bytes are not audio when the container is not recognized or holds no audio
  /// track. Audio is unsupported when its track has no codec parameters, the codec is not one
  /// the analyzer decodes, the decoder cannot start, the sample rate or the channels are not
  /// stated, or the rate is above what the analyzer reads.
  pub(crate) fn from_source(
    source: Box<dyn MediaSource>,
    hint: &Hint,
  ) -> Result<Self, engine::Error> {
    let not_audio = |why: String| engine::Error::NotAudio(why);
    let unsupported = |why: &str| engine::Error::Unsupported(why.to_string());
    let file_bytes = source.byte_len();
    let stream = MediaSourceStream::new(source, Default::default());
    let format = symphonia::default::get_probe()
      .probe(
        hint,
        stream,
        FormatOptions::default(),
        MetadataOptions::default(),
      )
      .map_err(|e| not_audio(format!("the container was not recognized: {e}")))?;
    let track = format
      .default_track(TrackType::Audio)
      .ok_or_else(|| not_audio("the file holds no audio track".to_string()))?;
    let params = track
      .codec_params
      .as_ref()
      .and_then(|p| p.audio())
      .ok_or_else(|| unsupported("the audio track has no codec parameters"))?
      .clone();
    let track_id = track.id;
    let time_base = track.time_base;
    let frames = track.num_frames;
    let codecs = symphonia::default::get_codecs();
    let codec = codecs
      .get_audio_decoder(params.codec)
      .map(|registered| registered.codec.info.short_name.to_string())
      .ok_or_else(|| unsupported("the codec is not one the analyzer can decode"))?;
    let decoder = codecs
      .make_audio_decoder(&params, &AudioDecoderOptions::default())
      .map_err(|e| unsupported(&format!("the decoder could not start: {e}")))?;
    let sample_rate = params
      .sample_rate
      .filter(|rate| *rate > 0)
      .ok_or_else(|| unsupported("the sample rate is not stated"))?;
    if sample_rate > MAX_RATE {
      return Err(unsupported(&format!(
        "the sample rate of {sample_rate} Hz is above the {MAX_RATE} Hz the analyzer reads"
      )));
    }
    let all = params.channels.as_ref().map_or(0, |c| c.count());
    if all == 0 {
      return Err(unsupported("the number of channels is not stated"));
    }
    let channels = all.min(ANALYZED_CHANNELS);
    debug!(codec, sample_rate, channels = all, "audio opened");
    Ok(Self {
      format,
      decoder,
      track: track_id,
      time_base,
      claimed: Claimed {
        codec,
        sample_rate,
        bit_depth: stated_bits(&params),
        channels: u32::try_from(all).unwrap_or(u32::MAX),
        lossless: lossless(params.codec),
        frames,
      },
      channels,
      pending: vec![Vec::new(); channels],
      taken: 0,
      scratch: Vec::new(),
      bits: None,
      decoded: (0, 0),
      file_bytes,
      densest: None,
      skipped: 0,
      stopped: None,
    })
  }

  /// The bits per sample the integer samples decoded so far use: the sample type's width less
  /// the low bits that were zero in every one. `None` for float samples and while every sample
  /// was zero.
  pub(crate) fn bits_used(&self) -> Option<u32> {
    let (set, width) = self.bits?;
    (set != 0).then(|| width - set.trailing_zeros().min(width))
  }

  /// The audio's bitrate in kbit/s from the packets decoded so far: their bytes over their
  /// length, so tags and cover art around the stream do not count. `None` before any.
  pub(crate) fn bitrate_kbps(&self) -> Option<u32> {
    let (bytes, frames) = self.decoded;
    (frames > 0).then(|| {
      let seconds = frames as f64 / f64::from(self.claimed.sample_rate);
      (bytes as f64 * 8.0 / seconds / 1000.0).round() as u32
    })
  }

  /// The most seconds of audio the file's bytes can hold at the density of the packets decoded
  /// so far: the most sample frames per byte any of them carried. A file states its length in
  /// tables that can point many packets at the same bytes, and cannot hold more audio than its
  /// bytes at its densest packet, so the bound is as good as the packets read. `None` before a
  /// packet is decoded, or when the source does not say how long it is.
  pub(crate) fn most_seconds(&self) -> Option<f64> {
    let (bytes, frames) = self.densest?;
    let held = self.file_bytes? as f64 * frames as f64 / bytes as f64;
    Some(held / f64::from(self.claimed.sample_rate))
  }

  /// Fills `window`, one vector per analyzed channel, with the next `frames` sample frames.
  pub(crate) fn fill(&mut self, window: &mut [Vec<f32>], frames: usize) -> Filled {
    for channel in window.iter_mut() {
      channel.clear();
    }
    loop {
      let available = self.pending[0].len() - self.taken;
      let wanted = frames - window[0].len();
      let moved = available.min(wanted);
      for (channel, pending) in window.iter_mut().zip(&self.pending) {
        channel.extend_from_slice(&pending[self.taken..self.taken + moved]);
      }
      self.taken += moved;
      if window[0].len() == frames {
        return Filled::Full;
      }
      if !self.decode_next() {
        return Filled::End(window[0].len());
      }
    }
  }

  /// Moves to `seconds` from the start, dropping what was decoded and not taken. The container
  /// moves to the start of a packet, so the sample frames between it and the position are
  /// decoded and dropped.
  pub(crate) fn seek(&mut self, seconds: f64) -> Result<(), String> {
    let time = Time::try_from_secs_f64(seconds).ok_or("the position is out of range")?;
    let sought = self
      .format
      .seek(
        SeekMode::Accurate,
        SeekTo::Time {
          time,
          track_id: Some(self.track),
        },
      )
      .map_err(|e| format!("seeking failed: {e}"))?;
    self.decoder.reset();
    for pending in &mut self.pending {
      pending.clear();
    }
    self.taken = 0;
    // a seek that lands past the position has nothing to drop
    let ticks = sought
      .required_ts
      .checked_delta(sought.actual_ts)
      .map(|ticks| u128::try_from(ticks.get()).unwrap_or(0));
    let mut early = ticks
      .and_then(|ticks| self.frame_at(ticks))
      .and_then(|frames| usize::try_from(frames).ok())
      .ok_or("the position is out of range")?;
    while early > 0 {
      if self.pending[0].len() == self.taken && !self.decode_next() {
        break;
      }
      let dropped = early.min(self.pending[0].len() - self.taken);
      self.taken += dropped;
      early -= dropped;
    }
    Ok(())
  }

  /// Moves to the packet holding sample frame `frame`; false when the container cannot place
  /// the frame among the packets it indexes, which is also so when another track of the file
  /// ends before it, since every track is moved. The container may then not read on. The error
  /// says why, in words, when the position is out of range or the seek fails otherwise.
  pub(crate) fn seek_frame(&mut self, frame: u64) -> Result<bool, String> {
    let (numer, denom) = self.ticks();
    let rate = u128::from(self.claimed.sample_rate);
    let ticks = u128::from(frame) * denom / (numer * rate);
    let ts = i64::try_from(ticks).map_err(|_| "the position is out of range")?;
    let sought = self.format.seek(
      SeekMode::Accurate,
      SeekTo::Timestamp {
        ts: Timestamp::new(ts),
        track_id: self.track,
      },
    );
    match sought {
      Ok(_) => {
        self.decoder.reset();
        Ok(true)
      }
      Err(Error::SeekError(SeekErrorKind::OutOfRange)) => Ok(false),
      Err(e) => Err(format!("seeking failed: {e}")),
    }
  }

  /// Decodes the next packet into `samples` and returns the frames it covers. The samples are
  /// interleaved over every channel, each at the top of its 32 bits. The range is the sample
  /// frames the container says the packet covers, from its timestamp for its duration, which
  /// the samples can fall short of or run past. `None` at the end of the file or when decoding
  /// cannot go on, with the reason in `stopped`.
  pub(crate) fn next_interleaved(&mut self, samples: &mut Vec<i32>) -> Option<Range<u64>> {
    let (pts, dur) = self.step()?;
    self.decoder.last_decoded().copy_to_vec_interleaved(samples);
    let frames = self.frames_of(pts, dur);
    if frames.is_none() {
      debug!(pts, dur, "a packet's timestamp is out of range");
      self.stopped = Some("the file's timing is out of range".to_string());
    }
    frames
  }

  /// Whether a sample frame lasts a whole number of the container's timestamp ticks, so that a
  /// packet's timestamp and duration place its first and last frame exactly.
  pub(crate) fn ticks_fit_frames(&self) -> bool {
    let (numer, denom) = self.ticks();
    denom % (numer * u128::from(self.claimed.sample_rate)) == 0
  }

  /// The sample frames a packet at timestamp `pts` lasting `dur` covers; `None` when they are out
  /// of range.
  fn frames_of(&self, pts: i64, dur: u64) -> Option<Range<u64>> {
    let pts = u128::try_from(pts).ok()?;
    Some(self.frame_at(pts)?..self.frame_at(pts + u128::from(dur))?)
  }

  /// The sample frame `ticks` timestamp ticks from the start, below 2^65; `None` when it is out
  /// of range.
  fn frame_at(&self, ticks: u128) -> Option<u64> {
    let (numer, denom) = self.ticks();
    // a tick's numerator is below 2^32 and the rate at most `MAX_RATE`, below 2^20, so the
    // product fits
    let rate = u128::from(self.claimed.sample_rate);
    u64::try_from(ticks * numer * rate / denom).ok()
  }

  /// A timestamp tick as a fraction of a second; one sample frame when the container gives none.
  fn ticks(&self) -> (u128, u128) {
    self.time_base.map_or_else(
      || (1, u128::from(self.claimed.sample_rate)),
      |base| (u128::from(base.numer.get()), u128::from(base.denom.get())),
    )
  }

  /// Decodes the next packet of the track into `pending`; false at the end of the file or when
  /// decoding cannot go on, with the reason in `stopped`.
  fn decode_next(&mut self) -> bool {
    let shift = self.taken;
    for pending in &mut self.pending {
      pending.drain(..shift);
    }
    self.taken = 0;
    if self.step().is_none() {
      return false;
    }
    self
      .decoder
      .last_decoded()
      .copy_to_vecs_planar::<f32>(&mut self.scratch);
    // the analyzed channels must stay the same length
    if self.scratch.len() < self.channels {
      debug!(
        planes = self.scratch.len(),
        "a packet holds fewer channels than the stream"
      );
      self.stopped = Some("a packet holds fewer channels than the stream".to_string());
      return false;
    }
    for (pending, plane) in self.pending.iter_mut().zip(&self.scratch) {
      pending.extend_from_slice(plane);
    }
    true
  }

  /// Decodes the next packet of the track that holds samples, which the decoder then holds, and
  /// returns its timestamp and duration; `None` at the end of the file or when decoding cannot
  /// go on, with the reason in `stopped`.
  fn step(&mut self) -> Option<(i64, u64)> {
    loop {
      let packet = match self.format.next_packet() {
        Ok(Some(packet)) => packet,
        Ok(None) => return None,
        Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
          debug!("audio ends early");
          self.stopped = Some("the file ends before its last packet".to_string());
          return None;
        }
        Err(e) => {
          debug!(error = %e, "audio could not be read on");
          self.stopped = Some(format!("reading stopped: {e}"));
          return None;
        }
      };
      if packet.track_id != self.track {
        continue;
      }
      let decoded = match self.decoder.decode(&packet) {
        Ok(decoded) => decoded,
        Err(Error::DecodeError(reason)) => {
          trace!(reason, "audio packet skipped");
          if self.skip() {
            return None;
          }
          continue;
        }
        Err(Error::IoError(e)) => {
          trace!(error = %e, "audio packet skipped");
          if self.skip() {
            return None;
          }
          continue;
        }
        Err(e) => {
          debug!(error = %e, "audio could not be decoded on");
          self.stopped = Some(format!("decoding stopped: {e}"));
          return None;
        }
      };
      if decoded.frames() == 0 {
        continue;
      }
      note_bits(&mut self.bits, &decoded);
      let (bytes, frames) = (packet.data.len() as u64, decoded.frames() as u64);
      self.decoded.0 += bytes;
      self.decoded.1 += frames;
      // a packet of no bytes says nothing of how much audio a byte can carry
      if bytes > 0 && self.densest.is_none_or(|(b, f)| frames * b > f * bytes) {
        self.densest = Some((bytes, frames));
      }
      return Some((packet.pts.get(), packet.dur.get()));
    }
  }

  /// Counts a packet skipped; true, with the reason in `stopped`, once so
  /// many were skipped that reading on is not worth it.
  fn skip(&mut self) -> bool {
    self.skipped += 1;
    if self.skipped < MAX_SKIPPED {
      return false;
    }
    debug!(skipped = self.skipped, "too many audio packets skipped");
    self.stopped = Some(format!(
      "reading stopped after {MAX_SKIPPED} packets could not be decoded"
    ));
    true
  }
}

/// The bits per sample `params` state. A container says nothing of them for ALAC, whose magic
/// cookie does; the decoder made from `params` read the same cookie, so a cookie that does not
/// read here never gets this far.
fn stated_bits(params: &AudioCodecParameters) -> Option<u32> {
  params.bits_per_sample.or_else(|| {
    let cookie = params.extra_data.as_deref()?;
    let read = (params.codec == CODEC_ID_ALAC).then(|| MagicCookie::read(cookie));
    Some(u32::from(read?.ok()?.bit_depth))
  })
}

/// Whether a codec keeps every bit: the lossless codecs and plain PCM, but not the companded
/// telephone formats.
fn lossless(codec: AudioCodecId) -> bool {
  if codec == CODEC_ID_FLAC || codec == CODEC_ID_ALAC {
    return true;
  }
  codec != CODEC_ID_PCM_ALAW
    && codec != CODEC_ID_PCM_MULAW
    && symphonia::default::get_codecs()
      .get_audio_decoder(codec)
      .is_some_and(|registered| registered.codec.info.short_name.starts_with("pcm_"))
}

/// Adds the bits set in the integer samples of `decoded` to `bits`, kept with the sample
/// type's width. Decoders put a sample's bits at the top of its type, so bits never used show
/// as zeros at the bottom.
fn note_bits(bits: &mut Option<(u32, u32)>, decoded: &GenericAudioBufferRef<'_>) {
  fn set<S: symphonia::core::audio::sample::Sample>(
    buffer: &AudioBuffer<S>,
    raw: impl Fn(S) -> u32,
  ) -> u32 {
    buffer
      .iter_planes()
      .flat_map(|plane| plane.iter().copied())
      .fold(0, |set, sample| set | raw(sample))
  }
  let (found, width) = match decoded {
    GenericAudioBufferRef::U8(b) => (set(b, u32::from), 8),
    GenericAudioBufferRef::S8(b) => (set(b, |s| u32::from(s as u8)), 8),
    GenericAudioBufferRef::U16(b) => (set(b, u32::from), 16),
    GenericAudioBufferRef::S16(b) => (set(b, |s| u32::from(s as u16)), 16),
    GenericAudioBufferRef::U24(b) => (set(b, |s: u24| s.inner() & 0xff_ffff), 24),
    GenericAudioBufferRef::S24(b) => (set(b, |s: i24| s.inner() as u32 & 0xff_ffff), 24),
    GenericAudioBufferRef::U32(b) => (set(b, |s| s), 32),
    GenericAudioBufferRef::S32(b) => (set(b, |s| s as u32), 32),
    _ => return,
  };
  let (before, _) = bits.unwrap_or((0, width));
  *bits = Some((before | found, width));
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::fixtures::TONE;

  fn opened(bytes: Vec<u8>) -> Decoder {
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &hint).unwrap()
  }

  #[test]
  fn alac_states_its_bit_depth_in_its_magic_cookie() {
    assert_eq!(opened(TONE.to_vec()).claimed.bit_depth, Some(16));
    // the cookie is the last 24 bytes of the 36-byte `alac` box inside the sample entry; its
    // bit depth is the sixth of them
    let mut bytes = TONE.to_vec();
    let inner = bytes
      .windows(8)
      .position(|w| w == [0, 0, 0, 0x24, b'a', b'l', b'a', b'c'])
      .unwrap();
    let depth = inner + 8 + 4 + 5;
    assert_eq!(bytes[depth], 16);
    bytes[depth] = 24;
    assert_eq!(opened(bytes).claimed.bit_depth, Some(24));
  }
}
