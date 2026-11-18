//! reads DSD files, DSF and DSDIFF: a reader that hands out the one-bit samples a block a
//! packet, and a decoder that filters them down to PCM for the analysis.
//!
//! the PCM is at a sixteenth of the one-bit rate, and no more than 352.8 or 384 kHz: 176.4 kHz
//! for DSD64, 352.8 kHz from DSD128 on. the filter is a Kaiser-windowed sinc that leaves an
//! eighth of the samples, read from tables a byte at a time, then half-band stages that each
//! leave half. every stage is designed for 120 dB of attenuation and passes 0.45 of the PCM rate

use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroU32;

use symphonia::core::audio::{
  AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
};
use symphonia::core::codecs::audio::{
  AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
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
use symphonia::core::io::{MediaSource, MediaSourceStream, ScopedStream};
use symphonia::core::meta::{Metadata, MetadataLog};
use symphonia::core::packet::{Packet, PacketRef};
use symphonia::core::units::{Duration, TimeBase, Timestamp};

/// the codec of one-bit samples, which symphonia names no id for
pub(crate) const CODEC_ID_DSD: AudioCodecId = AudioCodecId::new(FourCc::new(*b"DSD "));

/// the most channels a header may state
const MAX_CHANNELS: u32 = 6;
/// the most chunks of a DSDIFF file looked at before it is given up on
const MAX_CHUNKS: u32 = 4096;
/// the bytes of one channel in a packet, which are those of one channel in a DSF block
const BLOCK: usize = 4096;
/// the highest PCM rate the samples are filtered down to, as a multiple of 44.1 or 48 kHz
const MAX_PCM_MULTIPLE: u32 = 8;
/// the part of the PCM rate every stage of the filter passes
const PASS: f64 = 0.45;
/// the stopband attenuation every stage of the filter is designed for, in dB
const ATTENUATION_DB: f64 = 120.0;
/// a byte whose every other bit is set, which a lowpass reads as silence
const IDLE: u8 = 0x69;

const FORMAT: FormatInfo = FormatInfo {
  // symphonia names no format for DSD files
  format: FormatId::new(FourCc::new(*b"DSD ")),
  short_name: "dsd",
  long_name: "DSD Stream File or DSDIFF",
};

const CODEC: CodecInfo = CodecInfo {
  short_name: "dsd",
  long_name: "Direct Stream Digital",
  profiles: &[],
};

const SUPPORTED: [SupportedAudioCodec; 1] = [SupportedAudioCodec {
  id: CODEC_ID_DSD,
  info: CODEC,
}];

/// how a file lays out its one-bit samples
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
  /// blocks of 4096 bytes per channel in turn, the first sample in a byte's lowest bit when
  /// `lsb_first`
  Dsf { lsb_first: bool },
  /// a byte per channel in turn, the first sample in a byte's highest bit
  Dff,
}

#[derive(Clone, Copy)]
struct Header {
  layout: Layout,
  /// one-bit samples per second
  rate: u32,
  channels: u32,
  /// one-bit samples per channel
  samples: u64,
  /// where the sample bytes start
  data_at: u64,
}

/// the multiple of 44.1 or 48 kHz a one-bit `rate` is: 64, 128, 256 or 512
fn multiple(rate: u32) -> Option<u32> {
  [44_100u32, 48_000]
    .into_iter()
    .filter(|base| rate.is_multiple_of(*base))
    .map(|base| rate / base)
    .find(|multiple| [64, 128, 256, 512].contains(multiple))
}

/// the multiple of 44.1 or 48 kHz a one-bit `rate` is, as DSD's rates are named: 64 for DSD64
pub(crate) fn multiple_of(rate: u32) -> u32 {
  multiple(rate).unwrap_or(rate / 44_100)
}

/// the PCM rate one-bit samples at `rate` are filtered down to: a sixteenth of it, and no more
/// than [`MAX_PCM_MULTIPLE`] times its base rate
fn pcm_rate(rate: u32) -> Option<u32> {
  let multiple = multiple(rate)?;
  Some(rate / multiple * (multiple / 16).min(MAX_PCM_MULTIPLE))
}

