//! Audio the analyzer is tested on.
//!
//! It is made in the test from a fixed seed:
//!
//! - noise shaped like music's spectrum, written as FLAC, WavPack and Monkey's Audio
//! - the same noise through the LAME MP3 encoder and back, for the transcodes
//! - the same noise through libopus, as Opus streams in Ogg, MP4 and WebM
//! - tones as the one-bit samples of DSF and DSDIFF
//!
//! The files kept as bytes:
//!
//! - a short ALAC tone and five short HE-AAC files, which nothing here can encode
//! - a clip from the WavPack and Monkey's Audio reference encoders, with the samples it was
//!   made from
//!
//! As test support, every function here panics when encoding, writing or decoding fails.

use std::path::Path;

use flacenc::component::BitRepr;
use flacenc::error::Verify;
use mp3lame_encoder::{Builder, DualPcm, FlushNoGap, Quality};
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

use crate::decode::{Decoder, Filled, hint_of};
use crate::grid::{AAC_HOP, DctIv, Window, aac_bands, fold};

/// Bitrate an MP3 is written at.
pub use mp3lame_encoder::Bitrate;

/// ALAC tone, 16-bit stereo at 8000 Hz, in an MP4 container.
///
/// It holds four packets of 4096, 4096, 4096 and 3712 frames, one after the other in its media
/// data.
pub const TONE: &[u8] = include_bytes!("fixtures/tone.m4a");

/// HE-AAC file that states its band replication after the core's config.
///
/// Each HE-AAC file here is three seconds of stereo pink noise at 44.1 kHz with its core at
/// 22.05 kHz, made by fdkaac 1.0.9 from generated noise, at 48 kbit/s and, for HE-AAC v2, at
/// 32 kbit/s. Nothing here can encode them. Each states its band replication in another way:
///
/// - after the core's config (this file)
/// - in place of it ([`HE_AAC_HIERARCHICAL`])
/// - nowhere, in an MP4 whose track states the output rate ([`HE_AAC_IMPLICIT`])
/// - nowhere, in an ADTS stream ([`HE_AAC_ADTS`])
pub const HE_AAC: &[u8] = include_bytes!("fixtures/he-aac.m4a");
/// HE-AAC file whose config states HE-AAC first.
///
/// It is made as [`HE_AAC`] is.
pub const HE_AAC_HIERARCHICAL: &[u8] = include_bytes!("fixtures/he-aac-hierarchical.m4a");
/// HE-AAC file in an MP4 whose config states the core alone.
///
/// It is made as [`HE_AAC`] is.
pub const HE_AAC_IMPLICIT: &[u8] = include_bytes!("fixtures/he-aac-implicit.m4a");
/// HE-AAC file as an ADTS stream, whose headers state the core alone.
///
/// It is made as [`HE_AAC`] is.
pub const HE_AAC_ADTS: &[u8] = include_bytes!("fixtures/he-aac.aac");
/// HE-AAC v2 file, one coded channel made two by parametric stereo.
///
/// It is made as [`HE_AAC`] is.
pub const HE_AAC_V2: &[u8] = include_bytes!("fixtures/he-aac-v2.m4a");

/// Quarter of a second of 16-bit stereo noise and tones at 44.1 kHz.
///
/// The samples are little-endian. It is what [`WAVPACK`], [`APE_NORMAL`] and [`APE_INSANE`]
/// were made from, and what the reference decoders give back for them.
pub const CLIP_PCM: &[u8] = include_bytes!("fixtures/clip.pcm");
/// [`CLIP_PCM`] as WavPack, made by `wavpack -hx3` 5.9.0.
///
/// It has joint stereo and the extra modes the encoder here does not write.
pub const WAVPACK: &[u8] = include_bytes!("fixtures/clip.wv");
/// [`CLIP_PCM`] as hybrid WavPack at 320 kbit/s without its correction file.
///
/// Without the correction file the stream is lossy.
pub const WAVPACK_HYBRID: &[u8] = include_bytes!("fixtures/clip-hybrid.wv");

/// [`CLIP_PCM`] as Monkey's Audio at its normal level, made by `mac` 13.45.
pub const APE_NORMAL: &[u8] = include_bytes!("fixtures/clip-normal.ape");
/// [`CLIP_PCM`] as Monkey's Audio at its insane level, made by `mac` 13.45.
pub const APE_INSANE: &[u8] = include_bytes!("fixtures/clip-insane.ape");

/// Length of most generated files, in seconds.
///
/// It is more than the 30 s a reading needs.
pub const SECONDS: usize = 34;

/// Sample frames in each block of the FLAC written here.
pub const FLAC_BLOCK: usize = 4096;

/// Audio in memory: a sample rate and one sample vector per channel.
#[derive(Clone, Debug)]
pub struct Audio {
  /// Samples per second.
  pub rate: u32,
  /// Samples of each channel, from -1 to 1, all the same length.
  pub channels: Vec<Vec<f32>>,
}

/// a small fixed-seed generator, so every run makes the same audio
struct Rng(u64);

impl Rng {
  fn next(&mut self) -> f64 {
    // xorshift64*
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    let value = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
    (value >> 11) as f64 / (1u64 << 53) as f64
  }

