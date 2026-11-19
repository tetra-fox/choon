//! the MDCT the AAC, MP3 and Opus analyses share: the fold of a windowed frame and an
//! orthonormal DCT-IV through a complex FFT of half its length

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::num_traits::Float;
use rustfft::{Fft, FftNum, FftPlanner};

/// the input of the DCT-IV whose output is the MDCT of `frame`, 2N windowed samples:
/// (-c_r - d, a - b_r) for its quarters a, b, c, d, so that the DCT-IV gives
/// X_k = sqrt(2/N) sum_n x_n cos(pi/N (n + 1/2 + N/2)(k + 1/2))
pub(crate) fn fold<T: Float>(frame: &[T], folded: &mut [T]) {
  fold_with(frame, folded, |c, d| -*c - *d, |a, b| *a - *b);
}

/// [`fold`] over any elements: `negated_sum(c, d)` is -c - d and `difference(a, b)` is a - b
pub(super) fn fold_with<T, U>(
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

/// the orthonormal DCT-IV of length N through a complex FFT of length N/2:
/// X_k = sqrt(2/N) sum_n u_n cos(pi/N (n + 1/2)(k + 1/2))
pub(crate) struct DctIv<T = f32> {
  fft: Arc<dyn Fft<T>>,
  /// exp(-i pi n / N), for the input
  pre: Vec<Complex<T>>,
  /// sqrt(2/N) exp(-i pi (k + 1/4) / N), for the output
  post: Vec<Complex<T>>,
  buffer: Vec<Complex<T>>,
  scratch: Vec<Complex<T>>,
}

impl<T: FftNum + Float> DctIv<T> {
  pub(crate) fn new(n: usize) -> Self {
    let half = n / 2;
    let fft = FftPlanner::new().plan_fft_forward(half);
    let narrowed = |value: f64| T::from_f64(value).expect("a float holds a number of size one");
    let turn = |angle: f64, scale: f64| {
      Complex::new(narrowed(scale * angle.cos()), narrowed(scale * angle.sin()))
    };
    let step = std::f64::consts::PI / n as f64;
    let scale = (2.0 / n as f64).sqrt();
    Self {
      pre: (0..half).map(|i| turn(-step * i as f64, 1.0)).collect(),
      post: (0..half)
        .map(|k| turn(-step * (k as f64 + 0.25), scale))
        .collect(),
      buffer: vec![Complex::new(T::zero(), T::zero()); half],
      scratch: vec![Complex::new(T::zero(), T::zero()); fft.get_inplace_scratch_len()],
      fft,
    }
  }

  /// writes the transform of `input` to `output`, both of length N.
  ///
  /// with z_n = u_2n + i u_(N-1-2n) and Y_k = sum_n z_n exp(-i pi (2n + 1/2)(2k + 1/2) / N),
  /// X_2k is the real part of Y_k and X_(N-1-2k) minus its imaginary part.
  pub(crate) fn process(&mut self, input: &[T], output: &mut [T]) {
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
    let turned = || {
      self
        .buffer
        .iter()
        .zip(&self.post)
        .map(|(b, post)| *b * *post)
    };
    for (pair, y) in pairs.iter_mut().zip(turned()) {
      pair[0] = y.re;
    }
    for (pair, y) in pairs.iter_mut().rev().zip(turned()) {
      pair[1] = -y.im;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
}
