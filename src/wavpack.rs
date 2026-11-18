//! reads WavPack files: a reader that hands out one block a packet, and a decoder that decodes
//! each block through `wavicle`. a hybrid file, which is lossy, is refused by name

use std::io::{Seek, SeekFrom};
use std::num::NonZeroU32;

use symphonia::core::audio::{
  AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
};
use symphonia::core::codecs::audio::well_known::CODEC_ID_WAVPACK;
use symphonia::core::codecs::audio::{
  AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia::core::codecs::{CodecInfo, CodecParameters};
use symphonia::core::errors::{
  Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia::core::formats::probe::{
  ProbeDataMatchSpec, ProbeFormatData, ProbeableFormat, Score, Scoreable,
};
use symphonia::core::formats::well_known::FORMAT_ID_WAVPACK;
use symphonia::core::formats::{
  FormatInfo, FormatOptions, FormatReader, MediaInfo, SeekMode, SeekTo, SeekedTo, Track,
};
use symphonia::core::io::{MediaSource, MediaSourceStream, ReadBytes, ScopedStream};
use symphonia::core::meta::{Metadata, MetadataLog};
use symphonia::core::packet::{Packet, PacketRef};
use symphonia::core::units::{Duration, TimeBase, Timestamp};

/// the bytes of a block's header
const HEADER: usize = 32;
/// the most bytes a block is read with, which is the most the format's own decoder takes
const MAX_BLOCK: usize = 1 << 20;
/// the most sample frames a block is read with, likewise
const MAX_BLOCK_FRAMES: u32 = 131_072;
/// the blocks without audio read past at the start before the file is given up on
const MAX_LEADING_BLOCKS: usize = 64;
/// the flag of a block coded in hybrid mode, which rounds its samples
const HYBRID: u32 = 1 << 3;
/// the flag of a block that holds DSD
const DSD: u32 = 1 << 31;

const FORMAT: FormatInfo = FormatInfo {
  format: FORMAT_ID_WAVPACK,
  short_name: "wavpack",
  long_name: "WavPack",
};

const CODEC: CodecInfo = CodecInfo {
  short_name: "wavpack",
  long_name: "WavPack",
  profiles: &[],
};

const SUPPORTED: [SupportedAudioCodec; 1] = [SupportedAudioCodec {
  id: CODEC_ID_WAVPACK,
  info: CODEC,
}];

/// what a block's header states
#[derive(Clone, Copy)]
struct Header {
  /// the bytes of the whole block, its header among them
  length: usize,
  /// the sample frame the block starts at
  index: u64,
  /// the sample frames it holds, none for a block of metadata alone
  frames: u32,
  flags: u32,
  /// the sample frames of the file, which the first block states when it knows
  total: Option<u64>,
}

impl Header {
  /// the header in `bytes`; `None` when they do not start a block the reader takes
  fn parse(bytes: &[u8; HEADER]) -> Option<Self> {
    let word =
      |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    if &bytes[..4] != b"wvpk" {
      return None;
    }
    // the size counts what follows its own field
    let length = usize::try_from(word(4)).ok()?.checked_add(8)?;
    let frames = word(20);
    if !(HEADER..=MAX_BLOCK).contains(&length) || frames > MAX_BLOCK_FRAMES {
      return None;
    }
    // the low word of the total is all ones when the encoder did not know it
    let total = (word(12) != u32::MAX).then(|| u64::from(bytes[11]) << 32 | u64::from(word(12)));
    Some(Self {
      length,
      index: u64::from(bytes[10]) << 32 | u64::from(word(16)),
      frames,
      flags: word(24),
      total,
    })
  }
}

/// the header at the reader's position, which is left after it; `None` at the end of the file
/// and at anything that is not a block, as the tags after the audio
fn next_header(reader: &mut MediaSourceStream<'_>) -> Result<Option<Header>> {
  let mut bytes = [0; HEADER];
  let mut read = 0;
  while read < HEADER {
    match reader.read_buf(&mut bytes[read..]) {
      Ok(0) => return Ok(None),
      Ok(count) => read += count,
      Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
      Err(e) => return Err(e.into()),
    }
  }
  Ok(Header::parse(&bytes))
}

pub(crate) struct WavPackReader<'s> {
  reader: MediaSourceStream<'s>,
  tracks: Vec<Track>,
  media_info: MediaInfo,
  metadata: MetadataLog,
  /// where the first block starts
  first: u64,
  /// where the block last handed out or sought to starts, and its first sample frame
  last: Option<(u64, u64)>,
  /// the sample frames of the file, when its first block states them
  total: Option<u64>,
  /// the sample frame after the last one handed out
  read_to: u64,
}

impl<'s> WavPackReader<'s> {
  fn try_new(mut reader: MediaSourceStream<'s>) -> Result<Self> {
    let first = reader.pos();
    // the first block with audio states the file's rate, channels and sample format, which
    // the decoder reads from it
    let mut leading = 0;
    let (header, block) = loop {
      let at = reader.pos();
      let Some(header) = next_header(&mut reader)? else {
        return unsupported_error("wavpack: the file holds no block with audio");
      };
      if header.flags & DSD != 0 {
        return unsupported_error("wavpack: DSD in WavPack is not read");
      }
      if header.flags & HYBRID != 0 {
        return unsupported_error(
          "wavpack: the file is hybrid WavPack, which is lossy without its correction file, \
           and is not decoded",
        );
      }
      reader.seek(SeekFrom::Start(at))?;
      let block = reader.read_boxed_slice_exact(header.length)?;
      if header.frames > 0 {
        break (header, block);
      }
      leading += 1;
      if leading == MAX_LEADING_BLOCKS {
        return unsupported_error("wavpack: the file holds no block with audio");
      }
    };
    let Ok(decoded) = wavicle::decode_stream(&block) else {
      return unsupported_error(
        "wavpack: the first block is not one the decoder reads: more than two channels, \
         8-bit samples and streams older than WavPack 4 are not",
      );
    };
    let Some(rate) = NonZeroU32::new(decoded.sample_rate) else {
      return unsupported_error("wavpack: the sample rate is not stated");
    };
    let mut params = AudioCodecParameters::new();
    params
      .for_codec(CODEC_ID_WAVPACK)
      .with_sample_rate(rate.get())
      .with_channels(Channels::Discrete(decoded.channels as u16));
    if !decoded.is_float {
      params.with_bits_per_sample(decoded.bits_per_sample);
    }
    let mut track = Track::new(0);
    track
      .with_codec_params(CodecParameters::Audio(params))
      .with_time_base(TimeBase::new(NonZeroU32::MIN, rate));
    if let Some(total) = header.total {
      track.with_num_frames(total);
    }
    reader.seek(SeekFrom::Start(first))?;
    Ok(Self {
      media_info: MediaInfo::from_track(&track),
      tracks: vec![track],
      reader,
      metadata: MetadataLog::default(),
      first,
      last: None,
      total: header.total,
      read_to: 0,
    })
  }
}

impl Scoreable for WavPackReader<'_> {
  fn score(_: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
    Ok(Score::Supported(255))
  }
}

