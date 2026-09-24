//! Painting: brush dabs/strokes, flood fill, lines, rectangles.

use super::Rect;
use super::blend::over;
use space3d_core::ImageData;

/// A round brush.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Brush {
    /// Straight-alpha RGBA color (its alpha multiplies `opacity`).
    pub color: [u8; 4],
    /// Radius in pixels.
    pub radius: f32,
    /// Overall opacity per dab (0..1).
    pub opacity: f32,
    /// Fraction of the radius painted at full strength (1 = hard edge, 0 = soft all the way).
    pub hardness: f32,
}

impl Default for Brush {
    fn default() -> Self {
        Self {
            color: [255; 4],
            radius: 8.0,
            opacity: 1.0,
            hardness: 0.5,
        }
    }
}

impl Brush {
    /// Brush of `color` and `radius` with full opacity and a hard edge.
    pub fn hard(color: [u8; 4], radius: f32) -> Self {
        Self {
            color,
            radius,
            opacity: 1.0,
            hardness: 1.0,
        }
    }

    /// Coverage (0..1) at distance `dist` from the dab centre, including edge anti-aliasing.
    pub fn coverage(&self, dist: f32) -> f32 {
        let r = self.radius.max(0.5);
        let edge = (r - dist + 0.5).clamp(0.0, 1.0);
        if edge <= 0.0 {
            return 0.0;
        }
        let hard = self.hardness.clamp(0.0, 1.0);
        let d = dist / r;
        let falloff = if d <= hard {
            1.0
        } else {
            let t = ((d - hard) / (1.0 - hard).max(1e-6)).clamp(0.0, 1.0);
            1.0 - t * t * (3.0 - 2.0 * t)
        };
        falloff.min(edge) * self.opacity.clamp(0.0, 1.0)
    }

    /// Pixels a dab at `(x, y)` can touch on a `width × height` image.
    pub fn bounds(&self, x: f32, y: f32, width: u32, height: u32) -> Rect {
        let r = self.radius.max(0.5) + 1.0;
        Rect::from_bounds(x - r, y - r, x + r, y + r, width, height)
    }
}

/// Stamp one brush dab centred on `(x, y)` (pixel centres are integers). Returns the
/// touched region.
pub fn brush_dab(img: &mut ImageData, x: f32, y: f32, brush: &Brush) -> Rect {
    let rect = brush.bounds(x, y, img.width, img.height);
    for py in rect.y..rect.bottom() {
        for px in rect.x..rect.right() {
            let d = ((px as f32 - x).powi(2) + (py as f32 - y).powi(2)).sqrt();
            let c = brush.coverage(d);
            if c > 0.0 {
                let i = ((py * img.width + px) * 4) as usize;
                over(&mut img.pixels[i..i + 4], brush.color, c);
            }
        }
    }
    rect
}

/// Stamp dabs from `from` to `to` every `spacing × radius` pixels (e.g. 0.25). Returns the
/// touched region.
pub fn brush_stroke(img: &mut ImageData, from: (f32, f32), to: (f32, f32), brush: &Brush, spacing: f32) -> Rect {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let len = (dx * dx + dy * dy).sqrt();
    let step = (brush.radius * spacing).max(0.5);
    let n = (len / step).ceil().max(1.0) as usize;
    let mut rect = Rect::EMPTY;
    for i in 0..=n {
        let t = i as f32 / n as f32;
        rect = rect.union(brush_dab(img, from.0 + dx * t, from.1 + dy * t, brush));
    }
    rect
}

