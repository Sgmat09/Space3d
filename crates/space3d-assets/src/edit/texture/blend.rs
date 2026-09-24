//! Layer blending / compositing.

use super::{to_u8, unorm};
use crate::par;
use space3d_core::ImageData;

/// How [`composite`] combines a source layer with the destination.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlendMode {
    /// Replace: `dst = lerp(dst, src, opacity)` on all four channels (source alpha is copied,
    /// not used for mixing).
    Normal,
    /// Porter-Duff source-over using the source alpha × opacity.
    #[default]
    AlphaOver,
    /// Multiply colors, then composite over using source alpha × opacity.
    Multiply,
    /// Add colors (clamped, "linear dodge"), then composite over using source alpha × opacity.
    Add,
}

/// Blend a single straight-alpha RGBA8 pixel. Separable modes follow the W3C compositing
/// model: `Cs' = (1 - αb)·Cs + αb·B(Cb, Cs)`, then source-over.
pub fn blend_pixel(dst: [u8; 4], src: [u8; 4], mode: BlendMode, opacity: f32) -> [u8; 4] {
    let o = opacity.clamp(0.0, 1.0);
    if mode == BlendMode::Normal {
        return std::array::from_fn(|i| to_u8(unorm(dst[i]) + (unorm(src[i]) - unorm(dst[i])) * o));
    }
    let sa = unorm(src[3]) * o;
    if sa <= 0.0 {
        return dst;
    }
    if sa >= 1.0 && mode == BlendMode::AlphaOver {
        return [src[0], src[1], src[2], 255];
    }
    let da = unorm(dst[3]);
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 {
        return [0; 4];
    }
    let mut out = [0u8; 4];
    for i in 0..3 {
        let cs = unorm(src[i]);
        let cb = unorm(dst[i]);
        let b = match mode {
            BlendMode::Multiply => cs * cb,
            BlendMode::Add => (cs + cb).min(1.0),
            BlendMode::AlphaOver | BlendMode::Normal => cs,
        };
        let cs2 = (1.0 - da) * cs + da * b;
        out[i] = to_u8((cs2 * sa + cb * da * (1.0 - sa)) / oa);
    }
    out[3] = to_u8(oa);
    out
}

/// Straight-alpha source-over of a flat color with extra coverage (used by painting).
#[inline]
pub(crate) fn over(dst: &mut [u8], color: [u8; 4], coverage: f32) {
    let p = blend_pixel([dst[0], dst[1], dst[2], dst[3]], color, BlendMode::AlphaOver, coverage);
    dst[..4].copy_from_slice(&p);
}

/// Composite `src` onto `dst` with its top-left at `(x, y)` (may be negative / partly
/// outside). Rows are processed in parallel on native.
pub fn composite(dst: &mut ImageData, src: &ImageData, x: i32, y: i32, mode: BlendMode, opacity: f32) {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + src.width as i32).min(dst.width as i32);
    let y1 = (y + src.height as i32).min(dst.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let sw = src.width as usize;
    par::for_each_row(&mut dst.pixels, dst.width as usize * 4, |dy, row| {
        let dy = dy as i32;
        if dy < y0 || dy >= y1 {
            return;
        }
        let sy = (dy - y) as usize;
        for dx in x0..x1 {
            let sx = (dx - x) as usize;
            let si = (sy * sw + sx) * 4;
            let s = [
                src.pixels[si],
                src.pixels[si + 1],
                src.pixels[si + 2],
                src.pixels[si + 3],
            ];
            let di = dx as usize * 4;
            let d = [row[di], row[di + 1], row[di + 2], row[di + 3]];
            row[di..di + 4].copy_from_slice(&blend_pixel(d, s, mode, opacity));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_modes() {
        let d = [200, 100, 50, 255];
        let s = [100, 100, 100, 255];
        assert_eq!(blend_pixel(d, s, BlendMode::Normal, 1.0), s);
        assert_eq!(blend_pixel(d, s, BlendMode::Normal, 0.0), d);
        assert_eq!(blend_pixel(d, s, BlendMode::AlphaOver, 1.0), s);
        assert_eq!(blend_pixel(d, [0, 0, 0, 0], BlendMode::AlphaOver, 1.0), d);
        assert_eq!(blend_pixel(d, s, BlendMode::Multiply, 1.0), [78, 39, 20, 255]);
        assert_eq!(blend_pixel(d, s, BlendMode::Add, 1.0), [255, 200, 150, 255]);
        let half = blend_pixel([0, 0, 0, 255], [255, 255, 255, 255], BlendMode::AlphaOver, 0.5);
        assert!((127..=128).contains(&half[0]));
        // Over a transparent destination the source color comes through unchanged.
        let t = blend_pixel([0, 0, 0, 0], [10, 20, 30, 128], BlendMode::Multiply, 1.0);
        assert_eq!(t, [10, 20, 30, 128]);
    }

    #[test]
    fn composite_clips_and_offsets() {
        let mut dst = ImageData::filled(4, 4, [0, 0, 0, 255]);
        let src = ImageData::filled(2, 2, [255, 0, 0, 255]);
        composite(&mut dst, &src, 3, -1, BlendMode::AlphaOver, 1.0);
        assert_eq!(dst.get(3, 0), [255, 0, 0, 255]);
        assert_eq!(dst.get(3, 1), [0, 0, 0, 255]);
        assert_eq!(dst.get(2, 0), [0, 0, 0, 255]);
        composite(&mut dst, &src, 10, 10, BlendMode::Add, 1.0);
        let big_src = ImageData::filled(300, 300, [0, 255, 0, 255]);
        let mut big = ImageData::filled(200, 200, [0, 0, 255, 255]);
        composite(&mut big, &big_src, -50, -50, BlendMode::Add, 1.0);
        assert_eq!(big.get(199, 199), [0, 255, 255, 255]);
    }
}
