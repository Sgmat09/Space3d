//! Image comparison: per-channel tolerance, allowed fraction of differing pixels, SSIM, and
//! diff visualisation (differences in magenta over a dimmed copy of the expected image).

use serde::{Deserialize, Serialize};
use space3d_core::ImageData;

/// How strictly two images must match.
///
/// A pixel *differs* when any channel differs by more than `channel_tolerance`. The comparison
/// passes when sizes match, the fraction of differing pixels is at most `max_diff_fraction`
/// and — if `min_ssim` is set — the structural similarity is at least `min_ssim`. For a purely
/// perceptual check set `max_diff_fraction` to `1.0` and rely on `min_ssim`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompareOptions {
    /// Largest per-channel difference (0..=255) that still counts as equal.
    pub channel_tolerance: u8,
    /// Allowed fraction (0..=1) of differing pixels.
    pub max_diff_fraction: f64,
    /// Minimum mean SSIM (0..=1) over 8x8 luma windows, if set.
    pub min_ssim: Option<f64>,
    /// Ignore the alpha channel.
    pub ignore_alpha: bool,
}

impl Default for CompareOptions {
    /// Tolerance 2/255 per channel, at most 0.1 % differing pixels, no SSIM requirement.
    fn default() -> Self {
        Self {
            channel_tolerance: 2,
            max_diff_fraction: 0.001,
            min_ssim: None,
            ignore_alpha: false,
        }
    }
}

impl CompareOptions {
    /// Bit-exact comparison.
    pub fn exact() -> Self {
        Self {
            channel_tolerance: 0,
            max_diff_fraction: 0.0,
            min_ssim: None,
            ignore_alpha: false,
        }
    }

    /// Lenient preset for cross-GPU goldens: tolerance 8, 1 % pixels, SSIM ≥ 0.98.
    pub fn perceptual() -> Self {
        Self {
            channel_tolerance: 8,
            max_diff_fraction: 0.01,
            min_ssim: Some(0.98),
            ignore_alpha: true,
        }
    }

    /// Set the per-channel tolerance.
    pub fn channel_tolerance(mut self, t: u8) -> Self {
        self.channel_tolerance = t;
        self
    }

    /// Set the allowed fraction of differing pixels.
    pub fn max_diff_fraction(mut self, f: f64) -> Self {
        self.max_diff_fraction = f;
        self
    }

    /// Require a minimum SSIM.
    pub fn min_ssim(mut self, s: f64) -> Self {
        self.min_ssim = Some(s);
        self
    }

    /// Ignore alpha.
    pub fn ignore_alpha(mut self, ignore: bool) -> Self {
        self.ignore_alpha = ignore;
        self
    }
}

/// Result of [`compare_images`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffStats {
    /// Size of the expected image.
    pub expected_size: (u32, u32),
    /// Size of the actual image.
    pub actual_size: (u32, u32),
    /// Pixels compared (0 on size mismatch).
    pub total_pixels: u64,
    /// Pixels whose max channel difference exceeds the tolerance.
    pub differing_pixels: u64,
    /// `differing_pixels / total_pixels` (1.0 on size mismatch).
    pub diff_fraction: f64,
    /// Largest channel difference seen.
    pub max_channel_diff: u8,
    /// Mean absolute channel difference over all compared channels.
    pub mean_channel_diff: f64,
    /// Mean SSIM of the luma channel (`None` on size mismatch).
    pub ssim: Option<f64>,
    /// Whether the comparison passed under `options`.
    pub passed: bool,
    /// Options used.
    pub options: CompareOptions,
}

impl DiffStats {
    /// Both images have the same size.
    pub fn size_matches(&self) -> bool {
        self.expected_size == self.actual_size
    }

