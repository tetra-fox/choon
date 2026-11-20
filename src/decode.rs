//! opens a file through symphonia and hands out its first two channels as floats in one-second
//! windows, noting the bits its integer samples use

use std::fmt;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::LazyLock;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::audio::sample::{i24, u24};
use symphonia::core::audio::{Audio, AudioBuffer};
use symphonia::core::codecs::audio::well_known::{
  CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_MONKEYS_AUDIO, CODEC_ID_MP1, CODEC_ID_MP2,
  CODEC_ID_MP3, CODEC_ID_OPUS, CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW, CODEC_ID_WAVPACK,
};
use symphonia::core::codecs::audio::{
  AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions,
};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::errors::{Error, SeekErrorKind};
use symphonia::core::formats::probe::{Hint, Probe};
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Timestamp};
use symphonia_common::apple::audio::alac::MagicCookie;
use tracing::{debug, trace, warn};

use crate::engine;

mod aac;
mod ape;
pub(crate) mod dsd;
mod ending;
pub(crate) mod opus;
mod wave;
mod wavpack;

use dsd::CODEC_ID_DSD;
use opus::OpusUse;

/// the most channels the analyzer reads; the rest of a surround file is left out
const ANALYZED_CHANNELS: usize = 2;

/// the highest sample rate the analyzer reads; a window holds a second of samples, so a header
/// claiming more is not trusted with the memory
const MAX_RATE: u32 = 768_000;

/// what is said of a read that came to the end of the file inside a packet. a FLAC file's
/// last frame runs to the end of the file for its reader, so bytes after it read as this too
const CUT_SHORT: &str = "the last packet is cut short, or bytes that are no audio follow it";

/// how many undecodable packets are skipped before reading stops
const MAX_SKIPPED: u32 = 1000;

/// the audio an Opus decoder is given before a position sought to, in seconds. its band energies
/// are predicted from the frame before, and after this much what a fresh decoder predicts
/// differs from a read from the start by less than a millionth
const OPUS_PRE_ROLL: f64 = 0.5;

/// the sample frames an HE-AAC decoder is given before a position sought to: its band
/// replication takes two of its frames to start
const SBR_PRE_ROLL: u64 = 4096;

/// the sample frames a DSD decoder is given before a position sought to, which is more than
/// its filter holds
const DSD_PRE_ROLL: u64 = 1024;

/// the codecs the analyzer decodes: symphonia's own, Opus, WavPack, Monkey's Audio, DSD, and
/// AAC through a decoder that reads HE-AAC, in the place of symphonia's
fn codecs() -> &'static CodecRegistry {
  static CODECS: LazyLock<CodecRegistry> = LazyLock::new(|| {
    let mut codecs = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut codecs);
    codecs.register_audio_decoder::<aac::AacDecoder>();
    codecs.register_audio_decoder::<symphonia_opus::OpusDecoder>();
    codecs.register_audio_decoder::<wavpack::WavPackDecoder>();
    codecs.register_audio_decoder::<ape::ApeDecoder>();
    codecs.register_audio_decoder::<dsd::DsdDecoder>();
    codecs
  });
  &CODECS
}

/// why a reader here does not read a file it knows. symphonia's probe hands a reader's error
/// back as it is, so the reason goes through it as the payload of an io error
#[derive(Debug)]
struct Refused(&'static str);

impl fmt::Display for Refused {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.0)
  }
}

impl std::error::Error for Refused {}

/// the error of a reader here that knows the file and says `why` it does not read it
fn refused<T>(why: &'static str) -> symphonia::core::errors::Result<T> {
  Err(Error::IoError(std::io::Error::other(Refused(why))))
}

/// the timestamp a reader here is asked to seek to, on its one track with time base `base`:
/// an error when the source cannot seek, or when a time has no timestamp on the track
fn seek_timestamp(
  seekable: bool,
  base: Option<TimeBase>,
  to: SeekTo,
) -> symphonia::core::errors::Result<Timestamp> {
  if !seekable {
    return Err(Error::SeekError(SeekErrorKind::Unseekable));
  }
  match to {
    SeekTo::Timestamp { ts, .. } => Ok(ts),
    SeekTo::Time { time, .. } => base
      .and_then(|base| base.calc_timestamp(time))
      .ok_or(Error::SeekError(SeekErrorKind::OutOfRange)),
  }
}

