//! Frame grids that MP3, AAC and Opus encoders leave in decoded audio.
//!
//! An encoder transforms its input in frames on a fixed grid and quantizes what the transform
//! gives. The decoder's synthesis inverts the encoder's analysis, so running that analysis again
//! on the decoded samples, with frames on the encoder's grid, gives the quantized values back.
//! With frames at any other offset they are mixed with the neighbouring frames'. Lossless audio
//! has no such offset.
//!
//! The scan reads four stretches of 40960 samples, at 20, 40, 60 and 80% of the file.
//!
//! A file at 44.1 or 48 kHz is scanned for MP3 and AAC a second time with its stretches brought
//! to the other of the two rates, by a long filter that keeps the sample times the rates share.
//! A decode converted between them carries its grid at the rate it was decoded at, and the
//! conversion back leaves the quantized values close enough to count ([`OtherRate`]).
//!
//! # MP3 and AAC
//!
//! Both zero many transform values, and on the grid every zeroed value comes out at the level
//! of the samples' rounding noise. For each offset of a codec's grid, the scan counts the
//! transform values that stand above the rounding noise of 16-bit samples, on a log scale, in
//! left and right or in mid and side, whichever pair counts fewer in a frame. The offset with
//! the lowest count, and how far that count sits below the others, is the codec's
//! [`CodecScan`]. The AAC analysis is a long block's: a 2048-point MDCT at a hop of 1024. It
//! runs once with each of the two windows the format gives a long block, the
//! Kaiser-Bessel-derived one, which ffmpeg's encoder uses, and the sine one, which Apple's
//! uses. The values an encoder zeroed come back only under its own window, so each analysis
//! finds the other's grid weakly. The MP3 analysis is the layer III hybrid filterbank: the
//! 512-tap polyphase filterbank, an 18-line MDCT per subband and the alias-reduction
//! butterflies, at a hop of 576. Short blocks are not analyzed.
//!
//! The values an MP3 encoder kept are whole numbers to the power 4/3 times their scalefactor
//! band's step. With the granules at the count's offset, the bands whose values sit on one
//! step are counted too ([`Mp3Steps`]), which shows a grid whose count stands out too little.
//!
//! # Opus
//!
//! Opus's CELT layer codes the energy of each of its 21 bands, in each frame, as a step from a
//! prediction: half the band's energy a frame earlier plus a sum over the frame's lower bands.
//! The step is a whole number of 6 dB refined by a few bits. On the grid, each band's energy
//! comes back, and taking the prediction off it leaves a number on that lattice. A frame counts
//! when at least 80% of the bands that stand over the samples' rounding noise, in each channel,
//! are within 0.003 of a 32nd of 6 dB. The offset that counts the most frames is the
//! [`OpusScan`].
//!
//! The analysis is of 20 ms frames at 48 kHz, the rate Opus runs at: a 1920-point MDCT at a
//! hop of 960 with CELT's low-overlap window, after the encoder's pre-emphasis. Samples at
//! 44.1 kHz are brought back to 48 kHz first; other rates are not scanned. The windows the
//! file is measured at are read too, on the one grid a decode has when nothing was cut from
//! its start.
//!
//! A frame is read as one long block after another, with the pitch post-filter off in both.
//! Where one frame at least sits on the lattice that way, the frames that sit on it read
//! another way are counted with it: as the eight short blocks of a transient frame, after such
//! a frame, or coded without the frame before them. Frames of the hybrid mode, which codes only
//! the bands from 8 kHz up this way, are counted apart ([`OpusHybrid`]).
//!
//! # Limits
//!
//! A grid is evidence that the samples were decoded from that codec, at some point.
//!
//! - Resampling after decoding moves an MP3 or AAC grid off the samples. It is looked for
//!   again in a file at 44.1 or 48 kHz brought to the other of the two rates; after any other
//!   conversion it is not found. Opus's energies are still found after a conversion from 48 to
//!   44.1 kHz.
//! - Clipping hides a grid frame by frame.
//! - Vorbis's grid shows through the AAC scan, so a Vorbis decode reads as AAC.
//! - Opus frames of 2.5, 5 and 10 ms, frames the pitch post-filter ran over, and its SILK
//!   mode are not found.
//! - A gain after an Opus decode moves the lattice, and the scan searches for one move a
//!   channel ([`OpusShifted`]). A level that changes over time, as under a fade or a limiter,
//!   hides the lattice where it changes.

use std::fmt;
use std::sync::Arc;

use crate::lattice;
use crate::resample::Resampler;
use crate::thresholds::ReadingThresholds;
use rayon::prelude::*;
use rustfft::num_complex::Complex;
use rustfft::num_traits::Float;
use rustfft::{Fft, FftPlanner};

/// AAC's hop, the length of its long frames' new samples
pub(crate) const AAC_HOP: usize = 1024;
/// MP3's hop, a granule
const MP3_HOP: usize = 576;
/// the number of segments scanned
pub(crate) const SEGMENTS: usize = 4;
/// the samples of one segment: 40 AAC hops
pub(crate) const SEGMENT: usize = 40 * AAC_HOP;
/// where the segments start, as fractions of the file's length
const SEGMENT_AT: [f64; SEGMENTS] = [0.2, 0.4, 0.6, 0.8];
/// the rounding noise of 16-bit samples in samples scaled to [-1, 1): 2^-15 / sqrt(12)
const NOISE16: f64 = 8.809_710_218_959_115e-6;
/// the rms of a hybrid filterbank value per unit rms of white input, which puts the MP3 noise
/// level at the 16-bit rounding noise of the filterbank's output
const MP3_NOISE_GAIN: f64 = 0.527_861_394_696_74;
/// MAD times this estimates the standard deviation of normally distributed values
const MAD_TO_SD: f64 = 1.4826;

/// Codec whose frame grid the scan looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Codec {
  /// AAC, read with a long block's transform under either of its windows.
  ///
  /// Vorbis decodes read as AAC too.
  Aac,
  /// MP3, layer III.
  Mp3,
  /// Opus, by the band energies of its CELT layer in 20 ms frames.
  Opus,
}

impl fmt::Display for Codec {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Aac => "AAC",
      Self::Mp3 => "MP3",
      Self::Opus => "Opus",
    })
  }
}

/// How far one codec's grid stands out in a file.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct CodecScan {
  /// Offset of the frames that count the fewest values, in samples.
  ///
  /// It is where frames start, modulo the codec's hop, counted from the file's first sample.
  pub offset: usize,
  /// How far the count at `offset` sits below the others, in robust standard deviations.
  ///
  /// The counts of the four segments are summed per offset; the standard deviation is
  /// estimated from the median absolute deviation.
  pub z: f64,
  /// Number of segments whose own lowest count is at `offset`.
  pub segments_agreeing: usize,
  /// Each segment's own z, in file order.
  pub segment_z: [f64; SEGMENTS],
}

/// Offset of an Opus decode's frames when nothing was cut from its start, in samples.
///
/// It is where frames start at 48 kHz, modulo the 960 samples of a 20 ms frame, counted from
/// the file's first sample.
pub const OPUS_UNCUT_OFFSET: usize = lattice::UNCUT_OFFSET;

/// How many frames of a file carry Opus's band energies.
///
/// A frame counts when the energies of its bands, taken with the analysis of Opus's CELT layer,
/// sit on the lattice of the codec's energy quantizer. Samples at 44.1 kHz are brought back to
/// the 48 kHz Opus runs at first.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpusScan {
  /// Offset of the frame grid that counts the most frames in the four segments, in samples.
  ///
  /// It is where frames start at 48 kHz, modulo the 960 samples of a 20 ms frame, counted
  /// from the file's first sample. Where no offset counts a frame it is the uncut decode's.
  pub offset: usize,
  /// Number of frames counted at `offset`, over the four segments.
  pub frames: usize,
  /// Each segment's count at `offset`, in file order.
  pub segment_frames: [usize; SEGMENTS],
  /// Number of frames counted in the windows measured, on the grid of an uncut decode.
  ///
  /// The grid is the one a decode has when nothing was cut from its start: libopus's encoders
  /// all delay their input by the same 312 samples. Up to 60 windows are read.
  pub uncut_frames: usize,
  /// Frames counted with a change of level after the decode searched for.
  ///
  /// `None` in measures kept by a version of the analyzer that did not search for one.
  #[cfg_attr(feature = "serde", serde(default))]
  pub shifted: Option<OpusShifted>,
  /// Frames counted as Opus's hybrid mode codes them.
  ///
  /// `None` in measures kept by a version of the analyzer that did not count them.
  #[cfg_attr(feature = "serde", serde(default))]
  pub hybrid: Option<OpusHybrid>,
}

