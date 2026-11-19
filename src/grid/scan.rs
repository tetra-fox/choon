//! the scan of a file's segments: each codec's analysis of each segment as a job on rayon's
//! thread pool, and what the jobs found gathered into a [`Scan`]

use rayon::prelude::*;

use super::aac::{Aac, AacStepper, Window, aac_bands};
use super::lattice;
use super::mp3::{GranuleSteps, Mp3, mp3_bands};
use super::resample::Resampler;
use super::segments::{Segment, Segments};
use super::{
  AacSteps, CodecScan, MP3_HOP, Mp3Neighbours, Mp3Steps, OPUS_UNCUT_OFFSET, OpusHybrid, OpusScan,
  OpusShifted, OpusShort, OtherRate, SEGMENTS, Scan,
};

/// MAD times this estimates the standard deviation of normally distributed values
const MAD_TO_SD: f64 = 1.4826;

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
pub(super) fn active(values: &[f32], t2: f32) -> f64 {
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

#[cfg(test)]
mod tests {
  use super::*;

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