/// a call into a container's reader once it is open, a panic in it given as an error.
/// symphonia-format-isomp4 0.6.1 adds an MP4's sample counts in 32 bits (atoms/stts.rs,
/// find_timing_for_sample), which panics in a build with overflow checks on a table whose
/// counts pass 2^32. the default panic hook still prints it, and panic = "abort" still aborts
// TODO: remove once a symphonia release checks that sum
fn caught<T>(
  call: impl FnOnce() -> symphonia::core::errors::Result<T>,
) -> symphonia::core::errors::Result<T> {
  catch_unwind(AssertUnwindSafe(call)).unwrap_or_else(|_| {
    warn!("a panic in a container's reader was caught, the file read no further");
    Err(Error::DecodeError("the container's reader failed"))
  })
}

/// the containers the analyzer reads: symphonia's own, WavPack, Monkey's Audio, DSF and DSDIFF
fn probe() -> &'static Probe {
  static PROBE: LazyLock<Probe> = LazyLock::new(|| {
    let mut probe = Probe::new();
    symphonia::default::register_enabled_formats(&mut probe);
    probe.register_format::<wavpack::WavPackReader<'_>>();
    probe.register_format::<ape::ApeReader<'_>>();
    probe.register_format::<dsd::DsdReader<'_>>();
    probe
  });
  &PROBE
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

/// What an HE-AAC stream's decoder rebuilt beside its AAC-LC core.
///
/// The core codes the lower half of the band at half the sample rate. Spectral band
/// replication rebuilds the upper half from the lower with a few bits of guidance, so the band
/// above the core holds none of the source's own waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct HeAac {
  /// Sample rate of the core, in Hz.
  ///
  /// It is half the rate of the decoded audio.
  pub core_rate: u32,
  /// Whether parametric stereo rebuilt the two channels from one.
  ///
  /// An HE-AAC v2 stream is coded this way.
  pub parametric_stereo: bool,
}

impl HeAac {
  /// Returns the top of the band the core can code, in Hz.
  ///
  /// It is half the core's sample rate. The core stops at or under it, and everything above
  /// was rebuilt.
  pub fn core_top_hz(&self) -> f64 {
    f64::from(self.core_rate) / 2.0
  }
}

impl fmt::Display for HeAac {
  /// Writes the profile's name, `HE-AAC` or `HE-AAC v2`.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(if self.parametric_stereo {
      "HE-AAC v2"
    } else {
      "HE-AAC"
    })
  }
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
  /// the bytes after an Ogg file's last page that the reader was not given
  pub(crate) left_out: Option<u64>,
  /// the audio tracks the file holds beside the one read
  pub(crate) other_tracks: usize,
  /// whether the file's source can be sought in
  pub(crate) seekable: bool,
  /// what an Opus stream's header and packets state
  opus: Option<Opus>,
  /// what places the padding of an MPEG audio stream, whose packets are trimmed here
  mpeg: Option<Mpeg>,
  /// the sample frames a seek can decode and drop before its position, where the format's
  /// packets are long enough for that to matter: Monkey's Audio's frames, of up to 27 s
  pub(crate) seek_decodes: u64,
  /// what an HE-AAC stream's decoder rebuilt
  pub(crate) he_aac: Option<HeAac>,
  /// the one-bit sample rate of a DSD file
  pub(crate) dsd_rate: Option<u32>,
}

/// what the analyzer keeps of an Opus stream beside its samples
struct Opus {
  /// decoded frames still to drop before the audio starts: the header's pre-skip, until a seek
  skip: u64,
  /// frames the track's timestamps count before the audio starts
  delay: u64,
  tally: opus::Tally,
}