/// How many frames of a file carry the band energies of Opus's hybrid mode.
///
/// The hybrid mode codes audio under 8 kHz with a voice coder and the four bands above with
/// CELT. A frame is tested on the two bands from 9.6 to 15.6 kHz: the voice coder's output
/// reaches into the band under them, and the band over them comes back inexact from a file
/// at 44.1 kHz. It counts when both bands' steps from their predictions sit on the lattice,
/// in every channel where both stand over the samples' rounding noise.
///
/// Two bands land on the lattice by chance in a few frames of a hundred, so a count is read
/// as a share of the frames tested. A decode coded by CELT alone shows some such frames too.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpusHybrid {
  /// Offset of the grid that counts the most such frames in the segments, in samples.
  ///
  /// It is where frames start at 48 kHz, modulo the 960 samples of a 20 ms frame, counted
  /// from the file's first sample. Where no offset counts a frame it is the uncut decode's.
  pub offset: usize,
  /// Number of frames counted at `offset`, over the four segments.
  pub frames: usize,
  /// Number of frames at `offset` whose two bands could be tested.
  pub tested: usize,
  /// Number of different pairs of steps the frames counted at `offset` sit on.
  ///
  /// The frames of a steady tone land on the same pair over and over, a decode's on many.
  pub steps: usize,
  /// Number of frames counted in the windows measured, on the grid of an uncut decode.
  pub uncut_frames: usize,
  /// Number of frames in the windows measured whose two bands could be tested.
  pub uncut_tested: usize,
  /// Number of different pairs of steps the frames counted in the windows sit on.
  pub uncut_steps: usize,
}

impl OpusHybrid {
  /// Returns the offset, the frames counted and the frames tested of the larger count.
  ///
  /// The segments are counted at [`OpusHybrid::offset`], the windows at [`OPUS_UNCUT_OFFSET`].
  pub fn most(&self) -> (usize, usize, usize) {
    if self.uncut_frames > self.frames {
      (OPUS_UNCUT_OFFSET, self.uncut_frames, self.uncut_tested)
    } else {
      (self.offset, self.frames, self.tested)
    }
  }
}

/// How many frames carry Opus's band energies at another level than the decoder's.
///
/// A gain on every sample of a channel moves each band's step off the lattice by the same
/// amount, half the gain, so the lattice is still there, moved. The scan takes, for each
/// channel, the move that the most frames name, and counts the frames on the lattice once it
/// is taken off.
///
/// The counts cannot tell what the gain was. Moves 0.8 of 6 dB apart put all but a frame's
/// lowest bands on one lattice, and so do moves a lattice step apart in most frames.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpusShifted {
  /// Offset of the grid that counts the most such frames in the segments, in samples.
  ///
  /// It is where frames start at 48 kHz, modulo the 960 samples of a 20 ms frame, counted
  /// from the file's first sample. Where no offset counts a frame it is the uncut decode's.
  pub offset: usize,
  /// Number of frames counted at `offset`, over the four segments.
  pub frames: usize,
  /// Number of frames counted in the windows measured, on the grid of an uncut decode.
  pub uncut_frames: usize,
}

impl OpusScan {
  /// Returns the offset and the number of frames of the larger of the two counts.
  ///
  /// The segments are counted at [`OpusScan::offset`], the windows at [`OPUS_UNCUT_OFFSET`].
  pub fn most(&self) -> (usize, usize) {
    if self.uncut_frames > self.frames {
      (OPUS_UNCUT_OFFSET, self.uncut_frames)
    } else {
      (self.offset, self.frames)
    }
  }
}

impl OpusShifted {
  /// Returns the offset and the number of frames of the larger of the two counts.
  ///
  /// The segments are counted at [`OpusShifted::offset`], the windows at
  /// [`OPUS_UNCUT_OFFSET`].
  pub fn most(&self) -> (usize, usize) {
    if self.uncut_frames > self.frames {
      (OPUS_UNCUT_OFFSET, self.uncut_frames)
    } else {
      (self.offset, self.frames)
    }
  }
}

/// Scan of a file for the frame grids of MP3, AAC and Opus.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Scan {
  /// AAC's grid, with the Kaiser-Bessel-derived window.
  pub aac: CodecScan,
  /// AAC's grid, with the sine window.
  ///
  /// `None` in measures kept by a version of the analyzer that did not scan with it.
  #[cfg_attr(feature = "serde", serde(default))]
  pub aac_sine: Option<CodecScan>,
  /// MP3's grid.
  pub mp3: CodecScan,
  /// MP3's quantizer steps on that grid.
  ///
  /// `None` at a sample rate other than 32, 44.1 and 48 kHz, and in measures kept by a version
  /// of the analyzer that did not look for them.
  #[cfg_attr(feature = "serde", serde(default))]
  pub mp3_steps: Option<Mp3Steps>,
  /// Opus's frames, `None` at a sample rate other than 44.1 and 48 kHz.
  #[cfg_attr(feature = "serde", serde(default))]
  pub opus: Option<OpusScan>,
  /// MP3's and AAC's grids in the samples brought to the other of 44.1 and 48 kHz.
  ///
  /// `None` at any other sample rate, and in measures kept by a version of the analyzer that
  /// did not scan at the other rate.
  #[cfg_attr(feature = "serde", serde(default))]
  pub other_rate: Option<OtherRate>,
}

/// How many scalefactor bands of a file hold values on MP3's quantizer.
///
/// An MP3 encoder keeps each value of a scalefactor band as a whole number to the power 4/3
/// times the band's step, and a step is a whole power of the fourth root of two. With the
/// granules at the MP3 scan's [`CodecScan::offset`], a long block's kept values come back on
/// that lattice. The count takes each granule as left and right or as mid and side, whichever
/// puts more bands on whole steps.
///
/// A band is tested when three of its values at least stand over the rounding noise of 16-bit
/// samples and its smallest stands far enough over it for a lattice to show. A step finer
/// than that noise leaves nothing to find, as the highest bitrates' steps are.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Mp3Steps {
  /// Number of bands tested, over the four segments.
  pub tested: usize,
  /// Number of bands whose values all sit on the lattice of one step.
  pub stepped: usize,
  /// Number of those whose step is a whole power of the fourth root of two.
  ///
  /// A gain after the decode moves every step by the same amount off the whole powers.
  pub whole: usize,
  /// Each segment's share of `whole`, in file order.
  pub segment_whole: [usize; SEGMENTS],
}

/// Scan for MP3 and AAC of a file's samples brought to the other of 44.1 and 48 kHz.
///
/// Offsets are where frames start at `rate`, counted from the file's first sample. A stretch
/// brought from 48 to 44.1 kHz holds fewer samples, so it counts fewer frames than one at the
/// file's own rate.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OtherRate {
  /// Rate in Hz the samples were brought to.
  pub rate: u32,
  /// AAC's grid, with the Kaiser-Bessel-derived window.
  pub aac: CodecScan,
  /// AAC's grid, with the sine window.
  pub aac_sine: CodecScan,
  /// MP3's grid.
  pub mp3: CodecScan,
}

/// What shows a codec's frame grid.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Evidence {
  /// Offset whose transform values count far fewer than the others, for MP3 and AAC.
  Count(CodecScan),
  /// Such an offset in the samples brought to another rate, for a decode converted afterwards.
  CountAt {
    /// Rate in Hz the samples were brought to.
    rate: u32,
    /// Offset and how far its count stands out, at that rate.
    #[cfg_attr(feature = "serde", serde(flatten))]
    scan: CodecScan,
  },
  /// Scalefactor bands whose values sit on the quantizer's steps, for MP3.
  Steps {
    /// Offset of the granules the bands were read at, in samples.
    offset: usize,
    /// Counts of the bands.
    steps: Mp3Steps,
  },
  /// Frames whose band energies sit on the quantizer's lattice, for Opus.
  Lattice(OpusScan),
  /// Frames whose band energies sit on that lattice once a change of level is taken off.
  Shifted {
    /// Counts of the frames.
    shifted: OpusShifted,
  },
  /// Frames whose top bands sit on that lattice as Opus's hybrid mode codes them.
  Hybrid {
    /// Counts of the frames.
    hybrid: OpusHybrid,
  },
}

impl Evidence {
  /// Returns the offset of the frame grid, in samples.
  ///
  /// For Opus it is the offset of the larger count, as [`OpusScan::most`] gives it.
  pub fn offset(&self) -> usize {
    match self {
      Self::Count(scan) | Self::CountAt { scan, .. } => scan.offset,
      Self::Steps { offset, .. } => *offset,
      Self::Lattice(scan) => scan.most().0,
      Self::Shifted { shifted } => shifted.most().0,
      Self::Hybrid { hybrid } => hybrid.most().0,
    }
  }
}

/// Encoder family whose delay and window a grid is consistent with.
///
/// An encoder delays its input by a fixed number of samples, which puts the grid of its decode
/// at a fixed offset, and an AAC encoder keeps to one of the two windows for its long blocks.
/// The guess holds for a decode whose start was not cut, made by a decoder that cuts the delay
/// the file states, as ffmpeg does. A cut start or another decoder moves the offset, and
/// encoders that share a delay and a window read alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Family {
  /// LAME, whose tag states its delay of 576 samples.
  Lame,
  /// Helix, with a delay of 1680 samples.
  Helix,
  /// MP3 encoder that states no delay for a decoder to cut: Shine, BladeEnc, GOGO.
  ///
  /// A LAME file reads the same from a decoder that cuts no delay.
  Untagged,
  /// Apple's AAC encoder: the sine window and a delay of 2112 samples.
  Apple,
  /// FAAC: the sine window, its delay cut whole.
  Faac,
  /// Nero's AAC encoder: the Kaiser-Bessel-derived window and a delay of 2624 samples.
  ///
  /// A Vorbis decode reads as AAC, and at this offset about one time in thirteen.
  Nero,
  /// ffmpeg's AAC encoder or fdk-aac: the Kaiser-Bessel-derived window, their delay cut whole.
  FfmpegOrFdk,
  /// libopus, every version of which delays by 312 samples.
  Libopus,
}

