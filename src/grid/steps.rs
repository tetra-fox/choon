//! the quantizer step the values of a scalefactor band sit on, which the MP3 and the AAC
//! analysis both look for: each value a whole number to the power 4/3 times the band's step

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

/// what a band's values show of a quantizer's step
#[derive(Clone, Copy)]
pub(super) enum BandStep {
  /// too few values over the noise, or the smallest too near it for a lattice to show
  Untested,
  /// no step puts every value over the noise on its lattice
  Off,
  /// the step that does
  On(f64),
}

impl BandStep {
  pub(super) fn tested(self) -> bool {
    !matches!(self, Self::Untested)
  }

  pub(super) fn step(self) -> Option<f64> {
    match self {
      Self::On(step) => Some(step),
      Self::Untested | Self::Off => None,
    }
  }
}

/// the vectors [`band_step`] fills for each band, kept from one band to the next
#[derive(Default)]
pub(super) struct BandScratch {
  values: Vec<f64>,
  points: Vec<f64>,
}

/// the step of a band whose lines are `lines`, each with rounding noise of `sigma`. the
/// smallest value over the noise is taken as the whole number 1, 2 and so on in turn
pub(super) fn band_step(lines: &[f64], sigma: f64, scratch: &mut BandScratch) -> BandStep {
  let BandScratch { values, points } = scratch;
  values.clear();
  values.extend(
    lines
      .iter()
      .map(|line| line.abs())
      .filter(|value| *value > OVER_NOISE * sigma),
  );
  if values.len() < MIN_VALUES {
    return BandStep::Untested;
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
    points.clear();
    points.extend(
      values
        .iter()
        .map(|value| (value / step).powf(0.75).round().max(1.0).powf(4.0 / 3.0)),
    );
    let squares: f64 = points.iter().map(|point| point * point).sum();
    let step = values.iter().zip(&*points).map(|(v, p)| v * p).sum::<f64>() / squares;
    // a value's own noise, and what the noise of the others leaves of the step's
    let near = |point: f64| ON_POINT * sigma * (1.0 + point / squares.sqrt());
    if values
      .iter()
      .zip(&*points)
      .all(|(value, point)| (value - point * step).abs() < near(*point))
    {
      return BandStep::On(step);
    }
  }
  if tested {
    BandStep::Off
  } else {
    BandStep::Untested
  }
}

/// the whole step of a band whose lines are `lines` and whose values sit on `step`, in quarter
/// powers of two of `step / unit`, with its largest quantized value. `None` where the step is
/// not a whole power of the fourth root of two
pub(super) fn whole_step(step: f64, unit: f64, lines: &[f64]) -> Option<(i32, f64)> {
  let fourths = 4.0 * (step / unit).log2();
  ((fourths - fourths.round()).abs() < WHOLE_STEP).then(|| {
    let top = lines.iter().fold(0.0f64, |top, line| top.max(line.abs()));
    // a step within 2^-100 and 2^100, so its quarter powers fit
    (fourths.round() as i32, (top / step).powf(0.75).round())
  })
}
