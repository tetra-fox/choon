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
//! An AAC encoder keeps its values the same way. With the frames at each window's count
//! offset, the bands of a long block whose values sit on one step are counted under that
//! window ([`AacSteps`]). They show under the window the encoder used and not under the other,
//! and a Vorbis decode, whose grid the count finds too, does not show them.
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
//! Frames of 10, 5 and 2.5 ms are counted apart too, each length on a grid of its own
//! ([`OpusShort`]).
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
//! - Vorbis's grid shows through the AAC scan, so a Vorbis decode reads as AAC by the count.
//!   Its values do not sit on AAC's steps.
//! - Opus frames the pitch post-filter ran over and its SILK mode are not found, nor are
//!   frames of 2.5, 5 and 10 ms coded as several blocks or at another level than the decoder's.
//! - A gain after an Opus decode moves the lattice, and the scan searches for one move a
//!   channel ([`OpusShifted`]). A level that changes over time, as under a fade or a limiter,
//!   hides the lattice where it changes.

use std::fmt;

use rayon::prelude::*;

use crate::thresholds::{FraunhoferSteps, ReadingThresholds};

pub(crate) mod aac;
pub(crate) mod lattice;
pub(crate) mod mdct;
mod resample;
pub(crate) mod segments;
mod steps;

use aac::{Aac, AacStepper, Window, aac_bands};
use mdct::fold_with;
use resample::Resampler;
use segments::{Segment, Segments};
use steps::{BandScratch, WHOLE_STEP, band_step};

/// AAC's hop, the length of its long frames' new samples
pub(crate) const AAC_HOP: usize = 1024;
/// MP3's hop, a granule
const MP3_HOP: usize = 576;
/// the number of segments scanned
pub(crate) const SEGMENTS: usize = 4;
/// the samples of one segment: 40 AAC hops
pub(crate) const SEGMENT: usize = 40 * AAC_HOP;
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
  /// Opus, by the band energies of its CELT layer.
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
  /// Frames counted once a change of level after the decode is taken off.
  ///
  /// `None` in measures kept by a version of the analyzer that did not search for one.
  #[cfg_attr(feature = "serde", serde(default))]
  pub shifted: Option<OpusShifted>,
  /// Frames counted as Opus's hybrid mode codes them.
  ///
  /// `None` in measures kept by a version of the analyzer that did not count them.
  #[cfg_attr(feature = "serde", serde(default))]
  pub hybrid: Option<OpusHybrid>,
  /// Frames counted as frames of 10, 5 and 2.5 ms, in that order.
  ///
  /// Empty in measures kept by a version of the analyzer that did not count them.
  #[cfg_attr(feature = "serde", serde(default))]
  pub short: Vec<OpusShort>,
}

/// How many frames of a file carry Opus's band energies as frames shorter than 20 ms.
///
/// A frame of 10, 5 or 2.5 ms holds the same bands in fewer transform values and predicts
/// each band with other shares of the frame before and of its lower bands. A frame counts
/// when its bands' steps sit on the lattice after the frame before it. Where an offset counts
/// such a frame, the frames that sit on the lattice without the frame before are counted with
/// it.
///
/// A frame coded as several blocks, one the pitch post-filter ran over, and one at another
/// level than the decoder's are not counted.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpusShort {
  /// Length of a frame in samples at 48 kHz: 480, 240 or 120.
  pub length: usize,
  /// Offset of the grid that counts the most frames in the segments, in samples.
  ///
  /// It is where frames start at 48 kHz, modulo `length`, counted from the file's first
  /// sample. Where no offset counts a frame it is the uncut decode's.
  pub offset: usize,
  /// Number of frames counted at `offset`, over the four segments.
  pub frames: usize,
  /// Offset of an uncut decode's grid that counts the most frames in the windows, in samples.
  ///
  /// libopus delays its input by 312 samples, or by 120 in its low-delay mode, which ffmpeg
  /// sets for frames under 10 ms. The windows are counted on the grid each delay leaves.
  pub uncut_offset: usize,
  /// Number of frames counted in the windows measured, at `uncut_offset`.
  pub uncut_frames: usize,
}

impl OpusShort {
  /// Returns the offset and the number of frames of the larger of the two counts.
  ///
  /// The segments are counted at [`OpusShort::offset`], the windows at
  /// [`OpusShort::uncut_offset`].
  pub fn most(&self) -> (usize, usize) {
    if self.uncut_frames > self.frames {
      (self.uncut_offset, self.uncut_frames)
    } else {
      (self.offset, self.frames)
    }
  }