    /// Human-readable reasons the comparison failed (empty when it passed).
    pub fn failure_reasons(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.size_matches() {
            out.push(format!(
                "size mismatch: expected {}x{}, got {}x{}",
                self.expected_size.0, self.expected_size.1, self.actual_size.0, self.actual_size.1
            ));
            return out;
        }
        if self.diff_fraction > self.options.max_diff_fraction {
            out.push(format!(
                "{} of {} pixels differ ({:.3}%) > allowed {:.3}% (channel tolerance {})",
                self.differing_pixels,
                self.total_pixels,
                self.diff_fraction * 100.0,
                self.options.max_diff_fraction * 100.0,
                self.options.channel_tolerance
            ));
        }
        if let (Some(min), Some(s)) = (self.options.min_ssim, self.ssim)
            && s < min
        {
            out.push(format!("SSIM {s:.5} < required {min:.5}"));
        }
        out
    }

    /// One-line summary of the statistics.
    pub fn summary(&self) -> String {
        if !self.size_matches() {
            return self.failure_reasons().join("; ");
        }
        format!(
            "{}/{} pixels differ ({:.3}%), max channel diff {}, mean channel diff {:.3}, SSIM {}",
            self.differing_pixels,
            self.total_pixels,
            self.diff_fraction * 100.0,
            self.max_channel_diff,
            self.mean_channel_diff,
            self.ssim.map_or("n/a".to_string(), |s| format!("{s:.5}"))
        )
    }
}

fn channels(opts: &CompareOptions) -> usize {
    if opts.ignore_alpha { 3 } else { 4 }
}

/// Compare `actual` against `expected`.
pub fn compare_images(expected: &ImageData, actual: &ImageData, options: &CompareOptions) -> DiffStats {
    let expected_size = (expected.width, expected.height);
    let actual_size = (actual.width, actual.height);
    if expected_size != actual_size
        || expected.pixels.len() != actual.pixels.len()
        || expected.pixels.len() != expected.width as usize * expected.height as usize * 4
    {
        return DiffStats {
            expected_size,
            actual_size,
            total_pixels: 0,
            differing_pixels: 0,
            diff_fraction: 1.0,
            max_channel_diff: 255,
            mean_channel_diff: 255.0,
            ssim: None,
            passed: false,
            options: *options,
        };
    }
    let n = channels(options);
    let mut differing = 0u64;
    let mut max = 0u8;
    let mut sum = 0u64;
    for (p, q) in expected
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(actual.pixels.as_chunks::<4>().0)
    {
        let mut pixel_max = 0u8;
        for c in 0..n {
            let d = p[c].abs_diff(q[c]);
            sum += d as u64;
            pixel_max = pixel_max.max(d);
        }
        max = max.max(pixel_max);
        if pixel_max > options.channel_tolerance {
            differing += 1;
        }
    }
    let total = expected.width as u64 * expected.height as u64;
    let diff_fraction = if total == 0 {
        0.0
    } else {
        differing as f64 / total as f64
    };
    let ssim = ssim(expected, actual);
    let passed = diff_fraction <= options.max_diff_fraction && options.min_ssim.is_none_or(|m| ssim >= m);
    DiffStats {
        expected_size,
        actual_size,
        total_pixels: total,
        differing_pixels: differing,
        diff_fraction,
        max_channel_diff: max,
        mean_channel_diff: if total == 0 {
            0.0
        } else {
            sum as f64 / (total as f64 * n as f64)
        },
        ssim: Some(ssim),
        passed,
        options: *options,
    }
}

fn luma(p: &[u8; 4]) -> f64 {
    0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64
}