impl Family {
  /// the family of an MP3 grid at `offset`
  fn of_mp3(offset: usize) -> Option<Self> {
    match offset {
      48 => Some(Self::Lame),
      96 => Some(Self::Helix),
      1 => Some(Self::Untagged),
      _ => None,
    }
  }

  /// the family of an AAC grid at `offset`, found under the sine window when `sine`
  fn of_aac(offset: usize, sine: bool) -> Option<Self> {
    match (sine, offset) {
      (true, 960) => Some(Self::Apple),
      (true, 0) => Some(Self::Faac),
      (false, 448) => Some(Self::Nero),
      (false, 0) => Some(Self::FfmpegOrFdk),
      _ => None,
    }
  }
}

impl fmt::Display for Family {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Lame => "LAME",
      Self::Helix => "Helix",
      Self::Untagged => "an MP3 encoder that states no delay",
      Self::Apple => "Apple's AAC encoder",
      Self::Faac => "FAAC",
      Self::Nero => "Nero's AAC encoder",
      Self::FfmpegOrFdk => "ffmpeg's AAC encoder or fdk-aac",
      Self::Libopus => "libopus",
    })
  }
}

/// Frame grid a reading finds.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Found {
  /// Codec whose grid it is.
  pub codec: Codec,
  /// Encoder family the grid's offset is consistent with, where it is one's.
  pub family: Option<Family>,
  /// What shows the grid.
  #[cfg_attr(feature = "serde", serde(flatten))]
  pub evidence: Evidence,
}

/// the grid `scan` shows under `thresholds`, with the encoder family its offset fits
pub(crate) fn found(scan: &Scan, thresholds: &ReadingThresholds) -> Option<Found> {
  let (codec, evidence) = shown(scan, thresholds)?;
  // which of AAC's windows a count was taken under
  let sine = |count: &CodecScan| {
    scan.aac_sine.as_ref() == Some(count)
      || scan
        .other_rate
        .as_ref()
        .is_some_and(|other| &other.aac_sine == count)
  };
  let family = match (&evidence, codec) {
    (Evidence::Count(count) | Evidence::CountAt { scan: count, .. }, Codec::Aac) => {
      Family::of_aac(count.offset, sine(count))
    }
    (_, Codec::Aac) => None,
    (_, Codec::Mp3) => Family::of_mp3(evidence.offset()),
    (_, Codec::Opus) => (evidence.offset() == OPUS_UNCUT_OFFSET).then_some(Family::Libopus),
  };
  Some(Found {
    codec,
    family,
    evidence,
  })
}

/// the codec whose grid `scan` shows under `thresholds`, and what shows it. in turn:
///
/// - MP3 or AAC over its z threshold with as many segments agreeing as its own threshold asks,
///   the one furthest over its z threshold when several are. AAC is read under each window
/// - MP3 by its quantizer's steps in enough segments
/// - Opus with enough frames on its lattice in the segments or in the windows: at the
///   decoder's level, at another, or as the hybrid mode codes them
/// - MP3 or AAC at the other rate
///
/// the codec decoded last leaves the grid that counts, and Opus's energies can show through
/// an MP3 made from its decode
fn shown(scan: &Scan, thresholds: &ReadingThresholds) -> Option<(Codec, Evidence)> {
  let aac = |scan| {
    (
      Codec::Aac,
      scan,
      thresholds.aac_grid_z,
      thresholds.aac_grid_segments_agreeing,
    )
  };
  let mp3 = |scan| {
    (
      Codec::Mp3,
      scan,
      thresholds.mp3_grid_z,
      thresholds.mp3_grid_segments_agreeing,
    )
  };
  let clearest = |scans: [Option<(Codec, &CodecScan, f64, usize)>; 3]| {
    scans
      .into_iter()
      .flatten()
      .filter(|(_, scan, z, agreeing)| scan.z > *z && scan.segments_agreeing >= *agreeing)
      .max_by(|(_, a, za, _), (_, b, zb, _)| (a.z / za).total_cmp(&(b.z / zb)))
      .map(|(codec, scan, _, _)| (codec, scan.clone()))
  };
  let own = clearest([
    Some(aac(&scan.aac)),
    scan.aac_sine.as_ref().map(aac),
    Some(mp3(&scan.mp3)),
  ])
  .map(|(codec, scan)| (codec, Evidence::Count(scan)));
  // the quantizer's steps show a grid whose count stands out too little
  let stepped = || {
    let steps = scan.mp3_steps.as_ref()?;
    let segments = steps.segment_whole.iter();
    let showing = segments
      .filter(|whole| **whole >= thresholds.mp3_whole_steps)
      .count();
    (showing >= thresholds.mp3_grid_segments_agreeing.max(1)).then(|| {
      let evidence = Evidence::Steps {
        offset: scan.mp3.offset,
        steps: steps.clone(),
      };
      (Codec::Mp3, evidence)
    })
  };
  // a grid at the other rate was laid before the conversion, so one on the file's own
  // samples was laid after it and is named first
  let converted = || {
    let other = scan.other_rate.as_ref()?;
    let (codec, scan) = clearest([
      Some(aac(&other.aac)),
      Some(aac(&other.aac_sine)),
      Some(mp3(&other.mp3)),
    ])?;
    let evidence = Evidence::CountAt {
      rate: other.rate,
      scan,
    };
    Some((codec, evidence))
  };
  let opus = || {
    let opus = scan.opus.as_ref()?;
    let evidence = if opus.frames >= thresholds.opus_grid_frames
      || opus.uncut_frames >= thresholds.opus_uncut_frames
    {
      Evidence::Lattice(opus.clone())
    } else if let Some(shifted) = opus.shifted.as_ref().filter(|shifted| {
      shifted.frames >= thresholds.opus_shifted_frames
        || shifted.uncut_frames >= thresholds.opus_shifted_uncut_frames
    }) {
      // the lattice at the decoder's own level names the decode more closely, so it comes first
      Evidence::Shifted {
        shifted: shifted.clone(),
      }
    } else {
      let hybrid = opus.hybrid.as_ref()?;
      let enough = |frames: usize, tested: usize, steps: usize| {
        steps >= thresholds.opus_hybrid_steps
          && frames as f64 >= thresholds.opus_hybrid_share * tested as f64
      };
      if !enough(hybrid.frames, hybrid.tested, hybrid.steps)
        && !enough(hybrid.uncut_frames, hybrid.uncut_tested, hybrid.uncut_steps)
      {
        return None;
      }
      Evidence::Hybrid {
        hybrid: hybrid.clone(),
      }
    };
    Some((Codec::Opus, evidence))
  };
  own.or_else(stepped).or_else(opus).or_else(converted)
}

/// one stretch of the file the grid is scanned in, filled as its samples are decoded
struct Segment {
  /// the sample frame it starts at
  start: u64,
  left: Vec<f64>,
  right: Vec<f64>,
}

impl Segment {
  /// the sample frame the segment needs next
  fn next(&self) -> u64 {
    self.start + self.left.len() as u64
  }

  fn full(&self) -> bool {
    self.left.len() == SEGMENT
  }

  /// the segment at the other of 44.1 and 48 kHz, cut to the length of one at the file's rate.
  /// its start is a frame of the file at that rate. `None` when nothing is left of it
  fn brought(&self, resampler: &Resampler) -> Option<Self> {
    let (channels, start) = resampler.brought(&[&self.left, &self.right], self.start)?;
    let [mut left, mut right] = <[Vec<f64>; 2]>::try_from(channels).ok()?;
    left.truncate(SEGMENT);
    right.truncate(SEGMENT);
    Some(Self { start, left, right })
  }

  /// how many of `available` frames from `first` on the segment takes, and from which
  fn wanted(&self, first: u64, available: usize) -> Option<(usize, usize)> {
    let next = self.next();
    let from = usize::try_from(next.checked_sub(first)?).ok()?;
    let count = (SEGMENT - self.left.len()).min(available.checked_sub(from)?);
    (count > 0).then_some((from, count))
  }
}

/// the segments of a file the grid is scanned in
pub(crate) struct Segments(Vec<Segment>);

impl Segments {
  /// the segments of a file of `frames` sample frames; `None` when it is shorter than one
  pub(crate) fn of(frames: u64) -> Option<Self> {
    let length = SEGMENT as u64;
    if frames < length {
      return None;
    }
    let segments = SEGMENT_AT
      .iter()
      .map(|at| Segment {
        // a length below 2^53 is exact as an f64; the start is the fraction floored
        start: ((frames as f64 * at) as u64).min(frames - length),
        left: Vec::with_capacity(SEGMENT),
        right: Vec::with_capacity(SEGMENT),
      })
      .collect();
    Some(Self(segments))
  }

  /// takes what the segments need of `window`, one vector per analyzed channel, whose first
  /// sample is frame `first`. a mono window counts as both channels.
  pub(crate) fn take(&mut self, first: u64, window: &[Vec<f32>]) {
    let [left, right] = match window {
      [mono] => [mono, mono],
      [left, right, ..] => [left, right],
      [] => return,
    };
    for segment in &mut self.0 {
      if let Some((from, count)) = segment.wanted(first, left.len()) {
        let range = from..from + count;
        segment
          .left
          .extend(left[range.clone()].iter().map(|s| f64::from(*s)));
        segment
          .right
          .extend(right[range].iter().map(|s| f64::from(*s)));
      }
    }
  }

