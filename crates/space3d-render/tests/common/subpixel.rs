//! Sub-pixel image registration for the precision tests: how far did the content of a patch move
//! between two captured frames?
//!
//! The method (documented in `docs/graphics/frame.md`, "Sky precision"):
//!
//! 1. Both frames become luminance grids (`f64`, the encoded byte value / 255), low-passed by a
//!    Gaussian (σ ≈ 1.2 px; linear and shift-invariant, so the shift is unchanged). Without it
//!    the interpolation and gradient kernels of step 3 are inaccurate on content near the pixel
//!    grid's Nyquist frequency and the estimate is biased by tens of percent for shifts of a
//!    few hundredths of a pixel.
//! 2. **Integer estimate**: phase correlation of the Hann-windowed, mean-removed patch of each
//!    frame (a 2D FFT, the cross-power spectrum normalised to unit magnitude, the inverse FFT;
//!    its peak is the shift). Robust to any amount of shift up to half the patch.
//! 3. **Sub-pixel refinement**: Gauss-Newton on the sum of squared differences
//!    `E(d) = Σ (B(p) − A(p − d))²` over the patch with `A` resampled by a Catmull-Rom
//!    (bicubic) kernel, whose pixel-locking bias is far below bilinear's. Converges in a few
//!    iterations from the phase-correlation start; at 8-bit output the residual noise is
//!    ~0.01 px on textures with detail at the pixel scale.
//!
//! The result is the translation `d` with `B(p) ≈ A(p − d)`: content moved by `+d` (x right,
//! y down) from `A` to `B`.
#![allow(dead_code)]

use space3d_core::ImageData;

/// A luminance grid.
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub v: Vec<f64>,
}

impl Gray {
    /// Rec. 601 luma of the encoded bytes, 0..1.
    pub fn from_image(img: &ImageData) -> Self {
        let (w, h) = (img.width as usize, img.height as usize);
        let v = img
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| (0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64) / 255.0)
            .collect();
        Self { w, h, v }
    }

    /// The grid convolved with a Gaussian of standard deviation `sigma` pixels (edges clamped).
    pub fn blurred(&self, sigma: f64) -> Self {
        let radius = (sigma * 3.0).ceil() as isize;
        let kernel: Vec<f64> = (-radius..=radius)
            .map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp())
            .collect();
        let norm: f64 = kernel.iter().sum();
        let kernel: Vec<f64> = kernel.iter().map(|k| k / norm).collect();
        let mut tmp = vec![0.0; self.v.len()];
        for y in 0..self.h as isize {
            for x in 0..self.w as isize {
                tmp[y as usize * self.w + x as usize] = kernel
                    .iter()
                    .enumerate()
                    .map(|(k, w)| w * self.at(x + k as isize - radius, y))
                    .sum();
            }
        }
        let horizontal = Self {
            w: self.w,
            h: self.h,
            v: tmp,
        };
        let mut out = vec![0.0; self.v.len()];
        for y in 0..self.h as isize {
            for x in 0..self.w as isize {
                out[y as usize * self.w + x as usize] = kernel
                    .iter()
                    .enumerate()
                    .map(|(k, w)| w * horizontal.at(x, y + k as isize - radius))
                    .sum();
            }
        }
        Self {
            w: self.w,
            h: self.h,
            v: out,
        }
    }

    fn at(&self, x: isize, y: isize) -> f64 {
        let x = x.clamp(0, self.w as isize - 1) as usize;
        let y = y.clamp(0, self.h as isize - 1) as usize;
        self.v[y * self.w + x]
    }

    /// Catmull-Rom (bicubic) sample at continuous pixel coordinates (pixel centres at integers).
    pub fn cubic(&self, x: f64, y: f64) -> f64 {
        let (fx, fy) = (x.floor(), y.floor());
        let (wx, wy) = (catmull_rom(x - fx), catmull_rom(y - fy));
        let (ix, iy) = (fx as isize, fy as isize);
        let mut sum = 0.0;
        for (j, wj) in wy.iter().enumerate() {
            let mut row = 0.0;
            for (i, wi) in wx.iter().enumerate() {
                row += wi * self.at(ix + i as isize - 1, iy + j as isize - 1);
            }
            sum += wj * row;
        }
        sum
    }
}

/// Catmull-Rom weights of the taps at −1, 0, 1, 2 for a fractional position `t` in 0..1.
fn catmull_rom(t: f64) -> [f64; 4] {
    let (t2, t3) = (t * t, t * t * t);
    [
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    ]
}

/// A square patch (`size` a power of two) whose top-left pixel is `(x, y)`.
#[derive(Clone, Copy, Debug)]
pub struct Patch {
    pub x: usize,
    pub y: usize,
    pub size: usize,
}

impl Patch {
    pub fn center(&self) -> (f64, f64) {
        (
            self.x as f64 + (self.size as f64 - 1.0) / 2.0,
            self.y as f64 + (self.size as f64 - 1.0) / 2.0,
        )
    }
}

/// In-place radix-2 FFT of `re`/`im` (`inverse`: the unnormalised inverse transform).
fn fft(re: &mut [f64], im: &mut [f64], inverse: bool) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = 2.0 * std::f64::consts::PI / len as f64 * if inverse { 1.0 } else { -1.0 };
        let (wr, wi) = (angle.cos(), angle.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let next = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = next;
            }
        }
        len <<= 1;
    }
}

