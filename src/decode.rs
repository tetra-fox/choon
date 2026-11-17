//! opens a file through symphonia and hands out its first two channels as floats in one-second
//! windows, noting the bits its integer samples use

use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::audio::sample::{i24, u24};
use symphonia::core::audio::{Audio, AudioBuffer};
use symphonia::core::codecs::audio::well_known::{
  CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_OPUS, CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW,
};
use symphonia::core::codecs::audio::{
  AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions,
};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::errors::{Error, SeekErrorKind};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase, Timestamp};
use symphonia_common::apple::audio::alac::MagicCookie;
use tracing::{debug, trace};

use crate::engine;
use crate::opus::{self, OpusUse};

/// the most channels the analyzer reads; the rest of a surround file is left out
const ANALYZED_CHANNELS: usize = 2;

/// the highest sample rate the analyzer reads; a window holds a second of samples, so a header
/// claiming more is not trusted with the memory
const MAX_RATE: u32 = 768_000;

/// how many undecodable packets are skipped before reading stops
const MAX_SKIPPED: u32 = 1000;

/// the audio an Opus decoder is given before a position sought to, in seconds. its band energies
/// are predicted from the frame before, and after this much what a fresh decoder predicts
/// differs from a read from the start by less than a millionth
const OPUS_PRE_ROLL: f64 = 0.5;

/// the codecs the analyzer decodes: symphonia's own, and Opus
fn codecs() -> &'static CodecRegistry {
  static CODECS: LazyLock<CodecRegistry> = LazyLock::new(|| {
    let mut codecs = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut codecs);
    codecs.register_audio_decoder::<symphonia_opus::OpusDecoder>();
    codecs
  });
  &CODECS
}

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

/// an audio file open for decoding
pub(crate) struct Decoder {
  format: Box<dyn FormatReader>,
  decoder: Box<dyn AudioDecoder>,
  track: u32,
  /// what the track's timestamps count, when the container says
  time_base: Option<TimeBase>,
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
  /// what an Opus stream's header and packets state
  opus: Option<Opus>,
}

/// what the analyzer keeps of an Opus stream beside its samples
struct Opus {
  /// decoded frames still to drop before the audio starts: the header's pre-skip, until a seek
  skip: u64,
  /// frames the track's timestamps count before the audio starts
  delay: u64,
  tally: opus::Tally,
}

/// a packet decoded: where the container puts it, and the frames at its ends that are not audio
struct Stepped {
  pts: i64,
  dur: u64,
  /// frames to drop from the start and the end of what the decoder gave
  trim: (usize, usize),
}

/// a hint of what the file at `path` holds: its extension
pub(crate) fn hint_of(path: &Path) -> Hint {
  let mut hint = Hint::new();
  if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
    hint.with_extension(extension);
  }
  hint
}

/// what filling a window gave
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Filled {
  /// the window is full
  Full,
  /// the file ended first; the window holds this many frames
  End(usize),
}

