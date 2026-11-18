//! decodes AAC through `rusty_aac` behind symphonia's decoder interface: AAC-LC, and HE-AAC with
//! the spectral band replication and parametric stereo symphonia's own decoder leaves out

// TODO: remove this module and the rusty_aac dependency once a symphonia release decodes SBR and
// PS (https://github.com/pdeljanov/Symphonia/pull/473)

use rusty_aac::config::StreamConfig;
use symphonia::core::audio::{
  AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
};
use symphonia::core::codecs::CodecInfo;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{
  AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia::core::errors::{Error, Result, unsupported_error};
use symphonia::core::packet::PacketRef;
use tracing::{debug, trace};

/// the most channels a frame is taken with; the format's channel configurations go no further
const MAX_CHANNELS: u16 = 8;

/// the most sample frames a packet is taken with: an ADTS frame's four blocks of 1024, each
/// doubled by SBR
const MAX_FRAMES: usize = 4 * 2 * 1024;

/// the object type of AAC-LC, which a stream without a config is
const LC: u8 = 2;

const INFO: CodecInfo = CodecInfo {
  short_name: "aac",
  long_name: "Advanced Audio Coding",
  profiles: &[],
};

const SUPPORTED: [SupportedAudioCodec; 1] = [SupportedAudioCodec {
  id: CODEC_ID_AAC,
  info: INFO,
}];

/// what a stream's config states of its core, before SBR and parametric stereo
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Core {
  pub(crate) rate: u32,
  pub(crate) channels: usize,
}

/// the stream config of `params`: its AudioSpecificConfig, or AAC-LC at the stated rate and
/// channels for a stream that carries none, as an ADTS one
fn config(params: &AudioCodecParameters) -> Result<StreamConfig> {
  if let Some(bytes) = params.extra_data.as_deref() {
    return rusty_aac::config::parse(bytes).or_else(|e| {
      debug!(error = %e, "aac config not read");
      unsupported_error("aac: the stream's config is not one the decoder reads")
    });
  }
  let Some(rate) = params.sample_rate else {
    return unsupported_error("aac: the sample rate is not stated");
  };
  // a channel configuration counts its channels up to six, and 7 is eight of them
  let channels = match params.channels.as_ref().map(Channels::count) {
    Some(count @ 1..=6) => count as u8,
    Some(8) => 7,
    _ => return unsupported_error("aac: the channels are not stated"),
  };
  Ok(StreamConfig::lc(LC, rate, channels))
}

/// the core rate and channels the config of `params` states; `None` when it has no readable one
pub(crate) fn core(params: &AudioCodecParameters) -> Option<Core> {
  let config = config(params).ok()?;
  Some(Core {
    rate: config.sample_rate,
    channels: config.channels(),
  })
}

pub(crate) struct AacDecoder {
  inner: rusty_aac::AacDecoder,
  /// what a reset makes the decoder from again
  config: StreamConfig,
  params: AudioCodecParameters,
  buffer: AudioBuffer<f32>,
  /// the rate and channels of the first frame decoded, which every later one must have
  shape: Option<(u32, u16)>,
}

impl AacDecoder {
  fn try_new(params: &AudioCodecParameters) -> Result<Self> {
    if params.codec != CODEC_ID_AAC {
      return unsupported_error("aac: not an AAC stream");
    }
    let config = config(params)?;
    // the first frame says what the buffer holds; until then it is an empty one of one channel
    let spec = AudioSpec::new(config.sample_rate, Channels::Discrete(1));
    Ok(Self {
      inner: rusty_aac::AacDecoder::with_stream_config(config.clone()),
      config,
      params: params.clone(),
      buffer: AudioBuffer::new(spec, 0),
      shape: None,
    })
  }

  fn decode(&mut self, packet: &[u8]) -> Result<()> {
    self.buffer.clear();
    let frame = self.inner.decode(packet, None).map_err(|e| {
      trace!(error = %e, "aac packet not decoded");
      Error::DecodeError("aac: the packet could not be decoded")
    })?;
    let channels = usize::from(frame.channels);
    if frame.channels == 0 || frame.channels > MAX_CHANNELS || frame.sample_rate == 0 {
      return Err(Error::DecodeError("aac: a frame's shape is out of range"));
    }
    let frames = frame.samples.len() / channels;
    if frames > MAX_FRAMES || frame.samples.len() % channels != 0 {
      return Err(Error::DecodeError("aac: a frame's length is out of range"));
    }
    let shape = (frame.sample_rate, frame.channels);
    match self.shape {
      None => {
        let spec = AudioSpec::new(frame.sample_rate, Channels::Discrete(frame.channels));
        self.buffer = AudioBuffer::new(spec, MAX_FRAMES);
        self
          .params
          .with_sample_rate(frame.sample_rate)
          .with_channels(Channels::Discrete(frame.channels));
        self.shape = Some(shape);
      }
      Some(first) if first != shape => {
        return Err(Error::DecodeError(
          "aac: the stream changes its rate or channels",
        ));
      }
      Some(_) => {}
    }
    self.buffer.render_uninit(Some(frames));
    for channel in 0..channels {
      let plane = self
        .buffer
        .plane_mut(channel)
        .ok_or(Error::DecodeError("aac: a channel has no plane"))?;
      let samples = frame.samples.iter().skip(channel).step_by(channels);
      for (out, sample) in plane.iter_mut().zip(samples) {
        *out = *sample;
      }
    }
    Ok(())
  }
}

impl AudioDecoder for AacDecoder {
  fn reset(&mut self) {
    self.inner = rusty_aac::AacDecoder::with_stream_config(self.config.clone());
    self.buffer.clear();
  }

  fn codec_info(&self) -> &CodecInfo {
    &INFO
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

impl RegisterableAudioDecoder for AacDecoder {
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
