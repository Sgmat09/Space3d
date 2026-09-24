//! Resize, crop, flip and rotate.

use crate::par;
use space3d_core::ImageData;

/// Resampling filter for [`resize`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Nearest neighbour (pixel art, masks).
    Nearest,
    /// Bilinear interpolation on premultiplied alpha.
    #[default]
    Bilinear,
}

/// Resample to `width × height`.
pub fn resize(img: &ImageData, width: u32, height: u32, filter: Filter) -> ImageData {
    if width == 0 || height == 0 || img.width == 0 || img.height == 0 {
        return ImageData::new(width, height);
    }
    if (width, height) == (img.width, img.height) {
        return img.clone();
    }
    match filter {
        Filter::Nearest => resize_nearest(img, width, height),
        Filter::Bilinear => resize_bilinear(img, width, height),
    }
}

fn resize_nearest(img: &ImageData, w: u32, h: u32) -> ImageData {
    let (sw, sh) = (img.width as u64, img.height as u64);
    let xs: Vec<usize> = (0..w as u64)
        .map(|x| (((2 * x + 1) * sw) / (2 * w as u64)) as usize)
        .collect();
    let pixels = par::build_rows(h as usize, w as usize * 4, 0u8, |y, row| {
        let sy = (((2 * y as u64 + 1) * sh) / (2 * h as u64)) as usize;
        let src = &img.pixels[sy * sw as usize * 4..(sy + 1) * sw as usize * 4];
        for (x, &sx) in xs.iter().enumerate() {
            row[x * 4..x * 4 + 4].copy_from_slice(&src[sx * 4..sx * 4 + 4]);
        }
    });
    ImageData {
        width: w,
        height: h,
        pixels,
    }
}

/// Source sample positions for each destination coordinate: `(i0, i1, frac)`.
fn taps(dst: u32, src: u32) -> Vec<(usize, usize, f32)> {
    let scale = src as f32 / dst as f32;
    (0..dst)
        .map(|d| {
            let s = ((d as f32 + 0.5) * scale - 0.5).max(0.0);
            let i0 = (s as usize).min(src as usize - 1);
            let i1 = (i0 + 1).min(src as usize - 1);
            (i0, i1, (s - i0 as f32).clamp(0.0, 1.0))
        })
        .collect()
}

#[inline]
fn premul(p: &[u8]) -> [f32; 4] {
    let a = p[3] as f32;
    let k = a * (1.0 / 255.0);
    [p[0] as f32 * k, p[1] as f32 * k, p[2] as f32 * k, a]
}

#[inline]
pub(crate) fn unpremul(c: [f32; 4], out: &mut [u8]) {
    let a = c[3];
    if a <= 0.5 {
        out[..4].copy_from_slice(&[0, 0, 0, 0]);
        return;
    }
    let inv = 255.0 / a;
    for i in 0..3 {
        out[i] = (c[i] * inv + 0.5).clamp(0.0, 255.0) as u8;
    }
    out[3] = (a + 0.5).clamp(0.0, 255.0) as u8;
}

fn resize_bilinear(img: &ImageData, w: u32, h: u32) -> ImageData {
    let xt = taps(w, img.width);
    let yt = taps(h, img.height);
    let sw = img.width as usize;
    let pixels = par::build_rows(h as usize, w as usize * 4, 0u8, |y, row| {
        let (y0, y1, fy) = yt[y];
        let r0 = &img.pixels[y0 * sw * 4..(y0 + 1) * sw * 4];
        let r1 = &img.pixels[y1 * sw * 4..(y1 + 1) * sw * 4];
        for (x, &(x0, x1, fx)) in xt.iter().enumerate() {
            let a = premul(&r0[x0 * 4..]);
            let b = premul(&r0[x1 * 4..]);
            let c = premul(&r1[x0 * 4..]);
            let d = premul(&r1[x1 * 4..]);
            let mut o = [0.0f32; 4];
            for i in 0..4 {
                let top = a[i] + (b[i] - a[i]) * fx;
                let bot = c[i] + (d[i] - c[i]) * fx;
                o[i] = top + (bot - top) * fy;
            }
            unpremul(o, &mut row[x * 4..x * 4 + 4]);
        }
    });
    ImageData {
        width: w,
        height: h,
        pixels,
    }
}

/// Copy out a sub-rectangle (clamped to the image).
pub fn crop(img: &ImageData, x: u32, y: u32, width: u32, height: u32) -> ImageData {
    let x0 = x.min(img.width);
    let y0 = y.min(img.height);
    let w = width.min(img.width - x0);
    let h = height.min(img.height - y0);
    let pixels = super::Rect::new(x0, y0, w, h).extract(img);
    ImageData {
        width: w,
        height: h,
        pixels,
    }
}

/// Mirror left ↔ right in place.
pub fn flip_horizontal(img: &mut ImageData) {
    let w = img.width as usize;
    par::for_each_row(&mut img.pixels, w * 4, |_, row| {
        for x in 0..w / 2 {
            let (l, r) = row.split_at_mut((w - 1 - x) * 4);
            l[x * 4..x * 4 + 4].swap_with_slice(&mut r[..4]);
        }
    });
}

