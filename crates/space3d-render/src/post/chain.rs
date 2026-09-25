//! The post chain of a post group: pooled bloom levels, the exposure texture, the LDR
//! intermediate before FXAA, and the composite pass (graphics spec §1.9). Written by the frame
//! milestone F4 (composite only: exposure, tonemapping, dither, alpha, rect mapping and
//! upscale) and grown by the post area (PO1–PO6).

use super::PostGroupPlan;
use crate::node::FrameMode;
use crate::output::{Tonemapper, pcg};
use bytemuck::{Pod, Zeroable};

/// Effect bits of `PostParams::flags.x` (composite.wgsl `S3D_POST_*`).
#[allow(dead_code, reason = "the post area (PO1–PO5) sets the effect bits")]
pub(crate) mod bits {
    pub const BLOOM: u32 = 1;
    pub const AUTO_EXPOSURE: u32 = 2;
    pub const VIGNETTE: u32 = 4;
    pub const CA: u32 = 8;
    pub const GRAIN: u32 = 16;
    pub const DITHER: u32 = 32;
    pub const GRADE: u32 = 64;
    pub const LUT: u32 = 128;
    pub const DISTORTION: u32 = 256;
    pub const TRANSPARENT: u32 = 512;
    pub const SCENE_ALPHA: u32 = 1024;
    pub const EXACT: u32 = 2048;
}

/// Matches `PostParams` in composite.wgsl: one 256-byte uniform slot per post group. Only
/// `frame` varies per frame (animated grain, dither), so still scenes with those off upload
/// nothing.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub(crate) struct GpuPost {
    /// The group's rect in `D` pixels (x, y, w, h), unclipped.
    pub out_rect: [f32; 4],
    /// Its rect in `S` pixels, unclipped.
    pub src_rect: [f32; 4],
    /// 1/S.w, 1/S.h, rect aspect (w/h), D height in pixels.
    pub src_texel: [f32; 4],
    /// Linear exposure multiplier, -, bloom intensity, bloom mode (0 additive, 1 energy).
    pub exposure: [f32; 4],
    pub vignette: [f32; 4],
    pub vignette_color: [f32; 4],
    pub lens: [f32; 4],
    pub grain: [f32; 4],
    /// White balance × saturation (three std140 columns).
    pub grade_matrix: [[f32; 4]; 3],
    pub lift_contrast: [f32; 4],
    pub gamma_lut: [f32; 4],
    pub gain_lut: [f32; 4],
    /// Effect bits ([`bits`]), tonemapper id, -, -.
    pub flags: [u32; 4],
    /// Grain seed, dither seed, -, -.
    pub frame: [u32; 4],
}

/// Dither seed of frame `frame` (graphics spec §4.3): `pcg(0x9E3779B9 ^ frame)`.
pub(crate) fn dither_seed(frame: u64) -> u32 {
    pcg(0x9E37_79B9 ^ frame as u32)
}

impl GpuPost {
    /// Parameters of one post group (the composite-only chain of F4).
    pub(crate) fn new(plan: &PostGroupPlan, frame: u64) -> Self {
        let s = &plan.settings;
        let mut flags = 0;
        if s.dither && !plan.passthrough {
            flags |= bits::DITHER;
        }
        if plan.transparent {
            flags |= bits::TRANSPARENT;
        }
        if plan.scene_alpha {
            flags |= bits::SCENE_ALPHA;
        }
        if plan.scene_rect[2..] == plan.output_rect[2..] {
            flags |= bits::EXACT;
        }
        // The Ldr fallback exposes and tonemaps per fragment already; passthrough copies.
        let hdr = plan.mode == FrameMode::Hdr && !plan.passthrough;
        let (exposure, tonemapper) = if hdr {
            (s.exposure.exp2(), s.tonemapper)
        } else {
            (1.0, Tonemapper::None)
        };
        let rect = |r: [i32; 4]| [r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32];
        let (sw, sh) = (plan.scene_size.0.max(1) as f32, plan.scene_size.1.max(1) as f32);
        let out = rect(plan.output_rect);
        let mut p = GpuPost::zeroed();
        p.out_rect = out;
        p.src_rect = rect(plan.scene_rect);
        p.src_texel = [1.0 / sw, 1.0 / sh, out[2] / out[3].max(1.0), plan.output_size.1 as f32];
        p.exposure = [exposure, 0.0, 0.0, 0.0];
        p.grade_matrix = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];
        p.flags = [flags, tonemapper.id(), 0, 0];
        p.frame = [
            0,
            if flags & bits::DITHER != 0 {
                dither_seed(frame)
            } else {
                0
            },
            0,
            0,
        ];
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_post_matches_wgsl() {
        let src = super::super::composite_source();
        let module = naga::front::wgsl::parse_str(&src).unwrap();
        let (_, params) = module
            .types
            .iter()
            .find(|(_, t)| t.name.as_deref() == Some("PostParams"))
            .unwrap();
        let naga::TypeInner::Struct { members, span } = &params.inner else {
            panic!("PostParams is a struct")
        };
        assert_eq!(*span as usize, std::mem::size_of::<GpuPost>());
        assert_eq!(std::mem::size_of::<GpuPost>(), 256);
        let offset = |name: &str| members.iter().find(|m| m.name.as_deref() == Some(name)).unwrap().offset as usize;
        assert_eq!(offset("grade_matrix"), std::mem::offset_of!(GpuPost, grade_matrix));
        assert_eq!(offset("flags"), std::mem::offset_of!(GpuPost, flags));
        assert_eq!(offset("frame"), std::mem::offset_of!(GpuPost, frame));
        for (name, bit) in [
            ("S3D_POST_BLOOM", bits::BLOOM),
            ("S3D_POST_DITHER", bits::DITHER),
            ("S3D_POST_TRANSPARENT", bits::TRANSPARENT),
            ("S3D_POST_SCENE_ALPHA", bits::SCENE_ALPHA),
            ("S3D_POST_EXACT", bits::EXACT),
            ("S3D_POST_DISTORTION", bits::DISTORTION),
        ] {
            assert!(src.contains(&format!("const {name}: u32 = {bit}u;")), "{name}");
        }
        crate::pipeline::validate_wgsl(&src).unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn dither_seed_is_the_spec_hash() {
        assert_eq!(dither_seed(0), pcg(0x9E37_79B9));
        assert_ne!(dither_seed(1), dither_seed(2));
    }
}