  /// takes what the segments need of `samples`, interleaved over `channels` channels, each at
  /// the top of its 32 bits, whose first sample is frame `first`
  pub(crate) fn take_interleaved(&mut self, first: u64, samples: &[i32], channels: usize) {
    let frames = samples.len() / channels;
    let scale = 1.0 / 2_147_483_648.0;
    for segment in &mut self.0 {
      if let Some((from, count)) = segment.wanted(first, frames) {
        for frame in samples.chunks_exact(channels).skip(from).take(count) {
          let right = frame.get(1).unwrap_or(&frame[0]);
          segment.left.push(f64::from(frame[0]) * scale);
          segment.right.push(f64::from(*right) * scale);
        }
      }
    }
  }

  /// the indices of the segments not yet full
  pub(crate) fn missing(&self) -> Vec<usize> {
    (0..self.0.len()).filter(|i| !self.0[*i].full()).collect()
  }

  /// the sample frame segment `index` needs next; `None` once it is full
  pub(crate) fn needs(&self, index: usize) -> Option<u64> {
    let segment = &self.0[index];
    (!segment.full()).then(|| segment.next())
  }

  /// whether segment `index` has no samples yet
  pub(crate) fn untouched(&self, index: usize) -> bool {
    self.0[index].left.is_empty()
  }

  /// whether every segment is full
  pub(crate) fn complete(&self) -> bool {
    self.0.iter().all(Segment::full)
  }
}

/// what the scan for Opus's frames needs beside the segments
pub(crate) struct Opus {
  /// the file's sample rate
  pub(crate) rate: lattice::Rate,
  /// the bits of its samples, whose rounding noise a band must stand over
  pub(crate) bits: u32,
  /// what the frames in the windows measured count, on the grid of an uncut decode
  pub(crate) uncut: lattice::Counted,
  /// whether MP3 and AAC are scanned for at the other rate too
  pub(crate) other_rate: bool,
}

/// the jobs of one scan: MP3 and AAC under each of its windows in each segment. at a rate
/// Opus is looked for at, `opus`, as many for Opus as there are segments, and with
/// `other_rate` MP3 and AAC again in each segment at the other rate
pub(crate) fn jobs(opus: bool, other_rate: bool) -> usize {
  (3 + usize::from(opus) + 3 * usize::from(opus && other_rate)) * SEGMENTS
}
/// the tasks a job's offsets are split into at most, so that one file's scan can fill the pool
const TASKS: usize = 32;

/// the scan of full `segments`, each codec of each segment a job on rayon's thread pool that
/// splits its offsets into tasks on the same pool; Opus is among the codecs with `opus`. `go_on`
/// is asked before each job, `None` once it says not to, and `ended` is called as each job ends.
/// panics when a segment is not full.
pub(crate) fn scan(
  segments: &Segments,
  rate: u32,
  opus: Option<Opus>,
  go_on: impl Fn() -> bool + Sync,
  ended: impl Fn() + Sync,
) -> Option<Scan> {
  assert!(segments.complete(), "every segment is full before a scan");
  let stretches = opus.as_ref().map(|opus| {
    let stretches: Vec<_> = segments
      .0
      .iter()
      .map(|segment| ([&segment.left[..], &segment.right[..]], segment.start))
      .collect();
    lattice::Stretches::of(&stretches, opus.rate, opus.bits)
  });
  // the segments at the other rate, each brought there as a task on the pool
  let other = opus.as_ref().filter(|opus| opus.other_rate).map(|opus| {
    let (rate, resampler) = match opus.rate {
      lattice::Rate::Native => (44_100, Resampler::to_44100()),
      lattice::Rate::From44100 => (48_000, Resampler::to_48000()),
    };
    let segments: Vec<Option<Segment>> = segments
      .0
      .par_iter()
      .map(|segment| segment.brought(resampler))
      .collect();
    (rate, segments)
  });
  // Opus's offsets are split into as many jobs as there are segments, each over every segment
  let part = lattice::FRAME / SEGMENTS;
  let counts = |segment, rate| {
    [
      Job::Aac(Window::Kbd, segment, rate),
      Job::Aac(Window::Sine, segment, rate),
      Job::Mp3(segment, rate),
    ]
  };
  let jobs: Vec<Job> = segments
    .0
    .iter()
    .flat_map(|segment| counts(segment, Rate::Own))
    .chain(
      other
        .iter()
        .flat_map(|(_, segments)| segments.iter().flatten())
        .flat_map(|segment| counts(segment, Rate::Other)),
    )
    .chain(
      stretches
        .iter()
        .flat_map(|stretches| (0..SEGMENTS).map(move |i| Job::Opus(stretches, i * part))),
    )
    .collect();
  let curves: Vec<Curve> = jobs
    .into_par_iter()
    .map(|job| {
      go_on().then(|| {
        let curve = match job {
          Job::Aac(window, segment, rate) => Curve::Aac(
            window,
            rate,
            in_file(&Aac::curve(segment, window), segment.start),
          ),
          Job::Mp3(segment, rate) => {
            Curve::Mp3(rate, in_file(&Mp3::new().curve(segment), segment.start))
          }
          // the counts come in the file's offsets
          Job::Opus(stretches, from) => Curve::Opus(stretches.counted(from..from + part)),
        };
        ended();
        curve
      })
    })
    .collect::<Option<_>>()?;
  // each codec's curves at the file's rate and at the other
  let (mut aac, mut sine, mut mp3) = <[[Vec<Vec<f64>>; 2]; 3]>::default().into();
  let mut frames = Vec::new();
  for curve in curves {
    match curve {
      Curve::Aac(Window::Kbd, rate, curve) => aac[rate as usize].push(curve),
      Curve::Aac(Window::Sine, rate, curve) => sine[rate as usize].push(curve),
      Curve::Mp3(rate, curve) => mp3[rate as usize].push(curve),
      // the jobs come back in the order of their offsets
      Curve::Opus(counted) => frames.extend(counted),
    }
  }
  let [aac, other_aac] = aac;
  let [sine, other_sine] = sine;
  let [mp3, other_mp3] = mp3;
  // a segment nothing is left of at the other rate leaves that rate unscanned
  let other_rate = other
    .filter(|_| other_mp3.len() == SEGMENTS)
    .map(|(rate, _)| OtherRate {
      rate,
      aac: codec_scan(&other_aac),
      aac_sine: codec_scan(&other_sine),
      mp3: codec_scan(&other_mp3),
    });
  let mp3 = codec_scan(&mp3);
  let mp3_steps = mp3_bands(rate).map(|bands| {
    let analysis = Mp3::new();
    let mut steps = Mp3Steps {
      tested: 0,
      stepped: 0,
      whole: 0,
      segment_whole: [0; SEGMENTS],
    };
    for (segment, whole) in segments.0.iter().zip(0..) {
      // below the hop, so it fits
      let start = (segment.start % MP3_HOP as u64) as usize;
      let offset = (mp3.offset + MP3_HOP - start) % MP3_HOP;
      let before = steps.whole;
      analysis.steps(segment, offset, bands, &mut steps);
      steps.segment_whole[whole] = steps.whole - before;
    }
    steps
  });
  Some(Scan {
    other_rate,
    aac: codec_scan(&aac),
    aac_sine: Some(codec_scan(&sine)),
    mp3,
    mp3_steps,
    opus: opus.map(|opus| opus_scan(&frames, &opus.uncut)),
  })
}

/// the rate a segment is scanned at: the file's own, or the other of 44.1 and 48 kHz
#[derive(Clone, Copy)]
enum Rate {
  Own,
  Other,
}