impl ProbeableFormat<'_> for WavPackReader<'_> {
  fn try_probe_new(
    reader: MediaSourceStream<'_>,
    _: FormatOptions,
  ) -> Result<Box<dyn FormatReader + '_>> {
    Ok(Box::new(WavPackReader::try_new(reader)?))
  }

  fn probe_data() -> &'static [ProbeFormatData] {
    &[ProbeFormatData {
      spec: ProbeDataMatchSpec {
        extensions: &["wv"],
        mime_types: &["audio/wavpack", "audio/x-wavpack"],
        markers: &[b"wvpk"],
      },
      info: FORMAT,
    }]
  }
}

impl FormatReader for WavPackReader<'_> {
  fn format_info(&self) -> &FormatInfo {
    &FORMAT
  }

  fn media_info(&self) -> &MediaInfo {
    &self.media_info
  }

  fn metadata(&mut self) -> Metadata<'_> {
    self.metadata.metadata()
  }

  /// moves to the block that holds the timestamp, reading the headers of the blocks on the way
  /// from the last one handed out, or from the first when the timestamp is before that
  fn seek(&mut self, _: SeekMode, to: SeekTo) -> Result<SeekedTo> {
    if !self.reader.is_seekable() {
      return seek_error(SeekErrorKind::Unseekable);
    }
    let required_ts = match to {
      SeekTo::Timestamp { ts, .. } => ts,
      SeekTo::Time { time, .. } => {
        let base = self.tracks[0].time_base;
        base
          .and_then(|base| base.calc_timestamp(time))
          .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
      }
    };
    let Ok(wanted) = u64::try_from(required_ts.get()) else {
      return seek_error(SeekErrorKind::OutOfRange);
    };
    let mut at = match self.last {
      Some((offset, index)) if index <= wanted => offset,
      _ => self.first,
    };
    loop {
      self.reader.seek(SeekFrom::Start(at))?;
      let Some(header) = next_header(&mut self.reader)? else {
        return seek_error(SeekErrorKind::OutOfRange);
      };
      if header.frames > 0 && wanted < header.index + u64::from(header.frames) {
        // a block that starts past the timestamp follows a gap, which no block fills
        if header.index > wanted {
          return seek_error(SeekErrorKind::OutOfRange);
        }
        self.reader.seek(SeekFrom::Start(at))?;
        self.last = Some((at, header.index));
        let actual_ts = Timestamp::new(header.index as i64);
        return Ok(SeekedTo {
          track_id: 0,
          required_ts,
          actual_ts,
        });
      }
      at += header.length as u64;
    }
  }

  fn tracks(&self) -> &[Track] {
    &self.tracks
  }

  fn next_packet(&mut self) -> Result<Option<Packet>> {
    loop {
      let at = self.reader.pos();
      let Some(header) = next_header(&mut self.reader)? else {
        // tags follow the last block. bytes that are not a block before the length the file
        // states are damage, or the file was cut between two blocks
        if self.total.is_some_and(|total| self.read_to < total) {
          return decode_error("wavpack: the blocks stop before the length the file states");
        }
        return Ok(None);
      };
      self.reader.seek(SeekFrom::Start(at))?;
      let block = self.reader.read_boxed_slice_exact(header.length)?;
      if header.frames == 0 {
        continue;
      }
      self.last = Some((at, header.index));
      self.read_to = header.index + u64::from(header.frames);
      return Ok(Some(Packet::new(
        0,
        // a block's index is 40 bits
        Timestamp::new(header.index as i64),
        Duration::new(u64::from(header.frames)),
        block,
      )));
    }
  }

  fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
  where
    Self: 's,
  {
    self.reader
  }
}

