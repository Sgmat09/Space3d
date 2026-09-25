//! Output encodings and tonemapping: [`OutputEncoding`], [`Tonemapper`] (one enum for the whole
//! engine, with CPU mirrors of the shader curves) and the [`OUTPUT_WGSL`] module every shader
//! drawing into a frame pass uses to encode its color (graphics spec §2.2.3, Appendix B).

use glam::{Mat3, Vec3};

/// How a fragment shader drawing into a pass must encode its color (see
/// [`crate::PassTarget::output`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OutputEncoding {
    /// Linear, unclamped (float targets; sRGB-format targets encode in hardware).
    #[default]
    Linear,
    /// Clamp to 0..1 and encode sRGB in the shader (8-bit non-sRGB targets without an sRGB
    /// view).
    ManualSrgb,
    /// LDR fallback of a post camera: exposure × tonemapper in the shader, then linear (sRGB
    /// target).
    Tonemapped,
}

impl OutputEncoding {
    /// Value of the `S3D_OUTPUT` pipeline override and of `s3d_output`'s `encoding`: 0, 1, 2.
    pub const fn id(self) -> u32 {
        match self {
            Self::Linear => 0,
            Self::ManualSrgb => 1,
            Self::Tonemapped => 2,
        }
    }
}

/// HDR → display mapping (one enum for the whole engine: post, the Ldr fallback, particles and
/// overlays).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Tonemapper {
    /// Clamp to 0..1.
    None,
    /// Luminance Reinhard `c / (1 + L)`: hue-preserving, never white.
    Reinhard,
    /// Stephen Hill's ACES fit.
    AcesFitted,
    /// Sobotka AgX (minimal fit, no look): saturated lights go to white without hue skew.
    #[default]
    AgX,
    /// Khronos PBR Neutral: faithful base colors (UI and product previews).
    KhronosPbrNeutral,
}

impl Tonemapper {
    /// Operator id of `s3d_tonemap` (`GpuView.output.y`): 0 None, 1 Reinhard, 2 AcesFitted,
    /// 3 AgX, 4 KhronosPbrNeutral.
    pub const fn id(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Reinhard => 1,
            Self::AcesFitted => 2,
            Self::AgX => 3,
            Self::KhronosPbrNeutral => 4,
        }
    }

    /// CPU mirror of `s3d_tonemap` (linear in → display-linear 0..1 out). Used by tests, by the
    /// Ldr clear-color path and by UI previews.
    pub fn apply(self, linear: Vec3) -> Vec3 {
        let c = linear.max(Vec3::ZERO);
        match self {
            Self::None => c.clamp(Vec3::ZERO, Vec3::ONE),
            Self::Reinhard => (c / (1.0 + luma(c))).clamp(Vec3::ZERO, Vec3::ONE),
            Self::AcesFitted => aces_fitted(c),
            Self::AgX => agx(c),
            Self::KhronosPbrNeutral => pbr_neutral(c),
        }
    }
}

fn luma(c: Vec3) -> f32 {
    c.dot(Vec3::new(0.2126, 0.7152, 0.0722))
}

/// WGSL `v * M` (row vector times matrix, `M` given by columns).
fn row_mul(v: Vec3, m: Mat3) -> Vec3 {
    Vec3::new(v.dot(m.x_axis), v.dot(m.y_axis), v.dot(m.z_axis))
}

fn aces_fitted(c: Vec3) -> Vec3 {
    let i = Mat3::from_cols(
        Vec3::new(0.59719, 0.35458, 0.04823),
        Vec3::new(0.07600, 0.90834, 0.01566),
        Vec3::new(0.02840, 0.13383, 0.83777),
    );
    let o = Mat3::from_cols(
        Vec3::new(1.60475, -0.53108, -0.07367),
        Vec3::new(-0.10208, 1.10813, -0.00605),
        Vec3::new(-0.00327, -0.07276, 1.07602),
    );
    let v = row_mul(c, i);
    let r = (v * (v + 0.0245786) - 0.000_090_537) / (v * (0.983729 * v + 0.432_951) + 0.238081);
    row_mul(r, o).clamp(Vec3::ZERO, Vec3::ONE)
}

// The literals mirror `s3d_agx` in output.wgsl digit for digit.
#[allow(clippy::excessive_precision)]
fn agx(c: Vec3) -> Vec3 {
    let m = Mat3::from_cols(
        Vec3::new(0.842479062253094, 0.0423282422610123, 0.0423756549057051),
        Vec3::new(0.0784335999999992, 0.878468636469772, 0.0784336),
        Vec3::new(0.0792237451477643, 0.0791661274605434, 0.879142973793104),
    );
    let mi = Mat3::from_cols(
        Vec3::new(1.19687900512017, -0.0528968517574562, -0.0529716355144438),
        Vec3::new(-0.0980208811401368, 1.15190312990417, -0.0980434501171241),
        Vec3::new(-0.0990297440797205, -0.0989611768448433, 1.15107367264116),
    );
    let (lo, hi) = (-12.47393f32, 4.026069f32);
    let log = (m * c).max(Vec3::splat(1e-10));
    let mut v = Vec3::new(log.x.log2(), log.y.log2(), log.z.log2()).clamp(Vec3::splat(lo), Vec3::splat(hi));
    v = (v - lo) / (hi - lo);
    let v2 = v * v;
    let v4 = v2 * v2;
    v = 15.5 * v4 * v2 - 40.14 * v4 * v + 31.96 * v4 - 6.868 * v2 * v + 0.4298 * v2 + 0.1191 * v - 0.00232;
    let d = (mi * v).max(Vec3::ZERO);
    Vec3::new(d.x.powf(2.2), d.y.powf(2.2), d.z.powf(2.2))
}

