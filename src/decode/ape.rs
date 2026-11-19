//! reads Monkey's Audio files: a reader that hands out one frame a packet by the file's seek
//! table, and a decoder that decodes each frame through `ape-decoder`. files it does not decode
//! right are refused by name: those made before Monkey's Audio 3.99, and float and 32-bit ones

use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroU32;
use std::panic::{AssertUnwindSafe, catch_unwind};

use ape_decoder::{ApeFileInfo, FrameDecoder};
use symphonia::core::audio::{
  AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
};
use symphonia::core::codecs::audio::well_known::CODEC_ID_MONKEYS_AUDIO;
use symphonia::core::codecs::audio::{
  AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia::core::codecs::{CodecInfo, CodecParameters};
use symphonia::core::common::FourCc;
use symphonia::core::errors::{
  Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia::core::formats::probe::{
  ProbeDataMatchSpec, ProbeFormatData, ProbeableFormat, Score, Scoreable,
};
use symphonia::core::formats::{
  FormatId, FormatInfo, FormatOptions, FormatReader, MediaInfo, SeekMode, SeekTo, SeekedTo, Track,
};
use symphonia::core::io::{MediaSource, MediaSourceStream, ReadBytes, ScopedStream};
use symphonia::core::meta::{Metadata, MetadataLog};
use symphonia::core::packet::{Packet, PacketRef};
use symphonia::core::units::{Duration, TimeBase, Timestamp};
use tracing::{debug, trace, warn};

use super::refused;

/// the first version whose range coder the decoder has; older files fail their checksums in it
const OLDEST_VERSION: u16 = 3990;
/// the first version whose files start with a descriptor, which states its own length and the
/// header's
const DESCRIPTOR_VERSION: u16 = 3980;
/// the bytes of the format's descriptor and of its header, which later versions may add to
const DESCRIPTOR_BYTES: u32 = 52;
const HEADER_BYTES: u32 = 24;
/// the most bytes a descriptor or a header may state as its length
const MAX_STATED_BYTES: u32 = 1 << 16;
/// the most channels a file is read with
const MAX_CHANNELS: u16 = 8;
/// the most sample frames one frame of a file is read with. the highest compression level
/// packs 1179648
const MAX_FRAME_BLOCKS: u32 = 1 << 21;
/// the most bytes a frame's compressed data is read with
const MAX_FRAME_BYTES: u64 = 64 << 20;
/// the bytes a packet holds before its frame: the frame's alignment in the file, then its
/// sample frames
const PREFIX: usize = 5;
/// the header's flags of samples the decoder does not hand back as the file holds them
const BIG_ENDIAN: u16 = 1 << 9;
const SIGNED_8_BIT: u16 = 1 << 11;
const FLOAT: u16 = 1 << 12;
/// what a file of float samples starts with
const FLOAT_MAGIC: &[u8; 4] = b"MACF";
const UNREAD_SAMPLES: &str = "ape: float and 32-bit samples are not read";

const FORMAT: FormatInfo = FormatInfo {
  // symphonia names no format for Monkey's Audio
  format: FormatId::new(FourCc::new(*b"APE ")),
  short_name: "ape",
  long_name: "Monkey's Audio",
};

const CODEC: CodecInfo = CodecInfo {
  short_name: "ape",
  long_name: "Monkey's Audio",
  profiles: &[],
};

const SUPPORTED: [SupportedAudioCodec; 1] = [SupportedAudioCodec {
  id: CODEC_ID_MONKEYS_AUDIO,
  info: CODEC,
}];

/// what the decoder is made from, as the reader passes it in the track's extra data
#[derive(Clone, Copy)]
struct Stream {
  version: u16,
  level: u16,
  bits: u16,
  channels: u16,
  frame_blocks: u32,
}

impl Stream {
  fn to_bytes(self) -> Box<[u8]> {
    let words = [self.version, self.level, self.bits, self.channels];
    let mut bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    bytes.extend_from_slice(&self.frame_blocks.to_le_bytes());
    bytes.into_boxed_slice()
  }

  fn from_bytes(bytes: &[u8]) -> Option<Self> {
    let bytes: &[u8; 12] = bytes.try_into().ok()?;
    let word = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    Some(Self {
      version: word(0),
      level: word(2),
      bits: word(4),
      channels: word(6),
      frame_blocks: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
    })
  }
}

pub(crate) struct ApeReader<'s> {
  reader: MediaSourceStream<'s>,
  info: ApeFileInfo,
  tracks: Vec<Track>,
  media_info: MediaInfo,
  metadata: MetadataLog,
  /// the frame the next packet holds
  next: u32,
}

impl<'s> ApeReader<'s> {
  fn try_new(mut reader: MediaSourceStream<'s>) -> Result<Self> {
    // a file of float samples starts with a magic of its own
    let start = reader.pos();
    let magic = reader.read_quad_bytes()?;
    reader.seek(SeekFrom::Start(start))?;
    if &magic == FLOAT_MAGIC {
      return refused(UNREAD_SAMPLES);
    }
    let info = match ape_decoder::format::parse(&mut reader) {
      Ok(info) => info,
      Err(e) => {
        debug!(error = %e, "ape header not read");
        return refused("ape: the header is not read");
      }
    };
    let header = &info.header;
    if info.descriptor.version < OLDEST_VERSION {
      return refused("ape: files older than Monkey's Audio 3.99 are not read");
    }
    if header.format_flags & FLOAT != 0 || header.bits_per_sample == 32 {
      return refused(UNREAD_SAMPLES);
    }
    if header.format_flags & (BIG_ENDIAN | SIGNED_8_BIT) != 0 {
      return refused("ape: big-endian and signed 8-bit samples are not read");
    }
    if !matches!(header.bits_per_sample, 8 | 16 | 24) {
      return refused("ape: the sample size is not 8, 16 or 24 bits");
    }
    if !(1..=MAX_CHANNELS).contains(&header.channels) {
      return refused("ape: the number of channels is out of range");
    }
    if !(1..=MAX_FRAME_BLOCKS).contains(&header.blocks_per_frame) {
      return refused("ape: the length of a frame is out of range");
    }
    let Some(rate) = NonZeroU32::new(header.sample_rate) else {
      return refused("ape: the sample rate is not stated");
    };
    let stream = Stream {
      version: info.descriptor.version,
      level: header.compression_level,
      bits: header.bits_per_sample,
      channels: header.channels,
      frame_blocks: header.blocks_per_frame,
    };
    let mut params = AudioCodecParameters::new();
    params
      .for_codec(CODEC_ID_MONKEYS_AUDIO)
      .with_sample_rate(rate.get())
      .with_channels(Channels::Discrete(header.channels))
      .with_bits_per_sample(u32::from(header.bits_per_sample))
      .with_extra_data(stream.to_bytes());
    let mut track = Track::new(0);
    track
      .with_codec_params(CodecParameters::Audio(params))
      .with_time_base(TimeBase::new(NonZeroU32::MIN, rate));
    if let Ok(blocks) = u64::try_from(info.total_blocks) {
      track.with_num_frames(blocks);
    }
    Ok(Self {
      media_info: MediaInfo::from_track(&track),
      tracks: vec![track],
      reader,
      info,
      metadata: MetadataLog::default(),
      next: 0,
    })
  }
}

impl ApeReader<'_> {
  /// the byte of the file frame `frame` starts at; `None` when the seek table does not hold it
  fn start_of(&self, frame: u32) -> Option<u64> {
    let entry = self.info.seek_table.get(usize::try_from(frame).ok()?)?;
    entry.checked_add(u64::from(self.info.junk_header_bytes))
  }
}

