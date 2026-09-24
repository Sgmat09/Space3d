//! Texture editing operations on [`ImageData`] (RGBA8, straight alpha).
//!
//! All functions are pure with respect to their inputs: they either mutate the image passed
//! as `&mut` or return a new one. Heavy per-row loops run on rayon when compiled natively
//! and the image has at least [`crate::PAR_ROW_THRESHOLD`] rows.
//!
//! Coordinates: pixel `(x, y)` has its centre at integer coordinates, `y` grows downward.

mod blend;
mod color;
mod filter;
mod generate;
mod geometry;
mod history;
mod io;
mod paint;

pub use blend::{BlendMode, blend_pixel, composite};
pub use color::{brightness_contrast, grayscale, hue_saturation, invert, tint};
pub use filter::{Blur, blur, box_blur, gaussian_blur, normal_map_from_height};
pub use generate::{checker, linear_gradient, radial_gradient, value_noise, white_noise};
pub use geometry::{Filter, crop, flip_horizontal, flip_vertical, resize, rotate90_ccw, rotate90_cw, rotate180};
pub use history::ImageEditHistory;
pub use io::{decode_image, decode_image_with_hint, encode_png};
pub use paint::{Brush, brush_dab, brush_stroke, draw_line, draw_rect, fill, flood_fill};

use space3d_core::ImageData;

/// Axis-aligned pixel rectangle (`x..x+width`, `y..y+height`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    /// Left column.
    pub x: u32,
    /// Top row.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// The empty rectangle.
    pub const EMPTY: Self = Self {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    /// Construct from position and size.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// The whole image.
    pub fn full(img: &ImageData) -> Self {
        Self::new(0, 0, img.width, img.height)
    }

    /// Zero area?
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Exclusive right edge.
    pub fn right(&self) -> u32 {
        self.x + self.width
    }

    /// Exclusive bottom edge.
    pub fn bottom(&self) -> u32 {
        self.y + self.height
    }

    /// Whether the pixel is inside.
    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    /// Smallest rectangle covering both (empty inputs are ignored).
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    /// Intersection with `0..width × 0..height`.
    pub fn clamp_to(self, width: u32, height: u32) -> Rect {
        let x0 = self.x.min(width);
        let y0 = self.y.min(height);
        let x1 = self.right().min(width);
        let y1 = self.bottom().min(height);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// Pixels whose centres may lie within the float bounds `[x0, x1] × [y0, y1]`,
    /// clamped to `width × height`.
    pub fn from_bounds(x0: f32, y0: f32, x1: f32, y1: f32, width: u32, height: u32) -> Rect {
        let fx0 = x0.floor().max(0.0);
        let fy0 = y0.floor().max(0.0);
        let fx1 = (x1.ceil() + 1.0).clamp(0.0, width as f32);
        let fy1 = (y1.ceil() + 1.0).clamp(0.0, height as f32);
        if !(fx1 > fx0 && fy1 > fy0) {
            return Rect::EMPTY;
        }
        Rect::new(fx0 as u32, fy0 as u32, (fx1 - fx0) as u32, (fy1 - fy0) as u32).clamp_to(width, height)
    }

    /// Copy this region's pixels out of `img` (row-major RGBA8).
    pub fn extract(&self, img: &ImageData) -> Vec<u8> {
        let r = self.clamp_to(img.width, img.height);
        let mut out = Vec::with_capacity((r.width * r.height * 4) as usize);
        for y in r.y..r.bottom() {
            let start = ((y * img.width + r.x) * 4) as usize;
            out.extend_from_slice(&img.pixels[start..start + (r.width * 4) as usize]);
        }
        out
    }

    /// Write pixels previously produced by [`Rect::extract`] back into `img`.
    pub fn write(&self, img: &mut ImageData, pixels: &[u8]) {
        let r = self.clamp_to(img.width, img.height);
        let row = (r.width * 4) as usize;
        for (i, y) in (r.y..r.bottom()).enumerate() {
            let start = ((y * img.width + r.x) * 4) as usize;
            img.pixels[start..start + row].copy_from_slice(&pixels[i * row..(i + 1) * row]);
        }
    }
}

#[inline]
pub(crate) fn unorm(v: u8) -> f32 {
    v as f32 * (1.0 / 255.0)
}

#[inline]
pub(crate) fn to_u8(v: f32) -> u8 {
    (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_ops() {
        let a = Rect::new(0, 0, 2, 2);
        let b = Rect::new(5, 5, 1, 1);
        assert_eq!(a.union(b), Rect::new(0, 0, 6, 6));
        assert_eq!(Rect::EMPTY.union(b), b);
        assert_eq!(Rect::new(3, 3, 10, 10).clamp_to(5, 4), Rect::new(3, 3, 2, 1));
        assert_eq!(Rect::from_bounds(-3.0, -3.0, -1.0, -1.0, 8, 8), Rect::EMPTY);
        let mut img = ImageData::checker(4, 1, [255; 4], [0, 0, 0, 255]);
        let r = Rect::new(1, 1, 2, 2);
        let saved = r.extract(&img);
        fill(&mut img, [9; 4]);
        r.write(&mut img, &saved);
        assert_eq!(img.get(1, 1), [255; 4]);
        assert_eq!(img.get(2, 1), [0, 0, 0, 255]);
        assert_eq!(img.get(0, 0), [9; 4]);
    }
}