  /// a normally distributed value
  fn gaussian(&mut self) -> f64 {
    let u = self.next().max(f64::MIN_POSITIVE);
    let v = self.next();
    (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
  }
}

/// Returns `seconds` of a sine at `hz`: a test tone.
///
/// It is one channel at 44.1 kHz, at -15 dBFS.
pub fn tone(hz: f64, seconds: usize) -> Audio {
  let rate = 44_100u32;
  let step = 2.0 * std::f64::consts::PI * hz / f64::from(rate);
  let mut audio = Audio {
    rate,
    channels: vec![
      (0..seconds * rate as usize)
        .map(|i| ((i as f64 * step) % (2.0 * std::f64::consts::PI)).sin() as f32)
        .collect(),
    ],
  };
  audio.set_level(-15.0);
  audio
}

/// Returns [`SECONDS`] of a tone at `hz` with its harmonics up to `top`.
///
/// The k-th harmonic is at 1/k^`fall`: a sawtooth with `fall` 1, as a synth or a bowed string
/// has, or a pulse with 0. One channel at 44.1 kHz at -15 dBFS.
pub fn harmonics(hz: f64, top: f64, fall: f64) -> Audio {
  let rate = 44_100u32;
  let length = SECONDS * rate as usize;
  let inverse = RealFftPlanner::<f32>::new().plan_fft_inverse(length);
  let mut spectrum = inverse.make_input_vec();
  // a whole number of cycles in the file puts every harmonic on a bin of its own
  let bin_hz = f64::from(rate) / length as f64;
  let mut harmonic = 1.0;
  while hz * harmonic < top {
    let bin = (hz * harmonic / bin_hz).round() as usize;
    spectrum[bin] = Complex::new(0.0, -1.0 / harmonic.powf(fall) as f32);
    harmonic += 1.0;
  }
  let mut channel = inverse.make_output_vec();
  inverse.process(&mut spectrum, &mut channel).unwrap();
  let mut audio = Audio {
    rate,
    channels: vec![channel],
  };
  audio.set_level(-15.0);
  audio
}

/// Returns music's spectrum at `hz`, roughly.
///
/// Its power falls 3 dB an octave, from 20 Hz up.
pub fn pink(hz: f64) -> f64 {
  if hz < 20.0 { 0.0 } else { (100.0 / hz).sqrt() }
}

/// Returns [`pink`] with nothing above `hz`.
///
/// It is an encoder's low-pass, or a filter applied when mastering.
pub fn cut_at(hz: f64) -> impl Fn(f64) -> f64 + Copy {
  move |f| if f < hz { pink(f) } else { 0.0 }
}

/// Returns [`pink`] with a comb of gaps between 16 and 20 kHz.
///
/// The gaps are 40 Hz wide every 80 Hz: what the gap measure counts.
pub fn combed(hz: f64) -> f64 {
  if (16_000.0..20_000.0).contains(&hz) && (hz % 80.0) < 40.0 {
    0.0
  } else {
    pink(hz)
  }
}

/// Returns `seconds` of orchestral-like stereo at 44.1 kHz.
///
/// It falls 6 dB an octave from 2 kHz and 24 dB an octave from 15 kHz, with notes every 3 s
/// decaying over 2.5 s and phrases swelling by 30 dB over 40 s, at -24 dBFS overall, so its
/// tails sit near the quantization noise.
pub fn classical(seconds: usize) -> Audio {
  let rate = 44_100u32;
  let spectrum =
    |hz: f64| pink(hz) / (1.0 + hz / 2_000.0).sqrt() / (1.0 + (hz / 15_000.0).powi(6)).sqrt();
  let mut audio = music_for(seconds, 21, rate, spectrum);
  for channel in &mut audio.channels {
    for (i, sample) in channel.iter_mut().enumerate() {
      let t = i as f64 / f64::from(rate);
      let phrase = 0.5 + 0.5 * (t / 20.0 * std::f64::consts::PI).sin();
      let level = 10f64.powf((-45.0 + 30.0 * phrase) / 20.0);
      let decay = (-(t % 3.0) / 2.5 * 3.0).exp().max(0.02);
      *sample *= (level * decay) as f32;
    }
  }
  audio.set_level(-24.0);
  audio
}

/// Returns [`pink`] rolling off gently from 3.5 kHz, as a lo-fi recording does.
///
/// The roll-off is 24 dB an octave.
pub fn lo_fi(hz: f64) -> f64 {
  pink(hz) / (1.0 + (hz / 3_500.0).powi(8)).sqrt()
}

/// `seconds` of noise at `rate` whose spectrum has the shape `envelope`, one channel
fn noise(seed: u64, seconds: usize, rate: u32, envelope: impl Fn(f64) -> f64) -> Vec<f32> {
  let length = seconds * rate as usize;
  let mut rng = Rng(seed | 1);
  let mut planner = RealFftPlanner::<f32>::new();
  let forward = planner.plan_fft_forward(length);
  let inverse = planner.plan_fft_inverse(length);
  let mut samples: Vec<f32> = (0..length).map(|_| rng.gaussian() as f32).collect();
  let mut spectrum = forward.make_output_vec();
  forward.process(&mut samples, &mut spectrum).unwrap();
  let bin_hz = f64::from(rate) / length as f64;
  for (bin, value) in spectrum.iter_mut().enumerate() {
    *value *= envelope(bin as f64 * bin_hz) as f32;
  }
  spectrum[0] = Complex::default();
  if let Some(last) = spectrum.last_mut() {
    last.im = 0.0;
  }
  inverse.process(&mut spectrum, &mut samples).unwrap();
  samples
}

/// Returns [`SECONDS`] of two partly correlated channels of shaped noise.
///
/// It has a slow swell in loudness, at -20 dBFS.
pub fn music(seed: u64, rate: u32, envelope: impl Fn(f64) -> f64 + Copy) -> Audio {
  music_for(SECONDS, seed, rate, envelope)
}

/// Returns [`music`] lasting `seconds`.
pub fn music_for(
  seconds: usize,
  seed: u64,
  rate: u32,
  envelope: impl Fn(f64) -> f64 + Copy,
) -> Audio {
  let common = noise(seed, seconds, rate, envelope);
  let apart = noise(seed.wrapping_mul(31), seconds, rate, envelope);
  let swell = |i: usize| 0.75 + 0.25 * (i as f64 / f64::from(rate) * 1.3).sin();
  let channel = |sign: f32| -> Vec<f32> {
    common
      .iter()
      .zip(&apart)
      .enumerate()
      .map(|(i, (c, a))| (c + sign * 0.5 * a) * swell(i) as f32)
      .collect()
  };
  let mut audio = Audio {
    rate,
    channels: vec![channel(1.0), channel(-1.0)],
  };
  audio.set_level(-20.0);
  audio
}

impl Audio {
  /// Scales the audio to an RMS level of `dbfs`.
  pub fn set_level(&mut self, dbfs: f64) {
    let count: usize = self.channels.iter().map(Vec::len).sum();
    let squares: f64 = self
      .channels
      .iter()
      .flatten()
      .map(|s| f64::from(*s).powi(2))
      .sum();
    let gain = (10f64.powf(dbfs / 20.0) / (squares / count as f64).sqrt()) as f32;
    for sample in self.channels.iter_mut().flatten() {
      *sample *= gain;
    }
  }

  /// Returns the audio with `other` added in at `gain`, sample for sample.
  pub fn mixed(mut self, other: &Audio, gain: f32) -> Audio {
    for (mine, theirs) in self.channels.iter_mut().zip(&other.channels) {
      for (a, b) in mine.iter_mut().zip(theirs) {
        *a += gain * b;
      }
    }
    self
  }