/// 2D FFT of an `n × n` grid (rows, then columns).
fn fft2(re: &mut [f64], im: &mut [f64], n: usize, inverse: bool) {
    for row in 0..n {
        fft(&mut re[row * n..][..n], &mut im[row * n..][..n], inverse);
    }
    let (mut cr, mut ci) = (vec![0.0; n], vec![0.0; n]);
    for col in 0..n {
        for row in 0..n {
            cr[row] = re[row * n + col];
            ci[row] = im[row * n + col];
        }
        fft(&mut cr, &mut ci, inverse);
        for row in 0..n {
            re[row * n + col] = cr[row];
            im[row * n + col] = ci[row];
        }
    }
}

/// Phase correlation of the patch of `a` and of `b`: the integer shift `(dx, dy)` with
/// `b(p) ≈ a(p − d)`.
fn phase_correlate(a: &Gray, b: &Gray, patch: Patch) -> (i64, i64) {
    let n = patch.size;
    let load = |g: &Gray| {
        let mut v = vec![0.0; n * n];
        let mut sum = 0.0;
        for y in 0..n {
            for x in 0..n {
                let s = g.at((patch.x + x) as isize, (patch.y + y) as isize);
                v[y * n + x] = s;
                sum += s;
            }
        }
        let mean = sum / (n * n) as f64;
        let hann = |i: usize| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * (i as f64 + 0.5) / n as f64).cos();
        for y in 0..n {
            for x in 0..n {
                v[y * n + x] = (v[y * n + x] - mean) * hann(x) * hann(y);
            }
        }
        v
    };
    let (mut ar, mut ai) = (load(a), vec![0.0; n * n]);
    let (mut br, mut bi) = (load(b), vec![0.0; n * n]);
    fft2(&mut ar, &mut ai, n, false);
    fft2(&mut br, &mut bi, n, false);
    // B · conj(A), normalised to unit magnitude.
    for k in 0..n * n {
        let (re, im) = (br[k] * ar[k] + bi[k] * ai[k], bi[k] * ar[k] - br[k] * ai[k]);
        let m = (re * re + im * im).sqrt().max(1e-12);
        ar[k] = re / m;
        ai[k] = im / m;
    }
    fft2(&mut ar, &mut ai, n, true);
    let peak = (0..n * n)
        .max_by(|&i, &j| ar[i].partial_cmp(&ar[j]).unwrap())
        .unwrap_or(0);
    let wrap = |i: usize| if i < n / 2 { i as i64 } else { i as i64 - n as i64 };
    (wrap(peak % n), wrap(peak / n))
}

/// The translation `d = (dx, dy)` (pixels) of the content of `patch` from `a` to `b`, with
/// `b(p) ≈ a(p − d)`. `stride` subsamples the pixels of the refinement (1: every pixel).
pub fn measure_shift(a: &Gray, b: &Gray, patch: Patch, stride: usize) -> (f64, f64) {
    assert_eq!((a.w, a.h), (b.w, b.h));
    let (ix, iy) = phase_correlate(a, b, patch);
    let mut d = (ix as f64, iy as f64);
    let stride = stride.max(1);
    for _ in 0..24 {
        let (mut jxx, mut jxy, mut jyy, mut gx, mut gy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for y in (patch.y..patch.y + patch.size).step_by(stride) {
            for x in (patch.x..patch.x + patch.size).step_by(stride) {
                let (sx, sy) = (x as f64 - d.0, y as f64 - d.1);
                let e = b.v[y * b.w + x] - a.cubic(sx, sy);
                // ∂e/∂d = ∇A(p − d), by central differences of the cubic.
                const H: f64 = 0.25;
                let ax = (a.cubic(sx + H, sy) - a.cubic(sx - H, sy)) / (2.0 * H);
                let ay = (a.cubic(sx, sy + H) - a.cubic(sx, sy - H)) / (2.0 * H);
                jxx += ax * ax;
                jxy += ax * ay;
                jyy += ay * ay;
                gx += ax * e;
                gy += ay * e;
            }
        }
        let det = jxx * jyy - jxy * jxy;
        if det.abs() < 1e-12 {
            break;
        }
        // Solve J Δ = −g with J = JᵀJ.
        let step = (-(jyy * gx - jxy * gy) / det, -(-jxy * gx + jxx * gy) / det);
        d.0 += step.0;
        d.1 += step.1;
        if step.0.abs() < 1e-5 && step.1.abs() < 1e-5 {
            break;
        }
    }
    d
}

/// Standard deviation of the luminance of `patch` (0..1): is there texture to register?
pub fn contrast(g: &Gray, patch: Patch) -> f64 {
    let n = (patch.size * patch.size) as f64;
    let (mut s, mut s2) = (0.0, 0.0);
    for y in patch.y..patch.y + patch.size {
        for x in patch.x..patch.x + patch.size {
            let v = g.v[y * g.w + x];
            s += v;
            s2 += v * v;
        }
    }
    (s2 / n - (s / n) * (s / n)).max(0.0).sqrt()
}