/// what every block of a stream has in common, as its first one states it
#[derive(Clone, Copy, PartialEq, Eq)]
struct Shape {
  rate: u32,
  channels: u32,
  bits: u32,
  float: bool,
}

/// a block's samples: integers at the top of their 32 bits, or floats
enum Samples {
  Int(AudioBuffer<i32>),
  Float(AudioBuffer<f32>),
}

pub(crate) struct WavPackDecoder {
  params: AudioCodecParameters,
  samples: Samples,
  shape: Option<Shape>,
}

impl WavPackDecoder {
  fn decode(&mut self, block: &[u8]) -> Result<()> {
    self.clear();
    let decoded = wavicle::decode_stream(block)
      .map_err(|_| Error::DecodeError("wavpack: the block could not be decoded"))?;
    let shape = Shape {
      rate: decoded.sample_rate,
      channels: decoded.channels,
      bits: decoded.bits_per_sample,
      float: decoded.is_float,
    };
    if !(1..=2).contains(&shape.channels) || !(1..=32).contains(&shape.bits) || shape.rate == 0 {
      return Err(Error::DecodeError(
        "wavpack: a block's shape is out of range",
      ));
    }
    let channels = shape.channels as usize;
    let frames = decoded.samples.len() / channels;
    if frames > MAX_BLOCK_FRAMES as usize {
      return Err(Error::DecodeError(
        "wavpack: a block's length is out of range",
      ));
    }
    match self.shape {
      None => {
        let spec = AudioSpec::new(shape.rate, Channels::Discrete(shape.channels as u16));
        let capacity = MAX_BLOCK_FRAMES as usize;
        self.samples = if shape.float {
          Samples::Float(AudioBuffer::new(spec, capacity))
        } else {
          Samples::Int(AudioBuffer::new(spec, capacity))
        };
        self.shape = Some(shape);
      }
      Some(first) if first != shape => {
        return Err(Error::DecodeError("wavpack: the stream changes its format"));
      }
      Some(_) => {}
    }
    let of = |channel: usize| decoded.samples.iter().skip(channel).step_by(channels);
    let missing = || Error::DecodeError("wavpack: a channel has no plane");
    match &mut self.samples {
      Samples::Int(buffer) => {
        buffer.render_uninit(Some(frames));
        // a sample's bits go to the top of its 32, where the analyzer looks for unused ones
        let shift = 32 - shape.bits;
        for channel in 0..channels {
          let plane = buffer.plane_mut(channel).ok_or_else(missing)?;
          for (out, sample) in plane.iter_mut().zip(of(channel)) {
            *out = sample.wrapping_shl(shift);
          }
        }
      }
      Samples::Float(buffer) => {
        buffer.render_uninit(Some(frames));
        for channel in 0..channels {
          let plane = buffer.plane_mut(channel).ok_or_else(missing)?;
          // the decoder hands a float's bits over as an integer
          for (out, sample) in plane.iter_mut().zip(of(channel)) {
            *out = f32::from_bits(*sample as u32);
          }
        }
      }
    }
    Ok(())
  }