  /// Returns the length of a frame in milliseconds.
  pub fn milliseconds(&self) -> f64 {
    self.length as f64 / 48.0
  }
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
  /// AAC's quantizer steps on the grid of `aac`, with the Kaiser-Bessel-derived window.
  ///
  /// `None` at a sample rate other than 44.1 and 48 kHz, and in measures kept by a version of
  /// the analyzer that did not look for them.
  #[cfg_attr(feature = "serde", serde(default))]
  pub aac_steps: Option<AacSteps>,
  /// AAC's quantizer steps on the grid of `aac_sine`, with the sine window.
  ///
  /// `None` where `aac_steps` is.
  #[cfg_attr(feature = "serde", serde(default))]
  pub aac_sine_steps: Option<AacSteps>,
  /// Opus's frames.
  ///
  /// `None` at a sample rate other than 44.1 and 48 kHz.
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
  /// How the whole steps of neighbouring bands compare.
  ///
  /// `None` in measures kept by a version of the analyzer that did not compare them.
  #[cfg_attr(feature = "serde", serde(default))]
  pub neighbours: Option<Mp3Neighbours>,
}

/// How the whole steps of neighbouring scalefactor bands compare, over the four segments.
///
/// An encoder sets each band's step through its scalefactor, and encoders differ in how
/// often and which way they move it from one band to the next. A pair is two neighbouring
/// bands of one channel of one granule that both show a whole step; its region is where the
/// lower band sits: bands 0 to 6, 7 to 12, and 13 up. A step is counted in quarter powers of
/// two.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Mp3Neighbours {
  /// Number of pairs, by region.
  pub pairs: [usize; 3],
  /// Number of pairs whose two steps are the same, by region.
  pub same: [usize; 3],
  /// Number of pairs whose higher band has the finer step, by region.
  pub finer: [usize; 3],
  /// Number of pairs whose higher band's step is two quarter powers finer.
  pub two_finer: usize,
  /// Number of pairs whose higher band's step is two quarter powers coarser.
  pub two_coarser: usize,
  /// Number of bands 0 to 5 with a whole step.
  pub low_bands: usize,
  /// Sum over those bands of log2 of one more than the largest quantized value.
  pub low_log: f64,
  /// Number of bands 12 up with a whole step.
  pub high_bands: usize,
  /// The same sum over those bands.
  pub high_log: f64,
}

impl Mp3Neighbours {
  /// the region of the pair whose lower band is `band`
  fn region(band: usize) -> usize {
    match band {
      0..=6 => 0,
      7..=12 => 1,
      _ => 2,
    }
  }

  /// takes a band with a whole step of `fourths` quarter powers whose largest quantized value
  /// is `largest`, after the band under it with the step `under` when that has one
  fn take(&mut self, band: usize, fourths: i32, largest: f64, under: Option<i32>) {
    if let Some(under) = under {
      let region = Self::region(band - 1);
      let moved = fourths - under;
      self.pairs[region] += 1;
      self.same[region] += usize::from(moved == 0);
      self.finer[region] += usize::from(moved < 0);
      self.two_finer += usize::from(moved == -2);
      self.two_coarser += usize::from(moved == 2);
    }
    let log = (largest + 1.0).log2();
    if band < 6 {
      self.low_bands += 1;
      self.low_log += log;
    } else if band >= 12 {
      self.high_bands += 1;
      self.high_log += log;
    }
  }

  /// the sum of `rule`'s weights over the shares of these counts; `None` with fewer pairs
  /// than the rule asks for, or no band at one end
  fn weighed(&self, rule: &FraunhoferSteps) -> Option<f64> {
    let total: usize = self.pairs.iter().sum();
    let few = total < rule.min_pairs.max(1)
      || self
        .pairs
        .iter()
        .any(|pairs| *pairs < rule.min_region_pairs.max(1));
    if few || self.low_bands == 0 || self.high_bands == 0 {
      return None;
    }
    let of = |count: usize, all: usize| count as f64 / all as f64;
    let regions: f64 = (0..3)
      .map(|region| {
        rule.same[region] * of(self.same[region], self.pairs[region])
          + rule.finer[region] * of(self.finer[region], self.pairs[region])
      })
      .sum();
    Some(
      regions
        + rule.two_finer * of(self.two_finer, total)
        + rule.two_coarser * of(self.two_coarser, total)
        + rule.low_log * self.low_log / self.low_bands as f64
        + rule.high_log * self.high_log / self.high_bands as f64
        + rule.low_share * of(self.pairs[0], total)
        + rule.high_share * of(self.pairs[2], total),
    )
  }

  fn add(&mut self, other: &Self) {
    for region in 0..3 {
      self.pairs[region] += other.pairs[region];
      self.same[region] += other.same[region];
      self.finer[region] += other.finer[region];
    }
    self.two_finer += other.two_finer;
    self.two_coarser += other.two_coarser;
    self.low_bands += other.low_bands;
    self.low_log += other.low_log;
    self.high_bands += other.high_bands;
    self.high_log += other.high_log;
  }
}

/// How many scalefactor bands of a file hold values on AAC's quantizer.
///
/// An AAC encoder keeps each value of a scalefactor band as a whole number to the power 4/3
/// times the band's step, and a step is a whole power of the fourth root of two, as in MP3
/// ([`Mp3Steps`]). With the frames at an AAC scan's [`CodecScan::offset`] and under the window
/// the encoder used, a long block's kept values come back on that lattice. Each band is taken
/// as left and right or as mid and side, whichever puts more of the two on whole steps.
///
/// A band is tested as an MP3 band is. Under the other window the values come back off the
/// lattice. Bands coded with temporal noise shaping, noise substitution or intensity stereo
/// are off it too, and so is every band of a short block.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct AacSteps {
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
  /// Scalefactor bands whose values sit on the quantizer's steps, for AAC.
  AacSteps {
    /// Offset of the frames the bands were read at, in samples.
    offset: usize,
    /// Counts of the bands, under the window that shows them.
    steps: AacSteps,
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
  /// Frames shorter than 20 ms whose band energies sit on that lattice.
  Short {
    /// Counts of the frames of one length.
    short: OpusShort,
  },
}

