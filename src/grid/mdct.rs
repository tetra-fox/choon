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

/// [`fold`] of `frame` times `window`, sample by sample, without the windowed frame in between
pub(crate) fn fold_windowed<T: Float>(frame: &[T], window: &[T], folded: &mut [T]) {
  let q = folded.len() / 2;
  fn quarters<T>(all: &[T], q: usize) -> [&[T]; 4] {
    let (ab, cd) = all.split_at(2 * q);
    let ((a, b), (c, d)) = (ab.split_at(q), cd.split_at(q));
    [a, b, c, d]
  }
  let ([a, b, c, d], [wa, wb, wc, wd]) = (quarters(frame, q), quarters(window, q));
  let (low, high) = folded.split_at_mut(q);
  let reversed = c.iter().zip(wc).rev();
  for ((folded, (c, wc)), (d, wd)) in low.iter_mut().zip(reversed).zip(d.iter().zip(wd)) {
    *folded = -(*c * *wc) - *d * *wd;
  }
  let reversed = b.iter().zip(wb).rev();
  for ((folded, (a, wa)), (b, wb)) in high.iter_mut().zip(a.iter().zip(wa)).zip(reversed) {
    *folded = *a * *wa - *b * *wb;
  }
}

/// the orthonormal DCT-IV of length N through a complex FFT of length N/2:
/// X_k = sqrt(2/N) sum_n u_n cos(pi/N (n + 1/2)(k + 1/2))
#[derive(Clone)]
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
  /// with z_n = u_2n + i u_(N-1-2n) and
  /// Y_k = sqrt(2/N) sum_n z_n exp(-i pi (2n + 1/2)(2k + 1/2) / N), X_2k is the real part of
  /// Y_k and X_(N-1-2k) minus its imaginary part.
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

/// a channel's samples split by the parity of their index, each half forward and reversed, so
/// that a frame's samples at every other index are contiguous upward and downward
pub(super) struct Strided {
  up: [Vec<f32>; 2],
  down: [Vec<f32>; 2],
}

impl Strided {
  pub(super) fn new(samples: &[f32]) -> Self {
    let up = [0, 1]
      .map(|parity| -> Vec<f32> { samples.iter().skip(parity).step_by(2).copied().collect() });
    let down = [0, 1].map(|parity| up[parity].iter().rev().copied().collect());
    Self { up, down }
  }

  /// `count` samples at every other index from `at` upward
  fn up(&self, at: usize, count: usize) -> &[f32] {
    &self.up[at % 2][at / 2..][..count]
  }

  /// `count` samples at every other index from `at` downward
  fn down(&self, at: usize, count: usize) -> &[f32] {
    let last = self.up[at % 2].len() - 1;
    &self.down[at % 2][last - at / 2..][..count]
  }
}

/// the MDCT of a windowed frame of a [`Strided`] channel, with the same arithmetic as
/// [`fold_windowed`] and [`DctIv::process`] and its lines in two halves instead of in order:
/// `even` holds X_2k and `odd` holds -X_(N-1-2k), for k from 0. every loop runs over
/// contiguous samples.
#[derive(Clone)]
pub(super) struct SplitMdct {
  dct: DctIv,
  /// the window at the samples of each run: the first half's c, d, a and b, then the second
  /// half's a, b, c and d
  windows: [Vec<f32>; 8],
  pre: [Vec<f32>; 2],
  post: [Vec<f32>; 2],
}

impl SplitMdct {
  /// for frames under `window`, 2N samples with N a multiple of four
  pub(super) fn new(window: &[f32]) -> Self {
    let q = window.len() / 4;
    let dct = DctIv::new(2 * q);
    let up = |from: usize| -> Vec<f32> { (0..q / 2).map(|n| window[from + 2 * n]).collect() };
    let down = |from: usize| -> Vec<f32> { (0..q / 2).map(|n| window[from - 2 * n]).collect() };
    Self {
      windows: [
        down(3 * q - 1),
        up(3 * q),
        down(q - 1),
        up(q),
        up(0),
        down(2 * q - 1),
        up(2 * q),
        down(4 * q - 1),
      ],
      pre: [
        dct.pre.iter().map(|c| c.re).collect(),
        dct.pre.iter().map(|c| c.im).collect(),
      ],
      post: [
        dct.post.iter().map(|c| c.re).collect(),
        dct.post.iter().map(|c| c.im).collect(),
      ],
      dct,
    }
  }