/// what one job of a scan looks for: MP3's or AAC's grid in one segment at one rate, or Opus's
/// frames in every segment at the offsets from the one given
#[derive(Clone, Copy)]
enum Job<'a> {
  Aac(Window, &'a Segment, Rate),
  Mp3(&'a Segment, Rate),
  Opus(&'a lattice::Stretches, usize),
}

/// what a job found: MP3's and AAC's counts of transform values on a log scale at each offset
/// of the file's grid at the job's rate, Opus's counts of frames at each of the job's offsets
enum Curve {
  Aac(Window, Rate, Vec<f64>),
  Mp3(Rate, Vec<f64>),
  Opus(Vec<lattice::Counted>),
}

/// the first of `counts` with the most, and its index; the uncut decode's offset when none
/// counts any
fn most(counts: impl Iterator<Item = usize>) -> (usize, usize) {
  counts
    .enumerate()
    .fold((OPUS_UNCUT_OFFSET, 0), |(at, most), (o, count)| {
      if count > most { (o, count) } else { (at, most) }
    })
}

/// the scan for Opus from what the segments' frames count at each offset of the file's grid,
/// and what the windows' count on the grid of an uncut decode
fn opus_scan(counted: &[lattice::Counted], uncut: &lattice::Counted) -> OpusScan {
  let (offset, frames) = most(counted.iter().map(|c| c.frames.iter().sum()));
  let (shifted_offset, shifted_frames) = most(counted.iter().map(|c| c.shifted));
  let (hybrid_offset, hybrid_frames) = most(counted.iter().map(|c| c.hybrid));
  OpusScan {
    offset,
    frames,
    segment_frames: std::array::from_fn(|i| counted[offset].frames[i]),
    uncut_frames: uncut.frames.iter().sum(),
    shifted: Some(OpusShifted {
      offset: shifted_offset,
      frames: shifted_frames,
      uncut_frames: uncut.shifted,
    }),
    hybrid: Some(OpusHybrid {
      offset: hybrid_offset,
      frames: hybrid_frames,
      tested: counted[hybrid_offset].hybrid_tested,
      steps: counted[hybrid_offset].hybrid_points,
      uncut_frames: uncut.hybrid,
      uncut_tested: uncut.hybrid_tested,
      uncut_steps: uncut.hybrid_points,
    }),
  }
}

/// a segment's curve, whose entry `o` is for frames starting `o` samples into the segment
/// modulo the hop, turned into the file's offsets for a segment starting at frame `start`
fn in_file(curve: &[f64], start: u64) -> Vec<f64> {
  let hop = curve.len();
  // below the hop, so it fits
  let shift = (start % hop as u64) as usize;
  (0..hop).map(|o| curve[(o + hop - shift) % hop]).collect()
}

/// the scan of one codec from its segments' curves in the file's offsets
fn codec_scan(curves: &[Vec<f64>]) -> CodecScan {
  let hop = curves[0].len();
  let summed: Vec<f64> = (0..hop)
    .map(|o| curves.iter().map(|c| c[o]).sum())
    .collect();
  let (z, offset) = robust_z(&summed);
  CodecScan {
    offset,
    z,
    segments_agreeing: curves.iter().filter(|c| robust_z(c).1 == offset).count(),
    segment_z: std::array::from_fn(|i| robust_z(&curves[i]).0),
  }
}

/// how far the lowest entry of `curve` sits below the median in robust standard deviations,
/// and its index: the first lowest
fn robust_z(curve: &[f64]) -> (f64, usize) {
  let middle = median(curve.to_vec());
  let mad = MAD_TO_SD * median(curve.iter().map(|c| (c - middle).abs()).collect());
  let (lowest, at) = curve
    .iter()
    .enumerate()
    .fold((f64::INFINITY, 0), |(low, at), (i, c)| {
      if *c < low { (*c, i) } else { (low, at) }
    });
  let z = if mad > 0.0 {
    (middle - lowest) / mad
  } else {
    0.0
  };
  (z, at)
}

/// the median of `values`, the mean of the middle two for an even count
fn median(mut values: Vec<f64>) -> f64 {
  values.sort_by(f64::total_cmp);
  let half = values.len() / 2;
  if values.len().is_multiple_of(2) {
    (values[half - 1] + values[half]) / 2.0
  } else {
    values[half]
  }
}

/// the bits of an f32 exponent and the exponent that puts a value in [1, 2)
const EXPONENT_BITS: u32 = 0x7f80_0000;
const ONE: u32 = 0x3f80_0000;

/// the products [`active`] keeps side by side, so they can be multiplied in parallel
const LANES: usize = 16;

/// the active count of `values`: the sum of log10(max(v^2 / t2, 1)), counted in single
/// precision.
///
/// the log of a product is the sum of the logs, so the values' exponents are summed and their
/// mantissas, each in [1, 2), multiplied in [`LANES`] products of up to 32 each, which stay
/// under 2^32, with one log per product.
fn active(values: &[f32], t2: f32) -> f64 {
  let mut exponents = [0i32; LANES];
  let mut products = [1.0f32; LANES];
  let mut logs = 0.0;
  for (i, group) in values.chunks(LANES).enumerate() {
    for ((product, exponent), v) in products.iter_mut().zip(&mut exponents).zip(group) {
      // t2 is a normal number, so every square at or above it is one too
      let bits = (v * v).max(t2).to_bits();
      *exponent += ((bits & EXPONENT_BITS) >> 23) as i32 - 127;
      *product *= f32::from_bits((bits & !EXPONENT_BITS) | ONE);
    }
    if i % 32 == 31 {
      logs += products.iter().map(|p| f64::from(*p).log2()).sum::<f64>();
      products = [1.0; LANES];
    }
  }
  logs += products.iter().map(|p| f64::from(*p).log2()).sum::<f64>();
  let exponents: i32 = exponents.iter().sum();
  (f64::from(exponents) + logs - values.len() as f64 * f64::from(t2).log2())
    * std::f64::consts::LOG10_2
}

/// the window of an AAC long block
#[derive(Clone, Copy)]
pub(crate) enum Window {
  /// Kaiser-Bessel derived, with an alpha of 4
  Kbd,
  Sine,
}

impl Window {
  /// the window over the 2048 samples of a long block
  pub(crate) fn samples(self) -> Vec<f32> {
    let length = 2 * AAC_HOP;
    match self {
      Self::Kbd => kbd_window(length, 4.0)
        .into_iter()
        .map(|w| w as f32)
        .collect(),
      Self::Sine => (0..length)
        .map(|n| (std::f64::consts::PI * (n as f64 + 0.5) / length as f64).sin() as f32)
        .collect(),
    }
  }
}

/// the AAC analysis: a long block's window and an orthonormal MDCT, transformed in single
/// precision
struct Aac {
  window: Vec<f32>,
  dct: DctIv,
  frame: Vec<f32>,
  folded: Vec<f32>,
  /// the transform of left and right
  values: [Vec<f32>; 2],
  /// mid and side of the transform
  mid_side: [Vec<f32>; 2],
}

impl Aac {
  fn new(window: Window) -> Self {
    Self {
      window: window.samples(),
      dct: DctIv::new(AAC_HOP),
      frame: vec![0.0; 2 * AAC_HOP],
      folded: vec![0.0; AAC_HOP],
      values: std::array::from_fn(|_| vec![0.0; AAC_HOP]),
      mid_side: std::array::from_fn(|_| vec![0.0; AAC_HOP]),
    }
  }

  /// the active count summed over the long frames of `segment`, for each offset of the grid
  /// from its first sample. every offset counts the same frames. runs of offsets are tasks on
  /// rayon's thread pool, each with an analysis of its own.
  fn curve(segment: &Segment, window: Window) -> Vec<f64> {
    let left: Vec<f32> = segment.left.iter().map(|s| *s as f32).collect();
    let right: Vec<f32> = segment.right.iter().map(|s| *s as f32).collect();
    (0..AAC_HOP)
      .into_par_iter()
      .with_min_len(AAC_HOP / TASKS)
      .map_init(
        || Self::new(window),
        |aac, offset| aac.count(&left, &right, offset),
      )
      .collect()
  }

  /// the active count summed over the long frames of `left` and `right` on the grid at `offset`
  fn count(&mut self, left: &[f32], right: &[f32], offset: usize) -> f64 {
    // the frames every offset holds
    let frames = (left.len() + 1 - 2 * AAC_HOP) / AAC_HOP;
    let t2 = (NOISE16 as f32).powi(2);
    let half = std::f32::consts::FRAC_1_SQRT_2;
    (0..frames)
      .map(|j| {
        let start = offset + j * AAC_HOP;
        self.mdct(&left[start..start + 2 * AAC_HOP], 0);
        self.mdct(&right[start..start + 2 * AAC_HOP], 1);
        let [m, s] = &mut self.mid_side;
        let [left, right] = &self.values;
        for ((m, s), (l, r)) in m.iter_mut().zip(s.iter_mut()).zip(left.iter().zip(right)) {
          *m = (l + r) * half;
          *s = (l - r) * half;
        }
        let lr = active(left, t2) + active(right, t2);
        let ms = active(m, t2) + active(s, t2);
        lr.min(ms)
      })
      .sum()
  }

  /// the orthonormal MDCT of `input`, 2048 samples, windowed, into `values[into]`
  fn mdct(&mut self, input: &[f32], into: usize) {
    for ((f, x), w) in self.frame.iter_mut().zip(input).zip(&self.window) {
      *f = x * w;
    }
    fold(&self.frame, &mut self.folded);
    self.dct.process(&self.folded, &mut self.values[into]);
  }
}

/// the input of the DCT-IV whose output is the MDCT of `frame`, 2N windowed samples:
/// (-c_r - d, a - b_r) for its quarters a, b, c, d, so that the DCT-IV gives
/// X_k = sqrt(2/N) sum_n x_n cos(pi/N (n + 1/2 + N/2)(k + 1/2))
pub(crate) fn fold<T: Float>(frame: &[T], folded: &mut [T]) {
  fold_with(frame, folded, |c, d| -*c - *d, |a, b| *a - *b);
}

/// [`fold`] over any elements: `negated_sum(c, d)` is -c - d and `difference(a, b)` is a - b
fn fold_with<T, U>(
  frame: &[T],
  folded: &mut [U],
  negated_sum: impl Fn(&T, &T) -> U,
  difference: impl Fn(&T, &T) -> U,
) {
  let q = folded.len() / 2;
  let (ab, cd) = frame.split_at(2 * q);
  let ((a, b), (c, d)) = (ab.split_at(q), cd.split_at(q));
  let (low, high) = folded.split_at_mut(q);
  for ((folded, c), d) in low.iter_mut().zip(c.iter().rev()).zip(d) {
    *folded = negated_sum(c, d);
  }
  for ((folded, a), b) in high.iter_mut().zip(a).zip(b.iter().rev()) {
    *folded = difference(a, b);
  }
}

/// the Kaiser-Bessel-derived window of `length` samples with `alpha`: the square root of the
/// running sum of a Kaiser window of `length / 2 + 1` points with beta pi alpha over its total,
/// mirrored
fn kbd_window(length: usize, alpha: f64) -> Vec<f64> {
  let points = length / 2 + 1;
  let beta = std::f64::consts::PI * alpha;
  let kaiser: Vec<f64> = (0..points)
    .map(|n| {
      let r = 2.0 * n as f64 / (points - 1) as f64 - 1.0;
      bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta)
    })
    .collect();
  let total: f64 = kaiser.iter().sum();
  let mut half = Vec::with_capacity(length / 2);
  let mut running = 0.0;
  for k in &kaiser[..points - 1] {
    running += k;
    half.push((running / total).sqrt());
  }
  let mirrored: Vec<f64> = half.iter().rev().copied().collect();
  half.extend(mirrored);
  half
}

/// the modified Bessel function of the first kind of order 0, from its power series
pub(crate) fn bessel_i0(x: f64) -> f64 {
  let quarter = x * x / 4.0;
  let mut term = 1.0;
  let mut sum = 1.0;
  for k in 1..100 {
    term *= quarter / (k * k) as f64;
    sum += term;
    if term < sum * 1e-17 {
      break;
    }
  }
  sum
}

/// the orthonormal DCT-IV of length N through a complex FFT of length N/2:
/// X_k = sqrt(2/N) sum_n u_n cos(pi/N (n + 1/2)(k + 1/2))
pub(crate) struct DctIv {
  fft: Arc<dyn Fft<f32>>,
  /// exp(-i pi n / N), for the input
  pre: Vec<Complex<f32>>,
  /// sqrt(2/N) exp(-i pi (k + 1/4) / N), for the output
  post: Vec<Complex<f32>>,
  buffer: Vec<Complex<f32>>,
  scratch: Vec<Complex<f32>>,
}

impl DctIv {
  pub(crate) fn new(n: usize) -> Self {
    let half = n / 2;
    let fft = FftPlanner::new().plan_fft_forward(half);
    let turn = |angle: f64, scale: f64| {
      Complex::new((scale * angle.cos()) as f32, (scale * angle.sin()) as f32)
    };
    let step = std::f64::consts::PI / n as f64;
    let scale = (2.0 / n as f64).sqrt();
    Self {
      pre: (0..half).map(|i| turn(-step * i as f64, 1.0)).collect(),
      post: (0..half)
        .map(|k| turn(-step * (k as f64 + 0.25), scale))
        .collect(),
      buffer: vec![Complex::default(); half],
      scratch: vec![Complex::default(); fft.get_inplace_scratch_len()],
      fft,
    }
  }

  /// writes the transform of `input` to `output`, both of length N.
  ///
  /// with z_n = u_2n + i u_(N-1-2n) and Y_k = sum_n z_n exp(-i pi (2n + 1/2)(2k + 1/2) / N),
  /// X_2k is the real part of Y_k and X_(N-1-2k) minus its imaginary part.
  pub(crate) fn process(&mut self, input: &[f32], output: &mut [f32]) {
    // pair i holds u_2i and u_(2i+1), so u_(N-1-2n) is the second of pair N/2-1-n
    let (pairs, _) = input.as_chunks::<2>();
    let turned = self.buffer.iter_mut().zip(&self.pre);
    for ((b, pre), (even, odd)) in turned.zip(pairs.iter().zip(pairs.iter().rev())) {
      *b = Complex::new(even[0], odd[1]) * pre;
    }
    self
      .fft
      .process_with_scratch(&mut self.buffer, &mut self.scratch);
    let (pairs, _) = output.as_chunks_mut::<2>();
    let turned = || self.buffer.iter().zip(&self.post).map(|(b, post)| b * post);
    for (pair, y) in pairs.iter_mut().zip(turned()) {
      pair[0] = y.re;
    }
    for (pair, y) in pairs.iter_mut().rev().zip(turned()) {
      pair[1] = -y.im;
    }
  }
}

/// half of the MPEG audio synthesis window D in units of 2^-16, as ffmpeg's encoder carries it;
/// the analysis window C is D / 32
const ENWINDOW: [i32; 257] = [
  0, -1, -1, -1, -1, -1, -1, -2, -2, -2, -2, -3, -3, -4, -4, -5, -5, -6, -7, -7, -8, -9, -10, -11,
  -13, -14, -16, -17, -19, -21, -24, -26, -29, -31, -35, -38, -41, -45, -49, -53, -58, -63, -68,
  -73, -79, -85, -91, -97, -104, -111, -117, -125, -132, -139, -147, -154, -161, -169, -176, -183,
  -190, -196, -202, -208, 213, 218, 222, 225, 227, 228, 228, 227, 224, 221, 215, 208, 200, 189,
  177, 163, 146, 127, 106, 83, 57, 29, -2, -36, -72, -111, -153, -197, -244, -294, -347, -401,
  -459, -519, -581, -645, -711, -779, -848, -919, -991, -1064, -1137, -1210, -1283, -1356, -1428,
  -1498, -1567, -1634, -1698, -1759, -1817, -1870, -1919, -1962, -2001, -2032, -2057, -2075, -2085,
  -2087, -2080, -2063, 2037, 2000, 1952, 1893, 1822, 1739, 1644, 1535, 1414, 1280, 1131, 970, 794,
  605, 402, 185, -45, -288, -545, -814, -1095, -1388, -1692, -2006, -2330, -2663, -3004, -3351,
  -3705, -4063, -4425, -4788, -5153, -5517, -5879, -6237, -6589, -6935, -7271, -7597, -7910, -8209,
  -8491, -8755, -8998, -9219, -9416, -9585, -9727, -9838, -9916, -9959, -9966, -9935, -9863, -9750,
  -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134, 6574, 5959, 5288, 4561, 3776, 2935, 2037,
  1082, 70, -998, -2122, -3300, -4533, -5818, -7154, -8540, -9975, -11455, -12980, -14548, -16155,
  -17799, -19478, -21189, -22929, -24694, -26482, -28289, -30112, -31947, -33791, -35640, -37489,
  -39336, -41176, -43006, -44821, -46617, -48390, -50137, -51853, -53534, -55178, -56778, -58333,
  -59838, -61289, -62684, -64019, -65290, -66494, -67629, -68692, -69679, -70590, -71420, -72169,
  -72835, -73415, -73908, -74313, -74630, -74856, -74992, 75038,
];

/// the alias-reduction coefficients of layer III
const ALIAS: [f64; 8] = [
  -0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037,
];

/// subband samples per granule and the lines each subband's MDCT gives
const SUBBAND_SAMPLES: usize = 36;
const LINES: usize = 18;
const SUBBANDS: usize = 32;
/// the subbands whose line sums [`Mp3::hybrid`] keeps side by side
const SIDE_BY_SIDE: usize = 16;

/// the MP3 analysis: the layer III hybrid filterbank, in double precision
struct Mp3 {
  /// the analysis window C over 512 taps in blocks of 64, each block's last tap first, which is
  /// the order the taps meet the input samples in
  window: [[f64; 64]; 8],
  /// cos((2i + 1)(k - 16) pi / 64) for k in 0..64, by subband i
  matrix: [[f64; SUBBANDS]; 64],
  /// the sine window over 36 subband samples, by subband: every odd sample of an odd subband is
  /// negated
  windows: [[f64; SUBBANDS]; SUBBAND_SAMPLES],
  /// cos(pi/18 (n + 1/2)(k + 1/2)), the DCT-IV that gives the 18-line MDCT
  /// sum_n w_n z_n cos(pi/72 (2n + 1 + 18)(2k + 1)) of the folded windowed samples, by line
  dct: Vec<[f64; LINES]>,
  /// cs and ca of each butterfly
  butterflies: [(f64, f64); 8],
}

impl Mp3 {
  fn new() -> Self {
    let mut d = [0.0; 512];
    for (i, v) in ENWINDOW.iter().enumerate() {
      let v = f64::from(*v);
      d[i] = v;
      if i > 0 {
        d[512 - i] = if i % 64 == 0 { v } else { -v };
      }
    }
    let pi = std::f64::consts::PI;
    Self {
      window: std::array::from_fn(|j| std::array::from_fn(|i| d[64 * j + 63 - i] / 65536.0 / 32.0)),
      matrix: std::array::from_fn(|k| {
        std::array::from_fn(|i| ((2 * i + 1) as f64 * (k as f64 - 16.0) * pi / 64.0).cos())
      }),
      windows: std::array::from_fn(|n| {
        std::array::from_fn(|sb| {
          let sign = if n % 2 == 1 && sb % 2 == 1 { -1.0 } else { 1.0 };
          sign * (pi / 36.0 * (n as f64 + 0.5)).sin()
        })
      }),
      dct: (0..LINES)
        .map(|k| std::array::from_fn(|n| (pi / 18.0 * (n as f64 + 0.5) * (k as f64 + 0.5)).cos()))
        .collect(),
      butterflies: ALIAS.map(|c| {
        let norm = (1.0 + c * c).sqrt();
        (1.0 / norm, c / norm)
      }),
    }
  }

  /// the subband samples of `x`, 32 for every 32 input samples: sample t reads the 512 inputs
  /// from 32 t on, the newest first.
  ///
  /// every sum here and in [`Mp3::hybrid`] adds its terms in the order of the formula, in one
  /// running total per output, so the inner loops run over the outputs side by side.
  fn polyphase(&self, x: &[f64]) -> Vec<[f64; SUBBANDS]> {
    let count = (x.len() - 512) / 32 + 1;
    (0..count)
      .map(|t| {
        // with the newest input at 32 t + 511, y[63 - k] is the sum over j of
        // x[newest - k - 64 j] window[k + 64 j]
        let (blocks, _) = x[32 * t..32 * t + 512].as_chunks::<64>();
        let mut y = [0.0; 64];
        for (taps, x) in self.window.iter().zip(blocks.iter().rev()) {
          for ((y, x), tap) in y.iter_mut().zip(x).zip(taps) {
            *y += x * tap;
          }
        }
        let mut subbands = [0.0; SUBBANDS];
        for (row, y) in self.matrix.iter().zip(y.iter().rev()) {
          for (subband, m) in subbands.iter_mut().zip(row) {
            *subband += m * y;
          }
        }
        subbands
      })
      .collect()
  }

  /// the 576 values of the granule whose 36 subband samples are `samples`, subband by subband
  fn hybrid(&self, samples: &[[f64; SUBBANDS]], out: &mut [f64; MP3_HOP]) {
    // each subband sample with its window, multiplied as the two are folded
    let windowed: [_; SUBBAND_SAMPLES] = std::array::from_fn(|n| (&samples[n], &self.windows[n]));
    let mut folded = [[0.0; SUBBANDS]; LINES];
    fold_with(
      &windowed,
      &mut folded,
      |c, d| std::array::from_fn(|sb| -(c.0[sb] * c.1[sb]) - d.0[sb] * d.1[sb]),
      |a, b| std::array::from_fn(|sb| a.0[sb] * a.1[sb] - b.0[sb] * b.1[sb]),
    );
    for (k, dct) in self.dct.iter().enumerate() {
      for from in (0..SUBBANDS).step_by(SIDE_BY_SIDE) {
        let mut line = [0.0; SIDE_BY_SIDE];
        for (d, folded) in dct.iter().zip(&folded) {
          for (l, f) in line.iter_mut().zip(&folded[from..from + SIDE_BY_SIDE]) {
            *l += d * f;
          }
        }
        for (i, l) in line.iter().enumerate() {
          out[(from + i) * LINES + k] = *l;
        }
      }
    }
    for sb in 0..SUBBANDS - 1 {
      for (i, (cs, ca)) in self.butterflies.iter().enumerate() {
        let lo = out[sb * LINES + LINES - 1 - i];
        let hi = out[(sb + 1) * LINES + i];
        out[sb * LINES + LINES - 1 - i] = lo * cs + hi * ca;
        out[(sb + 1) * LINES + i] = hi * cs - lo * ca;
      }
    }
  }

  /// the active count summed over the granules of `segment`, for each offset of the granule
  /// grid from its first sample. every offset counts the same number of granules. the 32
  /// phases of the polyphase filterbank are tasks on rayon's thread pool.
  fn curve(&self, segment: &Segment) -> Vec<f64> {
    let phases: Vec<[f64; LINES]> = (0..32)
      .into_par_iter()
      .map(|phase| self.phase(segment, phase))
      .collect();
    (0..MP3_HOP)
      .map(|offset| phases[offset % 32][offset / 32])
      .collect()
  }

  /// the counts of the offsets `phase` + 32 g, for g in 0..18: the granules that start at
  /// subband sample g of the filterbank run from input sample `phase`
  fn phase(&self, segment: &Segment, phase: usize) -> [f64; LINES] {
    let granules = (segment.left.len() - MP3_HOP - 512 - SUBBAND_SAMPLES * 32) / MP3_HOP;
    let t2 = (NOISE16 * MP3_NOISE_GAIN).powi(2) as f32;
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let (mut l, mut r) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    // left, right, mid and side in single precision, for the counts
    let mut narrow = [[0.0f32; MP3_HOP]; 4];
    let left = self.polyphase(&segment.left[phase..]);
    let right = self.polyphase(&segment.right[phase..]);
    let mut counts = [0.0; LINES];
    for (g, count) in counts.iter_mut().enumerate() {
      *count = (0..granules)
        .map(|granule| {
          let at = g + LINES * granule;
          self.hybrid(&left[at..at + SUBBAND_SAMPLES], &mut l);
          self.hybrid(&right[at..at + SUBBAND_SAMPLES], &mut r);
          for k in 0..MP3_HOP {
            narrow[0][k] = l[k] as f32;
            narrow[1][k] = r[k] as f32;
            narrow[2][k] = ((l[k] + r[k]) * half) as f32;
            narrow[3][k] = ((l[k] - r[k]) * half) as f32;
          }
          let count = narrow.each_ref().map(|values| active(values, t2));
          (count[0] + count[1]).min(count[2] + count[3])
        })
        .sum();
    }
    counts
  }
}

/// the value of a line quantized to 1 at a step of one, with samples from -1 to 1
const MP3_UNIT: f64 = 9.0;
/// a value stands over the rounding noise from this many of its standard deviations, and sits
/// on a lattice point within this many
const OVER_NOISE: f64 = 6.0;
const ON_POINT: f64 = 4.0;
/// the fewest values over the noise a tested band has
const MIN_VALUES: usize = 3;
/// how far over the noise a step stands for its lattice to show, in standard deviations
const MIN_STEP: f64 = 24.0;
/// the whole numbers a band's smallest value is tried as
const SMALLEST: std::ops::RangeInclusive<u32> = 1..=8;
/// how near a whole power of the fourth root of two a step is one, in fourths of an octave
const WHOLE_STEP: f64 = 0.05;

/// the edges of a long block's scalefactor bands in lines at `rate`, for the rates MPEG-1
/// layer III codes
fn mp3_bands(rate: u32) -> Option<&'static [usize; 23]> {
  match rate {
    44_100 => Some(&[
      0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 52, 62, 74, 90, 110, 134, 162, 196, 238, 288, 342, 418,
      576,
    ]),
    48_000 => Some(&[
      0, 4, 8, 12, 16, 20, 24, 30, 36, 42, 50, 60, 72, 88, 106, 128, 156, 190, 230, 276, 330, 384,
      576,
    ]),
    32_000 => Some(&[
      0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 54, 66, 82, 102, 126, 156, 194, 240, 296, 364, 448, 550,
      576,
    ]),
    _ => None,
  }
}