impl Evidence {
  /// Returns the offset of the frame grid, in samples.
  ///
  /// For Opus it is the offset of the larger count, as [`OpusScan::most`] gives it.
  pub fn offset(&self) -> usize {
    match self {
      Self::Count(scan) | Self::CountAt { scan, .. } => scan.offset,
      Self::Steps { offset, .. } | Self::AacSteps { offset, .. } => *offset,
      Self::Lattice(scan) => scan.most().0,
      Self::Shifted { shifted } => shifted.most().0,
      Self::Hybrid { hybrid } => hybrid.most().0,
      Self::Short { short } => short.most().0,
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
  /// MP3 encoder that states no delay for a decoder to cut: Shine, BladeEnc, GOGO, Apple's.
  ///
  /// A decode of Fraunhofer's encoder reads so too where its steps do not show it.
  ///
  /// A LAME file reads the same from a decoder that cuts no delay.
  Untagged,
  /// Apple's AAC encoder: the sine window and a delay of 2112 samples.
  Apple,
  /// FAAC: the sine window, its delay cut whole.
  Faac,
  /// Nero's AAC encoder: the Kaiser-Bessel-derived window and a delay of 2624 samples.
  ///
  /// A Vorbis decode reads as AAC, and at this offset about one time in thirteen. It shows no
  /// steps of AAC's quantizer ([`Found::quantizer`]).
  Nero,
  /// ffmpeg's AAC encoder or fdk-aac: the Kaiser-Bessel-derived window, their delay cut whole.
  FfmpegOrFdk,
  /// Fraunhofer's MP3 encoder: no delay stated, and its way of moving a band's step.
  ///
  /// It is told from the others that state no delay by
  /// [`ReadingThresholds::fraunhofer_steps`], fitted on the codec Windows ships.
  Fraunhofer,
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
      Self::Fraunhofer => "Fraunhofer's MP3 encoder",
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
  /// Bands on AAC's quantizer steps beside an AAC count at the file's own rate.
  ///
  /// `None` where they do not show under either window. A Vorbis decode, whose grid the AAC
  /// count finds too, does not show them, and neither does an HE-AAC decode.
  pub quantizer: Option<AacSteps>,
}

/// the grid `scan` shows under `thresholds`, with the encoder family its offset fits
pub(crate) fn found(scan: &Scan, thresholds: &ReadingThresholds) -> Option<Found> {
  let (codec, evidence, sine) = shown(scan, thresholds)?;
  // AAC's steps, which are read at the file's own rate
  let stepped = match (&evidence, codec) {
    (Evidence::CountAt { .. }, _) | (_, Codec::Mp3 | Codec::Opus) => None,
    (_, Codec::Aac) => aac_stepped(scan, thresholds),
  };
  let family = match (&evidence, codec) {
    // the steps show under the encoder's window alone, so they name it where they show
    (Evidence::Count(count) | Evidence::CountAt { scan: count, .. }, Codec::Aac) => {
      match &stepped {
        Some((window, _)) => Family::of_aac(window.offset, window.sine),
        None => Family::of_aac(count.offset, sine),
      }
    }
    (_, Codec::Aac) => stepped
      .as_ref()
      .and_then(|(window, _)| Family::of_aac(window.offset, window.sine)),
    // the steps were read on the grid of the file's own rate
    (Evidence::CountAt { .. }, Codec::Mp3) => Family::of_mp3(evidence.offset()),
    (_, Codec::Mp3) => {
      let weighed = scan
        .mp3_steps
        .as_ref()
        .and_then(|steps| steps.neighbours.as_ref())
        .and_then(|neighbours| neighbours.weighed(&thresholds.fraunhofer_steps));
      match Family::of_mp3(evidence.offset()) {
        Some(Family::Untagged)
          if weighed.is_some_and(|sum| sum > thresholds.fraunhofer_steps.over) =>
        {
          Some(Family::Fraunhofer)
        }
        family => family,
      }
    }
    (Evidence::Short { short }, Codec::Opus) => lattice::SHORT
      .iter()
      .filter(|length| length.length == short.length)
      .any(|length| length.uncut_offsets().contains(&short.most().0))
      .then_some(Family::Libopus),
    (_, Codec::Opus) => (evidence.offset() == OPUS_UNCUT_OFFSET).then_some(Family::Libopus),
  };
  let quantizer = stepped
    .filter(|_| matches!(evidence, Evidence::Count(_)))
    .map(|(_, steps)| steps.clone());
  Some(Found {
    codec,
    family,
    evidence,
    quantizer,
  })
}

/// a count that may show a grid: its codec, whether it was taken under AAC's sine window, and
/// the z and the agreement it has to pass
struct Counted<'a> {
  codec: Codec,
  sine: bool,
  scan: &'a CodecScan,
  z: f64,
  agreeing: usize,
}

/// the AAC window steps were read under: the offset of its count, and whether it is the sine one
struct StepsWindow {
  offset: usize,
  sine: bool,
}

/// the window under which AAC's steps show in enough segments of `scan`, with its counts: the
/// one with more whole steps when both do
fn aac_stepped<'a>(
  scan: &'a Scan,
  thresholds: &ReadingThresholds,
) -> Option<(StepsWindow, &'a AacSteps)> {
  let window = |count: &CodecScan, sine| StepsWindow {
    offset: count.offset,
    sine,
  };
  [
    scan
      .aac_steps
      .as_ref()
      .map(|steps| (window(&scan.aac, false), steps)),
    scan
      .aac_sine_steps
      .as_ref()
      .zip(scan.aac_sine.as_ref())
      .map(|(steps, count)| (window(count, true), steps)),
  ]
  .into_iter()
  .flatten()
  .filter(|(_, steps)| {
    let segments = steps.segment_whole.iter();
    let showing = segments
      .filter(|whole| **whole >= thresholds.aac_whole_steps)
      .count();
    showing >= thresholds.aac_grid_segments_agreeing.max(1)
  })
  .max_by_key(|(_, steps)| steps.whole)
}