impl Decoder {
  /// opens audio from any source of bytes, with `hint` naming what it might be
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
    let mut params = track
      .codec_params
      .as_ref()
      .and_then(|p| p.audio())
      .ok_or_else(|| unsupported("the audio track has no codec parameters"))?
      .clone();
    // symphonia 0.6.1 hands an MP4's Opus header over as the container stores it, big-endian
    // behind version 0, where the decoder reads the little-endian header of version 1
    // (https://github.com/pdeljanov/Symphonia/pull/398)
    // TODO: remove once a symphonia release carries the fix
    if params.codec == CODEC_ID_OPUS
      && format.format_info().short_name == "isomp4"
      && let Some(head) = params.extra_data.as_deref().map(opus::head_from_mp4)
    {
      params.with_extra_data(head.into_boxed_slice());
    }
    let track_id = track.id;
    let time_base = track.time_base;
    let opus = (params.codec == CODEC_ID_OPUS).then(|| Opus {
      // a header too short to state it is one the decoder refuses below
      skip: params
        .extra_data
        .as_deref()
        .and_then(opus::pre_skip)
        .unwrap_or(0),
      delay: u64::from(track.delay.unwrap_or(0)),
      tally: opus::Tally::default(),
    });
    // a container that counts the frames before the audio in its timestamps counts them in the
    // length it states too
    let delay = opus.as_ref().map_or(0, |opus| opus.delay);
    let frames = track.num_frames.map(|frames| frames.saturating_sub(delay));
    let codecs = codecs();
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
      opus,
    })
  }

  /// the time the packets decoded so far spend in each Opus mode and bandwidth; empty for other
  /// codecs
  pub(crate) fn opus_uses(&self) -> Vec<OpusUse> {
    self
      .opus
      .as_ref()
      .map_or_else(Vec::new, |opus| opus.tally.uses())
  }

  /// the sample type's width less the low bits that were zero in every integer sample decoded so
  /// far; `None` for float samples and while every sample was zero
  pub(crate) fn bits_used(&self) -> Option<u32> {
    let (set, width) = self.bits?;
    (set != 0).then(|| width - set.trailing_zeros().min(width))
  }

  /// the audio's bitrate in kbit/s from the packets decoded so far: their bytes over their
  /// length, so tags and cover art around the stream do not count. `None` before any.
  pub(crate) fn bitrate_kbps(&self) -> Option<u32> {
    let (bytes, frames) = self.decoded;
    (frames > 0).then(|| {
      let seconds = frames as f64 / f64::from(self.claimed.sample_rate);
      (bytes as f64 * 8.0 / seconds / 1000.0).round() as u32
    })
  }

  /// the most seconds of audio the file's bytes can hold at the densest packet decoded so far.
  /// tables can point many packets at the same bytes, so a stated length can be longer
  pub(crate) fn most_seconds(&self) -> Option<f64> {
    let (bytes, frames) = self.densest?;
    let held = self.file_bytes? as f64 * frames as f64 / bytes as f64;
    Some(held / f64::from(self.claimed.sample_rate))
  }

  /// fills `window`, one vector per analyzed channel, with the next `frames` sample frames
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

  /// moves to `seconds` from the start, dropping what was decoded and not taken. the container
  /// moves to the start of a packet, so the frames before the position are decoded and dropped
  pub(crate) fn seek(&mut self, seconds: f64) -> Result<(), String> {
    // an Opus stream is sought to a little before the position, in the time its container
    // counts, and the frames decoded on the way are dropped with the others
    let rate = f64::from(self.claimed.sample_rate);
    let (at, lead) = match &self.opus {
      Some(opus) => {
        let position = seconds + opus.delay as f64 / rate;
        let lead = position.min(OPUS_PRE_ROLL);
        (position - lead, (lead * rate).round() as usize)
      }
      None => (seconds, 0),
    };
    let time = Time::try_from_secs_f64(at).ok_or("the position is out of range")?;
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
    if let Some(opus) = &mut self.opus {
      // the frames before the audio are among those dropped below
      opus.skip = 0;
    }
    // the frames from where the container landed to the position are decoded and dropped. a
    // landing past the position, which symphonia 0.6.1's Matroska reader can give, fails
    let ticks = sought
      .required_ts
      .checked_delta(sought.actual_ts)
      .ok_or("the position is out of range")?;
    let ticks =
      u128::try_from(ticks.get()).map_err(|_| "the container sought past the position")?;
    let mut early = self
      .frame_at(ticks)
      .and_then(|frames| usize::try_from(frames).ok())
      .and_then(|frames| frames.checked_add(lead))
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

  /// moves to the packet holding sample frame `frame`; false when the container cannot place it
  /// among the packets it indexes, as when another track ends before it, and may then not read on
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

  /// decodes the next packet into `samples`, interleaved, each sample at the top of its 32 bits.
  /// returns the frames the container says it covers, which the samples can fall short of or pass
  pub(crate) fn next_interleaved(&mut self, samples: &mut Vec<i32>) -> Option<Range<u64>> {
    let Stepped { pts, dur, .. } = self.step()?;
    self.decoder.last_decoded().copy_to_vec_interleaved(samples);
    let frames = self.frames_of(pts, dur);
    if frames.is_none() {
      debug!(pts, dur, "a packet's timestamp is out of range");
      self.stopped = Some("the file's timing is out of range".to_string());
    }
    frames
  }

  /// whether a sample frame lasts a whole number of the container's timestamp ticks, so that a
  /// packet's timestamp and duration place its first and last frame exactly
  pub(crate) fn ticks_fit_frames(&self) -> bool {
    let (numer, denom) = self.ticks();
    denom % (numer * u128::from(self.claimed.sample_rate)) == 0
  }

  /// the sample frames a packet at timestamp `pts` lasting `dur` covers; `None` when they are out
  /// of range
  fn frames_of(&self, pts: i64, dur: u64) -> Option<Range<u64>> {
    let pts = u128::try_from(pts).ok()?;
    Some(self.frame_at(pts)?..self.frame_at(pts + u128::from(dur))?)
  }

  /// the sample frame `ticks` timestamp ticks from the start, below 2^65; `None` when it is out
  /// of range
  fn frame_at(&self, ticks: u128) -> Option<u64> {
    let (numer, denom) = self.ticks();
    // a tick's numerator is below 2^32 and the rate at most `MAX_RATE`, below 2^20, so the
    // product fits
    let rate = u128::from(self.claimed.sample_rate);
    u64::try_from(ticks * numer * rate / denom).ok()
  }

  /// a timestamp tick as a fraction of a second; one sample frame when the container gives none
  fn ticks(&self) -> (u128, u128) {
    self.time_base.map_or_else(
      || (1, u128::from(self.claimed.sample_rate)),
      |base| (u128::from(base.numer.get()), u128::from(base.denom.get())),
    )
  }

  /// decodes the next packet of the track into `pending`; false when there is none
  fn decode_next(&mut self) -> bool {
    let shift = self.taken;
    for pending in &mut self.pending {
      pending.drain(..shift);
    }
    self.taken = 0;
    let Some(Stepped { trim, .. }) = self.step() else {
      return false;
    };
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
    let length = self.scratch[0].len();
    let (start, end) = (trim.0.min(length), length.saturating_sub(trim.1));
    // what is left of the frames before the audio comes off the packet's start too
    let skipped = match &mut self.opus {
      Some(opus) => {
        let skipped = usize::try_from(opus.skip)
          .unwrap_or(usize::MAX)
          .min(end.saturating_sub(start));
        opus.skip -= skipped as u64;
        skipped
      }
      None => 0,
    };
    let kept = (start + skipped).min(end)..end;
    for (pending, plane) in self.pending.iter_mut().zip(&self.scratch) {
      pending.extend_from_slice(&plane[kept.clone()]);
    }
    true
  }

  /// decodes the next packet of the track that holds samples and returns its timestamp, its
  /// duration and its trim; `None` at the end of the file or when decoding cannot go on, the
  /// reason in `stopped`
  fn step(&mut self) -> Option<Stepped> {
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
      // the Opus decoder hands back every frame of a packet, the ones the container says to
      // trim too. they are still there when the frames decoded are the packet's duration and
      // its trims together
      let (lead, tail) = (packet.trim_start.get(), packet.trim_end.get());
      let untrimmed = self.opus.is_some()
        && packet
          .dur
          .get()
          .checked_add(lead)
          .and_then(|d| d.checked_add(tail))
          == Some(frames);
      let trim = if untrimmed {
        // both are at most the frames decoded, which fit a usize
        (lead as usize, tail as usize)
      } else {
        (0, 0)
      };
      if let Some(opus) = &mut self.opus {
        opus.tally.count(&packet.data);
      }
      return Some(Stepped {
        pts: packet.pts.get(),
        dur: packet.dur.get(),
        trim,
      });
    }
  }

  /// counts a packet skipped; true, with the reason in `stopped`, once so many were skipped that
  /// reading on is not worth it
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