impl Scoreable for ApeReader<'_> {
  /// a marker is a file's when a descriptor's lengths follow it. a file from before the
  /// descriptor has nothing to check and is taken, so that it is refused by name
  fn score(mut source: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
    let magic = source.read_quad_bytes()?;
    let version = source.read_u16()?;
    source.ignore_bytes(2)?;
    let (descriptor, header) = (source.read_u32()?, source.read_u32()?);
    let stated = (DESCRIPTOR_BYTES..=MAX_STATED_BYTES).contains(&descriptor)
      && (HEADER_BYTES..=MAX_STATED_BYTES).contains(&header);
    Ok(
      if &magic == FLOAT_MAGIC || version < DESCRIPTOR_VERSION || stated {
        Score::Supported(255)
      } else {
        Score::Unsupported
      },
    )
  }
}

impl ProbeableFormat<'_> for ApeReader<'_> {
  fn try_probe_new(
    reader: MediaSourceStream<'_>,
    _: FormatOptions,
  ) -> Result<Box<dyn FormatReader + '_>> {
    Ok(Box::new(ApeReader::try_new(reader)?))
  }

  fn probe_data() -> &'static [ProbeFormatData] {
    &[ProbeFormatData {
      spec: ProbeDataMatchSpec {
        extensions: &["ape"],
        mime_types: &["audio/ape", "audio/x-ape"],
        markers: &[b"MAC ", FLOAT_MAGIC],
      },
      info: FORMAT,
    }]
  }
}