  /// the lines of the frame of `channel` from sample `start` into `even` and `odd`, N/2 each
  pub(super) fn process(
    &mut self,
    channel: &Strided,
    start: usize,
    even: &mut [f32],
    odd: &mut [f32],
  ) {
    let half = self.dct.buffer.len();
    let (q, m) = (half, half / 2);
    let [wc1, wd1, wa1, wb1, wa2, wb2, wc2, wd2] = &self.windows;
    let (first, second) = self.dct.buffer.split_at_mut(m);
    let (pre_re, pre_im) = (&self.pre[0][..half], &self.pre[1][..half]);
    // u_2n is -c_r w - d w and u_(N-1-2n) is a_r w - b w
    let (c, d) = (
      channel.down(start + 3 * q - 1, m),
      channel.up(start + 3 * q, m),
    );
    let (a, b) = (channel.down(start + q - 1, m), channel.up(start + q, m));
    for n in 0..m {
      let (x, y) = (
        -(c[n] * wc1[n]) - d[n] * wd1[n],
        a[n] * wa1[n] - b[n] * wb1[n],
      );
      first[n] = Complex::new(x * pre_re[n] - y * pre_im[n], x * pre_im[n] + y * pre_re[n]);
    }
    // and past the middle u_2n is a w - b_r w and u_(N-1-2n) is -c w - d_r w
    let (a, b) = (channel.up(start, m), channel.down(start + 2 * q - 1, m));
    let (c, d) = (
      channel.up(start + 2 * q, m),
      channel.down(start + 4 * q - 1, m),
    );
    let (pre_re, pre_im) = (&pre_re[m..], &pre_im[m..]);
    for n in 0..m {
      let (x, y) = (
        a[n] * wa2[n] - b[n] * wb2[n],
        -(c[n] * wc2[n]) - d[n] * wd2[n],
      );
      second[n] = Complex::new(x * pre_re[n] - y * pre_im[n], x * pre_im[n] + y * pre_re[n]);
    }
    self
      .dct
      .fft
      .process_with_scratch(&mut self.dct.buffer, &mut self.dct.scratch);
    let (post_re, post_im) = (&self.post[0][..half], &self.post[1][..half]);
    let (buffer, even, odd) = (
      &self.dct.buffer[..half],
      &mut even[..half],
      &mut odd[..half],
    );
    for k in 0..half {
      even[k] = buffer[k].re * post_re[k] - buffer[k].im * post_im[k];
      odd[k] = buffer[k].re * post_im[k] + buffer[k].im * post_re[k];
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

  #[test]
  fn the_split_mdct_is_the_fold_and_the_transform_to_the_last_bit() {
    let n = 64;
    let samples: Vec<f32> = (0..5 * n)
      .map(|i| ((i * i) as f32 * 0.37).sin() * 0.8)
      .collect();
    let window: Vec<f32> = (0..2 * n)
      .map(|i| (std::f32::consts::PI * (i as f32 + 0.5) / (2 * n) as f32).sin())
      .collect();
    let channel = Strided::new(&samples);
    let mut split = SplitMdct::new(&window);
    let mut dct = DctIv::new(n);
    let (mut folded, mut lines) = (vec![0.0f32; n], vec![0.0f32; n]);
    let (mut even, mut odd) = (vec![0.0f32; n / 2], vec![0.0f32; n / 2]);
    for start in [0, 1, 2, 77, 3 * n - 1, 3 * n] {
      fold_windowed(&samples[start..start + 2 * n], &window, &mut folded);
      dct.process(&folded, &mut lines);
      split.process(&channel, start, &mut even, &mut odd);
      for k in 0..n / 2 {
        assert_eq!(even[k].to_bits(), lines[2 * k].to_bits(), "{start} {k}");
        assert_eq!(
          odd[k].to_bits(),
          (-lines[n - 1 - 2 * k]).to_bits(),
          "{start} {k}"
        );
      }
    }
  }
}