  /// Returns the first `seconds` of `self`, then the rest of `other`.
  pub fn spliced(&self, other: &Audio, seconds: usize) -> Audio {
    let split = seconds * self.rate as usize;
    Audio {
      rate: self.rate,
      channels: self
        .channels
        .iter()
        .zip(&other.channels)
        .map(|(a, b)| {
          a[..split]
            .iter()
            .chain(&b[split..b.len().min(a.len())])
            .copied()
            .collect()
        })
        .collect(),
    }
  }

  /// Returns the audio with a short burst of noise every `period` samples.
  ///
  /// The bursts are at the start of each period, as a lossy codec can leave at its frame edges.
  pub fn with_frame_rhythm(mut self, period: usize) -> Audio {
    let mut rng = Rng(5);
    for channel in &mut self.channels {
      for (i, sample) in channel.iter_mut().enumerate() {
        if i % period < 64 {
          *sample += 0.06 * rng.gaussian() as f32;
        }
      }
    }
    self
  }

  /// Returns the first channel twice, as a mono recording copied to stereo is.
  pub fn mono_to_stereo(&self) -> Audio {
    Audio {
      rate: self.rate,
      channels: vec![self.channels[0].clone(), self.channels[0].clone()],
    }
  }

  /// Returns the audio resampled to `rate` by padding its spectrum with silence.
  ///
  /// It is what a converter makes of it, with nothing above the old rate's band.
  pub fn resampled(&self, rate: u32) -> Audio {
    let mut planner = RealFftPlanner::<f32>::new();
    let channels = self
      .channels
      .iter()
      .map(|channel| {
        let length = channel.len();
        let target = length * rate as usize / self.rate as usize;
        let forward = planner.plan_fft_forward(length);
        let inverse = planner.plan_fft_inverse(target);
        let mut input = channel.clone();
        let mut spectrum = forward.make_output_vec();
        forward.process(&mut input, &mut spectrum).unwrap();
        let mut wide = inverse.make_input_vec();
        let shared = spectrum.len().min(wide.len()) - 1;
        wide[..shared].copy_from_slice(&spectrum[..shared]);
        let mut output = inverse.make_output_vec();
        inverse.process(&mut wide, &mut output).unwrap();
        // each transform scales by its length; this undoes the forward one
        let scale = 1.0 / length as f32;
        output.iter_mut().for_each(|s| *s *= scale);
        output
      })
      .collect();
    let mut audio = Audio { rate, channels };
    let level = self.level();
    audio.set_level(level);
    audio
  }

  fn level(&self) -> f64 {
    let count: usize = self.channels.iter().map(Vec::len).sum();
    let squares: f64 = self
      .channels
      .iter()
      .flatten()
      .map(|s| f64::from(*s).powi(2))
      .sum();
    10.0 * (squares / count as f64).log10()
  }