impl FormatReader for ApeReader<'_> {
  fn format_info(&self) -> &FormatInfo {
    &FORMAT
  }

  fn media_info(&self) -> &MediaInfo {
    &self.media_info
  }

  fn metadata(&mut self) -> Metadata<'_> {
    self.metadata.metadata()
  }

  /// moves to the frame that holds the timestamp. every frame is decoded on its own
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
    let frame_blocks = u64::from(self.info.header.blocks_per_frame);
    let frame = u64::try_from(required_ts.get())
      .ok()
      .and_then(|wanted| u32::try_from(wanted / frame_blocks).ok())
      .filter(|frame| *frame < self.info.header.total_frames);
    let Some(frame) = frame else {
      return seek_error(SeekErrorKind::OutOfRange);
    };
    self.next = frame;
    Ok(SeekedTo {
      track_id: 0,
      required_ts,
      // a frame index times a frame's length is under 2^53
      actual_ts: Timestamp::new((u64::from(frame) * frame_blocks) as i64),
    })
  }

  fn tracks(&self) -> &[Track] {
    &self.tracks
  }

  fn next_packet(&mut self) -> Result<Option<Packet>> {
    let frame = self.next;
    if frame >= self.info.header.total_frames {
      return Ok(None);
    }
    let last = frame + 1 == self.info.header.total_frames;
    // the last frame runs to the end of the file's audio, which a tag can follow
    let end = if last {
      let after = u64::from(self.info.descriptor.terminating_data_bytes);
      Some(self.info.file_bytes.saturating_sub(after))
    } else {
      self.start_of(frame + 1)
    };
    // a frame starts at any byte, and the decoder reads it in words of four from the last
    // word boundary before it, counted from the first frame
    let placed = self.start_of(frame).zip(self.start_of(0)).zip(end);
    let placed = placed.and_then(|((start, first), end)| {
      Some((
        start,
        start.checked_sub(first)? % 4,
        end.checked_sub(start)?,
      ))
    });
    let Some((start, remainder, bytes)) = placed else {
      return decode_error("ape: the seek table is out of order");
    };
    if bytes > MAX_FRAME_BYTES {
      return decode_error("ape: a frame's data is out of range");
    }
    let blocks = if last {
      self.info.header.final_frame_blocks
    } else {
      self.info.header.blocks_per_frame
    };
    self.reader.seek(SeekFrom::Start(start - remainder))?;
    let mut data = vec![remainder as u8];
    data.extend_from_slice(&blocks.to_le_bytes());
    // the decoder reads up to a word past the frame; the file's last frame can end before it
    let wanted = bytes + remainder + 4;
    (&mut self.reader).take(wanted).read_to_end(&mut data)?;
    if ((data.len() - PREFIX) as u64) < bytes + remainder {
      return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
    }
    self.next += 1;
    Ok(Some(Packet::new(
      0,
      Timestamp::new((u64::from(frame) * u64::from(self.info.header.blocks_per_frame)) as i64),
      Duration::new(u64::from(blocks)),
      data,
    )))
  }

  fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
  where
    Self: 's,
  {
    self.reader
  }
}

pub(crate) struct ApeDecoder {
  inner: FrameDecoder,
  stream: Stream,
  params: AudioCodecParameters,
  /// each sample at the top of its 32 bits
  buffer: AudioBuffer<i32>,
}

impl ApeDecoder {
  fn try_new(params: &AudioCodecParameters) -> Result<Self> {
    let stream = params.extra_data.as_deref().and_then(Stream::from_bytes);
    let (Some(stream), Some(rate)) = (stream, params.sample_rate) else {
      return unsupported_error("ape: the stream's parameters are missing");
    };
    let sized = (1..=MAX_CHANNELS).contains(&stream.channels)
      && (1..=MAX_FRAME_BLOCKS).contains(&stream.frame_blocks)
      && matches!(stream.bits, 8 | 16 | 24);
    if params.codec != CODEC_ID_MONKEYS_AUDIO || !sized {
      return unsupported_error("ape: the stream's parameters are out of range");
    }
    let inner = FrameDecoder::new(stream.version, stream.channels, stream.bits, stream.level)
      .or_else(|e| {
        debug!(error = %e, "ape decoder not made");
        unsupported_error("ape: the decoder does not read this version of the format")
      })?;
    let spec = AudioSpec::new(rate, Channels::Discrete(stream.channels));
    Ok(Self {
      inner,
      stream,
      params: params.clone(),
      buffer: AudioBuffer::new(spec, stream.frame_blocks as usize),
    })
  }