/// Replace the 4-connected region of pixels similar to the one at `(x, y)` (every channel
/// within `tolerance`) with `color`. Returns the number of pixels filled.
pub fn flood_fill(img: &mut ImageData, x: u32, y: u32, color: [u8; 4], tolerance: u8) -> usize {
    let (w, h) = (img.width as usize, img.height as usize);
    if x as usize >= w || y as usize >= h {
        return 0;
    }
    let seed = img.get(x, y);
    let matches = |p: &[u8]| (0..4).all(|c| p[c].abs_diff(seed[c]) <= tolerance);
    let mut visited = vec![false; w * h];
    let mut stack = vec![(x as usize, y as usize)];
    let mut filled = 0;
    let px = |pixels: &Vec<u8>, x: usize, y: usize| -> [u8; 4] {
        let i = (y * w + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    };
    while let Some((sx, sy)) = stack.pop() {
        if visited[sy * w + sx] || !matches(&px(&img.pixels, sx, sy)) {
            continue;
        }
        let mut lx = sx;
        while lx > 0 && !visited[sy * w + lx - 1] && matches(&px(&img.pixels, lx - 1, sy)) {
            lx -= 1;
        }
        let mut rx = sx;
        while rx + 1 < w && !visited[sy * w + rx + 1] && matches(&px(&img.pixels, rx + 1, sy)) {
            rx += 1;
        }
        for cx in lx..=rx {
            visited[sy * w + cx] = true;
            let i = (sy * w + cx) * 4;
            img.pixels[i..i + 4].copy_from_slice(&color);
            filled += 1;
        }
        for ny in [sy.wrapping_sub(1), sy + 1] {
            if ny >= h {
                continue;
            }
            let mut in_run = false;
            for cx in lx..=rx {
                let ok = !visited[ny * w + cx] && matches(&px(&img.pixels, cx, ny));
                if ok && !in_run {
                    stack.push((cx, ny));
                }
                in_run = ok;
            }
        }
    }
    filled
}

/// Fill the whole image with `color` (no blending).
pub fn fill(img: &mut ImageData, color: [u8; 4]) {
    for p in img.pixels.as_chunks_mut::<4>().0 {
        *p = color;
    }
}

fn blend_at(img: &mut ImageData, x: i32, y: i32, color: [u8; 4]) {
    if x >= 0 && y >= 0 && (x as u32) < img.width && (y as u32) < img.height {
        let i = ((y as u32 * img.width + x as u32) * 4) as usize;
        over(&mut img.pixels[i..i + 4], color, 1.0);
    }
}

/// 1-pixel Bresenham line (alpha-blended, clipped). For thick/soft lines use
/// [`brush_stroke`].
pub fn draw_line(img: &mut ImageData, from: (i32, i32), to: (i32, i32), color: [u8; 4]) {
    let (mut x, mut y) = from;
    let dx = (to.0 - x).abs();
    let dy = -(to.1 - y).abs();
    let sx = if x < to.0 { 1 } else { -1 };
    let sy = if y < to.1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        blend_at(img, x, y, color);
        if (x, y) == to {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Rectangle outline (1 px) or filled, alpha-blended and clipped.
pub fn draw_rect(img: &mut ImageData, x: i32, y: i32, width: u32, height: u32, color: [u8; 4], filled: bool) {
    if width == 0 || height == 0 {
        return;
    }
    let (x1, y1) = (x + width as i32 - 1, y + height as i32 - 1);
    for py in y..=y1 {
        let edge_row = py == y || py == y1;
        if filled || edge_row {
            for px in x..=x1 {
                blend_at(img, px, py, color);
            }
        } else {
            blend_at(img, x, py, color);
            if x1 != x {
                blend_at(img, x1, py, color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flood_fill_region() {
        // 6×6 black with a white vertical wall at x=3 splitting two regions.
        let mut img = ImageData::filled(6, 6, [0, 0, 0, 255]);
        for y in 0..6 {
            img.set(3, y, [255; 4]);
        }
        let n = flood_fill(&mut img, 0, 0, [255, 0, 0, 255], 0);
        assert_eq!(n, 18);
        assert_eq!(img.get(2, 5), [255, 0, 0, 255]);
        assert_eq!(img.get(4, 0), [0, 0, 0, 255]);
        assert_eq!(img.get(3, 3), [255; 4]);
        // Filling with a color inside the tolerance of the seed terminates.
        let mut same = ImageData::filled(5, 5, [10, 10, 10, 255]);
        assert_eq!(flood_fill(&mut same, 2, 2, [12, 12, 12, 255], 5), 25);
        // Tolerance joins similar colors.
        let mut grad = ImageData::filled(4, 1, [100, 100, 100, 255]);
        grad.set(3, 0, [110, 100, 100, 255]);
        assert_eq!(flood_fill(&mut grad.clone(), 0, 0, [0; 4], 5), 3);
        assert_eq!(flood_fill(&mut grad, 0, 0, [0; 4], 10), 4);
        assert_eq!(flood_fill(&mut ImageData::new(2, 2), 5, 5, [0; 4], 0), 0);
    }

    #[test]
    fn flood_fill_concave_region() {
        // A U-shaped wall; the region inside and around must be reached via backtracking.
        let mut img = ImageData::filled(7, 7, [0; 4]);
        for y in 1..6 {
            img.set(1, y, [255; 4]);
            img.set(5, y, [255; 4]);
        }
        for x in 1..6 {
            img.set(x, 5, [255; 4]);
        }
        let walls = 5 + 5 + 3;
        let n = flood_fill(&mut img, 3, 2, [9; 4], 0);
        assert_eq!(n, 49 - walls);
    }

    #[test]
    fn brush_dab_hardness_and_opacity() {
        let mut img = ImageData::filled(21, 21, [0, 0, 0, 255]);
        let rect = brush_dab(&mut img, 10.0, 10.0, &Brush::hard([255, 255, 255, 255], 4.0));
        assert!(rect.contains(10, 10) && rect.contains(6, 10));
        assert_eq!(img.get(10, 10), [255; 4]);
        assert_eq!(img.get(13, 10), [255; 4]);
        assert_eq!(img.get(10, 16), [0, 0, 0, 255]);
        let mut soft = ImageData::filled(21, 21, [0, 0, 0, 255]);
        let brush = Brush {
            color: [255; 4],
            radius: 8.0,
            opacity: 0.5,
            hardness: 0.0,
        };
        brush_dab(&mut soft, 10.0, 10.0, &brush);
        let c = soft.get(10, 10)[0];
        let e = soft.get(16, 10)[0];
        assert!((120..=135).contains(&c), "centre at 50% opacity, got {c}");
        assert!(e < c && e > 0, "soft falloff");
        // Transparent brush color paints nothing.
        let mut none = ImageData::filled(5, 5, [1, 2, 3, 255]);
        brush_dab(&mut none, 2.0, 2.0, &Brush::hard([255, 0, 0, 0], 3.0));
        assert!(none.pixels.chunks(4).all(|p| p == [1, 2, 3, 255]));
    }

    #[test]
    fn stroke_line_and_rect() {
        let mut img = ImageData::filled(32, 32, [0; 4]);
        let r = brush_stroke(&mut img, (2.0, 2.0), (28.0, 2.0), &Brush::hard([255; 4], 1.5), 0.25);
        assert!((2..=28).all(|x| img.get(x, 2) == [255; 4]));
        assert!(r.width >= 27);
        let mut l = ImageData::filled(5, 5, [0; 4]);
        draw_line(&mut l, (0, 0), (4, 4), [255; 4]);
        assert!((0..5).all(|i| l.get(i, i) == [255; 4]));
        assert_eq!(l.pixels.chunks(4).filter(|p| p[3] == 255).count(), 5);
        draw_line(&mut l, (-10, 2), (10, 2), [255; 4]);
        assert!((0..5).all(|i| l.get(i, 2) == [255; 4]));
        let mut r = ImageData::filled(6, 6, [0; 4]);
        draw_rect(&mut r, 1, 1, 4, 3, [255; 4], false);
        assert_eq!(r.pixels.chunks(4).filter(|p| p[3] == 255).count(), 10);
        assert_eq!(r.get(2, 2), [0; 4]);
        draw_rect(&mut r, -2, -2, 20, 20, [9, 9, 9, 255], true);
        assert!(r.pixels.chunks(4).all(|p| p == [9, 9, 9, 255]));
    }
}
