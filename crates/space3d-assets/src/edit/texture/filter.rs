//! Separable blurs (premultiplied alpha, clamp-to-edge) and normal-map generation.

use super::geometry::unpremul;
use super::{to_u8, unorm};
use crate::par;
use space3d_core::ImageData;

type Px = [f32; 4];

/// Blur kind and size for [`blur`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Blur {
    /// Box filter of `2 * radius + 1` pixels (two separable passes, O(1) per pixel).
    Box {
        /// Radius in pixels.
        radius: u32,
    },
    /// Gaussian with standard deviation `sigma` (kernel radius `ceil(3σ)`).
    Gaussian {
        /// Standard deviation in pixels.
        sigma: f32,
    },
}

/// Blur an image.
pub fn blur(img: &ImageData, kind: Blur) -> ImageData {
    match kind {
        Blur::Box { radius } => box_blur(img, radius),
        Blur::Gaussian { sigma } => gaussian_blur(img, sigma),
    }
}

fn to_premul(img: &ImageData) -> Vec<Px> {
    let w = img.width as usize;
    par::build_rows(img.height as usize, w, [0.0; 4], |y, row| {
        let src = &img.pixels[y * w * 4..(y + 1) * w * 4];
        for (o, p) in row.iter_mut().zip(src.as_chunks::<4>().0) {
            let a = p[3] as f32;
            let k = a * (1.0 / 255.0);
            *o = [p[0] as f32 * k, p[1] as f32 * k, p[2] as f32 * k, a];
        }
    })
}

fn from_premul(buf: &[Px], w: u32, h: u32) -> ImageData {
    let wu = w as usize;
    let pixels = par::build_rows(h as usize, wu * 4, 0u8, |y, row| {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            unpremul(buf[y * wu + x], px);
        }
    });
    ImageData {
        width: w,
        height: h,
        pixels,
    }
}

/// `w × h` → `h × w`.
fn transpose(buf: &[Px], w: usize, h: usize) -> Vec<Px> {
    par::build_rows(w, h, [0.0; 4], |x, row| {
        for (y, o) in row.iter_mut().enumerate() {
            *o = buf[y * w + x];
        }
    })
}

fn box_rows(src: &[Px], w: usize, h: usize, r: usize) -> Vec<Px> {
    let inv = 1.0 / (2 * r + 1) as f32;
    par::build_rows(h, w, [0.0; 4], |y, out| {
        let row = &src[y * w..(y + 1) * w];
        let at = |i: isize| row[i.clamp(0, w as isize - 1) as usize];
        let mut sum = [0.0f32; 4];
        for k in -(r as isize)..=(r as isize) {
            let p = at(k);
            for c in 0..4 {
                sum[c] += p[c];
            }
        }
        for (x, o) in out.iter_mut().enumerate() {
            *o = [sum[0] * inv, sum[1] * inv, sum[2] * inv, sum[3] * inv];
            let add = at(x as isize + r as isize + 1);
            let sub = at(x as isize - r as isize);
            for c in 0..4 {
                sum[c] += add[c] - sub[c];
            }
        }
    })
}

fn convolve_rows(src: &[Px], w: usize, h: usize, kernel: &[f32]) -> Vec<Px> {
    let r = kernel.len() / 2;
    par::build_rows(h, w, [0.0; 4], |y, out| {
        let row = &src[y * w..(y + 1) * w];
        let mut padded = Vec::with_capacity(w + 2 * r);
        padded.extend(std::iter::repeat_n(row[0], r));
        padded.extend_from_slice(row);
        padded.extend(std::iter::repeat_n(row[w - 1], r));
        for (x, o) in out.iter_mut().enumerate() {
            let mut acc = [0.0f32; 4];
            for (p, &k) in padded[x..x + kernel.len()].iter().zip(kernel) {
                acc[0] += p[0] * k;
                acc[1] += p[1] * k;
                acc[2] += p[2] * k;
                acc[3] += p[3] * k;
            }
            *o = acc;
        }
    })
}

fn separable(img: &ImageData, pass: impl Fn(&[Px], usize, usize) -> Vec<Px>) -> ImageData {
    let (w, h) = (img.width as usize, img.height as usize);
    let p = to_premul(img);
    let horizontal = pass(&p, w, h);
    let t = transpose(&horizontal, w, h);
    let vertical = pass(&t, h, w);
    let back = transpose(&vertical, h, w);
    from_premul(&back, img.width, img.height)
}

/// Box blur with the given radius (edges clamp).
pub fn box_blur(img: &ImageData, radius: u32) -> ImageData {
    if radius == 0 || img.width == 0 || img.height == 0 {
        return img.clone();
    }
    separable(img, |buf, w, h| box_rows(buf, w, h, radius as usize))
}

