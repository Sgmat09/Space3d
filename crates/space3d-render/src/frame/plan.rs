//! Frame planning: the frame's cameras in render order and their viewports in whole pixels.
//!
//! Grows into target groups, frame modes, post groups and view roles (graphics spec §1.3).

use super::FrameState;
use crate::components::Camera3d;
use glam::{Mat4, Vec4};
use space3d_core::{Entity, GlobalTransform, World};

#[derive(Clone, Copy)]
pub(super) struct CameraItem {
    pub(super) order: i32,
    pub(super) entity: u64,
    pub(super) camera: Camera3d,
    pub(super) transform: Mat4,
}

impl FrameState {
    /// Active cameras, sorted by `(order, entity)` (the render order).
    pub(super) fn collect_cameras(&mut self, world: &World) {
        let f = self;
        f.cameras.clear();
        for (e, cam, gt) in world.query::<(Entity, &Camera3d, &GlobalTransform)>().iter() {
            if cam.active {
                f.cameras.push(CameraItem {
                    order: cam.order,
                    entity: e.to_bits().get(),
                    camera: *cam,
                    transform: gt.0,
                });
            }
        }
        f.cameras.sort_unstable_by_key(|c| (c.order, c.entity));
    }
}

/// A view's viewport in whole pixels of its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ViewportRect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) w: u32,
    pub(super) h: u32,
}

impl ViewportRect {
    /// The rect covers every pixel of a `size` target.
    pub(super) fn covers(&self, size: (u32, u32)) -> bool {
        self.x <= 0
            && self.y <= 0
            && self.x as i64 + self.w as i64 >= size.0 as i64
            && self.y as i64 + self.h as i64 >= size.1 as i64
    }

    /// The rect to draw with, and a clip-space correction to apply after the projection.
    ///
    /// wgpu rejects viewports wider or higher than `max_texture_dimension_2d`, or reaching
    /// further than twice that from the origin: a validation error that loses the frame's
    /// commands. Camera viewports can be anything (`[-50.0, -50.0, 101.0, 101.0]` zooms into
    /// the center of a 101× larger image; non-finite values end up far outside). A rect within
    /// the limits is returned as is, with no correction — every in-range viewport draws exactly
    /// as before. Any other rect is drawn through its part within the target, with the
    /// projection remapped so that every pixel there shows exactly what the whole rect would
    /// (only the float rounding differs); a rect entirely outside the target draws nothing
    /// (a 1×1 rect just past the target's corner).
    pub(super) fn fitted(self, size: (u32, u32), max_dimension: u32) -> (ViewportRect, Mat4) {
        let max = max_dimension as i64;
        let (x, y, w, h) = (self.x as i64, self.y as i64, self.w as i64, self.h as i64);
        let valid = w <= max && h <= max && x >= -2 * max && y >= -2 * max && x + w < 2 * max && y + h < 2 * max;
        if valid {
            return (self, Mat4::IDENTITY);
        }
        let (tw, th) = (size.0.max(1) as i64, size.1.max(1) as i64);
        let (x0, y0) = (x.clamp(0, tw), y.clamp(0, th));
        let (x1, y1) = ((x + w).clamp(0, tw), (y + h).clamp(0, th));
        if x1 <= x0 || y1 <= y0 {
            let outside = ViewportRect {
                x: tw as i32,
                y: th as i32,
                w: 1,
                h: 1,
            };
            return (outside, Mat4::IDENTITY);
        }
        let clip = ViewportRect {
            x: x0 as i32,
            y: y0 as i32,
            w: (x1 - x0) as u32,
            h: (y1 - y0) as u32,
        };
        // NDC of the full rect → NDC of `clip` for the same pixel (y points up in NDC, down in
        // pixels).
        let (cw, ch) = ((x1 - x0) as f64, (y1 - y0) as f64);
        let (sx, sy) = (w as f64 / cw, h as f64 / ch);
        let tx = (2.0 * (x - x0) as f64 + w as f64 - cw) / cw;
        let ty = (ch - h as f64 - 2.0 * (y - y0) as f64) / ch;
        let fit = Mat4::from_cols(
            Vec4::new(sx as f32, 0.0, 0.0, 0.0),
            Vec4::new(0.0, sy as f32, 0.0, 0.0),
            Vec4::Z,
            Vec4::new(tx as f32, ty as f32, 0.0, 1.0),
        );
        (clip, fit)
    }

    /// `[x, y, width, height]` of the rect's part within a `size` target (at least one pixel,
    /// as a scissor rect must lie within the target): the scissor rect of the view's pass.
    pub(super) fn scissor(&self, size: (u32, u32)) -> [u32; 4] {
        let axis = |start: i32, len: u32, extent: u32| {
            let extent = extent.max(1) as i64;
            let a = (start as i64).clamp(0, extent - 1);
            let b = (start as i64 + len as i64).clamp(a + 1, extent);
            (a as u32, (b - a) as u32)
        };
        let (x, w) = axis(self.x, self.w, size.0);
        let (y, h) = axis(self.y, self.h, size.1);
        [x, y, w, h]
    }
}

/// Pixel rect of a normalized viewport `[x, y, width, height]` (top left origin) on a `size`
/// target: its edges rounded to the nearest pixel boundary, at least one pixel wide and high.
/// Rounded because GL takes integer viewports (wgpu's GL backend truncates position and size
/// separately, which would shift fractional viewports and open gaps between neighbours): every
/// backend draws the same pixels, and neighbouring viewports share their edges exactly.
///
/// A viewport whose edges round to the same boundary (thinner than about a pixel) gets the
/// pixel holding its center, kept within the target when the viewport overlaps the target:
/// growing its far edge instead would push a viewport in the last half pixel of the target
/// entirely outside it.
pub(super) fn viewport_rect([x, y, w, h]: [f32; 4], size: (u32, u32)) -> ViewportRect {
    let axis = |start: f32, len: f32, extent: u32| {
        let e = extent as f32;
        let a = (start * e).round();
        let b = ((start + len) * e).round();
        if b > a {
            return (a as i32, (b - a) as u32);
        }
        let center = ((start + 0.5 * len) * e).floor();
        let overlaps = start.min(start + len) < 1.0 && start.max(start + len) > 0.0;
        let p = if overlaps {
            center.clamp(0.0, (e - 1.0).max(0.0))
        } else {
            center
        };
        (p as i32, 1)
    };
    let (x, w) = axis(x, w, size.0);
    let (y, h) = axis(y, h, size.1);
    ViewportRect { x, y, w, h }
}