/// Mean structural similarity of the luma channels (8x8 windows, stride 4, standard
/// constants). 1.0 = identical; returns 0.0 for images of different sizes.
pub fn ssim(a: &ImageData, b: &ImageData) -> f64 {
    if (a.width, a.height) != (b.width, b.height) || a.pixels.len() != b.pixels.len() {
        return 0.0;
    }
    let (w, h) = (a.width as usize, a.height as usize);
    if w == 0 || h == 0 {
        return 1.0;
    }
    let la: Vec<f64> = a.pixels.as_chunks::<4>().0.iter().map(luma).collect();
    let lb: Vec<f64> = b.pixels.as_chunks::<4>().0.iter().map(luma).collect();
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let (ww, wh) = (w.min(8), h.min(8));
    let starts = |len: usize, win: usize| {
        let mut v: Vec<usize> = (0..=len - win).step_by(4).collect();
        if *v.last().unwrap() != len - win {
            v.push(len - win);
        }
        v
    };
    let (xs, ys) = (starts(w, ww), starts(h, wh));
    let n = (ww * wh) as f64;
    let mut total = 0.0;
    for &y0 in &ys {
        for &x0 in &xs {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for y in y0..y0 + wh {
                let row = y * w;
                for x in x0..x0 + ww {
                    let (p, q) = (la[row + x], lb[row + x]);
                    sa += p;
                    sb += q;
                    saa += p * p;
                    sbb += q * q;
                    sab += p * q;
                }
            }
            let (ma, mb) = (sa / n, sb / n);
            let va = (saa / n - ma * ma).max(0.0);
            let vb = (sbb / n - mb * mb).max(0.0);
            let cov = sab / n - ma * mb;
            total += ((2.0 * ma * mb + C1) * (2.0 * cov + C2)) / ((ma * ma + mb * mb + C1) * (va + vb + C2));
        }
    }
    total / (xs.len() * ys.len()) as f64
}

/// Magenta for pixels differing beyond `channel_tolerance` in any RGBA channel, a dimmed
/// grayscale copy of `expected` elsewhere. For size mismatches the canvas covers both images
/// and every pixel outside their overlap is magenta.
///
/// [`diff_image_with`] highlights exactly the pixels [`compare_images`] counts (it honors
/// [`CompareOptions::ignore_alpha`]).
pub fn diff_image(expected: &ImageData, actual: &ImageData, channel_tolerance: u8) -> ImageData {
    diff_pixels(expected, actual, channel_tolerance, 4)
}

/// [`diff_image`] using the comparison's settings: `channel_tolerance`, and alpha is ignored
/// when [`CompareOptions::ignore_alpha`] is set, so the diff highlights the same pixels that
/// [`compare_images`] counts as differing.
pub fn diff_image_with(expected: &ImageData, actual: &ImageData, options: &CompareOptions) -> ImageData {
    diff_pixels(expected, actual, options.channel_tolerance, channels(options))
}

