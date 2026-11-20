//! Frame grids that MP3, AAC and Opus encoders leave in decoded audio.
//!
//! An encoder transforms its input in frames on a fixed grid and quantizes what the transform
//! gives. The decoder's synthesis inverts the encoder's analysis, so running that analysis again
//! on the decoded samples, with frames on the encoder's grid, gives the quantized values back.
//! With frames at any other offset they are mixed with the neighbouring frames'. Lossless audio
//! has no such offset.
//!
//! The scan reads four stretches of 40960 samples, at 20, 40, 60 and 80% of the file. Where over
//! a twentieth of a stretch's sample frames are at full scale, the loudest stretch of the file that is not
//! clipped is read in its place: clipping after a decode fills the bands the encoder emptied.
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

use crate::thresholds::FraunhoferSteps;

pub(crate) mod aac;
mod found;
pub(crate) mod lattice;
pub(crate) mod mdct;
mod mp3;
mod resample;
mod scan;
pub(crate) mod segments;
mod steps;

pub use found::{Evidence, Family, Found};
pub(crate) use found::{found, mp3_near, nearest_share};
pub(crate) use scan::{Opus, jobs, scan};

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

/// Codec whose frame grid the scan looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
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
}
