//! Per-pixel color adjustments (in place, alpha preserved).

use super::{to_u8, unorm};
use crate::par;
use space3d_core::ImageData;

fn map_pixels(img: &mut ImageData, f: impl Fn(&mut [u8]) + Sync + Send) {
    let stride = img.width as usize * 4;
    par::for_each_row(&mut img.pixels, stride, |_, row| {
        for px in row.as_chunks_mut::<4>().0 {
            f(px);
        }
    });
}

fn apply_lut(img: &mut ImageData, lut: &[u8; 256]) {
    map_pixels(img, |px| {
        px[0] = lut[px[0] as usize];
        px[1] = lut[px[1] as usize];
        px[2] = lut[px[2] as usize];
    });
}

/// Adjust brightness (additive, `-1..1`) and contrast (`-1..1`, 0 = unchanged) of RGB.
pub fn brightness_contrast(img: &mut ImageData, brightness: f32, contrast: f32) {
    let c = contrast.clamp(-1.0, 0.999);
    let factor = (1.0 + c) / (1.0 - c);
    let lut: [u8; 256] = std::array::from_fn(|i| to_u8((i as f32 / 255.0 - 0.5) * factor + 0.5 + brightness));
    apply_lut(img, &lut);
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    let d = max - min;
    if d <= f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s <= 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
}

/// Shift hue by `hue_degrees`, scale saturation by `saturation` (1 = unchanged, 0 = gray)
/// and add `lightness` (`-1..1`) in HSL space.
pub fn hue_saturation(img: &mut ImageData, hue_degrees: f32, saturation: f32, lightness: f32) {
    let dh = hue_degrees / 360.0;
    let sat = saturation.max(0.0);
    map_pixels(img, |px| {
        let (h, s, l) = rgb_to_hsl(unorm(px[0]), unorm(px[1]), unorm(px[2]));
        let (r, g, b) = hsl_to_rgb(h + dh, (s * sat).clamp(0.0, 1.0), (l + lightness).clamp(0.0, 1.0));
        px[0] = to_u8(r);
        px[1] = to_u8(g);
        px[2] = to_u8(b);
    });
}

/// Convert RGB to Rec. 709 luma.
pub fn grayscale(img: &mut ImageData) {
    map_pixels(img, |px| {
        let y = (0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32 + 0.5) as u8;
        px[0] = y;
        px[1] = y;
        px[2] = y;
    });
}

/// Invert RGB.
pub fn invert(img: &mut ImageData) {
    map_pixels(img, |px| {
        px[0] = 255 - px[0];
        px[1] = 255 - px[1];
        px[2] = 255 - px[2];
    });
}

/// Multiply RGB by `color`, blended in by `strength` (0..1).
pub fn tint(img: &mut ImageData, color: [u8; 4], strength: f32) {
    let s = strength.clamp(0.0, 1.0);
    let luts: [[u8; 256]; 3] = std::array::from_fn(|c| {
        std::array::from_fn(|i| {
            let v = i as f32 / 255.0;
            to_u8(v + (v * unorm(color[c]) - v) * s)
        })
    });
    map_pixels(img, |px| {
        px[0] = luts[0][px[0] as usize];
        px[1] = luts[1][px[1] as usize];
        px[2] = luts[2][px[2] as usize];
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::texture::white_noise;

    #[test]
    fn invert_twice_is_identity() {
        let img = white_noise(20, 70, 3);
        let mut a = img.clone();
        invert(&mut a);
        assert_eq!(a.get(0, 0)[0], 255 - img.get(0, 0)[0]);
        invert(&mut a);
        assert_eq!(a, img);
    }

    #[test]
    fn neutral_adjustments_are_identity() {
        let img = white_noise(16, 16, 9);
        let mut a = img.clone();
        brightness_contrast(&mut a, 0.0, 0.0);
        assert_eq!(a, img);
        hue_saturation(&mut a, 0.0, 1.0, 0.0);
        let max_diff = a
            .pixels
            .iter()
            .zip(&img.pixels)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap();
        assert!(max_diff <= 1, "HSL roundtrip error {max_diff}");
        let mut t = img.clone();
        tint(&mut t, [255; 4], 1.0);
        assert_eq!(t, img);
    }

    #[test]
    fn grayscale_and_hue() {
        let mut img = ImageData::filled(2, 2, [255, 0, 0, 128]);
        let mut g = img.clone();
        grayscale(&mut g);
        let p = g.get(1, 1);
        assert_eq!((p[0], p[1], p[2], p[3]), (54, 54, 54, 128));
        hue_saturation(&mut img, 120.0, 1.0, 0.0);
        assert_eq!(img.get(0, 0), [0, 255, 0, 128]);
        hue_saturation(&mut img, 0.0, 0.0, 0.0);
        let p = img.get(0, 0);
        assert!(p[0] == p[1] && p[1] == p[2]);
    }

    #[test]
    fn brightness_and_contrast_move_values() {
        let mut img = ImageData::filled(1, 1, [100, 100, 100, 255]);
        brightness_contrast(&mut img, 0.2, 0.0);
        assert!(img.get(0, 0)[0] > 140);
        let mut c = ImageData::filled(1, 1, [200, 50, 128, 255]);
        brightness_contrast(&mut c, 0.0, 0.5);
        let p = c.get(0, 0);
        assert!(p[0] > 200 && p[1] < 50);
        let mut t = ImageData::filled(1, 1, [200, 200, 200, 255]);
        tint(&mut t, [255, 0, 0, 255], 1.0);
        assert_eq!(t.get(0, 0), [200, 0, 0, 255]);
    }
}