fn pbr_neutral(c_in: Vec3) -> Vec3 {
    let mut c = c_in;
    let x = c.min_element();
    let offset = if x < 0.08 { x - 6.25 * x * x } else { 0.04 };
    c -= offset;
    let peak = c.max_element();
    if peak < 0.76 {
        return c;
    }
    let d = 1.0 - 0.76;
    let new_peak = 1.0 - d * d / (peak + d - 0.76);
    c *= new_peak / peak;
    let g = 1.0 - 1.0 / (0.15 * (peak - new_peak) + 1.0);
    c.lerp(Vec3::splat(new_peak), g)
}

/// PCG hash (`s3d_pcg` in `shaders/common/hash.wgsl`, bit-identical): the seed of every noise
/// (dither, grain, particles) — see graphics spec §4.3.
pub(crate) fn pcg(v: u32) -> u32 {
    let s = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let w = ((s >> ((s >> 28) + 4)) ^ s).wrapping_mul(277_803_737);
    (w >> 22) ^ w
}

/// Uniform 0..1 per pixel and seed (`s3d_hash01`).
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) fn hash01(p: [u32; 2], seed: u32) -> f32 {
    pcg(p[0] ^ pcg(p[1] ^ pcg(seed))) as f32 * (1.0 / 4_294_967_295.0)
}

/// WGSL without bindings: `S3D_OUTPUT_LINEAR` / `S3D_OUTPUT_MANUAL_SRGB` /
/// `S3D_OUTPUT_TONEMAPPED`, `S3D_TM_*`, `s3d_linear_to_srgb`, `s3d_srgb_to_linear`,
/// `s3d_luma`, `s3d_tonemap(op: u32, c: vec3<f32>) -> vec3<f32>` and
/// `s3d_output(c: vec4<f32>, encoding: u32, exposure: f32, op: u32) -> vec4<f32>`.
pub const OUTPUT_WGSL: &str = include_str!("shaders/common/output.wgsl");

/// `s3d_pcg`, `s3d_hash01` (crate-internal; post and particles copy them).
pub(crate) const HASH_WGSL: &str = include_str!("shaders/common/hash.wgsl");

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Tonemapper; 5] = [
        Tonemapper::None,
        Tonemapper::Reinhard,
        Tonemapper::AcesFitted,
        Tonemapper::AgX,
        Tonemapper::KhronosPbrNeutral,
    ];

    #[test]
    fn ids_are_the_shader_constants() {
        assert_eq!(ALL.map(Tonemapper::id), [0, 1, 2, 3, 4]);
        assert_eq!(
            [
                OutputEncoding::Linear,
                OutputEncoding::ManualSrgb,
                OutputEncoding::Tonemapped
            ]
            .map(OutputEncoding::id),
            [0, 1, 2]
        );
        assert_eq!(Tonemapper::default(), Tonemapper::AgX);
        for (name, id) in [
            ("S3D_OUTPUT_LINEAR: u32 = 0u", 0),
            ("S3D_OUTPUT_MANUAL_SRGB: u32 = 1u", 1),
            ("S3D_OUTPUT_TONEMAPPED: u32 = 2u", 2),
        ] {
            assert!(OUTPUT_WGSL.contains(name), "{name} ({id})");
        }
    }

    #[test]
    fn tonemappers_map_into_display_range() {
        for t in ALL {
            assert!(t.apply(Vec3::ZERO).max_element() < 0.01, "{t:?}");
            for x in [0.05f32, 0.18, 0.5, 1.0, 2.0, 8.0, 64.0] {
                let y = t.apply(Vec3::splat(x));
                assert!(
                    y.min_element() >= 0.0 && y.max_element() <= 1.0 + 1e-4,
                    "{t:?}({x}) = {y}"
                );
            }
            // Negative input is clamped first.
            assert_eq!(t.apply(Vec3::splat(-1.0)), t.apply(Vec3::ZERO));
        }
        assert_eq!(
            Tonemapper::None.apply(Vec3::new(0.5, 2.0, 0.0)),
            Vec3::new(0.5, 1.0, 0.0)
        );
        let r = Tonemapper::Reinhard.apply(Vec3::ONE);
        assert!((r.x - 0.5).abs() < 1e-6, "{r}");
        // PBR Neutral keeps base colors below its compression start (0.76 − the toe offset).
        let n = Tonemapper::KhronosPbrNeutral.apply(Vec3::new(0.5, 0.3, 0.2));
        assert!((n - Vec3::new(0.46, 0.26, 0.16)).abs().max_element() < 1e-5, "{n}");
        // AgX sends a saturated bright light towards white.
        let a = Tonemapper::AgX.apply(Vec3::new(64.0, 4.0, 1.0));
        assert!(a.min_element() > 0.5, "{a}");
    }

    #[test]
    fn hashes_are_stable() {
        assert_eq!(pcg(0), 129_708_002);
        assert_ne!(pcg(1), pcg(2));
        let h = hash01([3, 4], 0x9E37_79B9);
        assert!((0.0..=1.0).contains(&h));
        assert_eq!(h, hash01([3, 4], 0x9E37_79B9));
    }

    #[test]
    fn wgsl_validates() {
        crate::pipeline::validate_wgsl(OUTPUT_WGSL).unwrap_or_else(|e| panic!("{e}"));
        crate::pipeline::validate_wgsl(HASH_WGSL).unwrap_or_else(|e| panic!("{e}"));
    }
}
