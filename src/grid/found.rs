//! the rule that reads a scan under a reading's thresholds: the codec whose grid it shows, what
//! shows it, and the encoder family the grid's offset fits

use std::fmt;

use super::lattice;
use super::{
  AacSteps, Codec, CodecScan, Mp3Steps, OPUS_UNCUT_OFFSET, OpusHybrid, OpusScan, OpusShifted,
  OpusShort, Scan,
};
use crate::thresholds::ReadingThresholds;

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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::grid::{Mp3Neighbours, OtherRate, SEGMENTS};

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
}