/// the codec whose grid `scan` shows under `thresholds`, and what shows it. in turn:
///
/// - MP3 or AAC over its z threshold with as many segments agreeing as its own threshold asks,
///   the one furthest over its z threshold when several are. AAC is read under each window
/// - MP3 by its quantizer's steps in enough segments, then AAC by its own
/// - Opus with enough frames on its lattice in the segments or in the windows: at the
///   decoder's level, at another, as frames under 20 ms, or as the hybrid mode codes them
/// - MP3 or AAC at the other rate
///
/// the codec decoded last leaves the grid that counts, and Opus's energies can show through
/// an MP3 made from its decode.
///
/// the last of the three is whether the evidence is a count taken under AAC's sine window
fn shown(scan: &Scan, thresholds: &ReadingThresholds) -> Option<(Codec, Evidence, bool)> {
  let aac = |scan, sine| Counted {
    codec: Codec::Aac,
    sine,
    scan,
    z: thresholds.aac_grid_z,
    agreeing: thresholds.aac_grid_segments_agreeing,
  };
  let mp3 = |scan| Counted {
    codec: Codec::Mp3,
    sine: false,
    scan,
    z: thresholds.mp3_grid_z,
    agreeing: thresholds.mp3_grid_segments_agreeing,
  };
  fn clearest<'a>(counts: [Option<Counted<'a>>; 3]) -> Option<Counted<'a>> {
    counts
      .into_iter()
      .flatten()
      .filter(|count| count.scan.z > count.z && count.scan.segments_agreeing >= count.agreeing)
      .max_by(|a, b| (a.scan.z / a.z).total_cmp(&(b.scan.z / b.z)))
  }
  let own = clearest([
    Some(aac(&scan.aac, false)),
    scan.aac_sine.as_ref().map(|scan| aac(scan, true)),
    Some(mp3(&scan.mp3)),
  ])
  .map(|count| (count.codec, Evidence::Count(count.scan.clone()), count.sine));
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
      (Codec::Mp3, evidence, false)
    })
  };
  let stepped_aac = || {
    let (window, steps) = aac_stepped(scan, thresholds)?;
    let evidence = Evidence::AacSteps {
      offset: window.offset,
      steps: steps.clone(),
    };
    Some((Codec::Aac, evidence, false))
  };
  // a grid at the other rate was laid before the conversion, so one on the file's own
  // samples was laid after it and is named first
  let converted = || {
    let other = scan.other_rate.as_ref()?;
    let aac = |scan, sine| Counted {
      z: thresholds.other_rate_aac_z,
      ..aac(scan, sine)
    };
    let count = clearest([
      Some(aac(&other.aac, false)),
      Some(aac(&other.aac_sine, true)),
      Some(mp3(&other.mp3)),
    ])?;
    let evidence = Evidence::CountAt {
      rate: other.rate,
      scan: count.scan.clone(),
    };
    Some((count.codec, evidence, count.sine))
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
    } else if let Some(short) = opus
      .short
      .iter()
      .filter(|short| {
        short.frames >= thresholds.opus_short_frames
          || short.uncut_frames >= thresholds.opus_short_uncut_frames
      })
      .max_by_key(|short| short.most().1)
    {
      Evidence::Short {
        short: short.clone(),
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
    Some((Codec::Opus, evidence, false))
  };
  own
    .or_else(stepped)
    .or_else(stepped_aac)
    .or_else(opus)
    .or_else(converted)
}

