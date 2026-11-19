//! the Kaiser window and the lowpass filters made under it

use std::f64::consts::PI;

/// the Kaiser window with `beta` at `r`, which runs from -1 at one end to 1 at the other
pub(crate) fn window(beta: f64, r: f64) -> f64 {
  bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta)
}

/// a lowpass of `taps` taps cut at `cutoff`, a fraction of the sample rate, under a Kaiser
/// window with `beta`
pub(crate) fn lowpass(taps: usize, cutoff: f64, beta: f64) -> Vec<f64> {
  let middle = (taps - 1) as f64 / 2.0;
  (0..taps)
    .map(|i| {
      let t = i as f64 - middle;
      let x = PI * 2.0 * cutoff * t;
      let sinc = if x == 0.0 { 1.0 } else { x.sin() / x };
      2.0 * cutoff * sinc * window(beta, t / middle)
    })
    .collect()
}

/// the modified Bessel function of the first kind of order 0, from its power series
fn bessel_i0(x: f64) -> f64 {
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