/// Gaussian blur with standard deviation `sigma` pixels (edges clamp).
pub fn gaussian_blur(img: &ImageData, sigma: f32) -> ImageData {
    if sigma <= 0.0 || img.width == 0 || img.height == 0 {
        return img.clone();
    }
    let r = (sigma * 3.0).ceil().max(1.0) as i32;
    let mut kernel: Vec<f32> = (-r..=r)
        .map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = kernel.iter().sum();
    kernel.iter_mut().for_each(|k| *k /= sum);
    separable(img, |buf, w, h| convolve_rows(buf, w, h, &kernel))
}

/// Build a tangent-space normal map (OpenGL / glTF convention: +Y up, green = up) from a
/// height map (Rec. 709 luma, 0..1). `strength` scales slopes: a height change of 1.0
/// across one pixel yields a slope of `strength`. Alpha is 255.
pub fn normal_map_from_height(img: &ImageData, strength: f32) -> ImageData {
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 {
        return img.clone();
    }
    let heights: Vec<f32> = img
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * unorm(p[0]) + 0.7152 * unorm(p[1]) + 0.0722 * unorm(p[2]))
        .collect();
    let at =
        |x: isize, y: isize| heights[y.clamp(0, h as isize - 1) as usize * w + x.clamp(0, w as isize - 1) as usize];
    let pixels = par::build_rows(h, w * 4, 0u8, |y, row| {
        let y = y as isize;
        for x in 0..w as isize {
            let gx = (at(x + 1, y - 1) + 2.0 * at(x + 1, y) + at(x + 1, y + 1))
                - (at(x - 1, y - 1) + 2.0 * at(x - 1, y) + at(x - 1, y + 1));
            let gy = (at(x - 1, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1))
                - (at(x - 1, y - 1) + 2.0 * at(x, y - 1) + at(x + 1, y - 1));
            // Sobel spans two pixels with weights summing to 4 → divide by 8 for a per-pixel slope.
            let n = glam::Vec3::new(-gx / 8.0 * strength, gy / 8.0 * strength, 1.0).normalize();
            let o = x as usize * 4;
            row[o] = to_u8(n.x * 0.5 + 0.5);
            row[o + 1] = to_u8(n.y * 0.5 + 0.5);
            row[o + 2] = to_u8(n.z * 0.5 + 0.5);
            row[o + 3] = 255;
        }
    });
    ImageData {
        width: img.width,
        height: img.height,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::texture::{checker, white_noise};

    #[test]
    fn blur_preserves_constant_images() {
        let img = ImageData::filled(40, 90, [12, 200, 77, 255]);
        assert_eq!(box_blur(&img, 3), img);
        assert_eq!(gaussian_blur(&img, 2.0), img);
        assert_eq!(blur(&img, Blur::Box { radius: 0 }), img);
    }

    #[test]
    fn blur_smooths_and_preserves_mean() {
        let img = checker(64, 64, 1, [255, 255, 255, 255], [0, 0, 0, 255]);
        for b in [box_blur(&img, 2), gaussian_blur(&img, 1.5)] {
            assert_eq!((b.width, b.height), (64, 64));
            let p = b.get(30, 30)[0];
            assert!((110..=145).contains(&p), "checker should blur toward gray, got {p}");
            let mean_in: f64 = img.pixels.iter().map(|&v| v as f64).sum::<f64>();
            let mean_out: f64 = b.pixels.iter().map(|&v| v as f64).sum::<f64>();
            assert!((mean_in - mean_out).abs() / mean_in < 0.01);
        }
        let noise = white_noise(128, 128, 5);
        let var = |i: &ImageData| {
            let red: Vec<f64> = i.pixels.chunks(4).map(|p| p[0] as f64).collect();
            let m = red.iter().sum::<f64>() / red.len() as f64;
            red.iter().map(|v| (v - m).powi(2)).sum::<f64>()
        };
        assert!(var(&gaussian_blur(&noise, 2.0)) < var(&noise) * 0.2);
    }

    #[test]
    fn blur_does_not_bleed_transparent_color() {
        let mut img = ImageData::filled(8, 8, [255, 0, 0, 0]);
        img.set(4, 4, [0, 0, 255, 255]);
        let b = box_blur(&img, 1);
        let p = b.get(3, 4);
        assert_eq!(p[0], 0);
        assert_eq!(p[2], 255);
        assert!(p[3] > 0);
    }

    #[test]
    fn normal_map_of_flat_and_slope() {
        let flat = ImageData::filled(4, 4, [128, 128, 128, 255]);
        let n = normal_map_from_height(&flat, 1.0);
        assert!(n.pixels.chunks(4).all(|p| p == [128, 128, 255, 255]));
        // Height increases to the right → normal tilts left (red < 128).
        let mut ramp = ImageData::new(8, 8);
        for y in 0..8 {
            for x in 0..8 {
                let v = (x * 30) as u8;
                ramp.set(x, y, [v, v, v, 255]);
            }
        }
        let n = normal_map_from_height(&ramp, 4.0);
        let p = n.get(4, 4);
        assert!(p[0] < 100 && (126..=130).contains(&p[1]));
    }
}
