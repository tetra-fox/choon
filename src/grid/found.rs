//! the rule that reads a scan under a reading's thresholds: the codec whose grid it shows, what
//! shows it, and the encoder family the grid's offset fits

use std::fmt;

use super::lattice;
use super::{
  AacSteps, Codec, CodecScan, Mp3Steps, OPUS_UNCUT_OFFSET, OpusHybrid, OpusScan, OpusShifted,
  OpusShort, OpusUnfiltered, Scan,
};
use crate::thresholds::ReadingThresholds;

/// What shows a codec's frame grid.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Evidence {
  /// Offset whose transform values count far fewer than the others.
  ///
  /// It is the count of MP3, MP2, AAC, Vorbis, WMA or AC-3. Where enough segments' subband
  /// counts sit at the phase an uncut MP3, Vorbis or MP2 decode has, it is the subband count,
  /// however little it stands out ([`ReadingThresholds::block_phase_segments`]).
  Count(CodecScan),
  /// Such an offset in the samples brought to another rate, for a decode converted afterwards.
  CountAt {
    /// Rate in Hz the samples were brought to.
    rate: u32,
    /// Offset and how far its count stands out, at that rate.
    #[cfg_attr(feature = "serde", serde(flatten))]
    scan: CodecScan,
  },
  /// Such an offset in some of the segments alone, for a file decoded from the codec in part.
  Part {
    /// Number of segments that show the grid.
    ///
    /// A segment shows it by its own count at the offset or, for MP3, by enough scalefactor
    /// bands on whole steps of the quantizer.
    segments: usize,
    /// Offset and how far its count stands out.
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
  /// Frames whose band energies sit on that lattice once the pitch post-filter is taken off.
  Unfiltered {
    /// Counts of the frames.
    unfiltered: OpusUnfiltered,
  },
}

impl Evidence {
  /// Returns the offset of the frame grid, in samples.
  ///
  /// For Opus it is the offset of the larger count, as [`OpusScan::most`] gives it.
  pub fn offset(&self) -> usize {
    match self {
      Self::Count(scan) | Self::CountAt { scan, .. } | Self::Part { scan, .. } => scan.offset,
      Self::Steps { offset, .. } | Self::AacSteps { offset, .. } => *offset,
      Self::Lattice(scan) => scan.most().0,
      Self::Shifted { shifted } => shifted.most().0,
      Self::Hybrid { hybrid } => hybrid.most().0,
      Self::Short { short } => short.most().0,
      Self::Unfiltered { .. } => OPUS_UNCUT_OFFSET,
    }
  }
}

/// Encoder family a grid is consistent with, by its quantizer's steps or by its offset.
///
/// An encoder moves the quantizer's step from one scalefactor band to the next in a way of
/// its own, which the steps of its decode show wherever the grid sits
/// ([`ReadingThresholds::mp3_family_steps`], [`ReadingThresholds::aac_family_steps`]).
///
/// An encoder also delays its input by a fixed number of samples, which puts the grid of its
/// decode at a fixed offset, and an AAC encoder keeps to one of the two windows for its long
/// blocks. A family the steps do not name is named by these. That guess holds for a decode
/// whose start was not cut, made by a decoder that cuts the delay the file states, as ffmpeg
/// does. A cut start or another decoder moves the offset, and encoders that share a delay and
/// a window read alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum Family {
  /// LAME, whose tag states its delay of 576 samples.
  Lame,
  /// Helix, with a delay of 1680 samples.
  Helix,
  /// MP3 encoder that states no delay for a decoder to cut: BladeEnc, GOGO, Apple's.
  ///
  /// A decode of Fraunhofer's encoder or of Shine reads so too where its steps do not name
  /// it, and so does a LAME file from a decoder that cuts no delay.
  Untagged,
  /// Apple's AAC encoder: the sine window and a delay of 2112 samples.
  Apple,
  /// FAAC: the sine window, its delay cut whole.
  Faac,
  /// Nero's AAC encoder: the Kaiser-Bessel-derived window and a delay of 2624 samples.
  ///
  /// Named only where the steps of AAC's quantizer show ([`Found::quantizer`]). A Vorbis
  /// decode's AAC count sits at this offset about one time in thirteen, and shows none.
  Nero,
  /// ffmpeg's AAC encoder or fdk-aac: the Kaiser-Bessel-derived window, their delay cut whole.
  ///
  /// The two share window and delay, and read so where the steps name neither.
  FfmpegOrFdk,
  /// ffmpeg's AAC encoder, by its quantizer's steps.
  Ffmpeg,
  /// fdk-aac, by its quantizer's steps.
  FdkAac,
  /// Fraunhofer's MP3 encoder, by its quantizer's steps.
  ///
  /// The rule was fitted on the codec Windows ships.
  Fraunhofer,
  /// Shine, by its quantizer's steps: one step for every band of a granule.
  ///
  /// Other encoders leave the steps alone at the highest bitrates, so Shine is named under
  /// them.
  Shine,
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
      Self::Ffmpeg => "ffmpeg's AAC encoder",
      Self::FdkAac => "fdk-aac",
      Self::Fraunhofer => "Fraunhofer's MP3 encoder",
      Self::Shine => "Shine",
      Self::Libopus => "libopus",
    })
  }
}