fn le32(bytes: &[u8]) -> u32 {
  u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le64(bytes: &[u8]) -> u64 {
  u64::from(le32(bytes)) | u64::from(le32(&bytes[4..])) << 32
}

fn be64(bytes: &[u8]) -> u64 {
  bytes[..8]
    .iter()
    .fold(0, |value, byte| value << 8 | u64::from(*byte))
}

/// what stops a header from being read: the file's bytes giving out, or a reason in words
enum Unread {
  Io(std::io::Error),
  Why(&'static str),
}

impl From<std::io::Error> for Unread {
  fn from(e: std::io::Error) -> Self {
    Self::Io(e)
  }
}

/// the header of the DSF or DSDIFF file `file` starts with, `length` bytes long
fn read_header(file: &mut (impl Read + Seek), length: u64) -> std::result::Result<Header, Unread> {
  let mut magic = [0u8; 4];
  file.read_exact(&mut magic)?;
  match &magic {
    b"DSD " => dsf(file, length),
    b"FRM8" => dff(file, length),
    _ => Err(Unread::Why("dsd: neither a DSF nor a DSDIFF file")),
  }
}

fn dsf(file: &mut impl Read, length: u64) -> std::result::Result<Header, Unread> {
  // the rest of the DSD chunk (24 bytes), the fmt chunk (52) and the data chunk's header (12)
  let mut head = [0u8; 88];
  file.read_exact(&mut head)?;
  if le64(&head) != 28 || &head[24..28] != b"fmt " || le64(&head[28..]) != 52 {
    return Err(Unread::Why(
      "dsd: the DSF header's chunks are not the sizes the format gives them",
    ));
  }
  let fmt = &head[36..76];
  let (version, format, channels) = (le32(fmt), le32(&fmt[4..]), le32(&fmt[12..]));
  let (rate, bits, samples, block) = (
    le32(&fmt[16..]),
    le32(&fmt[20..]),
    le64(&fmt[24..]),
    le32(&fmt[32..]),
  );
  if version != 1 || format != 0 {
    return Err(Unread::Why("dsd: the DSF file does not hold plain DSD"));
  }
  if !(1..=MAX_CHANNELS).contains(&channels) {
    return Err(Unread::Why("dsd: the number of channels is out of range"));
  }
  if multiple(rate).is_none() {
    return Err(Unread::Why("dsd: the sample rate is not a DSD rate"));
  }
  if bits != 1 && bits != 8 {
    return Err(Unread::Why("dsd: the bits per sample are neither 1 nor 8"));
  }
  if block as usize != BLOCK || &head[76..80] != b"data" {
    return Err(Unread::Why(
      "dsd: the DSF file's blocks are not laid out as the format lays them",
    ));
  }
  let data_at = 92;
  // the samples the file's bytes can hold bound what the header states
  let held = length.saturating_sub(data_at) / u64::from(channels) * 8;
  let stated = le64(&head[80..]).saturating_sub(12) / u64::from(channels) * 8;
  Ok(Header {
    layout: Layout::Dsf {
      lsb_first: bits == 1,
    },
    rate,
    channels,
    samples: samples.min(held).min(stated),
    data_at,
  })
}

fn dff(file: &mut (impl Read + Seek), length: u64) -> std::result::Result<Header, Unread> {
  let overflow = || Unread::Why("dsd: a DSDIFF chunk's size is out of range");
  let mut head = [0u8; 12];
  file.read_exact(&mut head)?;
  if &head[8..12] != b"DSD " {
    return Err(Unread::Why("dsd: the DSDIFF form is not DSD"));
  }
  let (mut rate, mut channels, mut compression) = (None, None, None);
  let mut at = 16u64;
  for _ in 0..MAX_CHUNKS {
    if at.checked_add(12).is_none_or(|end| end > length) {
      break;
    }
    file.seek(SeekFrom::Start(at))?;
    let mut chunk = [0u8; 12];
    file.read_exact(&mut chunk)?;
    let size = be64(&chunk[4..]);
    let body = at + 12;
    match &chunk[..4] {
      b"PROP" => {
        let end = body.checked_add(size).filter(|end| *end <= length);
        let end = end.ok_or_else(overflow)?;
        // the properties follow the four bytes that say they are of sound
        let mut inner = body + 4;
        for _ in 0..MAX_CHUNKS {
          if inner.checked_add(12).is_none_or(|next| next > end) {
            break;
          }
          file.seek(SeekFrom::Start(inner))?;
          file.read_exact(&mut chunk)?;
          let inner_size = be64(&chunk[4..]);
          let mut field = [0u8; 4];
          match &chunk[..4] {
            b"FS  " if inner_size >= 4 => {
              file.read_exact(&mut field)?;
              rate = Some(u32::from_be_bytes(field));
            }
            b"CHNL" if inner_size >= 2 => {
              file.read_exact(&mut field[..2])?;
              channels = Some(u32::from(u16::from_be_bytes([field[0], field[1]])));
            }
            b"CMPR" if inner_size >= 4 => {
              file.read_exact(&mut field)?;
              compression = Some(field);
            }
            _ => {}
          }
          // a chunk is padded to an even length
          inner = inner_size
            .checked_add(12 + (inner_size & 1))
            .and_then(|step| inner.checked_add(step))
            .ok_or_else(overflow)?;
        }
      }
      b"DSD " => {
        if compression != Some(*b"DSD ") {
          return Err(Unread::Why(
            "dsd: the DSDIFF file's compression is not plain DSD",
          ));
        }
        let rate = rate.filter(|rate| multiple(*rate).is_some());
        let rate = rate.ok_or(Unread::Why(
          "dsd: the sample rate is missing or not a DSD rate",
        ))?;
        let channels = channels.filter(|channels| (1..=MAX_CHANNELS).contains(channels));
        let channels = channels.ok_or(Unread::Why(
          "dsd: the number of channels is missing or out of range",
        ))?;
        let held = (length - body).min(size);
        return Ok(Header {
          layout: Layout::Dff,
          rate,
          channels,
          samples: held / u64::from(channels) * 8,
          data_at: body,
        });
      }
      b"DST " => {
        return Err(Unread::Why(
          "dsd: the DSDIFF file is DST-compressed, which is not decoded",
        ));
      }
      _ => {}
    }
    at = size
      .checked_add(12 + (size & 1))
      .and_then(|step| at.checked_add(step))
      .ok_or_else(overflow)?;
  }
  Err(Unread::Why("dsd: the DSDIFF file holds no sound data"))
}

pub(crate) struct DsdReader<'s> {
  reader: MediaSourceStream<'s>,
  header: Header,
  tracks: Vec<Track>,
  media_info: MediaInfo,
  metadata: MetadataLog,
  /// the PCM frames one packet's samples make
  packet_frames: u64,
  /// the packet handed out next
  next: u64,
}

impl<'s> DsdReader<'s> {
  fn try_new(mut reader: MediaSourceStream<'s>) -> Result<Self> {
    let Some(length) = reader.byte_len() else {
      return unsupported_error("dsd: the length of the file is not known");
    };
    let header = match read_header(&mut reader, length) {
      Ok(header) => header,
      Err(Unread::Io(e)) => return Err(e.into()),
      Err(Unread::Why(why)) => return unsupported_error(why),
    };
    let rate = pcm_rate(header.rate).and_then(NonZeroU32::new);
    let Some(rate) = rate else {
      return unsupported_error("dsd: the sample rate is not a DSD rate");
    };
    let decimation = u64::from(header.rate / rate);
    let mut params = AudioCodecParameters::new();
    params
      .for_codec(CODEC_ID_DSD)
      .with_sample_rate(rate.get())
      .with_channels(Channels::Discrete(header.channels as u16))
      // the decoder makes its filter for the one-bit rate
      .with_extra_data(header.rate.to_le_bytes().into());
    let mut track = Track::new(0);
    track
      .with_codec_params(CodecParameters::Audio(params))
      .with_time_base(TimeBase::new(NonZeroU32::MIN, rate))
      .with_num_frames(header.samples / decimation);
    Ok(Self {
      media_info: MediaInfo::from_track(&track),
      tracks: vec![track],
      reader,
      header,
      metadata: MetadataLog::default(),
      packet_frames: (BLOCK * 8) as u64 / decimation,
      next: 0,
    })
  }

  /// the bytes each channel holds
  fn channel_bytes(&self) -> u64 {
    self.header.samples.div_ceil(8)
  }
}

impl Scoreable for DsdReader<'_> {
  fn score(_: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
    Ok(Score::Supported(255))
  }
}