  fn clear(&mut self) {
    match &mut self.samples {
      Samples::Int(buffer) => buffer.clear(),
      Samples::Float(buffer) => buffer.clear(),
    }
  }
}

impl AudioDecoder for WavPackDecoder {
  fn reset(&mut self) {
    // every block is decoded on its own
    self.clear();
  }

  fn codec_info(&self) -> &CodecInfo {
    &CODEC
  }

  fn codec_params(&self) -> &AudioCodecParameters {
    &self.params
  }

  fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
    if let Err(problem) = self.decode(packet.data) {
      self.clear();
      return Err(problem);
    }
    Ok(self.last_decoded())
  }

  fn finalize(&mut self) -> FinalizeResult {
    FinalizeResult::default()
  }

  fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
    match &self.samples {
      Samples::Int(buffer) => buffer.as_generic_audio_buffer_ref(),
      Samples::Float(buffer) => buffer.as_generic_audio_buffer_ref(),
    }
  }
}

impl RegisterableAudioDecoder for WavPackDecoder {
  fn try_registry_new(
    params: &AudioCodecParameters,
    _: &AudioDecoderOptions,
  ) -> Result<Box<dyn AudioDecoder>> {
    if params.codec != CODEC_ID_WAVPACK {
      return unsupported_error("wavpack: not a WavPack stream");
    }
    // the first block says what the buffer holds; until then it is an empty one of one channel
    let rate = params.sample_rate.unwrap_or(0);
    let spec = AudioSpec::new(rate, Channels::Discrete(1));
    Ok(Box::new(Self {
      params: params.clone(),
      samples: Samples::Int(AudioBuffer::new(spec, 0)),
      shape: None,
    }))
  }

  fn supported_codecs() -> &'static [SupportedAudioCodec] {
    &SUPPORTED
  }
}