  /// the audio as integers of `bits`, with triangular dither, the same dither in every
  /// channel so identical channels stay identical
  fn quantized(&self, bits: u32, seed: u64) -> Vec<Vec<i32>> {
    let full = f64::from(1u32 << (bits - 1));
    self
      .channels
      .iter()
      .map(|channel| {
        let mut rng = Rng(seed | 1);
        channel
          .iter()
          .map(|s| {
            let dither = rng.next() - rng.next();
            (f64::from(*s) * full + dither)
              .round()
              .clamp(-full, full - 1.0) as i32
          })
          .collect()
      })
      .collect()
  }
}

/// Writes `audio` as FLAC at `path`.
///
/// The samples are `bits`-bit, or 16-bit stored in 24 bits when `padded`.
pub fn write_flac(audio: &Audio, bits: u32, padded: bool, path: &Path) {
  std::fs::write(path, flac(audio, bits, padded)).unwrap();
}

/// Returns `audio` encoded as FLAC, as [`write_flac`] writes it.
pub fn flac(audio: &Audio, bits: u32, padded: bool) -> Vec<u8> {
  let mut sink = flacenc::bitsink::ByteSink::new();
  flac_stream(audio, bits, padded).write(&mut sink).unwrap();
  sink.into_inner()
}

/// Returns `audio` as a WavPack stream of `bits`-bit samples, the ones [`flac`] writes.
pub fn wavpack(audio: &Audio, bits: u32) -> Vec<u8> {
  let planes = audio.quantized(bits, 7);
  let interleaved: Vec<i32> = (0..planes[0].len())
    .flat_map(|i| planes.iter().map(move |p| p[i]))
    .collect();
  let params = wavicle::EncodeParams {
    channels: planes.len() as u32,
    sample_rate: audio.rate,
    bits_per_sample: bits,
  };
  wavicle::encode_int(params, &interleaved).unwrap()
}

/// Returns `audio` as a Monkey's Audio file of `bits`-bit samples.
///
/// `bits` is 16 or 24, and the samples are the ones [`flac`] writes. The level is extra high
/// with `extra_high` and fast without.
pub fn ape(audio: &Audio, bits: u32, extra_high: bool) -> Vec<u8> {
  let planes = audio.quantized(bits, 7);
  let width = (bits / 8) as usize;
  let data: Vec<u8> = (0..planes[0].len())
    .flat_map(|i| planes.iter().map(move |p| p[i]))
    .flat_map(|sample| sample.to_le_bytes().into_iter().take(width))
    .collect();
  // the encoder takes a WAV file: its 44-byte header, then the samples
  let align = (width * planes.len()) as u16;
  let mut wav = b"RIFF".to_vec();
  wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
  wav.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0");
  wav.extend_from_slice(&(planes.len() as u16).to_le_bytes());
  wav.extend_from_slice(&audio.rate.to_le_bytes());
  wav.extend_from_slice(&(audio.rate * u32::from(align)).to_le_bytes());
  wav.extend_from_slice(&align.to_le_bytes());
  wav.extend_from_slice(&(bits as u16).to_le_bytes());
  wav.extend_from_slice(b"data");
  wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
  wav.extend_from_slice(&data);
  let level = if extra_high {
    oxideav_ape::CompressionLevel::ExtraHigh
  } else {
    oxideav_ape::CompressionLevel::Fast
  };
  oxideav_ape::encode_wav(&wav, level).unwrap()
}

/// One-bit samples of a DSD file, one vector of bytes per channel.
///
/// The first sample of each byte is in its highest bit, and each channel is whole blocks of
/// 4096 bytes, the last filled out with the pattern DSD holds for silence.
#[derive(Clone, Debug)]
pub struct OneBit {
  /// One-bit samples per second.
  pub rate: u32,
  /// One-bit samples in each channel.
  pub samples: u64,
  /// Bytes of each channel.
  pub channels: Vec<Vec<u8>>,
}

/// Returns `seconds` of tones as one-bit samples at 64 times 44.1 kHz.
///
/// Each channel holds the tone of its entry in `hz` at half of full scale, made at the one-bit
/// rate and put through a second-order sigma-delta modulator.
pub fn one_bit_tones(hz: &[f64], seconds: usize) -> OneBit {
  let rate = 64 * 44_100u32;
  let samples = seconds * rate as usize;
  let channels = hz
    .iter()
    .map(|hz| {
      let mut bytes = vec![0x69u8; samples.div_ceil(8).next_multiple_of(4096)];
      bytes[..samples.div_ceil(8)].fill(0);
      // the quantizer's error of the two steps before, which the loop feeds back so that it
      // comes out shaped by (1 - z^-1)^2
      let (mut before, mut two_before) = (0.0f64, 0.0f64);
      let step = std::f64::consts::TAU * hz / f64::from(rate);
      for n in 0..samples {
        let wanted = 0.5 * (step * n as f64).sin() - 2.0 * before + two_before;
        let bit = wanted >= 0.0;
        two_before = before;
        before = if bit { 1.0 } else { -1.0 } - wanted;
        if bit {
          bytes[n / 8] |= 0x80 >> (n % 8);
        }
      }
      bytes
    })
    .collect();
  OneBit {
    rate,
    samples: samples as u64,
    channels,
  }
}

impl OneBit {
  /// Returns the samples as a DSF file, the first sample of each byte in its lowest bit.
  pub fn dsf(&self) -> Vec<u8> {
    let data: usize = self.channels.iter().map(Vec::len).sum();
    let mut file = b"DSD ".to_vec();
    file.extend_from_slice(&28u64.to_le_bytes());
    file.extend_from_slice(&(92 + data as u64).to_le_bytes());
    file.extend_from_slice(&0u64.to_le_bytes());
    file.extend_from_slice(b"fmt ");
    file.extend_from_slice(&52u64.to_le_bytes());
    let count = self.channels.len() as u32;
    // the version, plain DSD, the channel layout, the channels, the rate, a bit per sample
    for field in [1, 0, count.min(2), count, self.rate, 1] {
      file.extend_from_slice(&field.to_le_bytes());
    }
    file.extend_from_slice(&self.samples.to_le_bytes());
    file.extend_from_slice(&4096u32.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(b"data");
    file.extend_from_slice(&(12 + data as u64).to_le_bytes());
    for block in 0..self.channels[0].len() / 4096 {
      for channel in &self.channels {
        let bytes = &channel[block * 4096..(block + 1) * 4096];
        file.extend(bytes.iter().map(|byte| byte.reverse_bits()));
      }
    }
    file
  }

  /// Returns the samples as a DSDIFF file.
  pub fn dff(&self) -> Vec<u8> {
    let chunk = |id: &[u8; 4], body: &[u8]| {
      let mut chunk = id.to_vec();
      chunk.extend_from_slice(&(body.len() as u64).to_be_bytes());
      chunk.extend_from_slice(body);
      // a chunk is padded to an even length
      chunk.resize(chunk.len().next_multiple_of(2), 0);
      chunk
    };
    let held = self.samples.div_ceil(8) as usize;
    let data: Vec<u8> = (0..held)
      .flat_map(|i| self.channels.iter().map(move |channel| channel[i]))
      .collect();
    let mut channels = (self.channels.len() as u16).to_be_bytes().to_vec();
    for id in [b"SLFT", b"SRGT", b"C   ", b"LFE ", b"LS  ", b"RS  "]
      .iter()
      .take(self.channels.len())
    {
      channels.extend_from_slice(*id);
    }
    let mut properties = b"SND ".to_vec();
    properties.extend(chunk(b"FS  ", &self.rate.to_be_bytes()));
    properties.extend(chunk(b"CHNL", &channels));
    properties.extend(chunk(b"CMPR", b"DSD \x0enot compressed\0"));
    let mut form = b"DSD ".to_vec();
    form.extend(chunk(b"FVER", &[1, 5, 0, 0]));
    form.extend(chunk(b"PROP", &properties));
    form.extend(chunk(b"DSD ", &data));
    chunk(b"FRM8", &form)
  }
}

/// Returns `audio` as a WavPack stream of float samples.
pub fn wavpack_float(audio: &Audio) -> Vec<u8> {
  let interleaved: Vec<f32> = (0..audio.channels[0].len())
    .flat_map(|i| audio.channels.iter().map(move |c| c[i]))
    .collect();
  wavicle::encode_float(audio.channels.len() as u32, audio.rate, &interleaved).unwrap()
}

/// Returns `audio` encoded as `bits`-bit FLAC in an Ogg container.
///
/// Every page but the last holds three frames of [`FLAC_BLOCK`] sample frames each, so a seek
/// lands on a page and reads on from its first frame.
pub fn ogg_flac(audio: &Audio, bits: u32) -> Vec<u8> {
  let stream = flac_stream(audio, bits, false);
  let bytes = |part: &dyn Fn(&mut flacenc::bitsink::ByteSink)| {
    let mut sink = flacenc::bitsink::ByteSink::new();
    part(&mut sink);
    sink.into_inner()
  };
  // the mapping's version 1.0 and one header packet after this one, then the native signature
  // and the stream information as a metadata block of 34 bytes
  let mut first = b"\x7fFLAC\x01\0\0\x01fLaC\0\0\0\x22".to_vec();
  first.extend(bytes(&|sink| stream.stream_info().write(sink).unwrap()));
  // the last metadata block: a comment block with an empty vendor and no comments
  let comment = [0x84, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0];
  let frames: Vec<Vec<u8>> = (0..stream.frame_count())
    .map(|n| bytes(&|sink| stream.frame(n).unwrap().write(sink).unwrap()))
    .collect();
  let pages = frames.chunks(3);
  let last = pages.len() + 1;
  let mut out = ogg_page(0x02, 0, 0, &[first]);
  out.extend(ogg_page(0, 0, 1, &[comment.to_vec()]));
  let mut held = 0;
  for (index, page) in pages.enumerate() {
    held += (page.len() * FLAC_BLOCK) as u64;
    let sequence = index as u32 + 2;
    let kind = if sequence as usize == last { 0x04 } else { 0 };
    out.extend(ogg_page(kind, held, sequence, page));
  }
  out
}

/// one Ogg page of whole `packets`: `kind` flags the stream's first and last page, and
/// `granule` is how many sample frames the stream holds up to the page's end
fn ogg_page(kind: u8, granule: u64, sequence: u32, packets: &[Vec<u8>]) -> Vec<u8> {
  let mut page = b"OggS\0".to_vec();
  page.push(kind);
  page.extend_from_slice(&granule.to_le_bytes());
  // the stream's serial number, the page's place in it, and the checksum filled in below
  page.extend_from_slice(&1u32.to_le_bytes());
  page.extend_from_slice(&sequence.to_le_bytes());
  page.extend_from_slice(&[0; 4]);
  // a packet is laced as bytes of 255 and one below 255 that ends it
  let lacing: Vec<u8> = packets
    .iter()
    .flat_map(|packet| {
      std::iter::repeat_n(255, packet.len() / 255).chain([(packet.len() % 255) as u8])
    })
    .collect();
  page.push(u8::try_from(lacing.len()).unwrap());
  page.extend(lacing);
  page.extend(packets.iter().flatten());
  let checksum = page.iter().fold(0u32, |crc, byte| {
    (0..8).fold(crc ^ (u32::from(*byte) << 24), |crc, _| {
      (crc << 1) ^ if crc >> 31 == 1 { 0x04c1_1db7 } else { 0 }
    })
  });
  page[22..26].copy_from_slice(&checksum.to_le_bytes());
  page
}

/// `audio` as a FLAC stream of whole blocks of [`FLAC_BLOCK`] sample frames
fn flac_stream(audio: &Audio, bits: u32, padded: bool) -> flacenc::component::Stream {
  let mut planes = audio.quantized(if padded { 16 } else { bits }, 7);
  if padded {
    planes.iter_mut().flatten().for_each(|s| *s <<= 8);
  }
  let config = flacenc::config::Encoder::default()
    .into_verified()
    .map_err(|(_, e)| e)
    .unwrap();
  // flacenc states the short last block as the stream's smallest, which readers take as a
  // stream of varying block sizes; whole blocks only keep the stream a fixed-size one
  // (https://github.com/yotarok/flacenc-rs/issues/242)
  // TODO: encode the tail once a flacenc release after 0.5.1 carries the fix (PR #255)
  let frames = planes[0].len() / FLAC_BLOCK * FLAC_BLOCK;
  let interleaved: Vec<i32> = (0..frames)
    .flat_map(|i| planes.iter().map(move |p| p[i]))
    .collect();
  let source = flacenc::source::MemSource::from_samples(
    &interleaved,
    planes.len(),
    bits as usize,
    audio.rate as usize,
  );
  flacenc::encode_with_fixed_block_size(&config, source, FLAC_BLOCK).unwrap()
}

/// Writes `audio`, two channels at 44.1 kHz, as a constant-bitrate MP3 at `path`.
pub fn write_mp3(audio: &Audio, bitrate: Bitrate, path: &Path) {
  // rounded to 16 bits without dither
  let rounded = |channel: &Vec<f32>| -> Vec<i16> {
    channel
      .iter()
      .map(|s| (f64::from(*s) * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
      .collect()
  };
  let (left, right) = (rounded(&audio.channels[0]), rounded(&audio.channels[1]));
  let mut encoder = Builder::new()
    .unwrap()
    .with_num_channels(2)
    .unwrap()
    .with_sample_rate(audio.rate)
    .unwrap()
    .with_brate(bitrate)
    .unwrap()
    .with_quality(Quality::Good)
    .unwrap()
    .build()
    .unwrap();
  // the encoder writes into the space reserved and no further
  let mut bytes = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(left.len()));
  encoder
    .encode_to_vec(
      DualPcm {
        left: &left,
        right: &right,
      },
      &mut bytes,
    )
    .unwrap();
  bytes.reserve(7200);
  encoder.flush_to_vec::<FlushNoGap>(&mut bytes).unwrap();
  std::fs::write(path, bytes).unwrap();
}

/// Puts a cover picture of `bytes` bytes in front of the MP3 at `path`.
///
/// The picture is in an ID3v2.3 tag, as a tagger embeds album art.
pub fn add_cover(path: &Path, bytes: usize) {
  let mut picture = vec![0u8; bytes];
  picture[..4].copy_from_slice(&[0xff, 0xd8, 0xff, 0xe0]);
  let mut frame = vec![0u8];
  frame.extend_from_slice(b"image/jpeg\0\x03\0");
  frame.extend_from_slice(&picture);
  let mut tag = b"APIC".to_vec();
  tag.extend_from_slice(&(frame.len() as u32).to_be_bytes());
  tag.extend_from_slice(&[0, 0]);
  tag.extend_from_slice(&frame);
  // the tag's size is written seven bits to a byte
  let size = tag.len() as u32;
  let synchsafe = [size >> 21, size >> 14, size >> 7, size].map(|part| (part & 0x7f) as u8);
  let mut file = b"ID3\x03\0\0".to_vec();
  file.extend_from_slice(&synchsafe);
  file.extend_from_slice(&tag);
  file.extend_from_slice(&std::fs::read(path).unwrap());
  std::fs::write(path, file).unwrap();
}

/// Returns the audio of the file at `path`, decoded whole.
pub fn decoded(path: &Path) -> Audio {
  let file = std::fs::File::open(path).unwrap();
  let mut decoder = Decoder::from_source(Box::new(file), &hint_of(path)).unwrap();
  let rate = decoder.claimed.sample_rate;
  let mut channels = vec![Vec::new(); decoder.channels];
  let mut window = vec![Vec::new(); decoder.channels];
  loop {
    let filled = decoder.fill(&mut window, rate as usize);
    for (all, part) in channels.iter_mut().zip(&window) {
      all.extend_from_slice(part);
    }
    if filled != Filled::Full {
      break;
    }
  }
  Audio { rate, channels }
}

/// Returns `audio` through an MP3 encoder at `bitrate` and decoded again.
///
/// It is how a transcode starts.
pub fn through_mp3(audio: &Audio, bitrate: Bitrate, dir: &Path) -> Audio {
  let mp3 = dir.join(format!("through-{}.mp3", bitrate as u32));
  write_mp3(audio, bitrate, &mp3);
  decoded(&mp3)
}

/// Window of the long blocks [`through_transform`] codes with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LongWindow {
  /// Kaiser-Bessel-derived window, as ffmpeg's AAC encoder uses.
  KaiserBessel,
  /// Sine window, as Apple's AAC encoder uses.
  Sine,
}

/// Returns `audio` through a transform coder with AAC's long blocks and decoded again.
///
/// The coder is a 2048-point MDCT at a hop of 1024 under `window`. It drops every line above
/// 16 kHz and rounds the others to a step of 0.7 times the level of their band of 16 lines,
/// which zeroes most of them, as an encoder at a low bitrate does. The first block starts
/// `delay` samples before the audio, so the decode's frame grid is at an offset of
/// 1024 less `delay`.
pub fn through_transform(audio: &Audio, window: LongWindow, delay: usize) -> Audio {
  transform_coded(audio, window, delay, |lines| {
    for band in lines.chunks_mut(16) {
      let step = 0.7 * level(band);
      for line in band.iter_mut().filter(|_| step > 0.0) {
        *line = (*line / step).round() * step;
      }
    }
  })
}

/// Returns `audio` through the coder of [`through_transform`] with AAC's quantizer.
///
/// The lines under 16 kHz are rounded by scalefactor band, with the bands of a long block at
/// 44.1 and 48 kHz: each to a whole number to the power 4/3 times the band's step, the whole
/// power of the fourth root of two nearest 0.7 times the band's level.
pub fn through_stepped_transform(audio: &Audio, window: LongWindow, delay: usize) -> Audio {
  let bands = aac_bands(audio.rate).expect("the rate is 44.1 or 48 kHz");
  transform_coded(audio, window, delay, |lines| {
    for band in bands.windows(2) {
      let Some(band) = lines.get_mut(band[0]..band[1]) else {
        break;
      };
      let level = level(band);
      if level == 0.0 {
        continue;
      }
      let step = 2.0f32.powf((4.0 * (0.7 * level).log2()).round() / 4.0);
      for line in band {
        let whole = (line.abs() / step).powf(0.75).round();
        *line = line.signum() * whole.powf(4.0 / 3.0) * step;
      }
    }
  })
}

/// the root mean square of `lines`
fn level(lines: &[f32]) -> f32 {
  (lines.iter().map(|l| l * l).sum::<f32>() / lines.len() as f32).sqrt()
}

/// `audio` through a 2048-point MDCT at a hop of 1024 under `window` and back, the first block
/// `delay` samples before the audio: `quantize` rounds each block's lines under 16 kHz, and
/// the lines above are dropped
fn transform_coded(
  audio: &Audio,
  window: LongWindow,
  delay: usize,
  quantize: impl Fn(&mut [f32]),
) -> Audio {
  assert!(delay <= AAC_HOP, "the delay is at most a hop");
  let window = match window {
    LongWindow::KaiserBessel => Window::Kbd,
    LongWindow::Sine => Window::Sine,
  }
  .samples();
  let kept = (16_000.0 * 2.0 * AAC_HOP as f64 / f64::from(audio.rate)) as usize;
  let mut dct = DctIv::new(AAC_HOP);
  let (mut frame, mut folded) = (vec![0.0f32; 2 * AAC_HOP], vec![0.0f32; AAC_HOP]);
  let mut lines = vec![0.0f32; AAC_HOP];
  let quarter = AAC_HOP / 2;
  let channels = audio
    .channels
    .iter()
    .map(|channel| {
      // a hop of silence on each side, so every sample is under two blocks
      let mut padded = vec![0.0f32; delay + AAC_HOP];
      padded.extend_from_slice(channel);
      padded.resize(padded.len().next_multiple_of(AAC_HOP) + AAC_HOP, 0.0);
      let mut out = vec![0.0f32; padded.len()];
      for start in (0..padded.len() - AAC_HOP).step_by(AAC_HOP) {
        let block = &padded[start..start + 2 * AAC_HOP];
        for ((f, x), w) in frame.iter_mut().zip(block).zip(&window) {
          *f = x * w;
        }
        fold(&frame, &mut folded);
        dct.process(&folded, &mut lines);
        lines[kept.min(AAC_HOP)..].fill(0.0);
        quantize(&mut lines[..kept.min(AAC_HOP)]);
        // the orthonormal DCT-IV is its own inverse, and the transpose of the fold puts its
        // output back over the block's four quarters
        dct.process(&lines, &mut folded);
        let (low, high) = folded.split_at(quarter);
        let unfolded = (0..2 * AAC_HOP).map(|n| match (n / quarter, n % quarter) {
          (0, i) => high[i],
          (1, i) => -high[quarter - 1 - i],
          (2, i) => -low[quarter - 1 - i],
          (_, i) => -low[i],
        });
        for ((o, y), w) in out[start..].iter_mut().zip(unfolded).zip(&window) {
          *o += y * w;
        }
      }
      out[delay + AAC_HOP..delay + AAC_HOP + channel.len()].to_vec()
    })
    .collect();
  Audio {
    rate: audio.rate,
    channels,
  }
}

/// one ISO media box: its size, its type, then `body`
fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
  let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
  out.extend_from_slice(kind);
  out.extend_from_slice(body);
  out
}

/// a box with a version and flags of zero before `body`
fn mp4_full_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
  mp4_box(kind, &[&[0u8; 4][..], body].concat())
}