  fn decode(&mut self, packet: &[u8]) -> Result<()> {
    self.buffer.clear();
    let Some((prefix, frame)) = packet.split_first_chunk::<PREFIX>() else {
      return decode_error("ape: a packet is too short");
    };
    let blocks = u32::from_le_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]);
    if blocks > self.stream.frame_blocks {
      return decode_error("ape: a frame's length is out of range");
    }
    // ape-decoder 0.3.2 adds a damaged frame's filter products unchecked (nn_filter.rs), which
    // panics in a build with overflow checks and wraps, as the format's own decoder does, in
    // one without. the default panic hook still prints it, and panic = "abort" still aborts
    // TODO: remove once an ape-decoder release sums with wrapping arithmetic
    let decoding = AssertUnwindSafe(|| {
      self
        .inner
        .decode_frame(frame, u32::from(prefix[0]), blocks as usize)
    });
    let Ok(decoded) = catch_unwind(decoding) else {
      warn!("a panic in the Monkey's Audio decoder was caught, its frame skipped");
      // what a panic left of the decoder's state is not used again
      let Stream {
        version,
        level,
        bits,
        channels,
        ..
      } = self.stream;
      self.inner = FrameDecoder::new(version, channels, bits, level).map_err(|e| {
        debug!(error = %e, "ape decoder not made again");
        Error::DecodeError("ape: the decoder could not be made again")
      })?;
      return decode_error("ape: the decoder panicked on the frame");
    };
    let pcm = decoded.map_err(|e| {
      trace!(error = %e, "ape frame not decoded");
      Error::DecodeError("ape: the frame could not be decoded")
    })?;
    let channels = usize::from(self.stream.channels);
    let width = usize::from(self.stream.bits / 8);
    if pcm.len() != blocks as usize * channels * width {
      return decode_error("ape: a frame decoded to another length than it states");
    }
    self.buffer.render_uninit(Some(blocks as usize));
    for channel in 0..channels {
      let plane = self
        .buffer
        .plane_mut(channel)
        .ok_or(Error::DecodeError("ape: a channel has no plane"))?;
      let samples = pcm.chunks_exact(width).skip(channel).step_by(channels);
      for (out, sample) in plane.iter_mut().zip(samples) {
        // little-endian samples, 8-bit ones unsigned, each put at the top of its 32 bits
        *out = match *sample {
          [byte] => i32::from(byte ^ 0x80) << 24,
          [low, high] => i32::from(i16::from_le_bytes([low, high])) << 16,
          [low, middle, high] => i32::from_le_bytes([0, low, middle, high]),
          _ => unreachable!("a sample is one, two or three bytes"),
        };
      }
    }
    Ok(())
  }
}

impl AudioDecoder for ApeDecoder {
  fn reset(&mut self) {
    // every frame is decoded on its own
    self.buffer.clear();
  }

  fn codec_info(&self) -> &CodecInfo {
    &CODEC
  }

  fn codec_params(&self) -> &AudioCodecParameters {
    &self.params
  }

  fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
    if let Err(problem) = self.decode(packet.data) {
      self.buffer.clear();
      return Err(problem);
    }
    Ok(self.buffer.as_generic_audio_buffer_ref())
  }

  fn finalize(&mut self) -> FinalizeResult {
    FinalizeResult::default()
  }

  fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
    self.buffer.as_generic_audio_buffer_ref()
  }
}

impl RegisterableAudioDecoder for ApeDecoder {
  fn try_registry_new(
    params: &AudioCodecParameters,
    _: &AudioDecoderOptions,
  ) -> Result<Box<dyn AudioDecoder>> {
    Ok(Box::new(Self::try_new(params)?))
  }

  fn supported_codecs() -> &'static [SupportedAudioCodec] {
    &SUPPORTED
  }
}