/// What names a grid's encoder family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum NamedBy {
  /// How the quantizer's steps move from band to band, which holds wherever the grid sits.
  Steps,
  /// Offset of the grid, with the window for AAC, which a cut start moves.
  Offset,
  /// The steps and the offset, which name the same family.
  Both,
}

/// Frame grid a reading finds.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Found {
  /// Codec whose grid it is.
  pub codec: Codec,
  /// Encoder family the grid is consistent with, where it is one's.
  pub family: Option<Family>,
  /// What names the family, where it is named.
  pub named_by: Option<NamedBy>,
  /// What shows the grid.
  #[cfg_attr(feature = "serde", serde(flatten))]
  pub evidence: Evidence,
  /// Bands on AAC's quantizer steps beside an AAC count at the file's own rate.
  ///
  /// `None` where they do not show under either window. A Vorbis decode, whose grid the AAC
  /// count finds too, does not show them, and neither does an HE-AAC decode.
  pub quantizer: Option<AacSteps>,
}

/// the grid `scan` shows under `thresholds`, with the encoder family its steps or offset fit
pub(crate) fn found(scan: &Scan, thresholds: &ReadingThresholds) -> Option<Found> {
  let (codec, evidence, sine) = shown(scan, thresholds)?;
  // AAC's steps, which are read at the file's own rate
  let own_rate = !matches!(evidence, Evidence::CountAt { .. } | Evidence::Part { .. });
  let stepped = (codec == Codec::Aac && own_rate)
    .then(|| aac_stepped(scan, thresholds))
    .flatten();
  // the family the quantizer's steps name, and the one the grid's offset fits
  let (by_steps, by_offset) = match (&evidence, codec) {
    // a section's start is not the stream's, so its offset names no encoder, and the steps
    // are counted over stretches the section may not reach
    (Evidence::Part { .. }, _) => (None, None),
    (_, Codec::Aac) => {
      // the steps show under the encoder's window alone, so they name it where they show. a
      // Vorbis decode's count sits at Nero's offset too often for the count alone to name Nero
      let by_offset = match (&stepped, &evidence) {
        (Some((window, _)), _) => Family::of_aac(window.offset, window.sine),
        (None, Evidence::Count(count) | Evidence::CountAt { scan: count, .. }) => {
          Family::of_aac(count.offset, sine).filter(|family| *family != Family::Nero)
        }
        (None, _) => None,
      };
      // the rules are of encoders that use the Kaiser-Bessel-derived window, two of which
      // share an offset. an offset that is one encoder's alone stands before them
      let shared = matches!(by_offset, None | Some(Family::FfmpegOrFdk));
      let by_steps = stepped
        .as_ref()
        .filter(|(window, _)| !window.sine)
        .and_then(|(_, steps)| steps.neighbours.as_ref())
        .and_then(|neighbours| neighbours.family(&thresholds.aac_family_steps))
        .filter(|family| shared || Some(*family) == by_offset);
      (by_steps, by_offset)
    }
    // the steps were read on the grid of the file's own rate
    (Evidence::CountAt { .. }, Codec::Mp3) => (None, Family::of_mp3(evidence.offset())),
    // a cut start lands on an encoder's offset by chance, and the steps are the encoder's
    // own, so they stand before the offset
    (_, Codec::Mp3) => {
      let neighbours = scan
        .mp3_steps
        .as_ref()
        .and_then(|steps| steps.neighbours.as_ref());
      let by_steps =
        neighbours.and_then(|neighbours| neighbours.family(&thresholds.mp3_family_steps));
      (by_steps, Family::of_mp3(evidence.offset()))
    }
    (Evidence::Short { short }, Codec::Opus) => {
      let uncut = lattice::SHORT
        .iter()
        .filter(|length| length.length == short.length)
        .any(|length| length.uncut_offsets().contains(&short.most().0));
      (None, uncut.then_some(Family::Libopus))
    }
    (_, Codec::Opus) => {
      let uncut = evidence.offset() == OPUS_UNCUT_OFFSET;
      (None, uncut.then_some(Family::Libopus))
    }
    // no encoder of Vorbis, WMA, AC-3 or layer II is told from another
    (_, Codec::Vorbis | Codec::Wma | Codec::Mp2 | Codec::Ac3) => (None, None),
  };
  let (family, named_by) = match (by_steps, by_offset) {
    (Some(steps), Some(offset)) if steps == offset => (Some(steps), Some(NamedBy::Both)),
    (Some(steps), _) => (Some(steps), Some(NamedBy::Steps)),
    (None, Some(offset)) => (Some(offset), Some(NamedBy::Offset)),
    (None, None) => (None, None),
  };
  let quantizer = stepped
    .filter(|_| matches!(evidence, Evidence::Count(_)))
    .map(|(_, steps)| steps.clone());
  Some(Found {
    codec,
    family,
    named_by,
    evidence,
    quantizer,
  })
}

/// the samples each short block of a Vorbis stream at 44.1 or 48 kHz moves its long blocks by:
/// a run of them leaves the grid a whole number of these from where it was
const VORBIS_STEP: usize = 128;

/// where the long blocks of a libvorbis stream sit within a step, in a decode whose start was
/// not cut
const VORBIS_UNCUT: usize = 64;

/// the phase of the filterbank a layer II decode's count sits at when its start was not cut
const MP2_UNCUT: usize = 1;

/// the phases the subband count of an uncut decode sits at for a codec whose blocks start a
/// whole number of the filterbank's 32 samples apart: MP3's at the first, and Vorbis's at
/// either
const BLOCK_PHASES: [usize; 2] = [16, 17];