/// the bits per sample `params` state; ALAC states them in its magic cookie only. the decoder
/// made from `params` read the same cookie, so one that does not read here never gets this far
fn stated_bits(params: &AudioCodecParameters) -> Option<u32> {
  params.bits_per_sample.or_else(|| {
    let cookie = params.extra_data.as_deref()?;
    let read = (params.codec == CODEC_ID_ALAC).then(|| MagicCookie::read(cookie));
    Some(u32::from(read?.ok()?.bit_depth))
  })
}

/// whether a codec keeps every bit: the lossless codecs and plain PCM, but not the companded
/// telephone formats
fn lossless(codec: AudioCodecId) -> bool {
  if codec == CODEC_ID_FLAC || codec == CODEC_ID_ALAC {
    return true;
  }
  codec != CODEC_ID_PCM_ALAW
    && codec != CODEC_ID_PCM_MULAW
    && codecs()
      .get_audio_decoder(codec)
      .is_some_and(|registered| registered.codec.info.short_name.starts_with("pcm_"))
}

/// adds the bits set in the integer samples of `decoded` to `bits`. decoders put a sample's bits
/// at the top of its type, so bits never used show as zeros at the bottom
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
  use crate::fixtures::{self, OpusSettings, TONE};

  fn opened(bytes: Vec<u8>) -> Decoder {
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &hint).unwrap()
  }

  /// every sample of `bytes`, with what it claims
  fn decoded(bytes: Vec<u8>) -> (Claimed, Vec<Vec<f32>>) {
    let mut decoder = Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &Hint::new())
      .expect("the fixture opens");
    let rate = decoder.claimed.sample_rate as usize;
    let mut channels = vec![Vec::new(); decoder.channels];
    let mut window = vec![Vec::new(); decoder.channels];
    loop {
      let filled = decoder.fill(&mut window, rate);
      for (all, part) in channels.iter_mut().zip(&window) {
        all.extend_from_slice(part);
      }
      if filled != Filled::Full {
        break;
      }
    }
    assert_eq!(decoder.stopped, None);
    (decoder.claimed.clone(), channels)
  }

  fn rms(samples: &[f32]) -> f64 {
    (samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
  }

  /// the rms of the difference of `a` and `b` over the rms of `b`
  fn apart(a: &[f32], b: &[f32]) -> f64 {
    let difference: Vec<f32> = a.iter().zip(b).map(|(a, b)| a - b).collect();
    rms(&difference) / rms(b)
  }

  /// three seconds of a 1 kHz tone at 48 kHz, on `channels` channels
  fn tone_48k(channels: usize) -> fixtures::Audio {
    let tone = fixtures::tone(1000.0, 3).resampled(48_000);
    fixtures::Audio {
      rate: 48_000,
      channels: vec![tone.channels[0].clone(); channels],
    }
  }

  #[test]
  fn opus_decodes_to_the_length_rate_and_tone_it_was_made_from() {
    let settings = |bitrate: u32, frame: usize, voice: bool| OpusSettings {
      bitrate,
      frame,
      voice,
      gain: 0,
    };
    for (channels, settings) in [
      (2, settings(128_000, 960, false)),
      (2, settings(64_000, 960, false)),
      (1, settings(96_000, 480, false)),
      (2, settings(160_000, 240, false)),
      (1, settings(64_000, 120, false)),
      (1, settings(16_000, 1920, true)),
      (2, settings(32_000, 2880, true)),
    ] {
      let audio = tone_48k(channels);
      let stream = fixtures::opus(&audio, settings);
      let (claimed, samples) = decoded(stream.ogg());
      let said = format!("{channels} channels, {settings:?}");
      assert_eq!(claimed.codec, "opus", "{said}");
      assert_eq!(claimed.sample_rate, 48_000, "{said}");
      assert_eq!(claimed.channels as usize, channels, "{said}");
      assert!(!claimed.lossless, "{said}");
      assert_eq!(claimed.frames, Some(144_000), "{said}");
      assert_eq!(samples.len(), channels, "{said}");
      for channel in &samples {
        assert_eq!(channel.len(), 144_000, "{said}");
        // a tone of 1 kHz rises through zero a thousand times a second
        let rises = channel
          .windows(2)
          .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
          .count();
        assert!((2990..=3010).contains(&rises), "{said}: {rises} cycles");
      }
    }
  }

  #[test]
  fn an_opus_stream_starts_after_its_pre_skip_in_ogg_and_in_webm() {
    let audio = tone_48k(2);
    let stream = fixtures::opus(&audio, OpusSettings::music(128));
    assert!(stream.pre_skip > 0);
    let (_, ogg) = decoded(stream.ogg());
    // half a cycle off is twice the tone's level apart
    let off = apart(&ogg[0][1000..], &audio.channels[0][1000..]);
    assert!(off < 0.05, "{off}");
    let (claimed, webm) = decoded(stream.webm());
    assert_eq!(claimed.frames, None);
    // the container counts in milliseconds and keeps the last packet's padding
    let (length, most) = (webm[0].len(), 144_000 + 960);
    assert!((144_000..=most).contains(&length), "{length}");
    let off = apart(&webm[0][1000..144_000], &audio.channels[0][1000..]);
    assert!(off < 0.05, "{off}");
  }

  #[test]
  fn an_opus_headers_output_gain_scales_the_samples() {
    let audio = tone_48k(1);
    let plain = fixtures::opus(&audio, OpusSettings::music(96));
    let quieter = fixtures::OpusStream {
      settings: OpusSettings {
        // 6 dB down, in 1/256 dB
        gain: -6 * 256,
        ..plain.settings
      },
      ..plain.clone()
    };
    let (_, plain) = decoded(plain.ogg());
    let (_, quieter) = decoded(quieter.ogg());
    let ratio = rms(&quieter[0]) / rms(&plain[0]);
    assert!((ratio - 10f64.powf(-6.0 / 20.0)).abs() < 0.001, "{ratio}");
  }

  #[test]
  fn a_seek_in_opus_gives_the_samples_a_read_from_the_start_gives() {
    let audio = fixtures::music_for(12, 3, 48_000, fixtures::pink);
    let bytes = fixtures::opus(&audio, OpusSettings::music(128)).ogg();
    let (_, straight) = decoded(bytes.clone());
    let mut decoder =
      Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &Hint::new()).unwrap();
    let mut window = vec![Vec::new(); 2];
    for second in [5, 0, 11, 4] {
      decoder.seek(f64::from(second)).unwrap();
      assert_eq!(decoder.fill(&mut window, 48_000), Filled::Full);
      let from = second as usize * 48_000;
      let off = apart(&window[0], &straight[0][from..from + 48_000]);
      assert!(off < 1e-5, "second {second}: {off}");
    }
  }

  #[test]
  fn a_seek_that_lands_past_its_position_fails() {
    // the WebM's blocks state no duration, so its reader lands on the first block that starts
    // at or after a position, not on the one that holds it
    let audio = fixtures::music_for(12, 3, 48_000, fixtures::pink);
    let bytes = fixtures::opus(&audio, OpusSettings::music(128)).webm();
    let mut decoder =
      Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &Hint::new()).unwrap();
    assert_eq!(
      decoder.seek(5.0),
      Err("the container sought past the position".to_string())
    );
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
