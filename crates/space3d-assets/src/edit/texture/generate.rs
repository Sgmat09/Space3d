//! Procedural image generators (deterministic for a given seed).

use super::{to_u8, unorm};
use crate::par;
use space3d_core::ImageData;

fn build(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 4] + Sync + Send) -> ImageData {
    let pixels = par::build_rows(height as usize, width as usize * 4, 0u8, |y, row| {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = f(x as u32, y as u32);
        }
    });
    ImageData { width, height, pixels }
}

fn lerp_color(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    std::array::from_fn(|i| to_u8(unorm(a[i]) + (unorm(b[i]) - unorm(a[i])) * t))
}

/// Checkerboard of `cell`-sized squares starting with `a` at the top-left.
pub fn checker(width: u32, height: u32, cell: u32, a: [u8; 4], b: [u8; 4]) -> ImageData {
    let cell = cell.max(1);
    build(width, height, |x, y| {
        if ((x / cell) + (y / cell)).is_multiple_of(2) {
            a
        } else {
            b
        }
    })
}

/// Linear gradient from `from` to `to`. `angle_degrees` = 0 runs left → right, 90 runs
/// top → bottom.
pub fn linear_gradient(width: u32, height: u32, from: [u8; 4], to: [u8; 4], angle_degrees: f32) -> ImageData {
    let (s, c) = angle_degrees.to_radians().sin_cos();
    let (cx, cy) = ((width as f32 - 1.0) * 0.5, (height as f32 - 1.0) * 0.5);
    let extent = (cx * c).abs() + (cy * s).abs();
    build(width, height, |x, y| {
        let d = (x as f32 - cx) * c + (y as f32 - cy) * s;
        let t = if extent > 0.0 { (d / extent + 1.0) * 0.5 } else { 0.5 };
        lerp_color(from, to, t.clamp(0.0, 1.0))
    })
}

/// Radial gradient from `inner` at the centre to `outer` at the inscribed circle and beyond.
pub fn radial_gradient(width: u32, height: u32, inner: [u8; 4], outer: [u8; 4]) -> ImageData {
    let (cx, cy) = ((width as f32 - 1.0) * 0.5, (height as f32 - 1.0) * 0.5);
    let r = (width.min(height) as f32 * 0.5).max(f32::EPSILON);
    build(width, height, |x, y| {
        let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
        lerp_color(inner, outer, (d / r).clamp(0.0, 1.0))
    })
}

#[inline]
fn hash(x: i64, y: i64, seed: u64) -> u64 {
    // splitmix64 over the combined key
    let mut z = seed ^ (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[inline]
fn hash01(x: i64, y: i64, seed: u64) -> f32 {
    (hash(x, y, seed) >> 40) as f32 / (1u64 << 24) as f32
}

/// Opaque grayscale white noise.
pub fn white_noise(width: u32, height: u32, seed: u64) -> ImageData {
    build(width, height, |x, y| {
        let v = (hash(x as i64, y as i64, seed) >> 56) as u8;
        [v, v, v, 255]
    })
}

/// Opaque grayscale fractal value noise. `scale` is the size in pixels of the coarsest
/// lattice cell; each of `octaves` halves the cell size and the amplitude.
pub fn value_noise(width: u32, height: u32, seed: u64, scale: f32, octaves: u32) -> ImageData {
    let scale = scale.max(1.0);
    let octaves = octaves.clamp(1, 16);
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    let sample = |px: f32, py: f32, oct: u64| {
        let (x0, y0) = (px.floor(), py.floor());
        let (fx, fy) = (smooth(px - x0), smooth(py - y0));
        let (ix, iy) = (x0 as i64, y0 as i64);
        let s = seed.wrapping_add(oct.wrapping_mul(0x632B_E59B_D9B4_E019));
        let a = hash01(ix, iy, s);
        let b = hash01(ix + 1, iy, s);
        let c = hash01(ix, iy + 1, s);
        let d = hash01(ix + 1, iy + 1, s);
        let top = a + (b - a) * fx;
        let bot = c + (d - c) * fx;
        top + (bot - top) * fy
    };
    build(width, height, |x, y| {
        let (mut amp, mut freq, mut sum, mut norm) = (1.0f32, 1.0 / scale, 0.0f32, 0.0f32);
        for o in 0..octaves {
            sum += sample(x as f32 * freq, y as f32 * freq, o as u64) * amp;
            norm += amp;
            amp *= 0.5;
            freq *= 2.0;
        }
        let v = to_u8(sum / norm);
        [v, v, v, 255]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_are_deterministic_and_sized() {
        assert_eq!(white_noise(9, 5, 1), white_noise(9, 5, 1));
        assert_ne!(white_noise(9, 5, 1), white_noise(9, 5, 2));
        let v = value_noise(32, 16, 3, 8.0, 4);
        assert_eq!((v.width, v.height), (32, 16));
        assert_eq!(v, value_noise(32, 16, 3, 8.0, 4));
        let vals: Vec<u8> = v.pixels.chunks(4).map(|p| p[0]).collect();
        let (min, max) = (vals.iter().min().unwrap(), vals.iter().max().unwrap());
        assert!(max - min > 30, "noise should vary");
        let c = checker(4, 4, 2, [1; 4], [2; 4]);
        assert_eq!((c.get(0, 0), c.get(2, 0), c.get(2, 2)), ([1; 4], [2; 4], [1; 4]));
        assert_eq!(c, ImageData::checker(4, 2, [1; 4], [2; 4]));
    }

    #[test]
    fn gradients_hit_endpoints() {
        let g = linear_gradient(11, 3, [0, 0, 0, 255], [255, 255, 255, 255], 0.0);
        assert_eq!(g.get(0, 1), [0, 0, 0, 255]);
        assert_eq!(g.get(10, 1), [255; 4]);
        assert!((120..=135).contains(&g.get(5, 0)[0]));
        let v = linear_gradient(3, 11, [0; 4], [255; 4], 90.0);
        assert_eq!(v.get(1, 0), [0; 4]);
        assert_eq!(v.get(1, 10), [255; 4]);
        let r = radial_gradient(9, 9, [255; 4], [0; 4]);
        assert_eq!(r.get(4, 4), [255; 4]);
        assert_eq!(r.get(0, 0), [0; 4]);
    }
}