impl ProbeableFormat<'_> for DsdReader<'_> {
  fn try_probe_new(
    reader: MediaSourceStream<'_>,
    _: FormatOptions,
  ) -> Result<Box<dyn FormatReader + '_>> {
    Ok(Box::new(DsdReader::try_new(reader)?))
  }

  fn probe_data() -> &'static [ProbeFormatData] {
    &[ProbeFormatData {
      spec: ProbeDataMatchSpec {
        extensions: &["dsf", "dff"],
        mime_types: &["audio/dsf", "audio/x-dsf", "audio/x-dff"],
        markers: &[b"DSD ", b"FRM8"],
      },
      info: FORMAT,
    }]
  }
}

impl FormatReader for DsdReader<'_> {
  fn format_info(&self) -> &FormatInfo {
    &FORMAT
  }

  fn media_info(&self) -> &MediaInfo {
    &self.media_info
  }

  fn metadata(&mut self) -> Metadata<'_> {
    self.metadata.metadata()
  }

  /// moves to the packet that holds the timestamp
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
    let packets = self.channel_bytes().div_ceil(BLOCK as u64);
    let packet = u64::try_from(required_ts.get())
      .ok()
      .map(|wanted| wanted / self.packet_frames)
      .filter(|packet| *packet < packets);
    let Some(packet) = packet else {
      return seek_error(SeekErrorKind::OutOfRange);
    };
    self.next = packet;
    Ok(SeekedTo {
      track_id: 0,
      required_ts,
      // a file's bytes times eight are under 2^63
      actual_ts: Timestamp::new((packet * self.packet_frames) as i64),
    })
  }

  fn tracks(&self) -> &[Track] {
    &self.tracks
  }

  /// the next block of each channel's bytes, one channel after the other, the first sample of
  /// each byte in its highest bit
  fn next_packet(&mut self) -> Result<Option<Packet>> {
    let channels = self.header.channels as usize;
    let before = self.next * BLOCK as u64;
    let Some(left) = self
      .channel_bytes()
      .checked_sub(before)
      .filter(|left| *left > 0)
    else {
      return Ok(None);
    };
    // at most a block, so it fits
    let take = left.min(BLOCK as u64) as usize;
    let at = self.header.data_at + before * channels as u64;
    self.reader.seek(SeekFrom::Start(at))?;
    let mut data = vec![0u8; take * channels];
    match self.header.layout {
      // a block is whole on disk even when the audio ends inside it
      Layout::Dsf { lsb_first } => {
        let mut block = vec![0u8; BLOCK * channels];
        self.reader.read_exact(&mut block)?;
        let (stored, _) = block.as_chunks::<BLOCK>();
        for (plane, stored) in data.chunks_exact_mut(take).zip(stored) {
          for (to, from) in plane.iter_mut().zip(stored) {
            *to = if lsb_first {
              from.reverse_bits()
            } else {
              *from
            };
          }
        }
      }
      Layout::Dff => {
        let mut stored = vec![0u8; take * channels];
        self.reader.read_exact(&mut stored)?;
        for (channel, plane) in data.chunks_exact_mut(take).enumerate() {
          let bytes = stored.iter().skip(channel).step_by(channels);
          for (to, from) in plane.iter_mut().zip(bytes) {
            *to = *from;
          }
        }
      }
    }
    let packet = self.next;
    self.next += 1;
    Ok(Some(Packet::new(
      0,
      Timestamp::new((packet * self.packet_frames) as i64),
      Duration::new(self.packet_frames),
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

/// the modified Bessel function of the first kind of order 0, from its power series
fn bessel_i0(x: f64) -> f64 {
  let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
  while term > sum * 1e-17 {
    term *= (x / (2.0 * k)) * (x / (2.0 * k));
    sum += term;
    k += 1.0;
  }
  sum
}

/// the taps a Kaiser window needs for [`ATTENUATION_DB`] over a transition `width`, a fraction
/// of the sample rate
fn taps_for(width: f64) -> usize {
  ((ATTENUATION_DB - 7.95) / (14.36 * width)).ceil() as usize + 1
}

/// a lowpass of `taps` taps cut at `cutoff`, a fraction of the sample rate, under a Kaiser
/// window
fn lowpass(taps: usize, cutoff: f64) -> Vec<f64> {
  let beta = 0.1102 * (ATTENUATION_DB - 8.7);
  let middle = (taps - 1) as f64 / 2.0;
  (0..taps)
    .map(|i| {
      let t = i as f64 - middle;
      let x = std::f64::consts::PI * 2.0 * cutoff * t;
      let sinc = if x == 0.0 { 1.0 } else { x.sin() / x };
      let r = t / middle;
      let window = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta);
      2.0 * cutoff * sinc * window
    })
    .collect()
}

/// the first stage: bytes of one-bit samples to one float a byte
struct ByteStage {
  /// for each byte under the filter, the filter's output for each value of the byte
  tables: Vec<[f32; 256]>,
  /// the bytes still under the filter
  held: Vec<u8>,
}

impl ByteStage {
  /// a stage for one-bit samples at `rate` that keeps what is under `pass` Hz and removes what
  /// would fold onto it at an eighth of the rate
  fn new(rate: f64, pass: f64) -> Self {
    let stop = rate / 8.0 - pass;
    // whole bytes, and an even count of them so the filter's middle falls between two bytes
    let bytes = taps_for((stop - pass) / rate).div_ceil(16) * 2;
    let mut taps = lowpass(bytes * 8, (pass + stop) / 2.0 / rate);
    let sum: f64 = taps.iter().sum();
    for tap in &mut taps {
      *tap /= sum;
    }
    let (bytes_of_taps, _) = taps.as_chunks::<8>();
    let tables = bytes_of_taps
      .iter()
      .map(|eight| {
        let mut table = [0f32; 256];
        for (byte, out) in table.iter_mut().enumerate() {
          // a set bit is a sample of 1, a clear one a sample of -1
          let sum: f64 = eight
            .iter()
            .enumerate()
            .map(|(bit, tap)| {
              if byte >> (7 - bit) & 1 == 1 {
                *tap
              } else {
                -*tap
              }
            })
            .sum();
          *out = sum as f32;
        }
        table
      })
      .collect();
    Self {
      tables,
      // silence before the first byte puts the filter's middle on it
      held: vec![IDLE; bytes / 2],
    }
  }

  fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) {
    self.held.extend_from_slice(bytes);
    let span = self.tables.len();
    if self.held.len() < span {
      return;
    }
    let outputs = self.held.len() - span + 1;
    out.extend(self.held.windows(span).map(|window| {
      // four running sums, so one addition does not wait for the one before it
      let mut sums = [0f32; 4];
      let (bytes, last_bytes) = window.as_chunks::<4>();
      let (tables, last_tables) = self.tables.as_chunks::<4>();
      let rest: f32 = last_bytes
        .iter()
        .zip(last_tables)
        .map(|(byte, table)| table[usize::from(*byte)])
        .sum();
      for (bytes, tables) in bytes.iter().zip(tables) {
        for ((sum, byte), table) in sums.iter_mut().zip(bytes).zip(tables) {
          *sum += table[usize::from(*byte)];
        }
      }
      (sums[0] + sums[1]) + (sums[2] + sums[3]) + rest
    }));
    self.held.drain(..outputs);
  }
}

/// a half-band lowpass that leaves every other sample
struct HalfBand {
  /// the taps at even positions; those at odd positions are zero but the middle one, a half
  even: Vec<f32>,
  /// the samples at even positions still under the filter, and those at odd positions
  held: [Vec<f32>; 2],
  /// whether the next sample pushed is at an odd position
  odd: bool,
}

impl HalfBand {
  /// a stage for samples at `rate` that keeps what is under `pass` Hz
  fn new(rate: f64, pass: f64) -> Self {
    let width = (rate / 2.0 - 2.0 * pass) / rate;
    // 4k + 3 taps, so the outermost taps are not zero
    let taps = taps_for(width).saturating_sub(3).div_ceil(4) * 4 + 3;
    let all = lowpass(taps, 0.25);
    // the scale that makes the middle tap a half makes the sum one
    let scale = 0.5 / all[(taps - 1) / 2];
    let even: Vec<f32> = all
      .iter()
      .step_by(2)
      .map(|tap| (tap * scale) as f32)
      .collect();
    // the taps fall on the samples at odd positions and the middle tap on the even one between
    // them, so zeros before the first odd sample put the filter's middle on the first sample
    let held = [Vec::new(), vec![0.0; even.len() / 2]];
    Self {
      even,
      held,
      odd: false,
    }
  }

  fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) {
    let [even, odd] = &mut self.held;
    // a sample at an odd position left over from the push before goes first
    let samples = match (self.odd, samples.split_first()) {
      (true, Some((first, rest))) => {
        odd.push(*first);
        self.odd = false;
        rest
      }
      _ => samples,
    };
    let (pairs, last) = samples.as_chunks::<2>();
    even.extend(pairs.iter().map(|pair| pair[0]));
    if let [last] = last {
      self.odd = true;
      even.push(*last);
    }
    odd.extend(pairs.iter().map(|pair| pair[1]));
    let span = self.even.len();
    let ready = (odd.len() + 1).saturating_sub(span).min(even.len());
    out.extend(odd.windows(span).zip(even.iter()).map(|(window, middle)| {
      let mut sums = [0f32; 8];
      let (samples, last_samples) = window.as_chunks::<8>();
      let (taps, last_taps) = self.even.as_chunks::<8>();
      let rest: f32 = last_samples
        .iter()
        .zip(last_taps)
        .map(|(sample, tap)| sample * tap)
        .sum();
      for (samples, taps) in samples.iter().zip(taps) {
        for ((sum, sample), tap) in sums.iter_mut().zip(samples).zip(taps) {
          *sum += sample * tap;
        }
      }
      sums.iter().sum::<f32>() + rest + 0.5 * middle
    }));
    even.drain(..ready);
    odd.drain(..ready);
  }
}

/// one channel's filter from bytes of one-bit samples to PCM
struct Chain {
  first: ByteStage,
  halves: Vec<HalfBand>,
  scratch: [Vec<f32>; 2],
}

impl Chain {
  /// a filter from one-bit samples at `rate` to PCM at `pcm_rate`
  fn new(rate: u32, pcm_rate: u32) -> Self {
    let pass = PASS * f64::from(pcm_rate);
    let mut at = f64::from(rate) / 8.0;
    let mut halves = Vec::new();
    while at > f64::from(pcm_rate) {
      halves.push(HalfBand::new(at, pass));
      at /= 2.0;
    }
    Self {
      first: ByteStage::new(f64::from(rate), pass),
      halves,
      scratch: [Vec::new(), Vec::new()],
    }
  }

  /// adds the PCM that `bytes` bring out from under the filter to `out`. what is still under
  /// it comes out with the bytes after
  fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) {
    let [a, b] = &mut self.scratch;
    a.clear();
    self.first.push(bytes, a);
    for half in &mut self.halves {
      b.clear();
      half.push(a, b);
      std::mem::swap(a, b);
    }
    out.extend_from_slice(a);
  }
}