/// the step of a band whose lines are `lines`: `None` when the band is not tested, `Some(None)`
/// when no step puts every value over the noise on its lattice. the smallest such value is
/// taken as 1, 2 and so on times the step in turn
fn band_step(lines: &[f64]) -> Option<Option<f64>> {
  let sigma = NOISE16 * MP3_NOISE_GAIN;
  let values: Vec<f64> = lines
    .iter()
    .map(|line| line.abs())
    .filter(|value| *value > OVER_NOISE * sigma)
    .collect();
  if values.len() < MIN_VALUES {
    return None;
  }
  let smallest = values.iter().copied().fold(f64::INFINITY, f64::min);
  let mut tested = false;
  for whole in SMALLEST {
    let step = smallest / f64::from(whole).powf(4.0 / 3.0);
    if step < MIN_STEP * sigma {
      break;
    }
    tested = true;
    // each value's whole number to the power 4/3, and the step that fits them all best
    let points: Vec<f64> = values
      .iter()
      .map(|value| (value / step).powf(0.75).round().max(1.0).powf(4.0 / 3.0))
      .collect();
    let squares: f64 = points.iter().map(|point| point * point).sum();
    let step = values.iter().zip(&points).map(|(v, p)| v * p).sum::<f64>() / squares;
    // a value's own noise, and what the noise of the others leaves of the step's
    let near = |point: f64| ON_POINT * sigma * (1.0 + point / squares.sqrt());
    if values
      .iter()
      .zip(&points)
      .all(|(value, point)| (value - point * step).abs() < near(*point))
    {
      return Some(Some(step));
    }
  }
  tested.then_some(None)
}

