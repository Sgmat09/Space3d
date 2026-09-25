//! The post chain of a post group: pooled bloom levels, the exposure texture, the LDR
//! intermediate before FXAA, and the composite pass (graphics spec §1.9). Written by the frame
//! milestone F4 (composite only: exposure, tonemapping, dither, alpha, rect mapping and
//! upscale) and grown by the post area (PO1–PO6).

use super::PostGroupPlan;
use super::api::{BloomComposite, FilmGrain};
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

/// Second dither hash seed (`s3d_hash01(px, seed) − s3d_hash01(px, seed ^ DITHER_SECOND)`).
const DITHER_SECOND: u32 = 0x68E3_1DA4;

/// Grain seed of frame `frame` (graphics spec §4.3): `animated ? pcg(seed ^ frame) : seed`.
pub(crate) fn grain_seed(grain: &FilmGrain, frame: u64) -> u32 {
    if grain.animated {
        pcg(grain.seed ^ frame as u32)
    } else {
        grain.seed
    }
}

impl GpuPost {
    /// Parameters of one post group; `bloom`: its bloom chain is recorded this frame; `fxaa`:
    /// the composite writes the LDR intermediate and the FXAA pass finishes (grain, dither,
    /// encoding).
    pub(crate) fn new(plan: &PostGroupPlan, frame: u64, bloom: bool, fxaa: bool) -> Self {
        let s = &plan.settings;
        // The passthrough group is a plain copy and upscale: no effect.
        let effects = !plan.passthrough;
        let mut flags = 0;
        if s.dither && effects {
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
        // The distortion buffer is bound and written this frame (applying it is the post
        // area's: STUB(P-POST) PO5).
        if plan.distortion && effects {
            flags |= bits::DISTORTION;
        }
        // The Ldr fallback exposes and tonemaps per fragment already; passthrough copies.
        let hdr = plan.mode == FrameMode::Hdr && effects;
        let (exposure, tonemapper) = if hdr {
            (s.exposure.exp2(), s.tonemapper)
        } else {
            (1.0, Tonemapper::None)
        };
        let rect = |r: [i32; 4]| [r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32];
        let (sw, sh) = (plan.scene_size.0.max(1) as f32, plan.scene_size.1.max(1) as f32);
        let out = rect(plan.output_rect);
        let src = rect(plan.scene_rect);
        let mut p = GpuPost::zeroed();
        p.out_rect = out;
        p.src_rect = src;
        p.src_texel = [1.0 / sw, 1.0 / sh, out[2] / out[3].max(1.0), plan.output_size.1 as f32];
        p.exposure = [exposure, 0.0, 0.0, 0.0];
        if bloom {
            flags |= bits::BLOOM;
            let energy = s.bloom.composite == BloomComposite::EnergyConserving;
            p.exposure[2] = if energy {
                s.bloom.intensity.clamp(0.0, 1.0)
            } else {
                s.bloom.intensity.max(0.0)
            };
            p.exposure[3] = f32::from(u8::from(energy));
        }
        let v = &s.vignette;
        if effects && v.enabled && v.intensity > 0.0 {
            flags |= bits::VIGNETTE;
            p.vignette = [
                v.intensity.clamp(0.0, 1.0),
                v.smoothness.clamp(0.01, 1.0),
                v.roundness.clamp(0.0, 1.0),
                v.center.x,
            ];
            p.vignette_color = [v.color.x, v.color.y, v.color.z, v.center.y];
        }
        let ca = &s.chromatic_aberration;
        if effects && ca.enabled && ca.intensity > 0.0 {
            flags |= bits::CA;
            // Red and blue sit `d · |d|² · lens.x` apart (d: S pixels from the rect's center):
            // intensity × 2 % of the rect's width at its corners.
            let corner = 0.5 * (src[2] * src[2] + src[3] * src[3]).sqrt().max(1.0);
            p.lens[0] = ca.intensity.clamp(0.0, 1.0) * 0.02 * src[2] / (corner * corner * corner);
        }
        let g = &s.film_grain;
        if effects && g.enabled && g.intensity > 0.0 {
            flags |= bits::GRAIN;
            // Cells of `size` pixels at 1080p, scaled with the output height, at least a pixel.
            let cell = (g.size.max(0.0) * plan.output_size.1 as f32 / 1080.0).max(1.0);
            p.grain = [
                g.intensity.clamp(0.0, 1.0),
                cell,
                g.luminance_response.clamp(0.0, 1.0),
                0.0,
            ];
        }
        p.grade_matrix = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];
        p.flags = [flags, tonemapper.id(), u32::from(!fxaa), 0];
        // Seeds are hashed once here (`s3d_pcg(seed)`), not per pixel: the shader's
        // `post_hash01(p, pcg(seed))` equals `s3d_hash01(p, seed)` bit for bit.
        p.frame = [
            if flags & bits::GRAIN != 0 {
                pcg(grain_seed(g, frame))
            } else {
                0
            },
            if flags & bits::DITHER != 0 {
                pcg(dither_seed(frame))
            } else {
                0
            },
            if flags & bits::DITHER != 0 {
                pcg(dither_seed(frame) ^ DITHER_SECOND)
            } else {
                0
            },
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
            ("S3D_POST_VIGNETTE", bits::VIGNETTE),
            ("S3D_POST_CA", bits::CA),
            ("S3D_POST_GRAIN", bits::GRAIN),
        ] {
            assert!(src.contains(&format!("const {name}: u32 = {bit}u;")), "{name}");
        }
        crate::pipeline::validate_wgsl(&src).unwrap_or_else(|e| panic!("{e}"));
    }

    /// The shader hashes pre-hashed seeds (`post_hash01(p, pcg(seed))`): the same numbers as
    /// `s3d_hash01(p, seed)`, and the grain seed follows spec §4.3.
    #[test]
    fn seeds_are_prehashed() {
        let hash = |p: [u32; 2], hashed: u32| pcg(p[0] ^ pcg(p[1] ^ hashed)) as f32 * (1.0 / 4_294_967_295.0);
        for (p, seed) in [([0, 0], 0), ([17, 3], 0x9E37_79B9), ([1919, 1079], u32::MAX)] {
            assert_eq!(hash(p, pcg(seed)), crate::output::hash01(p, seed));
        }
        let still = FilmGrain {
            animated: false,
            seed: 42,
            ..FilmGrain::default()
        };
        assert_eq!((grain_seed(&still, 1), grain_seed(&still, 2)), (42, 42));
        let animated = FilmGrain {
            animated: true,
            ..still
        };
        assert_eq!(grain_seed(&animated, 7), pcg(42 ^ 7));
        assert_ne!(grain_seed(&animated, 7), grain_seed(&animated, 8));
    }

    #[test]
    fn dither_seed_is_the_spec_hash() {
        assert_eq!(dither_seed(0), pcg(0x9E37_79B9));
        assert_ne!(dither_seed(1), dither_seed(2));
    }
}