/// whether a grid of `codec` at `offset`, under AAC's sine window when `sine`, is where an
/// encoder's decode has it when its start was not cut
fn uncut(codec: Codec, sine: bool, offset: usize) -> bool {
  match codec {
    Codec::Mp3 => Family::of_mp3(offset).is_some(),
    Codec::Aac => Family::of_aac(offset, sine).is_some(),
    Codec::Vorbis => offset % VORBIS_STEP == VORBIS_UNCUT,
    // a decoder cuts nothing from an AC-3 stream, whose first block starts at its first sample
    Codec::Ac3 => offset == 0,
    // layer II's phase is weighed where its count is, against MP3's steps
    Codec::Mp2 | Codec::Wma | Codec::Opus => false,
  }
}

/// a count that may show a grid: its codec, whether it was taken under AAC's sine window, the
/// z it has to pass, how many of its segments have to agree, and the samples between offsets
/// that agree, `None` where they are one offset
#[derive(Clone, Copy)]
struct Counted<'a> {
  codec: Codec,
  sine: bool,
  scan: &'a CodecScan,
  z: f64,
  agreeing: usize,
  step: Option<usize>,
}

impl<'a> Counted<'a> {
  /// a count whose segments have to put their lowest count at the file's offset
  fn of(codec: Codec, sine: bool, scan: &'a CodecScan, z: f64, agreeing: usize) -> Self {
    Self {
      codec,
      sine,
      scan,
      z,
      agreeing,
      step: None,
    }
  }

  /// the segments whose own lowest count agrees with the file's and stands out past `z`
  fn agreed(&self, z: f64) -> usize {
    let scan = self.scan;
    let agrees = |at: usize| match self.step {
      Some(step) => at.abs_diff(scan.offset).is_multiple_of(step),
      None => at == scan.offset,
    };
    let segments = scan.segment_offset.iter().zip(&scan.segment_z);
    segments
      .filter(|(at, own)| agrees(**at) && **own > z)
      .count()
  }
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
    showing >= thresholds.aac_whole_steps_segments.max(1)
  })
  .max_by_key(|(_, steps)| steps.whole)
}