pub(crate) struct DsdDecoder {
  params: AudioCodecParameters,
  /// the one-bit rate and the PCM rate
  rates: (u32, u32),
  chains: Vec<Chain>,
  planes: Vec<Vec<f32>>,
  buffer: AudioBuffer<f32>,
}

impl DsdDecoder {
  fn try_new(params: &AudioCodecParameters) -> Result<Self> {
    let rate = params.extra_data.as_deref().and_then(|bytes| {
      let bytes: [u8; 4] = bytes.try_into().ok()?;
      Some(u32::from_le_bytes(bytes))
    });
    let channels = params.channels.as_ref().map_or(0, Channels::count);
    let rates = rate.and_then(|rate| Some((rate, pcm_rate(rate)?)));
    let (Some(rates), true) = (rates, (1..=MAX_CHANNELS as usize).contains(&channels)) else {
      return unsupported_error("dsd: the stream's parameters are missing or out of range");
    };
    if params.codec != CODEC_ID_DSD || params.sample_rate != Some(rates.1) {
      return unsupported_error("dsd: the stream's parameters are not a DSD stream's");
    }
    let spec = AudioSpec::new(rates.1, Channels::Discrete(channels as u16));
    Ok(Self {
      params: params.clone(),
      rates,
      chains: (0..channels)
        .map(|_| Chain::new(rates.0, rates.1))
        .collect(),
      planes: vec![Vec::new(); channels],
      // a packet's bytes make a sixteenth as many frames as they hold bits, or fewer
      buffer: AudioBuffer::new(spec, BLOCK),
    })
  }