impl Mp3 {
  /// adds to `steps` the bands of `segment`'s granules on the grid at `offset` from its first
  /// sample, each granule as left and right or as mid and side, whichever has more bands on
  /// whole steps
  fn steps(&self, segment: &Segment, offset: usize, bands: &[usize; 23], steps: &mut Mp3Steps) {
    let granules = (segment.left.len() - MP3_HOP - 512 - SUBBAND_SAMPLES * 32) / MP3_HOP;
    let (phase, from) = (offset % 32, offset / 32);
    let left = self.polyphase(&segment.left[phase..]);
    let right = self.polyphase(&segment.right[phase..]);
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let (mut l, mut r) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    let (mut m, mut s) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    for granule in 0..granules {
      let at = from + LINES * granule;
      self.hybrid(&left[at..at + SUBBAND_SAMPLES], &mut l);
      self.hybrid(&right[at..at + SUBBAND_SAMPLES], &mut r);
      for k in 0..MP3_HOP {
        m[k] = (l[k] + r[k]) * half;
        s[k] = (l[k] - r[k]) * half;
      }
      let count = |pair: [&[f64; MP3_HOP]; 2]| {
        let (mut tested, mut stepped, mut whole) = (0, 0, 0);
        for lines in pair {
          for band in bands.windows(2) {
            let Some(step) = band_step(&lines[band[0]..band[1]]) else {
              continue;
            };
            tested += 1;
            if let Some(step) = step {
              stepped += 1;
              let fourths = 4.0 * (step / MP3_UNIT).log2();
              whole += usize::from((fourths - fourths.round()).abs() < WHOLE_STEP);
            }
          }
        }
        (tested, stepped, whole)
      };
      let (apart, joint) = (count([&l, &r]), count([&m, &s]));
      let (tested, stepped, whole) = if joint.2 > apart.2 { joint } else { apart };
      steps.tested += tested;
      steps.stepped += stepped;
      steps.whole += whole;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_opus_count_is_named_with_the_offset_it_was_counted_at() {
    // one chance frame in the segments, a decode's frames in the windows
    let scan = OpusScan {
      offset: 517,
      frames: 1,
      segment_frames: [0, 1, 0, 0],
      uncut_frames: 40,
      shifted: None,
      hybrid: None,
    };
    assert_eq!(scan.most(), (OPUS_UNCUT_OFFSET, 40));
    assert_eq!(Evidence::Lattice(scan.clone()).offset(), OPUS_UNCUT_OFFSET);
    // a decode whose start was cut shows in the segments alone
    let cut = OpusScan {
      frames: 150,
      uncut_frames: 0,
      ..scan
    };
    assert_eq!(cut.most(), (517, 150));
  }

  #[test]
  fn the_mdct_is_the_direct_sum() {
    let n = 16;
    let frame: Vec<f32> = (0..2 * n)
      .map(|i| ((i * i) as f32 * 0.37).sin() * 0.8)
      .collect();
    let mut folded = vec![0.0; n];
    let mut out = vec![0.0; n];
    fold(&frame, &mut folded);
    DctIv::new(n).process(&folded, &mut out);
    let pi = std::f64::consts::PI;
    for (k, got) in out.iter().enumerate() {
      let direct: f64 = frame
        .iter()
        .enumerate()
        .map(|(i, x)| {
          f64::from(*x)
            * (pi / n as f64 * (i as f64 + 0.5 + n as f64 / 2.0) * (k as f64 + 0.5)).cos()
        })
        .sum::<f64>()
        * (2.0 / n as f64).sqrt();
      assert!(
        (f64::from(*got) - direct).abs() < 1e-5,
        "{k}: {got} {direct}"
      );
    }
  }

  #[test]
  fn the_kbd_window_is_ffmpegs_long_window() {
    let window = kbd_window(2048, 4.0);
    // values of the reference implementation's window
    for (i, value) in [
      (0, 0.000_292_561_534_837_649_7),
      (100, 0.024_457_477_148_620_538),
      (511, 0.706_119_339_105_633_4),
      (1023, 0.999_999_957_203_872_2),
    ] {
      assert!((window[i] - value).abs() < 1e-12, "{i}: {}", window[i]);
    }
    for i in 0..1024 {
      let power = window[i].powi(2) + window[i + 1024].powi(2);
      assert!((power - 1.0).abs() < 1e-12, "{i}: {power}");
    }
  }

  #[test]
  fn the_hybrid_filterbank_matches_the_reference() {
    let x: Vec<f64> = (0..1800)
      .map(|n| {
        let n = f64::from(n);
        0.5 * (0.37 * n).sin() + 0.25 * (0.0011 * n * n).sin()
      })
      .collect();
    let mp3 = Mp3::new();
    let sub = mp3.polyphase(&x);
    assert_eq!(sub.len(), 41);
    let mut out = [0.0; MP3_HOP];
    mp3.hybrid(&sub[5..5 + SUBBAND_SAMPLES], &mut out);
    // values of the reference implementation on the same input, granule from subband sample 5
    for (i, value) in [
      (0, -1.091_283_183_323_872_5e-6),
      (7, -9.081_711_237_383_263e-7),
      (17, 6.967_136_897_000_289e-7),
      (18, 1.931_395_673_253_308_3e-6),
      (100, -6.038_671_146_916_679e-5),
      (287, 0.176_172_793_341_240_09),
      (288, 0.134_650_302_532_005_74),
      (431, -0.184_243_819_723_131_5),
      (575, -0.035_325_179_781_597_5),
    ] {
      assert!((out[i] - value).abs() < 1e-12, "{i}: {}", out[i]);
    }
    let squares: f64 = out.iter().map(|v| v * v).sum();
    assert!((squares - 28.590_620_430_713_802).abs() < 1e-9, "{squares}");
  }

  #[test]
  fn z_is_the_lowest_entry_below_the_median_in_robust_deviations() {
    let curve = [10.0, 11.0, 9.0, 10.0, 12.0, 8.0, 2.0, 10.0, 11.0, 9.0];
    // median 10, absolute deviations with median 1
    let (z, at) = robust_z(&curve);
    assert_eq!(at, 6);
    assert!((z - 8.0 / MAD_TO_SD).abs() < 1e-12, "{z}");
    assert_eq!(robust_z(&[3.0; 8]), (0.0, 0), "a flat curve has no z");
  }

  #[test]
  fn each_codec_asks_for_its_own_agreement() {
    let thresholds = ReadingThresholds {
      aac_grid_z: 9.0,
      aac_grid_segments_agreeing: 0,
      mp3_grid_z: 12.0,
      mp3_grid_segments_agreeing: 3,
      ..ReadingThresholds::default()
    };
    let codec_scan = |z: f64, segments_agreeing: usize| CodecScan {
      offset: 0,
      z,
      segments_agreeing,
      segment_z: [0.0; SEGMENTS],
    };
    let scan = |aac: (f64, usize), mp3: (f64, usize)| Scan {
      aac: codec_scan(aac.0, aac.1),
      aac_sine: None,
      mp3: codec_scan(mp3.0, mp3.1),
      opus: None,
      other_rate: None,
      mp3_steps: None,
    };
    let codec = |scan: &Scan| found(scan, &thresholds).map(|f| f.codec);
    // an MP3 dip in one segment is no grid, an AAC dip the segments place differently is one
    assert_eq!(codec(&scan((2.0, 0), (50.0, 1))), None);
    assert_eq!(codec(&scan((19.0, 1), (2.0, 0))), Some(Codec::Aac));
    assert_eq!(codec(&scan((9.0, 4), (12.0, 4))), None);
    // both over: 30 / 9 against 36 / 12
    assert_eq!(codec(&scan((30.0, 4), (36.0, 3))), Some(Codec::Aac));
    // an encoder's delay and window name its family: the tag LAME writes leaves its grid at 48
    let lame = found(&scan((2.0, 0), (50.0, 3)), &thresholds).expect("the grid is over");
    assert_eq!(lame.family, None, "offset 0 is no MP3 encoder's");
    assert_eq!(Family::of_mp3(48), Some(Family::Lame));
    assert_eq!(Family::of_mp3(1), Some(Family::Untagged));
    assert_eq!(Family::of_aac(960, true), Some(Family::Apple));
    assert_eq!(Family::of_aac(960, false), None);
    let ffmpeg = found(&scan((19.0, 1), (2.0, 0)), &thresholds).expect("the grid is over");
    assert_eq!(ffmpeg.family, Some(Family::FfmpegOrFdk));
    // a grid at the other rate is named where nothing sits on the file's own samples
    let converted = Scan {
      other_rate: Some(OtherRate {
        rate: 44_100,
        aac: codec_scan(2.0, 0),
        aac_sine: codec_scan(2.0, 0),
        mp3: codec_scan(50.0, 4),
      }),
      ..scan((2.0, 0), (2.0, 0))
    };
    let at_other = found(&converted, &thresholds).expect("the other rate's grid is over");
    assert!(matches!(
      at_other.evidence,
      Evidence::CountAt { rate: 44_100, .. }
    ));
    let opus = OpusScan {
      offset: OPUS_UNCUT_OFFSET,
      frames: 40,
      segment_frames: [10; SEGMENTS],
      uncut_frames: 400,
      shifted: None,
      hybrid: None,
    };
    let after = Scan {
      opus: Some(opus.clone()),
      ..converted
    };
    let last = found(&after, &thresholds).expect("the lattice is over");
    assert_eq!(last.evidence, Evidence::Lattice(opus));
    assert_eq!(last.family, Some(Family::Libopus));
    // a grid only the sine window shows is AAC's, with that window's evidence
    let sine = Scan {
      aac_sine: Some(codec_scan(40.0, 4)),
      ..scan((6.0, 1), (2.0, 0))
    };
    let shown = found(&sine, &thresholds).expect("the sine scan is over the threshold");
    assert_eq!(shown.codec, Codec::Aac);
    assert_eq!(shown.evidence, Evidence::Count(codec_scan(40.0, 4)));
    assert_eq!(shown.family, Some(Family::Faac));
  }

  #[test]
  fn segment_curves_are_turned_into_the_files_offsets_before_they_agree() {
    let hop = 8;
    let dip = |at: usize| -> Vec<f64> {
      (0..hop)
        .map(|o| if o == at { 1.0 } else { 10.0 + o as f64 * 0.1 })
        .collect()
    };
    // segments starting 3, 8 + 1, 16 + 7 and 5 samples into the file see a grid at file
    // offset 6 at segment offsets 3, 5, 7 and 1; the last dips elsewhere
    let curves = [
      in_file(&dip(3), 3),
      in_file(&dip(5), 9),
      in_file(&dip(7), 23),
      in_file(&dip(4), 5),
    ];
    let scan = codec_scan(&curves);
    assert_eq!(scan.offset, 6);
    assert_eq!(scan.segments_agreeing, 3);
  }
}