/// what the analyzer keeps of an MPEG audio stream beside its samples.
///
/// symphonia-bundle-mp3 0.6.1 states a length for a file with no Xing or VBRI header too, a
/// guess from the first frames' bitrate and the file's size (demuxer.rs, `try_new`), and marks
/// every frame past the length it states as trim. its decoder drops what is marked, so a file
/// longer than the guess, or than a header's wrong count, loses its end. the decoder is made
/// without that trimming, and a packet's end is trimmed here only inside the padding
/// TODO: let the decoder trim once symphonia's reader says whether its length is a guess
struct Mpeg {
  /// the timestamp the padding after the audio ends at, when the track states a length
  padded_end: Option<u64>,
  /// whether a LAME tag states the padding, which comes with a header's count of the frames
  counted: bool,
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
    let (source, left_out) = ending::ended(wave::sized(source));
    let seekable = source.is_seekable();
    let file_bytes = source.byte_len();
    let stream = MediaSourceStream::new(source, Default::default());
    // symphonia-format-riff 0.6.1 multiplies a WAVE file's channel count by its bytes a sample
    // in 16 bits (wave/chunks.rs, expected_block_align), which panics in a build with overflow
    // checks on a header that states thousands of channels. panic = "abort" still aborts
    // TODO: remove once a symphonia release checks that product
    let probing = AssertUnwindSafe(|| {
      probe().probe(
        hint,
        stream,
        FormatOptions::default(),
        MetadataOptions::default(),
      )
    });
    let format = catch_unwind(probing)
      .map_err(|_| {
        warn!("a panic in a container's reader was caught, the file not read");
        not_audio("the container was not recognized: its reader failed on the header".to_string())
      })?
      .map_err(|e| {
        let refused = match &e {
          Error::IoError(io) => io.get_ref().and_then(|inner| inner.downcast_ref()),
          _ => None,
        };
        match refused {
          Some(Refused(why)) => unsupported(why),
          // symphonia's probe found no reader for the bytes, which the reason already says
          None if matches!(e, Error::Unsupported(_)) => {
            debug!(error = %e, "container not recognized");
            not_audio("the container was not recognized".to_string())
          }
          None => not_audio(format!("the container was not recognized: {e}")),
        }
      })?;
    let track = format
      .default_track(TrackType::Audio)
      .ok_or_else(|| not_audio("the file holds no audio track".to_string()))?;
    let audio_tracks = format
      .tracks()
      .iter()
      .filter(|track| {
        track
          .codec_params
          .as_ref()
          .is_some_and(|p| p.audio().is_some())
      })
      .count();
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
    let opus_in_mp4 = params.codec == CODEC_ID_OPUS && format.format_info().short_name == "isomp4";
    if opus_in_mp4 && let Some(head) = params.extra_data.as_deref().map(opus::head_from_mp4) {
      params.with_extra_data(head.into_boxed_slice());
    }
    let track_id = track.id;
    let time_base = track.time_base;
    let opus = (params.codec == CODEC_ID_OPUS).then(|| {
      // a header too short to state it is one the decoder refuses below
      let pre_skip = params
        .extra_data
        .as_deref()
        .and_then(opus::pre_skip)
        .unwrap_or(0);
      Opus {
        skip: pre_skip,
        // an MP4's timestamps count the pre-skip, and symphonia states no delay for it
        delay: if opus_in_mp4 {
          pre_skip
        } else {
          u64::from(track.delay.unwrap_or(0))
        },
        tally: opus::Tally::default(),
      }
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
    let mpeg = matches!(params.codec, CODEC_ID_MP1 | CODEC_ID_MP2 | CODEC_ID_MP3).then(|| Mpeg {
      padded_end: track
        .num_frames
        .map(|frames| frames.saturating_add(u64::from(track.padding.unwrap_or(0)))),
      counted: track.padding.is_some(),
    });
    let mut options = AudioDecoderOptions::default();
    options.gapless = mpeg.is_none();
    let decoder = codecs
      .make_audio_decoder(&params, &options)
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
    let core = (params.codec == CODEC_ID_AAC).then(|| aac::core(&params));
    // an AAC stream's config states its channels where its track does not, as symphonia's MP4
    // reader leaves them out for an object type it does not know, AAC-ELD among them
    let all = params
      .channels
      .as_ref()
      .map(|c| c.count())
      .or_else(|| core.flatten().map(|core| core.channels))
      .unwrap_or(0);
    if all == 0 {
      return Err(unsupported("the number of channels is not stated"));
    }
    let channels = all.min(ANALYZED_CHANNELS);
    let ticks = track.num_frames;
    let mut opened = Self {
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
      left_out,
      other_tracks: audio_tracks.saturating_sub(1),
      seekable,
      opus,
      mpeg,
      seek_decodes: (params.codec == CODEC_ID_MONKEYS_AUDIO)
        .then(|| params.extra_data.as_deref().and_then(ape::frame_blocks))
        .flatten()
        .map_or(0, u64::from),
      he_aac: None,
      // the reader passes the decoder the one-bit rate as its extra data
      dsd_rate: (params.codec == CODEC_ID_DSD)
        .then(|| params.extra_data.as_deref()?.try_into().ok())
        .flatten()
        .map(u32::from_le_bytes),
    };
    if let Some(core) = core {
      opened.prime(core, ticks)?;
    }
    debug!(
      codec = opened.claimed.codec,
      sample_rate = opened.claimed.sample_rate,
      channels = opened.claimed.channels,
      "audio opened"
    );
    Ok(opened)
  }

  /// decodes an AAC stream's first packet, whose samples say what the track cannot: the rate
  /// and channels the decoder gives, which are twice the core's rate with band replication
  /// and two channels from one with parametric stereo. `ticks` is the length the track states
  fn prime(&mut self, core: Option<aac::Core>, ticks: Option<u64>) -> Result<(), engine::Error> {
    let unsupported = |why: String| engine::Error::Unsupported(why);
    let Some(Stepped { trim, .. }) = self.step() else {
      let why = self.stopped.take();
      return Err(unsupported(why.unwrap_or_else(|| {
        "the stream holds no packet the decoder reads".to_string()
      })));
    };
    let decoded = self.decoder.last_decoded();
    let (rate, all) = (decoded.spec().rate(), decoded.spec().channels().count());
    if rate > MAX_RATE {
      return Err(unsupported(format!(
        "the sample rate of {rate} Hz is above the {MAX_RATE} Hz the analyzer reads"
      )));
    }
    self.claimed.sample_rate = rate;
    self.claimed.channels = u32::try_from(all).unwrap_or(u32::MAX);
    // the track's ticks are the core's frames or the output's, whichever its container counts
    self.claimed.frames = ticks.and_then(|ticks| self.frame_at(u128::from(ticks)));
    self.channels = all.min(ANALYZED_CHANNELS);
    self.pending = vec![Vec::new(); self.channels];
    self.he_aac = core
      .filter(|core| core.rate.checked_mul(2) == Some(rate))
      .map(|core| HeAac {
        core_rate: core.rate,
        parametric_stereo: core.channels == 1 && all == 2,
      });
    self.keep(trim);
    Ok(())
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

  /// whether the length the file states is a count its header holds. an MPEG audio file's is
  /// taken for one only beside a LAME tag: without one it can be the reader's guess
  pub(crate) fn length_is_counted(&self) -> bool {
    self.mpeg.as_ref().is_none_or(|mpeg| mpeg.counted)
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
    // an Opus, HE-AAC or DSD stream is sought to a little before the position, in the time
    // its container counts, and the frames decoded on the way are dropped with the others
    let rate = self.claimed.sample_rate;
    let (delay, pre_roll) = match (&self.opus, self.he_aac, self.dsd_rate) {
      (Some(opus), ..) => (opus.delay, (OPUS_PRE_ROLL * f64::from(rate)) as u64),
      (None, Some(_), _) => (0, SBR_PRE_ROLL),
      (None, None, Some(_)) => (0, DSD_PRE_ROLL),
      (None, None, None) => (0, 0),
    };
    // the position and the place sought to are counted in whole sample frames, and the timestamp
    // is worked out from them. a time in seconds is rounded on its way to a timestamp, and a
    // tick lost there starts the window early
    let position = ((seconds * f64::from(rate)).round() as u64)
      .checked_add(delay)
      .ok_or("the position is out of range")?;
    let (numer, denom) = self.ticks();
    let ticks = u128::from(position - position.min(pre_roll)) * denom / (numer * u128::from(rate));
    let ts = i64::try_from(ticks).map_err(|_| "the position is out of range")?;
    let to = SeekTo::Timestamp {
      ts: Timestamp::new(ts),
      track_id: self.track,
    };
    let sought = caught(|| self.format.seek(SeekMode::Accurate, to))
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
    let landed = sought.actual_ts.get();
    if landed > ts {
      return Err("the container sought past the position".to_string());
    }
    let mut early = u128::try_from(landed)
      .ok()
      .and_then(|landed| self.frame_at(landed))
      .and_then(|landed| position.checked_sub(landed))
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

  /// moves to the packet holding sample frame `frame`; false when the container cannot place it
  /// among the packets it indexes, as when another track ends before it, and may then not read on
  pub(crate) fn seek_frame(&mut self, frame: u64) -> Result<bool, String> {
    let (numer, denom) = self.ticks();
    let rate = u128::from(self.claimed.sample_rate);
    let ticks = u128::from(frame) * denom / (numer * rate);
    let ts = i64::try_from(ticks).map_err(|_| "the position is out of range")?;
    let to = SeekTo::Timestamp {
      ts: Timestamp::new(ts),
      track_id: self.track,
    };
    match caught(|| self.format.seek(SeekMode::Accurate, to)) {
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

  /// whether the container's timestamps count sample frames
  fn ticks_are_frames(&self) -> bool {
    let (numer, denom) = self.ticks();
    denom == numer * u128::from(self.claimed.sample_rate)
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
    self.keep(trim)
  }

  /// adds the packet just decoded to `pending`, less `trim` frames at its ends; false, with the
  /// reason in `stopped`, when it holds fewer channels than the stream
  fn keep(&mut self, trim: (usize, usize)) -> bool {
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
      let packet = match caught(|| self.format.next_packet()) {
        Ok(Some(packet)) => packet,
        Ok(None) => return None,
        Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
          debug!("audio ends early");
          self.stopped = Some(CUT_SHORT.to_string());
          return None;
        }
        // symphonia asks for a new decoder where a stream ends and another begins, as in a
        // chained Ogg file. the next stream can be of another rate or codec, and is not read
        Err(Error::ResetRequired) => {
          debug!("another stream follows the one read");
          self.stopped = Some("another stream follows the first, which alone was read".to_string());
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
      // its trims together. an MP4 states no trim: its last packet lasts as long as its audio
      let (dur, lead, tail) = (
        packet.dur.get(),
        packet.trim_start.get(),
        packet.trim_end.get(),
      );
      let stated = dur.checked_add(lead).and_then(|d| d.checked_add(tail));
      // each is at most the frames decoded, which fit a usize
      let trim = if let Some(mpeg) = &self.mpeg {
        // a packet that ends past the padding holds audio the stated length leaves out
        let end = packet.pts.get().checked_add_unsigned(frames);
        let padding = mpeg
          .padded_end
          .zip(end)
          .is_some_and(|(padded, end)| u64::try_from(end).is_ok_and(|end| end <= padded));
        (
          lead.min(frames) as usize,
          if padding {
            tail.min(frames) as usize
          } else {
            0
          },
        )
      } else if self.opus.is_none() {
        (0, 0)
      } else if stated == Some(frames) {
        (lead as usize, tail as usize)
      } else if stated == Some(dur) && dur < frames && self.ticks_are_frames() {
        (0, (frames - dur) as usize)
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

/// the bits per sample `params` state of a codec whose samples are integers; ALAC states them
/// in its magic cookie only. the decoder made from `params` read the same cookie, so one that
/// does not read here never gets this far. a container can state a depth of any codec, and
/// CAF states 0 for a compressed one, which is none stated
fn stated_bits(params: &AudioCodecParameters) -> Option<u32> {
  if !(keeps_every_bit(params.codec) || pcm(params.codec)) {
    return None;
  }
  params.bits_per_sample.filter(|bits| *bits > 0).or_else(|| {
    let cookie = params.extra_data.as_deref()?;
    let read = (params.codec == CODEC_ID_ALAC).then(|| MagicCookie::read(cookie));
    Some(u32::from(read?.ok()?.bit_depth))
  })
}

/// whether a codec keeps every bit: the lossless codecs and plain PCM, but not the companded
/// telephone formats
fn lossless(codec: AudioCodecId) -> bool {
  keeps_every_bit(codec)
    || (codec != CODEC_ID_PCM_ALAW && codec != CODEC_ID_PCM_MULAW && pcm(codec))
}

/// whether a codec is one of the lossless coders
fn keeps_every_bit(codec: AudioCodecId) -> bool {
  [
    CODEC_ID_FLAC,
    CODEC_ID_ALAC,
    CODEC_ID_WAVPACK,
    CODEC_ID_MONKEYS_AUDIO,
    CODEC_ID_DSD,
  ]
  .contains(&codec)
}

/// whether a codec is PCM, plain or companded
fn pcm(codec: AudioCodecId) -> bool {
  codecs()
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
    let channels = fixtures::rest(&mut decoder);
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
  fn an_opus_stream_starts_after_its_pre_skip_in_ogg_mp4_and_webm() {
    let audio = tone_48k(2);
    let stream = fixtures::opus(&audio, OpusSettings::music(128));
    assert!(stream.pre_skip > 0);
    let (_, ogg) = decoded(stream.ogg());
    // half a cycle off is twice the tone's level apart
    let off = apart(&ogg[0][1000..], &audio.channels[0][1000..]);
    assert!(off < 0.05, "{off}");
    // an MP4 states its length with the pre-skip and ends its last packet where the audio does
    let (claimed, mp4) = decoded(stream.mp4());
    assert_eq!(claimed.frames, Some(144_000));
    assert_eq!(mp4[0].len(), 144_000);
    let off = apart(&mp4[0][1000..], &audio.channels[0][1000..]);
    assert!(off < 0.05, "{off}");
    let (claimed, webm) = decoded(stream.webm());
    assert_eq!(claimed.frames, None);
    assert_eq!(
      claimed.bit_depth, None,
      "the container's depth is not Opus's"
    );
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
    let stream = fixtures::opus(&audio, OpusSettings::music(128));
    for (container, bytes) in [("ogg", stream.ogg()), ("mp4", stream.mp4())] {
      let (_, straight) = decoded(bytes.clone());
      let mut decoder =
        Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &Hint::new()).unwrap();
      let mut window = vec![Vec::new(); 2];
      for second in [5, 0, 11, 4] {
        decoder.seek(f64::from(second)).unwrap();
        assert_eq!(decoder.fill(&mut window, 48_000), Filled::Full);
        let from = second as usize * 48_000;
        let off = apart(&window[0], &straight[0][from..from + 48_000]);
        assert!(off < 1e-6, "{container}, second {second}: {off}");
      }
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
  fn aac_eld_is_read_with_the_channels_its_config_states() {
    let (claimed, samples) = decoded(fixtures::AAC_ELD.to_vec());
    assert_eq!((claimed.sample_rate, claimed.channels), (44_100, 2));
    assert_eq!(claimed.frames, Some(samples[0].len() as u64));
    assert!(
      (44_100..45_200).contains(&samples[0].len()),
      "{}",
      samples[0].len()
    );
    // pink noise at 0.3 of full scale
    let level = rms(&samples[0][4410..]);
    assert!((0.02..0.2).contains(&level), "{level}");
  }

  #[test]
  fn a_seek_in_he_aac_gives_the_audio_a_read_from_the_start_gives() {
    let (claimed, straight) = decoded(fixtures::HE_AAC.to_vec());
    assert_eq!(claimed.frames, Some(straight[0].len() as u64));
    let mut decoder = Decoder::from_source(
      Box::new(std::io::Cursor::new(fixtures::HE_AAC.to_vec())),
      &Hint::new(),
    )
    .unwrap();
    let mut window = vec![Vec::new(); 2];
    for second in [2, 0, 1] {
      decoder.seek(f64::from(second)).unwrap();
      assert_eq!(decoder.fill(&mut window, 44_100), Filled::Full);
      let from = second as usize * 44_100;
      // the rebuilt band's noise differs from one decode to the next; the core does not
      let off = apart(&window[0], &straight[0][from..from + 44_100]);
      assert!(off < 0.2, "second {second}: {off}");
    }
  }

  #[test]
  fn wavpack_from_the_reference_encoder_decodes_to_the_samples_it_was_made_from() {
    let (claimed, samples) = decoded(fixtures::WAVPACK.to_vec());
    assert_eq!(claimed.codec, "wavpack");
    assert!(claimed.lossless);
    assert_eq!(
      (claimed.sample_rate, claimed.bit_depth, claimed.channels),
      (44_100, Some(16), 2)
    );
    let (frames, _) = fixtures::CLIP_PCM.as_chunks::<4>();
    assert_eq!(claimed.frames, Some(frames.len() as u64));
    for (channel, decoded) in samples.iter().enumerate() {
      let source: Vec<f32> = frames
        .iter()
        .map(|f| f32::from(i16::from_le_bytes([f[2 * channel], f[2 * channel + 1]])) / 32_768.0)
        .collect();
      assert_eq!(decoded, &source, "channel {channel}");
    }
  }

  #[test]
  fn wavpack_decodes_to_the_samples_flac_does() {
    let audio = fixtures::music_for(3, 3, 44_100, fixtures::pink);
    for bits in [16, 24] {
      let (claimed, packed) = decoded(fixtures::wavpack(&audio, bits));
      let (_, flac) = decoded(fixtures::flac(&audio, bits, false));
      assert_eq!(claimed.bit_depth, Some(bits), "{bits} bits");
      assert_eq!(claimed.frames, Some(132_300), "{bits} bits");
      assert_eq!(packed.len(), 2, "{bits} bits");
      // the FLAC written here ends at its last whole block
      for (packed, flac) in packed.iter().zip(&flac) {
        assert_eq!(packed.len(), 132_300, "{bits} bits");
        assert_eq!(flac.len(), 131_072, "{bits} bits");
        assert!(packed[..flac.len()] == flac[..], "{bits} bits");
      }
      // one channel alone, at another rate, is the same channel
      let mono = fixtures::Audio {
        rate: 48_000,
        channels: vec![audio.channels[0].clone()],
      };
      let (claimed, alone) = decoded(fixtures::wavpack(&mono, bits));
      assert_eq!((claimed.sample_rate, claimed.channels), (48_000, 1));
      assert!(alone == packed[..1], "{bits} bits, mono");
    }
    // float samples come back as they went in, with no depth to state
    let (claimed, packed) = decoded(fixtures::wavpack_float(&audio));
    assert_eq!(claimed.bit_depth, None);
    assert!(packed == audio.channels);
  }

  #[test]
  fn a_seek_in_wavpack_gives_the_samples_a_read_from_the_start_gives() {
    let audio = fixtures::music_for(5, 3, 44_100, fixtures::pink);
    let bytes = fixtures::wavpack(&audio, 16);
    let (_, straight) = decoded(bytes.clone());
    let mut decoder =
      Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &Hint::new()).unwrap();
    let mut window = vec![Vec::new(); 2];
    for second in [3, 0, 4, 1] {
      decoder.seek(f64::from(second)).unwrap();
      assert_eq!(decoder.fill(&mut window, 44_100), Filled::Full);
      let from = second as usize * 44_100;
      assert_eq!(
        window[1],
        straight[1][from..from + 44_100],
        "second {second}"
      );
    }
  }

  #[test]
  fn a_wavpack_file_whose_blocks_stop_before_its_stated_length_says_so() {
    let audio = fixtures::music_for(5, 3, 44_100, fixtures::pink);
    let whole = fixtures::wavpack(&audio, 16);
    let blocks: Vec<usize> = whole
      .windows(4)
      .enumerate()
      .filter_map(|(at, id)| (id == b"wvpk").then_some(at))
      .collect();
    let middle = blocks[blocks.len() / 2];
    // bytes that are not a block where one should start, and a file cut between two blocks
    let mut damaged = whole.clone();
    damaged[middle] = b'x';
    for (name, bytes) in [("damaged", damaged), ("cut", whole[..middle].to_vec())] {
      let source = Box::new(std::io::Cursor::new(bytes));
      let mut decoder = Decoder::from_source(source, &Hint::new()).unwrap();
      let mut window = vec![Vec::new(); 2];
      while decoder.fill(&mut window, 44_100) == Filled::Full {}
      let stopped = decoder
        .stopped
        .unwrap_or_else(|| panic!("{name}: no reason"));
      assert!(
        stopped.contains("stop before the length"),
        "{name}: {stopped}"
      );
    }
  }

  #[test]
  fn ape_from_the_reference_encoder_decodes_to_the_samples_it_was_made_from() {
    let (frames, _) = fixtures::CLIP_PCM.as_chunks::<4>();
    for (level, bytes) in [
      ("normal", fixtures::APE_NORMAL),
      ("insane", fixtures::APE_INSANE),
    ] {
      let (claimed, samples) = decoded(bytes.to_vec());
      assert_eq!(claimed.codec, "ape", "{level}");
      assert!(claimed.lossless, "{level}");
      assert_eq!(
        (claimed.sample_rate, claimed.bit_depth, claimed.channels),
        (44_100, Some(16), 2),
        "{level}"
      );
      assert_eq!(claimed.frames, Some(frames.len() as u64), "{level}");
      for (channel, decoded) in samples.iter().enumerate() {
        let source: Vec<f32> = frames
          .iter()
          .map(|f| f32::from(i16::from_le_bytes([f[2 * channel], f[2 * channel + 1]])) / 32_768.0)
          .collect();
        assert!(decoded == &source, "{level}, channel {channel}");
      }
    }
  }

  #[test]
  fn ape_decodes_to_the_samples_wavpack_does_and_seeks_to_them() {
    // long enough for several frames at either level
    let audio = fixtures::music_for(12, 3, 44_100, fixtures::pink);
    for (bits, extra_high) in [(16, false), (24, true)] {
      let bytes = fixtures::ape(&audio, bits, extra_high);
      let (claimed, straight) = decoded(bytes.clone());
      let (_, packed) = decoded(fixtures::wavpack(&audio, bits));
      assert_eq!(claimed.bit_depth, Some(bits));
      assert_eq!(claimed.frames, Some(12 * 44_100));
      assert!(straight == packed, "{bits} bits");
      let mut decoder =
        Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &Hint::new()).unwrap();
      let mut window = vec![Vec::new(); 2];
      for second in [7, 0, 11, 3] {
        decoder.seek(f64::from(second)).unwrap();
        assert_eq!(decoder.fill(&mut window, 44_100), Filled::Full);
        let from = second as usize * 44_100;
        assert!(
          window[1] == straight[1][from..from + 44_100],
          "{bits} bits, second {second}"
        );
      }
    }
  }

  #[test]
  fn dsd_decodes_to_the_tones_it_was_made_from_in_dsf_and_dsdiff() {
    let one_bit = fixtures::one_bit_tones(&[1000.0, 3000.0], 3);
    let (claimed, dsf) = decoded(one_bit.dsf());
    let (_, dff) = decoded(one_bit.dff());
    assert_eq!(claimed.codec, "dsd");
    assert!(claimed.lossless);
    assert_eq!(
      (claimed.sample_rate, claimed.bit_depth, claimed.channels),
      (176_400, None, 2)
    );
    assert_eq!(claimed.frames, Some(3 * 176_400));
    assert!(dsf == dff, "the two containers hold the same samples");
    // the last samples are still under the filter when the file ends
    let short = 3 * 176_400 - dsf[0].len();
    assert!(short < 100, "{short} frames short");
    for (channel, hz) in dsf.iter().zip([1000.0, 3000.0]) {
      // a tone rises through zero once a cycle, and one at half of full scale has this level
      let settled = &channel[1000..];
      let rises = settled
        .windows(2)
        .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
        .count();
      let cycles = hz * settled.len() as f64 / 176_400.0;
      assert!(
        (rises as f64 - cycles).abs() <= 1.0,
        "{hz} Hz: {rises} cycles"
      );
      let level = rms(settled);
      assert!(
        (level - 0.5 / 2f64.sqrt()).abs() < 0.001,
        "{hz} Hz: {level}"
      );
    }
  }

  #[test]
  fn a_seek_in_dsd_gives_the_samples_a_read_from_the_start_gives() {
    let bytes = fixtures::one_bit_tones(&[1000.0, 3000.0], 4).dsf();
    let (_, straight) = decoded(bytes.clone());
    let mut decoder =
      Decoder::from_source(Box::new(std::io::Cursor::new(bytes)), &Hint::new()).unwrap();
    let mut window = vec![Vec::new(); 2];
    for second in [2, 0, 1] {
      decoder.seek(f64::from(second)).unwrap();
      assert_eq!(decoder.fill(&mut window, 176_400), Filled::Full);
      let from = second as usize * 176_400;
      let off = apart(&window[1], &straight[1][from..from + 176_400]);
      assert!(off < 1e-6, "second {second}: {off}");
    }
  }

  #[test]
  fn a_marker_of_a_reader_here_in_the_bytes_before_a_file_does_not_claim_it() {
    let audio = fixtures::music_for(1, 3, 44_100, fixtures::pink);
    let flac = fixtures::flac(&audio, 16, false);
    for marker in [b"wvpk", b"MAC ", b"DSD ", b"FRM8"] {
      // the marker, then bytes that are no header of its format
      let mut bytes = marker.to_vec();
      bytes.extend_from_slice(&[0xff; 60]);
      bytes.extend_from_slice(&flac);
      let source = Box::new(std::io::Cursor::new(bytes));
      let name = String::from_utf8_lossy(marker);
      let decoder =
        Decoder::from_source(source, &Hint::new()).unwrap_or_else(|e| panic!("{name}: {e}"));
      assert_eq!(decoder.claimed.codec, "flac", "{name}");
    }
  }

  #[test]
  fn a_dsd_file_cut_before_its_stated_length_says_so() {
    let tones = fixtures::one_bit_tones(&[1000.0, 3000.0], 2);
    // a DSF file cut after a whole block of each channel, and a DSDIFF file cut anywhere
    let (dsf, dff) = (tones.dsf(), tones.dff());
    let cuts = [
      ("dsf", &dsf[..92 + 40 * 2 * 4096]),
      ("dff", &dff[..dff.len() / 2]),
    ];
    for (name, bytes) in cuts {
      let source = Box::new(std::io::Cursor::new(bytes.to_vec()));
      let mut decoder = Decoder::from_source(source, &Hint::new()).unwrap();
      let mut window = vec![Vec::new(); 2];
      while decoder.fill(&mut window, 176_400) == Filled::Full {}
      let stopped = decoder
        .stopped
        .unwrap_or_else(|| panic!("{name}: no reason"));
      assert!(
        stopped.contains("stop before the length"),
        "{name}: {stopped}"
      );
    }
  }

  #[test]
  fn dst_compressed_dsd_is_refused_by_name() {
    // the sound data chunk of a DST file is named for it
    let mut bytes = fixtures::one_bit_tones(&[1000.0], 1).dff();
    let at = bytes.windows(4).rposition(|id| id == b"DSD ").unwrap();
    bytes[at..at + 4].copy_from_slice(b"DST ");
    let source = Box::new(std::io::Cursor::new(bytes));
    let Err(engine::Error::Unsupported(why)) = Decoder::from_source(source, &Hint::new()) else {
      panic!("a DST file opened")
    };
    assert!(why.contains("DST-compressed"), "{why}");
  }

  #[test]
  fn a_depth_of_no_bits_is_not_a_stated_depth() {
    // a CAF file states 0 bits a channel for a compressed codec, and its ALAC track's depth
    // is in the cookie, the last 24 bytes of the sample entry's 36-byte `alac` box
    let inner = TONE
      .windows(8)
      .position(|w| w == [0, 0, 0, 0x24, b'a', b'l', b'a', b'c'])
      .unwrap();
    let cookie = &TONE[inner + 12..inner + 36];
    let mut params = AudioCodecParameters::new();
    params
      .for_codec(CODEC_ID_ALAC)
      .with_bits_per_sample(0)
      .with_extra_data(cookie.into());
    assert_eq!(stated_bits(&params), Some(16));
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