/// big-endian 32-bit words, as ISO media boxes hold their numbers
fn mp4_words(values: &[u32]) -> Vec<u8> {
  values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// an audio sample entry of `kind`: the channels, 16 bits a sample and the rate, then `boxes`
fn mp4_sample_entry(kind: &[u8; 4], channels: u8, rate: u32, boxes: &[u8]) -> Vec<u8> {
  mp4_box(
    kind,
    &[
      vec![0; 6],
      vec![0, 1, 0, 0, 0, 0],
      vec![0; 4],
      vec![0, channels, 0, 16, 0, 0, 0, 0],
      mp4_words(&[rate << 16]),
      boxes.to_vec(),
    ]
    .concat(),
  )
}

/// an MP4 of one audio track at `rate` holding `data`. `tables` gives the sample table's boxes
/// for a file whose data starts at the offset it is given, and must not change their size with it
fn mp4_file(rate: u32, tables: impl Fn(u32) -> Vec<u8>, data: &[u8]) -> Vec<u8> {
  let ftyp = mp4_box(b"ftyp", b"M4A \0\0\0\0M4A isom");
  let matrix = mp4_words(&[0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000]);
  let moov = |offset: u32| {
    let mvhd = mp4_full_box(
      b"mvhd",
      &[
        mp4_words(&[0, 0, 1000, 0, 0x1_0000]),
        vec![1, 0, 0, 0],
        vec![0; 8],
        matrix.clone(),
        vec![0; 24],
        mp4_words(&[2]),
      ]
      .concat(),
    );
    let tkhd = mp4_box(
      b"tkhd",
      &[
        mp4_words(&[7, 0, 0, 1, 0, 0, 0, 0]),
        vec![0, 0, 0, 0, 1, 0, 0, 0],
        matrix.clone(),
        mp4_words(&[0, 0]),
      ]
      .concat(),
    );
    let mdhd = mp4_full_box(
      b"mdhd",
      &[mp4_words(&[0, 0, rate, 0]), vec![0x55, 0xc4, 0, 0]].concat(),
    );
    let hdlr = mp4_full_box(
      b"hdlr",
      &[mp4_words(&[0]), b"soun".to_vec(), vec![0; 13]].concat(),
    );
    let stbl = mp4_box(b"stbl", &tables(offset));
    let smhd = mp4_full_box(b"smhd", &[0; 4]);
    let minf = mp4_box(b"minf", &[smhd, stbl].concat());
    let mdia = mp4_box(b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = mp4_box(b"trak", &[tkhd, mdia].concat());
    mp4_box(b"moov", &[mvhd, trak].concat())
  };
  // the audio follows the header boxes, whose size does not depend on where it starts
  let offset = (ftyp.len() + moov(0).len() + 8) as u32;
  [ftyp, moov(offset), mp4_box(b"mdat", data)].concat()
}

/// Writes an MP4 at `path` that states more audio than it holds.
///
/// The file is 16-bit mono PCM holding the first second of `audio` once. Its sample tables list
/// `chunks` seconds that all point at it and give every sample `delta` ticks of the sample rate,
/// so the file states `chunks` times `delta` seconds and every seek into that length succeeds.
pub fn write_repeating_mp4(audio: &Audio, chunks: u32, delta: u32, path: &Path) {
  let rate = audio.rate;
  let pcm: Vec<u8> = audio.channels[0][..rate as usize]
    .iter()
    .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
    .collect();
  let samples = chunks * rate;
  let tables = |offset: u32| {
    let sowt = mp4_sample_entry(b"sowt", 1, rate, &[]);
    let stsd = mp4_full_box(b"stsd", &[mp4_words(&[1]), sowt].concat());
    let stts = mp4_full_box(b"stts", &mp4_words(&[1, samples, delta]));
    let stsc = mp4_full_box(b"stsc", &mp4_words(&[1, 1, rate, 1]));
    let stsz = mp4_full_box(b"stsz", &mp4_words(&[2, samples]));
    let offsets: Vec<u32> = std::iter::once(chunks)
      .chain(std::iter::repeat_n(offset, chunks as usize))
      .collect();
    let stco = mp4_full_box(b"stco", &mp4_words(&offsets));
    [stsd, stts, stsc, stsz, stco].concat()
  };
  std::fs::write(path, mp4_file(rate, tables, &pcm)).unwrap();
}

/// Settings an Opus stream is encoded with.
#[derive(Clone, Copy, Debug)]
pub struct OpusSettings {
  /// Bitrate in bit/s.
  pub bitrate: u32,
  /// Length of a frame in sample frames: 120, 240, 480, 960, 1920 or 2880.
  pub frame: usize,
  /// Whether the encoder is set for voice.
  ///
  /// It is set for music otherwise.
  pub voice: bool,
  /// Output gain the stream's header states, in 1/256 dB.
  pub gain: i16,
}

impl OpusSettings {
  /// Returns the settings of music at `kbps` kbit/s in 20 ms frames, with no output gain.
  pub fn music(kbps: u32) -> Self {
    Self {
      bitrate: kbps * 1000,
      frame: 960,
      voice: false,
      gain: 0,
    }
  }
}

/// Opus packets of some audio, and what a container states of them.
#[derive(Clone, Debug)]
pub struct OpusStream {
  /// Packets, one frame each, in order.
  pub packets: Vec<Vec<u8>>,
  /// Decoded sample frames a player drops before the audio starts.
  pub pre_skip: u16,
  /// Length of the audio in sample frames, less than the packets decode to.
  pub frames: u64,
  /// Number of channels, one or two.
  pub channels: u8,
  /// Settings the packets were encoded with.
  pub settings: OpusSettings,
}

/// Returns `audio`, one or two channels at 48 kHz, encoded by libopus with `settings`.
pub fn opus(audio: &Audio, settings: OpusSettings) -> OpusStream {
  assert_eq!(audio.rate, 48_000, "the audio is at 48 kHz");
  let channels = match audio.channels.len() {
    1 => opus::Channels::Mono,
    _ => opus::Channels::Stereo,
  };
  let count = channels as usize;
  let application = if settings.voice {
    opus::Application::Voip
  } else {
    opus::Application::Audio
  };
  let mut encoder = opus::Encoder::new(48_000, channels, application).unwrap();
  encoder
    .set_bitrate(opus::Bitrate::Bits(settings.bitrate as i32))
    .unwrap();
  let pre_skip = encoder.get_lookahead().unwrap() as usize;
  let frames = audio.channels[0].len();
  // the decoder gives the audio back after the pre-skip, so silence that long follows it
  let padded = (frames + pre_skip).div_ceil(settings.frame) * settings.frame;
  let interleaved: Vec<f32> = (0..padded)
    .flat_map(|i| {
      audio.channels[..count]
        .iter()
        .map(move |channel| channel.get(i).copied().unwrap_or(0.0))
    })
    .collect();
  let packets = interleaved
    .chunks(settings.frame * count)
    .map(|frame| encoder.encode_vec_float(frame, 4000).unwrap())
    .collect();
  OpusStream {
    packets,
    pre_skip: pre_skip as u16,
    frames: frames as u64,
    channels: count as u8,
    settings,
  }
}

impl OpusStream {
  /// the identification header: the channels, the pre-skip, the input rate and the output gain,
  /// for one stream of one or two channels
  fn head(&self) -> Vec<u8> {
    let mut head = b"OpusHead\x01".to_vec();
    head.push(self.channels);
    head.extend_from_slice(&self.pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&self.settings.gain.to_le_bytes());
    head.push(0);
    head
  }

  /// Returns the stream in an Ogg container, as an `.opus` file holds it.
  ///
  /// Each page holds up to a second of packets, 50 at most, and the last page states where the
  /// audio ends.
  pub fn ogg(&self) -> Vec<u8> {
    // an empty vendor string and no comments
    let tags = b"OpusTags\0\0\0\0\0\0\0\0".to_vec();
    let mut out = ogg_page(0x02, 0, 0, &[self.head()]);
    out.extend(ogg_page(0, 0, 1, &[tags]));
    let pages = self.packets.chunks((48_000 / self.settings.frame).min(50));
    let last = pages.len();
    let end = u64::from(self.pre_skip) + self.frames;
    let mut held = 0;
    for (index, page) in pages.enumerate() {
      held += (page.len() * self.settings.frame) as u64;
      let (kind, granule) = if index + 1 == last {
        (0x04, end)
      } else {
        (0, held)
      };
      out.extend(ogg_page(kind, granule, index as u32 + 2, page));
    }
    out
  }

  /// Returns the stream in a WebM container.
  ///
  /// Each cluster holds a second of packets. The container counts in milliseconds and does not
  /// state where the audio ends, so a reader gets the frames that pad the last packet too.
  pub fn webm(&self) -> Vec<u8> {
    let unsigned = |value: u64| -> Vec<u8> {
      let bytes = value.to_be_bytes();
      let skip = bytes.iter().take_while(|b| **b == 0).count().min(7);
      bytes[skip..].to_vec()
    };
    let header = [
      ebml(&[0x42, 0x86], &[1]),
      ebml(&[0x42, 0xf7], &[1]),
      ebml(&[0x42, 0xf2], &[4]),
      ebml(&[0x42, 0xf3], &[8]),
      ebml(&[0x42, 0x82], b"webm"),
      ebml(&[0x42, 0x87], &[4]),
      ebml(&[0x42, 0x85], &[2]),
    ]
    .concat();
    // a tick of a millisecond, and the names of what wrote the file
    let info = [
      ebml(&[0x2a, 0xd7, 0xb1], &unsigned(1_000_000)),
      ebml(&[0x4d, 0x80], b"choon"),
      ebml(&[0x57, 0x41], b"choon"),
    ]
    .concat();
    let delay_ns = u64::from(self.pre_skip) * 1_000_000_000 / 48_000;
    // the rate, the channels, and a bit depth, which muxers state of Opus too
    let audio = [
      ebml(&[0xb5], &48_000f64.to_be_bytes()),
      ebml(&[0x9f], &[self.channels]),
      ebml(&[0x62, 0x64], &[16]),
    ]
    .concat();
    let track = [
      ebml(&[0xd7], &[1]),
      ebml(&[0x73, 0xc5], &[1]),
      ebml(&[0x83], &[2]),
      ebml(&[0x86], b"A_OPUS"),
      ebml(&[0x56, 0xaa], &unsigned(delay_ns)),
      ebml(&[0x56, 0xbb], &unsigned(80_000_000)),
      ebml(&[0x63, 0xa2], &self.head()),
      ebml(&[0xe1], &audio),
    ]
    .concat();
    let per_cluster = 48_000 / self.settings.frame;
    let clusters: Vec<u8> = self
      .packets
      .chunks(per_cluster)
      .enumerate()
      .flat_map(|(second, packets)| {
        let mut cluster = ebml(&[0xe7], &unsigned(second as u64 * 1000));
        for (index, packet) in packets.iter().enumerate() {
          let ms = (index * self.settings.frame / 48) as i16;
          // track 1, the time within the cluster, a keyframe
          let mut block = vec![0x81];
          block.extend_from_slice(&ms.to_be_bytes());
          block.push(0x80);
          block.extend_from_slice(packet);
          cluster.extend(ebml(&[0xa3], &block));
        }
        ebml(&[0x1f, 0x43, 0xb6, 0x75], &cluster)
      })
      .collect();
    let segment = [
      ebml(&[0x15, 0x49, 0xa9, 0x66], &info),
      ebml(&[0x16, 0x54, 0xae, 0x6b], &ebml(&[0xae], &track)),
      clusters,
    ]
    .concat();
    [
      ebml(&[0x1a, 0x45, 0xdf, 0xa3], &header),
      ebml(&[0x18, 0x53, 0x80, 0x67], &segment),
    ]
    .concat()
  }
}

impl OpusStream {
  /// Returns the stream in an MP4 container.
  ///
  /// Its header box stores the identification header's numbers big-endian behind a version of
  /// 0. The timestamps count the pre-skip, and the last packet's duration ends where the audio
  /// does.
  pub fn mp4(&self) -> Vec<u8> {
    let frame = self.settings.frame as u32;
    let count = self.packets.len() as u32;
    let mut dops = vec![0, self.channels];
    dops.extend_from_slice(&self.pre_skip.to_be_bytes());
    dops.extend_from_slice(&48_000u32.to_be_bytes());
    dops.extend_from_slice(&self.settings.gain.to_be_bytes());
    dops.push(0);
    let entry = mp4_sample_entry(b"Opus", self.channels, 48_000, &mp4_box(b"dOps", &dops));
    let end = u32::from(self.pre_skip) + self.frames as u32;
    let last = end - (count - 1) * frame;
    let sizes: Vec<u32> = self.packets.iter().map(|p| p.len() as u32).collect();
    let tables = |offset: u32| {
      let stsd = mp4_full_box(b"stsd", &[mp4_words(&[1]), entry.clone()].concat());
      let stts = mp4_full_box(b"stts", &mp4_words(&[2, count - 1, frame, 1, last]));
      let stsc = mp4_full_box(b"stsc", &mp4_words(&[1, 1, count, 1]));
      let stsz = mp4_full_box(
        b"stsz",
        &[mp4_words(&[0, count]), mp4_words(&sizes)].concat(),
      );
      let stco = mp4_full_box(b"stco", &mp4_words(&[1, offset]));
      [stsd, stts, stsc, stsz, stco].concat()
    };
    mp4_file(48_000, tables, &self.packets.concat())
  }
}

/// one EBML element: its id, its size in eight bytes, then `body`
fn ebml(id: &[u8], body: &[u8]) -> Vec<u8> {
  let mut out = id.to_vec();
  out.extend_from_slice(&((1u64 << 56) | body.len() as u64).to_be_bytes());
  out.extend_from_slice(body);
  out
}