fn diff_pixels(expected: &ImageData, actual: &ImageData, channel_tolerance: u8, channels: usize) -> ImageData {
    const MAGENTA: [u8; 4] = [255, 0, 255, 255];
    let (w, h) = (expected.width.max(actual.width), expected.height.max(actual.height));
    let mut out = ImageData::new(w, h);
    let inside = |img: &ImageData, x: u32, y: u32| x < img.width && y < img.height;
    for y in 0..h {
        for x in 0..w {
            let px = match (inside(expected, x, y), inside(actual, x, y)) {
                (true, true) => {
                    let (p, q) = (expected.get(x, y), actual.get(x, y));
                    let d = p[..channels]
                        .iter()
                        .zip(q[..channels].iter())
                        .map(|(a, b)| a.abs_diff(*b))
                        .max()
                        .unwrap_or(0);
                    if d > channel_tolerance {
                        MAGENTA
                    } else {
                        let g = (luma(&p) * 0.3 + 12.0) as u8;
                        [g, g, g, 255]
                    }
                }
                _ => MAGENTA,
            };
            out.set(x, y, px);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> ImageData {
        let mut img = ImageData::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.set(x, y, [(x * 255 / w) as u8, (y * 255 / h) as u8, 128, 255]);
            }
        }
        img
    }

    #[test]
    fn identical_images_pass_exactly() {
        let a = gradient(33, 17);
        let s = compare_images(&a, &a, &CompareOptions::exact());
        assert!(s.passed, "{}", s.summary());
        assert_eq!((s.differing_pixels, s.max_channel_diff), (0, 0));
        assert!((s.ssim.unwrap() - 1.0).abs() < 1e-12);
        assert!(s.failure_reasons().is_empty());
    }

    #[test]
    fn tolerance_and_fraction() {
        let a = gradient(10, 10);
        let mut b = a.clone();
        let p = b.get(1, 1);
        b.set(1, 1, [p[0].saturating_add(2), p[1], p[2], p[3]]);
        assert!(compare_images(&a, &b, &CompareOptions::default()).passed);
        assert!(!compare_images(&a, &b, &CompareOptions::exact()).passed);
        b.set(2, 2, [0, 255, 0, 255]);
        let s = compare_images(&a, &b, &CompareOptions::default());
        assert_eq!(s.differing_pixels, 1);
        assert!((s.diff_fraction - 0.01).abs() < 1e-12);
        assert!(!s.passed);
        assert!(
            s.failure_reasons()[0].contains("1 of 100 pixels"),
            "{:?}",
            s.failure_reasons()
        );
        assert!(compare_images(&a, &b, &CompareOptions::default().max_diff_fraction(0.02)).passed);
    }

    #[test]
    fn alpha_can_be_ignored() {
        let a = ImageData::filled(4, 4, [10, 20, 30, 255]);
        let b = ImageData::filled(4, 4, [10, 20, 30, 0]);
        assert!(!compare_images(&a, &b, &CompareOptions::default()).passed);
        assert!(compare_images(&a, &b, &CompareOptions::default().ignore_alpha(true)).passed);
        // The diff highlights exactly what the comparison counts.
        let magenta = |d: &ImageData| {
            d.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| **p == [255, 0, 255, 255])
                .count()
        };
        assert_eq!(magenta(&diff_image(&a, &b, 0)), 16);
        assert_eq!(magenta(&diff_image_with(&a, &b, &CompareOptions::default())), 16);
        assert_eq!(magenta(&diff_image_with(&a, &b, &CompareOptions::perceptual())), 0);
    }

    #[test]
    fn size_mismatch_fails_and_diff_covers_both() {
        let a = gradient(4, 4);
        let b = gradient(6, 3);
        let s = compare_images(&a, &b, &CompareOptions::default());
        assert!(!s.passed && !s.size_matches());
        assert!(s.failure_reasons()[0].contains("size mismatch"));
        let d = diff_image(&a, &b, 0);
        assert_eq!((d.width, d.height), (6, 4));
        assert_eq!(d.get(5, 0), [255, 0, 255, 255]);
        assert_eq!(d.get(0, 3), [255, 0, 255, 255]);
    }

    #[test]
    fn diff_image_highlights_only_changes() {
        let a = gradient(8, 8);
        let mut b = a.clone();
        b.set(3, 4, [255, 255, 255, 255]);
        let d = diff_image(&a, &b, 2);
        assert_eq!(d.get(3, 4), [255, 0, 255, 255]);
        let other = d.get(0, 0);
        assert_eq!(other[0], other[1]);
        assert!(other[0] < 100, "dimmed: {other:?}");
    }

    #[test]
    fn ssim_drops_with_structure_changes() {
        let a = gradient(64, 64);
        let noisy = {
            let mut b = a.clone();
            for (i, p) in b.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                if i % 3 == 0 {
                    p[0] = 255 - p[0];
                    p[1] = 255 - p[1];
                }
            }
            b
        };
        let s = ssim(&a, &noisy);
        assert!(s < 0.9, "ssim {s}");
        let opts = CompareOptions::default().max_diff_fraction(1.0).min_ssim(0.95);
        let stats = compare_images(&a, &noisy, &opts);
        assert!(!stats.passed);
        assert!(stats.failure_reasons()[0].contains("SSIM"));
        // Tiny images use a single window.
        let t = gradient(3, 2);
        assert!((ssim(&t, &t) - 1.0).abs() < 1e-12);
        assert_eq!(ssim(&t, &a), 0.0);
    }
}