/// Mirror top ↔ bottom in place.
pub fn flip_vertical(img: &mut ImageData) {
    let stride = img.width as usize * 4;
    let h = img.height as usize;
    for y in 0..h / 2 {
        let (top, bottom) = img.pixels.split_at_mut((h - 1 - y) * stride);
        top[y * stride..(y + 1) * stride].swap_with_slice(&mut bottom[..stride]);
    }
}

fn remap(img: &ImageData, w: u32, h: u32, src_of: impl Fn(usize, usize) -> (usize, usize) + Sync + Send) -> ImageData {
    let sw = img.width as usize;
    let pixels = par::build_rows(h as usize, w as usize * 4, 0u8, |y, row| {
        for x in 0..w as usize {
            let (sx, sy) = src_of(x, y);
            let i = (sy * sw + sx) * 4;
            row[x * 4..x * 4 + 4].copy_from_slice(&img.pixels[i..i + 4]);
        }
    });
    ImageData {
        width: w,
        height: h,
        pixels,
    }
}

/// Rotate 90° clockwise (dimensions swap).
pub fn rotate90_cw(img: &ImageData) -> ImageData {
    let h = img.height as usize;
    remap(img, img.height, img.width, move |x, y| (y, h - 1 - x))
}

/// Rotate 90° counter-clockwise (dimensions swap).
pub fn rotate90_ccw(img: &ImageData) -> ImageData {
    let w = img.width as usize;
    remap(img, img.height, img.width, move |x, y| (w - 1 - y, x))
}

/// Rotate 180°.
pub fn rotate180(img: &ImageData) -> ImageData {
    let (w, h) = (img.width as usize, img.height as usize);
    remap(img, img.width, img.height, move |x, y| (w - 1 - x, h - 1 - y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::texture::white_noise;

    fn gradient(w: u32, h: u32) -> ImageData {
        let mut img = ImageData::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.set(x, y, [x as u8, y as u8, (x * y) as u8, 255]);
            }
        }
        img
    }

    #[test]
    fn resize_dimensions_and_constant_color() {
        let img = ImageData::filled(10, 7, [10, 20, 30, 255]);
        for f in [Filter::Nearest, Filter::Bilinear] {
            let r = resize(&img, 23, 4, f);
            assert_eq!((r.width, r.height), (23, 4));
            assert_eq!(r.pixels.len(), 23 * 4 * 4);
            assert!(r.pixels.chunks(4).all(|p| p == [10, 20, 30, 255]));
        }
        assert_eq!(resize(&img, 0, 5, Filter::Bilinear).pixels.len(), 0);
    }

    #[test]
    fn nearest_upscale_duplicates_pixels() {
        let img = gradient(2, 2);
        let r = resize(&img, 4, 4, Filter::Nearest);
        assert_eq!(r.get(0, 0), img.get(0, 0));
        assert_eq!(r.get(1, 1), img.get(0, 0));
        assert_eq!(r.get(3, 2), img.get(1, 1));
    }

    #[test]
    fn bilinear_ignores_color_of_transparent_pixels() {
        let mut img = ImageData::new(2, 1);
        img.set(0, 0, [255, 0, 0, 255]);
        img.set(1, 0, [0, 255, 0, 0]);
        let r = resize(&img, 4, 1, Filter::Bilinear);
        let mid = r.get(2, 0);
        assert_eq!(mid[1], 0, "no green bleeding from a fully transparent pixel");
        assert!(mid[3] > 0 && mid[3] < 255);
    }

    #[test]
    fn flips_twice_are_identity() {
        let img = white_noise(33, 17, 7);
        let mut a = img.clone();
        flip_horizontal(&mut a);
        assert_ne!(a, img);
        flip_horizontal(&mut a);
        assert_eq!(a, img);
        flip_vertical(&mut a);
        assert_ne!(a, img);
        flip_vertical(&mut a);
        assert_eq!(a, img);
        let big = white_noise(8, 200, 1);
        let mut b = big.clone();
        flip_horizontal(&mut b);
        flip_horizontal(&mut b);
        assert_eq!(b, big);
    }

    #[test]
    fn rotations() {
        let img = gradient(5, 3);
        let cw = rotate90_cw(&img);
        assert_eq!((cw.width, cw.height), (3, 5));
        // Top-left goes to top-right under a clockwise turn.
        assert_eq!(cw.get(2, 0), img.get(0, 0));
        assert_eq!(rotate90_ccw(&cw), img);
        assert_eq!(rotate90_cw(&rotate90_cw(&img)), rotate180(&img));
        assert_eq!(rotate180(&rotate180(&img)), img);
        let mut flipped = img.clone();
        flip_horizontal(&mut flipped);
        flip_vertical(&mut flipped);
        assert_eq!(flipped, rotate180(&img));
    }

    #[test]
    fn crop_clamps() {
        let img = gradient(5, 5);
        let c = crop(&img, 3, 1, 10, 2);
        assert_eq!((c.width, c.height), (2, 2));
        assert_eq!(c.get(0, 0), img.get(3, 1));
        assert_eq!(c.get(1, 1), img.get(4, 2));
        assert_eq!(crop(&img, 9, 9, 1, 1).pixels.len(), 0);
    }
}