  fn decode(&mut self, packet: &[u8]) -> Result<()> {
    self.buffer.clear();
    let channels = self.chains.len();
    let bytes = packet.len() / channels;
    if bytes > BLOCK || !packet.len().is_multiple_of(channels) {
      return decode_error("dsd: a packet's length is out of range");
    }
    // an empty packet has no channel to split it into
    let stored = packet.chunks_exact(bytes.max(1));
    for ((chain, plane), stored) in self.chains.iter_mut().zip(&mut self.planes).zip(stored) {
      plane.clear();
      chain.push(stored, plane);
    }
    let frames = self.planes[0].len();
    if frames > self.buffer.capacity() {
      return decode_error("dsd: a packet filtered to more frames than it can hold");
    }
    self.buffer.render_uninit(Some(frames));
    for (channel, plane) in self.planes.iter().enumerate() {
      let out = self
        .buffer
        .plane_mut(channel)
        .ok_or(Error::DecodeError("dsd: a channel has no plane"))?;
      out.copy_from_slice(plane);
    }
    Ok(())
  }
}

impl AudioDecoder for DsdDecoder {
  fn reset(&mut self) {
    // what was under the filter belongs to the place read before
    for chain in &mut self.chains {
      *chain = Chain::new(self.rates.0, self.rates.1);
    }
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

impl RegisterableAudioDecoder for DsdDecoder {
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

#[cfg(test)]
mod tests {
  use super::*;

  /// the gain of the filter from one-bit samples at 64 times 44.1 kHz to PCM at `hz`, in dB.
  /// the filter is linear, so flipping one bit of silence gives its response to one sample;
  /// a flip at each of the sixteen places a PCM sample spans gives the response at every
  /// one-bit sample, which is summed against a tone
  fn gain_db(hz: f64) -> f64 {
    let (rate, pcm) = (2_822_400u32, 176_400u32);
    let decimation = (rate / pcm) as usize;
    let bytes = 2048;
    let filtered = |flip: Option<usize>| {
      let mut silence = vec![IDLE; bytes];
      if let Some(bit) = flip {
        silence[bit / 8] ^= 0x80 >> (bit % 8);
      }
      let mut out = Vec::new();
      Chain::new(rate, pcm).push(&silence, &mut out);
      out
    };
    let still = filtered(None);
    let (mut re, mut im) = (0.0f64, 0.0f64);
    let middle = bytes * 4;
    for place in 0..decimation {
      let flipped = filtered(Some(middle + place));
      // a flipped bit moves its sample by two, up when it was clear and down when it was set
      let bit = middle + place;
      let was_set = IDLE >> (7 - bit % 8) & 1 == 1;
      let step = if was_set { -2.0 } else { 2.0 };
      for (frame, (with, without)) in flipped.iter().zip(&still).enumerate() {
        let tap = f64::from(with - without) / step;
        let at = (frame * decimation) as f64 - place as f64;
        let turn = std::f64::consts::TAU * hz * at / f64::from(rate);
        re += tap * turn.cos();
        im += tap * turn.sin();
      }
    }
    10.0 * (re * re + im * im).log10()
  }

  #[test]
  fn the_filter_passes_its_band_flat_and_removes_what_would_fold_onto_it() {
    for hz in [0.0, 1_000.0, 20_000.0, 50_000.0, 79_000.0] {
      let gain = gain_db(hz);
      assert!(gain.abs() < 0.001, "{hz} Hz: {gain} dB");
    }
    // everything from the PCM rate less the passband up folds onto the passband
    for hz in [
      97_100.0,
      120_000.0,
      176_400.0,
      273_000.0,
      352_800.0,
      1_000_000.0,
    ] {
      let gain = gain_db(hz);
      assert!(gain < -115.0, "{hz} Hz: {gain} dB");
    }
  }

  #[test]
  fn each_dsd_rate_is_filtered_to_a_sixteenth_or_to_eight_times_its_base_rate() {
    assert_eq!(pcm_rate(64 * 44_100), Some(176_400));
    assert_eq!(pcm_rate(128 * 44_100), Some(352_800));
    assert_eq!(pcm_rate(512 * 44_100), Some(352_800));
    assert_eq!(pcm_rate(64 * 48_000), Some(192_000));
    assert_eq!(pcm_rate(256 * 48_000), Some(384_000));
    assert_eq!(pcm_rate(44_100), None);
    assert_eq!(pcm_rate(96 * 44_100), None);
  }
}