/// what the scan for Opus's frames needs beside the segments
pub(crate) struct Opus {
  /// the file's sample rate
  pub(crate) rate: lattice::Rate,
  /// the bits of its samples, whose rounding noise a band must stand over
  pub(crate) bits: u32,
  /// what the frames in the windows measured count, on the grid of an uncut decode
  pub(crate) uncut: lattice::Counted,
  /// the same for the short frames of each length, on the grid each delay of the encoder leaves
  pub(crate) uncut_short: [[[usize; 2]; 2]; 3],
}

/// the number of jobs in one scan: three in each segment, for MP3 and for AAC under each of its
/// windows. with `opus`, set at a rate Opus is looked for at, one more per segment and one per
/// length of short frame, and with `other_rate` too the three again in each segment at the
/// other rate
pub(crate) fn jobs(opus: bool, other_rate: bool) -> usize {
  (3 + usize::from(opus) + 3 * usize::from(opus && other_rate)) * SEGMENTS
    + lattice::SHORT.len() * usize::from(opus)
}

/// the scan of full `segments`. each codec of each segment is a job on rayon's thread pool that
/// splits its offsets into tasks on the same pool. Opus is among the codecs with `opus`, and
/// with `other_rate` a file at 44.1 or 48 kHz is scanned for MP3 and AAC at the other of the
/// two too. `go_on` is asked before each job and the scan is `None` once it says not to;
/// `ended` is called as each job ends. panics when a segment is not full.
pub(crate) fn scan(
  segments: &Segments,
  rate: u32,
  opus: Option<Opus>,
  other_rate: bool,
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
  let to_other = match rate {
    48_000 => Some((44_100, Resampler::to_44100())),
    44_100 => Some((48_000, Resampler::to_48000())),
    _ => None,
  };
  let other = to_other.filter(|_| other_rate).map(|(rate, resampler)| {
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
    .chain(
      stretches
        .iter()
        .flat_map(|stretches| lattice::SHORT.map(|short| Job::OpusShort(stretches, short))),
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
          Job::OpusShort(stretches, short) => Curve::OpusShort(stretches.short_counted(short)),
        };
        ended();
        curve
      })
    })
    .collect::<Option<_>>()?;
  // each codec's curves at the file's rate and at the other
  let (mut aac, mut sine, mut mp3) = <[[Vec<Vec<f64>>; 2]; 3]>::default().into();
  let mut frames = Vec::new();
  let mut short = Vec::new();
  for curve in curves {
    match curve {
      Curve::Aac(Window::Kbd, rate, curve) => aac[rate as usize].push(curve),
      Curve::Aac(Window::Sine, rate, curve) => sine[rate as usize].push(curve),
      Curve::Mp3(rate, curve) => mp3[rate as usize].push(curve),
      // the jobs come back in the order of their offsets
      Curve::Opus(counted) => frames.extend(counted),
      // and these in the order of the lengths
      Curve::OpusShort(counted) => short.push(counted),
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
  let (aac, sine, mp3) = (codec_scan(&aac), codec_scan(&sine), codec_scan(&mp3));
  // the quantizers' steps on the grids the counts found: each segment a task on the pool,
  // which reads MP3's granules and AAC's frames under each window side by side. they take a
  // small part of a count's time and are not among the jobs
  let (mp3_bands, aac_bands) = (mp3_bands(rate), aac_bands(rate));
  let stepped: Vec<SegmentSteps> = segments
    .0
    .par_iter()
    .map(|segment| {
      go_on().then(|| {
        let mp3_steps = || {
          mp3_bands.map(|bands| {
            // below the hop, so it fits
            let start = (segment.start % MP3_HOP as u64) as usize;
            let offset = (mp3.offset + MP3_HOP - start) % MP3_HOP;
            Mp3::new().steps(segment, offset, bands)
          })
        };
        let aac_steps = |window, count: &CodecScan| {
          aac_bands.map(|bands| AacStepper::new(window).steps(segment, count.offset, bands))
        };
        let (mp3_steps, (kbd, sine)) = rayon::join(mp3_steps, || {
          rayon::join(
            || aac_steps(Window::Kbd, &aac),
            || aac_steps(Window::Sine, &sine),
          )
        });
        SegmentSteps {
          mp3: mp3_steps,
          aac: kbd.zip(sine).map(<[_; 2]>::from),
        }
      })
    })
    .collect::<Option<_>>()?;
  // the granules are summed in file order, so the sums of logarithms come out the same
  // however the tasks ran
  let mp3_steps = mp3_bands.map(|_| {
    let mut steps = Mp3Steps {
      tested: 0,
      stepped: 0,
      whole: 0,
      segment_whole: [0; SEGMENTS],
      neighbours: None,
    };
    let mut neighbours = Mp3Neighbours::default();
    for (segment, in_segment) in stepped.iter().zip(&mut steps.segment_whole) {
      for granule in segment.mp3.iter().flatten() {
        steps.tested += granule.tested;
        steps.stepped += granule.stepped;
        steps.whole += granule.whole;
        *in_segment += granule.whole;
        neighbours.add(&granule.neighbours);
      }
    }
    steps.neighbours = Some(neighbours);
    steps
  });
  let aac_steps = |window: usize| {
    aac_bands.map(|_| {
      let mut steps = AacSteps {
        tested: 0,
        stepped: 0,
        whole: 0,
        segment_whole: [0; SEGMENTS],
      };
      for (segment, in_segment) in stepped.iter().zip(&mut steps.segment_whole) {
        for [tested, stepped, whole] in segment.aac.iter().map(|windows| windows[window]) {
          steps.tested += tested;
          steps.stepped += stepped;
          steps.whole += whole;
          *in_segment += whole;
        }
      }
      steps
    })
  };
  let (aac_steps, aac_sine_steps) = (aac_steps(0), aac_steps(1));
  Some(Scan {
    other_rate,
    aac,
    aac_sine: Some(sine),
    mp3,
    mp3_steps,
    aac_steps,
    aac_sine_steps,
    opus: opus.map(|opus| opus_scan(&frames, &opus.uncut, &short, opus.uncut_short)),
  })
}

/// the quantizers' steps in one segment: MP3's by granule, and AAC's bands tested, on one step
/// and on a whole one under the Kaiser-Bessel-derived window and under the sine one. each is
/// `None` at a rate its steps are not read at
struct SegmentSteps {
  mp3: Option<Vec<GranuleSteps>>,
  aac: Option<[[usize; 3]; 2]>,
}

/// the rate a segment is scanned at: the file's own, or the other of 44.1 and 48 kHz
#[derive(Clone, Copy)]
enum Rate {
  Own,
  Other,
}

/// what one job of a scan looks for: MP3's or AAC's grid in one segment at one rate, Opus's
/// frames in every segment at the offsets from the one given, or its short frames of one
/// length in every segment
#[derive(Clone, Copy)]
enum Job<'a> {
  Aac(Window, &'a Segment, Rate),
  Mp3(&'a Segment, Rate),
  Opus(&'a lattice::Stretches, usize),
  OpusShort(&'a lattice::Stretches, lattice::Short),
}

/// what a job found: MP3's and AAC's counts of transform values on a log scale at each offset
/// of the file's grid at the job's rate, Opus's counts of frames at each of the job's offsets
enum Curve {
  Aac(Window, Rate, Vec<f64>),
  Mp3(Rate, Vec<f64>),
  Opus(Vec<lattice::Counted>),
  /// at each offset, the short frames on the lattice after the frame before, and those with
  /// the ones on it without the frame before
  OpusShort(Vec<[usize; 2]>),
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
fn opus_scan(
  counted: &[lattice::Counted],
  uncut: &lattice::Counted,
  short: &[Vec<[usize; 2]>],
  uncut_short: [[[usize; 2]; 2]; 3],
) -> OpusScan {
  // the frames read without the frame before count where one sits on the lattice after it
  let with_alone = |[after, either]: [usize; 2]| if after > 0 { either } else { 0 };
  let short = lattice::SHORT
    .iter()
    .zip(short)
    .zip(uncut_short)
    .map(|((length, counted), uncut)| {
      let grids = length.uncut_offsets();
      // the first with the most, the first grid's where none counts any
      let most = |counts: &mut dyn Iterator<Item = (usize, usize)>| {
        counts.fold((grids[0], 0), |(at, most), (o, count)| {
          if count > most { (o, count) } else { (at, most) }
        })
      };
      let (offset, frames) = most(&mut counted.iter().copied().map(with_alone).enumerate());
      let (uncut_offset, uncut_frames) = most(&mut grids.into_iter().zip(uncut.map(with_alone)));
      OpusShort {
        length: length.length,
        offset,
        frames,
        uncut_offset,
        uncut_frames,
      }
    })
    .collect();
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
    short,
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

/// the bands of one granule: how many were tested, sit on one step, and on a whole one, and
/// how the whole steps of neighbouring bands compare
struct GranuleSteps {
  tested: usize,
  stepped: usize,
  whole: usize,
  neighbours: Mp3Neighbours,
}

impl Mp3 {
  /// the bands of each of `segment`'s granules on the grid at `offset` from its first sample,
  /// each granule as left and right or as mid and side, whichever has more bands on whole
  /// steps
  fn steps(&self, segment: &Segment, offset: usize, bands: &[usize; 23]) -> Vec<GranuleSteps> {
    let mut scratch = BandScratch::default();
    let granules = (segment.left.len() - MP3_HOP - 512 - SUBBAND_SAMPLES * 32) / MP3_HOP;
    let (phase, from) = (offset % 32, offset / 32);
    let left = self.polyphase(&segment.left[phase..]);
    let right = self.polyphase(&segment.right[phase..]);
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let (mut l, mut r) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    let (mut m, mut s) = ([0.0; MP3_HOP], [0.0; MP3_HOP]);
    let mut read = Vec::with_capacity(granules);
    for granule in 0..granules {
      let at = from + LINES * granule;
      self.hybrid(&left[at..at + SUBBAND_SAMPLES], &mut l);
      self.hybrid(&right[at..at + SUBBAND_SAMPLES], &mut r);
      for k in 0..MP3_HOP {
        m[k] = (l[k] + r[k]) * half;
        s[k] = (l[k] - r[k]) * half;
      }
      let mut count = |pair: [&[f64; MP3_HOP]; 2]| {
        let (mut tested, mut stepped, mut whole) = (0, 0, 0);
        let mut neighbours = Mp3Neighbours::default();
        for lines in pair {
          // the whole step of the band under the one looked at
          let mut under = None;
          for (at, band) in bands.windows(2).enumerate() {
            let lines = &lines[band[0]..band[1]];
            let step = band_step(lines, NOISE16 * MP3_NOISE_GAIN, &mut scratch);
            tested += usize::from(step.tested());
            let fourths = step.step().and_then(|step| {
              stepped += 1;
              let fourths = 4.0 * (step / MP3_UNIT).log2();
              ((fourths - fourths.round()).abs() < WHOLE_STEP).then(|| {
                let top = lines.iter().fold(0.0f64, |top, line| top.max(line.abs()));
                let largest = (top / step).powf(0.75).round();
                // a step within 2^-100 and 2^100, so its quarter powers fit
                let fourths = fourths.round() as i32;
                neighbours.take(at, fourths, largest, under);
                fourths
              })
            });
            whole += usize::from(fourths.is_some());
            under = fourths;
          }
        }
        GranuleSteps {
          tested,
          stepped,
          whole,
          neighbours,
        }
      };
      let (apart, joint) = (count([&l, &r]), count([&m, &s]));
      read.push(if joint.whole > apart.whole {
        joint
      } else {
        apart
      });
    }
    read
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
      short: Vec::new(),
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
  fn aacs_steps_name_the_window_and_show_a_grid_no_count_does() {
    let thresholds = ReadingThresholds::default();
    let count = |offset: usize, z: f64| CodecScan {
      offset,
      z,
      segments_agreeing: 4,
      segment_z: [z; SEGMENTS],
    };
    let steps = |whole: usize| AacSteps {
      tested: 12_000,
      stepped: 4 * whole,
      whole: 4 * whole,
      segment_whole: [whole; SEGMENTS],
    };
    let scan = |kbd: f64, sine: f64, kbd_steps: usize, sine_steps: usize| Scan {
      aac: count(448, kbd),
      aac_sine: Some(count(448, sine)),
      mp3: count(0, 2.0),
      opus: None,
      other_rate: None,
      mp3_steps: None,
      aac_steps: Some(steps(kbd_steps)),
      aac_sine_steps: Some(steps(sine_steps)),
    };
    let under = thresholds.aac_whole_steps - 1;
    // the count stands out further under the sine window, and the steps show under the other
    let nero = found(&scan(78.0, 93.0, 2_500, under), &thresholds).expect("the count is over");
    assert_eq!(nero.evidence, Evidence::Count(count(448, 93.0)));
    assert_eq!(nero.family, Some(Family::Nero));
    assert_eq!(nero.quantizer, Some(steps(2_500)));
    // without steps the count's own window names the family, and no encoder's is the sine at 448
    let unnamed = found(&scan(78.0, 93.0, under, under), &thresholds).expect("the count is over");
    assert_eq!((unnamed.family, unnamed.quantizer), (None, None));
    // no count over its threshold: the steps show the grid, and one stretch is enough
    let quiet = scan(4.0, 5.0, 2_500, under);
    let by_steps = found(&quiet, &thresholds).expect("the steps show");
    let evidence = Evidence::AacSteps {
      offset: 448,
      steps: steps(2_500),
    };
    assert_eq!((by_steps.codec, by_steps.evidence), (Codec::Aac, evidence));
    assert_eq!(
      (by_steps.family, by_steps.quantizer),
      (Some(Family::Nero), None)
    );
    let one = Scan {
      aac_steps: Some(AacSteps {
        segment_whole: [0, thresholds.aac_whole_steps, 0, 0],
        ..steps(0)
      }),
      ..quiet
    };
    assert_eq!(found(&one, &thresholds).map(|f| f.codec), Some(Codec::Aac));
    assert_eq!(found(&scan(4.0, 5.0, under, under), &thresholds), None);
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
      aac_steps: None,
      aac_sine_steps: None,
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
    // AAC at the other rate asks for more than at the file's own
    let faint = Scan {
      other_rate: Some(OtherRate {
        rate: 44_100,
        aac: codec_scan(12.0, 4),
        aac_sine: codec_scan(12.0, 4),
        mp3: codec_scan(2.0, 0),
      }),
      ..scan((2.0, 0), (2.0, 0))
    };
    assert_eq!(codec(&faint), None);
    let opus = OpusScan {
      offset: OPUS_UNCUT_OFFSET,
      frames: 40,
      segment_frames: [10; SEGMENTS],
      uncut_frames: 400,
      shifted: None,
      hybrid: None,
      short: Vec::new(),
    };
    let after = Scan {
      opus: Some(opus.clone()),
      ..converted
    };
    let last = found(&after, &thresholds).expect("the lattice is over");
    assert_eq!(last.evidence, Evidence::Lattice(opus));
    assert_eq!(last.family, Some(Family::Libopus));
    // an MP3 grid at offset 1 whose steps weigh over the threshold is Fraunhofer's; the sums
    // are the tuning's for two decodes of its codec
    let fraunhofer =
      |pairs, same, finer, twos: (usize, usize), low: (usize, f64), high: (usize, f64)| {
        Mp3Neighbours {
          pairs,
          same,
          finer,
          two_finer: twos.0,
          two_coarser: twos.1,
          low_bands: low.0,
          low_log: low.1,
          high_bands: high.0,
          high_log: high.1,
        }
      };
    let at_128 = fraunhofer(
      [725, 775, 1032],
      [216, 427, 631],
      [216, 231, 379],
      (488, 174),
      (838, 2179.864),
      (1492, 2424.526),
    );
    let at_320 = fraunhofer(
      [1118, 1058, 1078],
      [447, 623, 609],
      [313, 338, 377],
      (750, 246),
      (1108, 3340.91),
      (1510, 4099.994),
    );
    let rule = &thresholds.fraunhofer_steps;
    for (neighbours, sum) in [(&at_128, 28.7451), (&at_320, 28.6805)] {
      let weighed = neighbours.weighed(rule).expect("pairs enough");
      assert!((weighed - sum).abs() < 1e-3, "{weighed}");
    }
    // an encoder that never moves the step, and one with too few pairs to tell
    let flat = fraunhofer(
      [900; 3],
      [900; 3],
      [0; 3],
      (0, 0),
      (800, 2400.0),
      (1500, 2500.0),
    );
    assert!(flat.weighed(rule).expect("pairs enough") < rule.over);
    let few = fraunhofer([10; 3], [5; 3], [3; 3], (9, 2), (20, 60.0), (30, 50.0));
    assert_eq!(few.weighed(rule), None);
    let stepped = |neighbours: &Mp3Neighbours| Scan {
      mp3: codec_scan(50.0, 4),
      mp3_steps: Some(Mp3Steps {
        tested: 9000,
        stepped: 3400,
        whole: 3300,
        segment_whole: [800; SEGMENTS],
        neighbours: Some(neighbours.clone()),
      }),
      ..scan((2.0, 0), (2.0, 0))
    };
    let family = |mut scan: Scan, offset: usize| {
      scan.mp3.offset = offset;
      found(&scan, &thresholds).and_then(|found| found.family)
    };
    assert_eq!(family(stepped(&at_128), 1), Some(Family::Fraunhofer));
    assert_eq!(family(stepped(&flat), 1), Some(Family::Untagged));
    assert_eq!(family(stepped(&few), 1), Some(Family::Untagged));
    // LAME's offset names LAME whatever the steps weigh
    assert_eq!(family(stepped(&at_128), 48), Some(Family::Lame));
    // frames under 20 ms make a grid where no count of 20 ms frames does, the length with
    // the most frames
    let short = |length: usize, frames: usize, uncut_frames: usize| OpusShort {
      length,
      offset: 7,
      frames,
      uncut_offset: 60,
      uncut_frames,
    };
    let lengths = vec![short(480, 5, 0), short(240, 2, 90), short(120, 40, 0)];
    let shorter = Scan {
      opus: Some(OpusScan {
        offset: 0,
        frames: 1,
        segment_frames: [0; SEGMENTS],
        uncut_frames: 1,
        shifted: None,
        hybrid: None,
        short: lengths.clone(),
      }),
      ..scan((2.0, 0), (2.0, 0))
    };
    let by_short = found(&shorter, &thresholds).expect("the short frames are enough");
    let evidence = Evidence::Short {
      short: lengths[1].clone(),
    };
    assert_eq!(
      (by_short.codec, &by_short.evidence),
      (Codec::Opus, &evidence)
    );
    assert_eq!(by_short.evidence.offset(), 60);
    assert_eq!(by_short.family, Some(Family::Libopus));
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
  fn neighbouring_bands_are_compared_by_where_the_lower_one_sits() {
    let mut neighbours = Mp3Neighbours::default();
    // one channel: bands 5 to 8 with steps -40, -40, -42 and -40, then band 13 after a gap
    let mut under = None;
    for (band, fourths, largest) in [(5, -40, 7.0), (6, -40, 3.0), (7, -42, 1.0), (8, -40, 1.0)] {
      neighbours.take(band, fourths, largest, under);
      under = Some(fourths);
    }
    neighbours.take(13, -44, 3.0, None);
    assert_eq!(neighbours.pairs, [2, 1, 0]);
    assert_eq!(neighbours.same, [1, 0, 0]);
    assert_eq!(neighbours.finer, [1, 0, 0]);
    assert_eq!((neighbours.two_finer, neighbours.two_coarser), (1, 1));
    // band 5 alone is under band 6, band 13 alone from band 12 up
    assert_eq!((neighbours.low_bands, neighbours.low_log), (1, 3.0));
    assert_eq!((neighbours.high_bands, neighbours.high_log), (1, 2.0));
    let mut twice = neighbours.clone();
    twice.add(&neighbours);
    assert_eq!(twice.pairs, [4, 2, 0]);
    assert_eq!(twice.high_log, 4.0);
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