/// the codec whose grid `scan` shows under `thresholds`, and what shows it. in turn:
///
/// - MP3, MP2, AAC, Vorbis, WMA or AC-3 over its z threshold with as many segments agreeing as
///   its own threshold asks, the one furthest over its z threshold when several are. AAC is
///   read under each window
/// - MP3 by its quantizer's steps in enough segments, then AAC by its own
/// - Opus with enough frames on its lattice in the segments or in the windows: at the
///   decoder's level, at another, as frames under 20 ms, with the pitch post-filter taken
///   off, or as the hybrid mode codes them
/// - MP3 or AAC at the other rate
/// - MP3, Vorbis or MP2 by the phase every segment's subband count sits at
/// - one of the first group in some of the segments alone, each far over its z threshold, or
///   in fewer at an offset an uncut decode has
/// - MP3 by its quantizer's steps in fewer segments, as a grid in part of the file
///
/// the codec decoded last leaves the grid that counts, and Opus's energies can show through
/// an MP3 made from its decode.
///
/// the last of the three is whether the evidence is a count taken under AAC's sine window
fn shown(scan: &Scan, thresholds: &ReadingThresholds) -> Option<(Codec, Evidence, bool)> {
  let aac = |scan, sine| {
    Counted::of(
      Codec::Aac,
      sine,
      scan,
      thresholds.aac_grid_z,
      thresholds.aac_grid_segments_agreeing,
    )
  };
  let mp3 = |scan| {
    Counted::of(
      Codec::Mp3,
      false,
      scan,
      thresholds.mp3_grid_z,
      thresholds.mp3_grid_segments_agreeing,
    )
  };
  fn clearest<const N: usize>(counts: [Option<Counted<'_>>; N]) -> Option<Counted<'_>> {
    counts
      .into_iter()
      .flatten()
      .filter(|count| count.scan.z > count.z && count.agreed(f64::NEG_INFINITY) >= count.agreeing)
      .max_by(|a, b| (a.scan.z / a.z).total_cmp(&(b.scan.z / b.z)))
  }
  // MP3's steps, and the segments they show in
  let mp3_steps = scan.mp3_steps.as_ref().map(|steps| {
    let segments = steps.segment_whole.iter();
    let showing = segments
      .filter(|whole| **whole >= thresholds.mp3_whole_steps)
      .count();
    (steps, showing)
  });
  // an MP3 decode's subband count sits at its grid's phase, so the uncut phase says nothing
  // of layer II where MP3's steps show
  let mp3_unstepped = mp3_steps.is_none_or(|(_, showing)| showing == 0);
  let counts = [
    Some(aac(&scan.aac, false)),
    scan.aac_sine.as_ref().map(|scan| aac(scan, true)),
    // a Vorbis long block has AAC's length and hop, so each count finds the other's grid
    // weakly, and the window the encoder used stands furthest out
    scan.vorbis.as_ref().map(|scan| Counted {
      codec: Codec::Vorbis,
      sine: false,
      scan,
      z: thresholds.vorbis_grid_z,
      agreeing: thresholds.vorbis_grid_segments_agreeing,
      // short blocks move a stream's long ones, so its segments agree to the step alone
      step: Some(VORBIS_STEP),
    }),
    scan.wma.as_ref().map(|scan| {
      Counted::of(
        Codec::Wma,
        false,
        scan,
        thresholds.wma_grid_z,
        thresholds.wma_grid_segments_agreeing,
      )
    }),
    scan.ac3.as_ref().map(|scan| {
      Counted::of(
        Codec::Ac3,
        false,
        scan,
        thresholds.ac3_grid_z,
        thresholds.ac3_grid_segments_agreeing,
      )
    }),
    Some(mp3(&scan.mp3)),
    // layer II quantizes the filterbank's subband samples, which an MP3 decode's count does
    // not show. of 32 phases three segments share one by chance far more often than four
    scan.mp2.as_ref().map(|scan| {
      let z = if scan.segments_agreeing == super::SEGMENTS {
        thresholds.mp2_all_agreeing_grid_z
      } else if scan.offset == MP2_UNCUT && mp3_unstepped {
        thresholds.mp2_uncut_grid_z
      } else {
        thresholds.mp2_grid_z
      };
      Counted::of(
        Codec::Mp2,
        false,
        scan,
        z,
        thresholds.mp2_grid_segments_agreeing,
      )
    }),
  ];
  let own =
    clearest(counts).map(|count| (count.codec, Evidence::Count(count.scan.clone()), count.sine));
  // a section decoded from a codec leaves its grid in the segments that fall in it
  let part = || {
    let (segments, count) = counts
      .into_iter()
      .flatten()
      .map(|count| {
        // at an offset an uncut decode has, fewer segments are enough where the file's count
        // stands out, and one where its own stands far out or MP3's steps show in it
        let at_uncut = uncut(count.codec, count.sine, count.scan.offset);
        let agreeing = count.agreed(f64::NEG_INFINITY);
        let agreed = at_uncut
          && count.scan.z > count.z
          && agreeing >= thresholds.uncut_grid_segments_agreeing.max(1);
        let stepped = count.codec == Codec::Mp3
          && mp3_steps.is_some_and(|(steps, _)| {
            let segments = count.scan.segment_offset.iter().zip(&steps.segment_whole);
            segments
              .filter(|(at, _)| **at == count.scan.offset)
              .any(|(_, whole)| *whole >= thresholds.mp3_whole_steps)
          });
        let alone = at_uncut && (stepped || count.agreed(thresholds.lone_grid_z) > 0);
        let segments = count.agreed(thresholds.part_grid_z).max(if agreed {
          agreeing
        } else {
          usize::from(alone)
        });
        (segments, agreed || alone, count)
      })
      .filter(|(segments, uncut, _)| *uncut || *segments >= thresholds.part_grid_segments.max(1))
      .map(|(segments, _, count)| (segments, count))
      .max_by(|(a, x), (b, y)| a.cmp(b).then(x.scan.z.total_cmp(&y.scan.z)))?;
    let evidence = Evidence::Part {
      segments,
      scan: count.scan.clone(),
    };
    Some((count.codec, evidence, count.sine))
  };
  // the quantizer's steps show a grid whose count stands out too little
  let stepped = || {
    let (steps, showing) = mp3_steps?;
    (showing >= thresholds.mp3_grid_segments_agreeing.max(1)).then(|| {
      let evidence = Evidence::Steps {
        offset: scan.mp3.offset,
        steps: steps.clone(),
      };
      (Codec::Mp3, evidence, false)
    })
  };
  // and in fewer segments a grid in part of the file
  let stepped_part = || {
    let (_, showing) = mp3_steps?;
    (showing >= thresholds.mp3_whole_steps_segments.max(1)).then(|| {
      let evidence = Evidence::Part {
        segments: showing,
        scan: scan.mp3.clone(),
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
    let counts = [
      Some(aac(&other.aac, false)),
      Some(aac(&other.aac_sine, true)),
      Some(mp3(&other.mp3)),
    ];
    // one segment far out is enough at an offset an uncut decode has
    let alone = || {
      let counts = counts.into_iter().flatten();
      counts
        .filter(|count| uncut(count.codec, count.sine, count.scan.offset))
        .filter(|count| count.agreed(thresholds.lone_grid_z) > 0)
        .max_by(|a, b| a.scan.z.total_cmp(&b.scan.z))
    };
    let count = clearest(counts).or_else(alone)?;
    let evidence = Evidence::CountAt {
      rate: other.rate,
      scan: count.scan.clone(),
    };
    Some((count.codec, evidence, count.sine))
  };
  // the subband count sits at one phase in every segment of a decode whose blocks start a
  // whole number of the filterbank's samples apart, however little it stands out
  let phased = || {
    let subbands = scan.mp2.as_ref()?;
    let enough = |phases: &[usize]| {
      let segments = subbands.segment_offset.iter();
      segments.filter(|phase| phases.contains(phase)).count()
        >= thresholds.block_phase_segments.max(1)
    };
    let codec = if enough(&BLOCK_PHASES) {
      if subbands.segment_offset.contains(&BLOCK_PHASES[1]) {
        Codec::Vorbis
      } else {
        Codec::Mp3
      }
    } else if enough(&[MP2_UNCUT]) && mp3_unstepped {
      Codec::Mp2
    } else {
      return None;
    };
    Some((codec, Evidence::Count(subbands.clone()), false))
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
    } else if let Some(unfiltered) = opus
      .unfiltered
      .as_ref()
      .filter(|unfiltered| unfiltered.frames >= thresholds.opus_unfiltered_frames)
    {
      Evidence::Unfiltered {
        unfiltered: unfiltered.clone(),
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
    .or_else(phased)
    .or_else(part)
    .or_else(stepped_part)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::grid::{AacNeighbours, Mp3Neighbours, OtherRate, SEGMENTS};
  use crate::thresholds::{FamilySteps, StepBound, StepRule, StepShare};

  #[test]
  fn aacs_steps_name_the_window_and_show_a_grid_no_count_does() {
    let thresholds = ReadingThresholds::default();
    let count = |offset: usize, z: f64| CodecScan {
      offset,
      z,
      segments_agreeing: 4,
      segment_z: [z; SEGMENTS],
      segment_offset: [offset; SEGMENTS],
    };
    let steps = |whole: usize| AacSteps {
      tested: 12_000,
      stepped: 4 * whole,
      whole: 4 * whole,
      segment_whole: [whole; SEGMENTS],
      neighbours: None,
    };
    let scan = |kbd: f64, sine: f64, kbd_steps: usize, sine_steps: usize| Scan {
      aac: count(448, kbd),
      aac_sine: Some(count(448, sine)),
      vorbis: None,
      wma: None,
      ac3: None,
      mp3: count(0, 2.0),
      mp2: None,
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
    // nor does a count at Nero's offset and window name Nero, as a Vorbis decode's can sit there
    let vorbis = found(&scan(93.0, 78.0, under, under), &thresholds).expect("the count is over");
    assert_eq!(vorbis.evidence, Evidence::Count(count(448, 93.0)));
    assert_eq!((vorbis.family, vorbis.quantizer), (None, None));
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
      segment_offset: std::array::from_fn(|i| usize::from(i >= segments_agreeing)),
    };
    let scan = |aac: (f64, usize), mp3: (f64, usize)| Scan {
      aac: codec_scan(aac.0, aac.1),
      aac_sine: None,
      vorbis: None,
      wma: None,
      ac3: None,
      mp3: codec_scan(mp3.0, mp3.1),
      mp2: None,
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
    // a Vorbis stream's short blocks move its long ones by whole steps of 128: three segments
    // a whole number of steps from the file's offset are a grid
    let vorbis = |z: f64, segment_offset: [usize; SEGMENTS]| Scan {
      vorbis: Some(CodecScan {
        offset: 5,
        z,
        segments_agreeing: 1,
        segment_z: [0.0; SEGMENTS],
        segment_offset,
      }),
      ..scan((2.0, 0), (2.0, 0))
    };
    assert_eq!(
      codec(&vorbis(10.0, [5, 133, 901, 300])),
      Some(Codec::Vorbis)
    );
    assert_eq!(codec(&vorbis(10.0, [5, 134, 902, 300])), None);
    // of 32 phases three segments share one by chance, so MP2 short of its threshold is a
    // grid only where all four do
    let mp2 = |z: f64, segments_agreeing: usize| Scan {
      mp2: Some(codec_scan(z, segments_agreeing)),
      ..scan((2.0, 0), (2.0, 0))
    };
    let between = (thresholds.mp2_all_agreeing_grid_z + thresholds.mp2_grid_z) / 2.0;
    assert_eq!(codec(&mp2(between, 3)), None);
    assert_eq!(codec(&mp2(between, 4)), Some(Codec::Mp2));
    assert_eq!(
      codec(&mp2(thresholds.mp2_grid_z + 1.0, 3)),
      Some(Codec::Mp2)
    );
    // two segments far over the threshold at one offset are a grid in part of the file, and
    // one is none
    let part = |segment_z: [f64; SEGMENTS]| Scan {
      mp3: CodecScan {
        offset: 0,
        z: 90.0,
        segments_agreeing: 2,
        segment_z,
        segment_offset: [0, 7, 0, 9],
      },
      ..scan((2.0, 0), (2.0, 0))
    };
    let over = thresholds.part_grid_z + 1.0;
    let in_part = found(&part([over, 3.0, over, 3.0]), &thresholds).expect("two segments");
    assert!(matches!(
      in_part.evidence,
      Evidence::Part { segments: 2, .. }
    ));
    assert_eq!((in_part.codec, in_part.family), (Codec::Mp3, None));
    assert_eq!(
      codec(&part([over, over, 3.0, over])),
      None,
      "the second is elsewhere"
    );
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
      unfiltered: None,
    };
    let after = Scan {
      opus: Some(opus.clone()),
      ..converted
    };
    let last = found(&after, &thresholds).expect("the lattice is over");
    assert_eq!(last.evidence, Evidence::Lattice(opus));
    assert_eq!(last.family, Some(Family::Libopus));
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
        unfiltered: None,
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
  fn the_steps_name_an_mp3_encoder_wherever_the_grid_sits() {
    let thresholds = ReadingThresholds::default();
    // the tuning's counts for decodes whose starts were cut: LAME -V 2, Helix at 128 kbit/s,
    // Fraunhofer at 192, Shine at 128 and at 320, GOGO at 192
    let counts = |pairs, same, finer, far, twos: (usize, usize)| Mp3Neighbours {
      pairs,
      same,
      finer,
      far,
      two_finer: twos.0,
      two_coarser: twos.1,
      ..Mp3Neighbours::default()
    };
    let lame = Mp3Neighbours {
      low_bands: 1116,
      low_log: 4798.956,
      mid_bands: 1981,
      high_bands: 3006,
      high_log: 6203.828,
      ones: [1, 10, 252],
      ranked: 6098,
      coarsest: 1303,
      granules: 268,
      joint: 41,
      ..counts(
        [1115, 1932, 2224],
        [320, 641, 649],
        [422, 649, 1171],
        [299, 311, 261],
        (1087, 694),
      )
    };
    let helix = Mp3Neighbours {
      low_bands: 936,
      low_log: 2749.754,
      mid_bands: 1462,
      high_bands: 2022,
      high_log: 2931.149,
      ones: [16, 138, 946],
      ranked: 4369,
      coarsest: 558,
      granules: 268,
      joint: 48,
      ..counts(
        [917, 1379, 1415],
        [312, 521, 538],
        [401, 515, 577],
        [40, 38, 42],
        (1011, 658),
      )
    };
    let fraunhofer = Mp3Neighbours {
      low_bands: 1360,
      low_log: 4457.271,
      mid_bands: 2047,
      high_bands: 3304,
      high_log: 6723.253,
      ones: [8, 181, 395],
      ranked: 6700,
      coarsest: 1090,
      granules: 268,
      joint: 0,
      ..counts(
        [1299, 1966, 2497],
        [650, 1130, 1574],
        [436, 677, 855],
        [25, 9, 20],
        (1510, 354),
      )
    };
    let flat = |pairs: [usize; 3], bands: [usize; 3], logs: (f64, f64), ones| Mp3Neighbours {
      low_bands: bands[0],
      low_log: logs.0,
      mid_bands: bands[1],
      high_bands: bands[2],
      high_log: logs.1,
      ones,
      ranked: bands.iter().sum(),
      coarsest: bands.iter().sum(),
      granules: 268,
      joint: 0,
      ..counts(pairs, pairs, [0; 3], [0; 3], (0, 0))
    };
    let shine = flat(
      [1067, 1598, 1735],
      [1100, 1750, 2431],
      (3873.998, 3661.633),
      [2, 163, 1013],
    );
    // at 320 kbit/s other encoders leave the steps alone too, so its large values name none
    let shine_320 = flat(
      [435, 1671, 2370],
      [472, 1653, 3078],
      (2388.523, 9371.035),
      [0, 1, 139],
    );
    let gogo = Mp3Neighbours {
      low_bands: 965,
      low_log: 3738.925,
      mid_bands: 1667,
      high_bands: 2687,
      high_log: 5206.948,
      ones: [2, 84, 280],
      ranked: 5312,
      coarsest: 4541,
      granules: 268,
      joint: 1,
      ..counts(
        [916, 1633, 2032],
        [904, 1564, 1333],
        [7, 38, 641],
        [2, 5, 156],
        (320, 69),
      )
    };
    let family = |neighbours: &Mp3Neighbours, offset: usize| {
      let count = CodecScan {
        offset,
        z: 50.0,
        segments_agreeing: 4,
        segment_z: [12.0; SEGMENTS],
        segment_offset: [offset; SEGMENTS],
      };
      let quiet = CodecScan {
        z: 2.0,
        segments_agreeing: 0,
        ..count.clone()
      };
      let scan = Scan {
        aac: quiet,
        aac_sine: None,
        vorbis: None,
        wma: None,
        ac3: None,
        mp3: count,
        mp2: None,
        opus: None,
        other_rate: None,
        mp3_steps: Some(Mp3Steps {
          tested: 9000,
          stepped: 3400,
          whole: 3300,
          segment_whole: [800; SEGMENTS],
          neighbours: Some(neighbours.clone()),
        }),
        aac_steps: None,
        aac_sine_steps: None,
      };
      found(&scan, &thresholds).and_then(|found| found.family)
    };
    // an offset no encoder leaves, as after a cut
    for (neighbours, named) in [
      (&lame, Some(Family::Lame)),
      (&helix, Some(Family::Helix)),
      (&fraunhofer, Some(Family::Fraunhofer)),
      (&shine, Some(Family::Shine)),
      (&shine_320, None),
      (&gogo, None),
    ] {
      assert_eq!(family(neighbours, 300), named, "{neighbours:?}");
    }
    // the steps stand before an offset a cut can land on, and the offset names what they do not
    assert_eq!(family(&lame, 96), Some(Family::Lame));
    assert_eq!(family(&helix, 48), Some(Family::Helix));
    assert_eq!(family(&gogo, 48), Some(Family::Lame));
    assert_eq!(family(&gogo, 1), Some(Family::Untagged));
    assert_eq!(family(&fraunhofer, 1), Some(Family::Fraunhofer));
    // too few pairs to tell
    let few = Mp3Neighbours {
      pairs: [20, 60, 150],
      same: [20, 60, 150],
      ..shine.clone()
    };
    assert_eq!(family(&few, 1), Some(Family::Untagged));
    // counts kept before the far pairs and the ranked bands were counted name none, though
    // no far pair is within Helix's bounds
    for counted in [&fraunhofer, &helix, &lame] {
      let kept = Mp3Neighbours {
        far: [0; 3],
        mid_bands: 0,
        ones: [0; 3],
        ranked: 0,
        coarsest: 0,
        granules: 0,
        joint: 0,
        ..counted.clone()
      };
      assert_eq!(family(&kept, 1), Some(Family::Untagged));
    }
    // a decode inside the rules of two families is named neither's
    let both = ReadingThresholds {
      mp3_family_steps: FamilySteps {
        rules: vec![
          StepRule {
            family: Family::Shine,
            bounds: Vec::new(),
          },
          StepRule {
            family: Family::Helix,
            bounds: Vec::new(),
          },
        ],
        ..thresholds.mp3_family_steps.clone()
      },
      ..thresholds.clone()
    };
    assert_eq!(helix.family(&both.mp3_family_steps), None);
    // a region the codec does not have meets no bound
    let beyond = StepBound {
      share: StepShare::SameIn(3),
      min: Some(0.0),
      max: None,
    };
    let rules = FamilySteps {
      rules: vec![StepRule {
        family: Family::Helix,
        bounds: vec![beyond],
      }],
      ..thresholds.mp3_family_steps.clone()
    };
    assert_eq!(helix.family(&rules), None);
  }

  #[test]
  fn the_steps_tell_ffmpegs_aac_from_fdk_aac_and_leave_nero_to_its_offset() {
    let thresholds = ReadingThresholds::default();
    // the tuning's counts for decodes at 128 kbit/s whose starts were cut
    let ffmpeg = AacNeighbours {
      pairs: [1880, 2439, 2334, 1216],
      same: [218, 406, 397, 184],
      finer: [889, 1134, 1119, 808],
      one: [464, 691, 756, 334],
      far: [585, 484, 360, 246],
      bands: [2285, 2658, 2551, 1546],
      ones: [214, 1269, 1434, 1094],
      ranked: [2284, 2658, 2550, 1541],
      coarsest: [289, 29, 0, 0],
    };
    let fdk = AacNeighbours {
      pairs: [1936, 2208, 1718, 489],
      same: [1209, 1281, 1071, 228],
      finer: [426, 597, 434, 190],
      one: [398, 517, 372, 111],
      far: [53, 57, 54, 73],
      bands: [2182, 2362, 1850, 737],
      ones: [9, 397, 797, 454],
      ranked: [2177, 2358, 1844, 728],
      coarsest: [483, 188, 33, 7],
    };
    let nero = AacNeighbours {
      pairs: [1350, 1321, 1340, 508],
      same: [579, 601, 507, 219],
      finer: [440, 368, 506, 233],
      one: [299, 295, 324, 137],
      far: [191, 139, 146, 28],
      bands: [1513, 1422, 1415, 657],
      ones: [23, 179, 647, 638],
      ranked: [1506, 1422, 1415, 656],
      coarsest: [200, 46, 52, 3],
    };
    let family = |neighbours: Option<&AacNeighbours>, offset: usize, sine: bool| {
      let count = CodecScan {
        offset,
        z: 50.0,
        segments_agreeing: 4,
        segment_z: [12.0; SEGMENTS],
        segment_offset: [offset; SEGMENTS],
      };
      let quiet = CodecScan {
        z: 2.0,
        segments_agreeing: 0,
        ..count.clone()
      };
      let steps = |whole: usize| AacSteps {
        tested: 12_000,
        stepped: 4 * whole,
        whole: 4 * whole,
        segment_whole: [whole; SEGMENTS],
        neighbours: neighbours.cloned(),
      };
      let (kbd, other) = if sine { (20, 2_000) } else { (2_000, 20) };
      let scan = Scan {
        aac: if sine { quiet.clone() } else { count.clone() },
        aac_sine: Some(if sine { count } else { quiet.clone() }),
        vorbis: None,
        wma: None,
        ac3: None,
        mp3: quiet,
        mp2: None,
        opus: None,
        other_rate: None,
        mp3_steps: None,
        aac_steps: Some(steps(kbd)),
        aac_sine_steps: Some(steps(other)),
      };
      found(&scan, &thresholds).and_then(|found| found.family)
    };
    // the two share the offset of an uncut decode, and are told apart there and after a cut
    for offset in [0, 531] {
      assert_eq!(family(Some(&ffmpeg), offset, false), Some(Family::Ffmpeg));
      assert_eq!(family(Some(&fdk), offset, false), Some(Family::FdkAac));
    }
    // Nero's steps lie beside fdk-aac's, and no rule names them
    assert_eq!(family(Some(&nero), 761, false), None);
    assert_eq!(family(Some(&nero), 448, false), Some(Family::Nero));
    // steps kept without their neighbours leave the offset's name
    assert_eq!(family(None, 0, false), Some(Family::FfmpegOrFdk));
    assert_eq!(family(None, 531, false), None);
    // an offset that is one encoder's alone names it whatever the steps are like
    assert_eq!(family(Some(&fdk), 448, false), Some(Family::Nero));
    // the rules are of encoders that use the Kaiser-Bessel-derived window
    assert_eq!(family(Some(&fdk), 531, true), None);
    assert_eq!(family(Some(&fdk), 960, true), Some(Family::Apple));
  }

  #[test]
  fn an_uncut_decodes_offset_asks_for_less() {
    let thresholds = ReadingThresholds::default();
    // segments at `offsets` with own z `own`, summed to `z` at the first
    let count = |z: f64, offsets: [usize; SEGMENTS], own: [f64; SEGMENTS]| CodecScan {
      offset: offsets[0],
      z,
      segments_agreeing: offsets.iter().filter(|at| **at == offsets[0]).count(),
      segment_z: own,
      segment_offset: offsets,
    };
    let quiet = || count(3.0, [5, 300, 411, 520], [3.0; SEGMENTS]);
    let scan = |mp3: CodecScan| Scan {
      aac: quiet(),
      aac_sine: None,
      vorbis: None,
      wma: None,
      ac3: None,
      mp3,
      mp2: None,
      opus: None,
      other_rate: None,
      mp3_steps: None,
      aac_steps: None,
      aac_sine_steps: None,
    };
    let read = |scan: &Scan| found(scan, &thresholds).map(|f| (f.codec, f.evidence));
    // two segments agree: a grid in part at LAME's offset, none at an offset no encoder has
    let lame = count(8.0, [48, 48, 300, 411], [4.0; SEGMENTS]);
    let part = Evidence::Part {
      segments: 2,
      scan: lame.clone(),
    };
    assert_eq!(read(&scan(lame)), Some((Codec::Mp3, part)));
    assert_eq!(
      read(&scan(count(8.0, [50, 50, 300, 411], [4.0; SEGMENTS]))),
      None
    );
    // one segment far out by itself: a grid in part at LAME's offset alone
    let alone = count(6.0, [48, 9, 300, 411], [13.0, 3.0, 3.0, 3.0]);
    let part = Evidence::Part {
      segments: 1,
      scan: alone.clone(),
    };
    assert_eq!(read(&scan(alone)), Some((Codec::Mp3, part)));
    assert_eq!(
      read(&scan(count(6.0, [50, 9, 300, 411], [13.0, 3.0, 3.0, 3.0]))),
      None
    );
    // whole steps in two segments are a grid, in one they are none
    let steps = |segment_whole: [usize; SEGMENTS]| Scan {
      mp3_steps: Some(Mp3Steps {
        tested: 7_000,
        stepped: 120,
        whole: segment_whole.iter().sum(),
        segment_whole,
        neighbours: None,
      }),
      ..scan(quiet())
    };
    let enough = thresholds.mp3_whole_steps;
    let stepped = found(&steps([enough, 0, enough, 2]), &thresholds).expect("two show");
    assert!(matches!(
      stepped.evidence,
      Evidence::Part { segments: 2, .. }
    ));
    let whole = found(&steps([enough, enough, enough, 2]), &thresholds).expect("three show");
    assert!(matches!(whole.evidence, Evidence::Steps { .. }));
    assert_eq!(found(&steps([600, enough - 1, 0, 0]), &thresholds), None);
    // an MP2 count three segments put at the uncut phase, and at another
    let subbands = |z: f64, phases: [usize; SEGMENTS]| Scan {
      mp2: Some(count(z, phases, [3.0; SEGMENTS])),
      ..scan(quiet())
    };
    let codec = |scan: &Scan| found(scan, &thresholds).map(|f| f.codec);
    assert_eq!(codec(&subbands(7.0, [1, 1, 1, 20])), Some(Codec::Mp2));
    assert_eq!(codec(&subbands(7.0, [5, 5, 5, 20])), None);
    // every segment at a block's phase: Vorbis where one sits at its own, MP3 otherwise
    assert_eq!(codec(&subbands(3.0, [16, 17, 16, 16])), Some(Codec::Vorbis));
    assert_eq!(codec(&subbands(3.0, [16, 16, 16, 16])), Some(Codec::Mp3));
    assert_eq!(codec(&subbands(3.0, [16, 17, 16, 9])), None);
    // every segment at layer II's phase is MP2 however little the count stands out, unless
    // MP3's steps show
    assert_eq!(codec(&subbands(3.0, [1, 1, 1, 1])), Some(Codec::Mp2));
    assert_eq!(codec(&subbands(3.0, [1, 1, 1, 20])), None);
    let stepped = Scan {
      mp3_steps: steps([enough, 0, 0, 0]).mp3_steps,
      ..subbands(3.0, [1, 1, 1, 1])
    };
    assert_eq!(codec(&stepped), None);
    // steps in the one segment that sits at LAME's offset are a grid in part, and are none
    // in a segment elsewhere or at an offset no encoder has
    let one = |offsets: [usize; SEGMENTS], segment_whole: [usize; SEGMENTS]| Scan {
      mp3: count(4.0, offsets, [4.0; SEGMENTS]),
      ..steps(segment_whole)
    };
    let lone = found(&one([48, 9, 300, 411], [enough, 0, 0, 0]), &thresholds).expect("one shows");
    assert!(matches!(lone.evidence, Evidence::Part { segments: 1, .. }));
    assert_eq!(codec(&one([48, 9, 300, 411], [0, enough, 0, 0])), None);
    assert_eq!(codec(&one([50, 9, 300, 411], [enough, 0, 0, 0])), None);
    // at the other rate one segment far out is enough at LAME's offset, and not at another
    let converted = |offset: usize| Scan {
      other_rate: Some(OtherRate {
        rate: 44_100,
        aac: quiet(),
        aac_sine: quiet(),
        mp3: count(6.0, [offset, 9, 300, 411], [13.0, 3.0, 3.0, 3.0]),
      }),
      ..scan(quiet())
    };
    let at_other = found(&converted(48), &thresholds).expect("one stands out at LAME's offset");
    assert!(matches!(
      at_other.evidence,
      Evidence::CountAt { rate: 44_100, .. }
    ));
    assert_eq!(codec(&converted(50)), None);
  }
}
