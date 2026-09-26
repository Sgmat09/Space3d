//! Color grading (graphics spec §5.2 PO4; design-post §3.6), fused into the composite: the
//! white-balance × saturation matrix and the contrast in linear HDR before the tonemapper, the
//! ASC-CDL lift/gamma/gain and the 3D LUT in the sRGB-encoded display space after it, and the
//! WGSL hooks (`post_hdr_hook`, `post_ldr_hook`) of `ColorGrading::shader`. This module builds
//! the uniforms, mirrors the shader on the CPU (tests) and composes and validates hook shaders.

use super::api::ColorGrading;
use crate::shader::CustomShader;
use glam::{Mat3, Vec3};

/// Linear sRGB → LMS (CAT02-derived; Unity's white balance). Columns.
const LIN_TO_LMS: Mat3 = Mat3::from_cols(
    Vec3::new(3.90405e-1, 7.08416e-2, 2.31082e-2),
    Vec3::new(5.49941e-1, 9.63172e-1, 1.28021e-1),
    Vec3::new(8.92632e-3, 1.35775e-3, 9.36245e-1),
);
/// LMS → linear sRGB. Columns.
const LMS_TO_LIN: Mat3 = Mat3::from_cols(
    Vec3::new(2.85847e+0, -2.10182e-1, -4.18120e-2),
    Vec3::new(-1.62879e+0, 1.15820e+0, -1.18169e-1),
    Vec3::new(-2.48910e-2, 3.24281e-4, 1.06867e+0),
);
/// Rec. 709 luma (the saturation's grey).
const LUMA: Vec3 = Vec3::new(0.2126, 0.7152, 0.0722);
/// Middle grey: contrast pivots around it (log2 space; `post_grade_hdr`).
pub(crate) const PIVOT: f32 = 0.18;

/// LMS of the reference white shifted by `temperature` and `tint` (−1..1 each): the CIE xy of
/// the D65 white moved along the daylight locus (temperature) and across it (tint).
fn white_lms(temperature: f32, tint: f32) -> Vec3 {
    // ±1 spans the range Unity's white balance offers (±100).
    let t1 = temperature * (100.0 / 60.0);
    let t2 = tint * (100.0 / 60.0);
    let x = 0.31271 - t1 * if t1 < 0.0 { 0.1 } else { 0.05 };
    let y = 2.87 * x - 3.0 * x * x - 0.275_095_07 + t2 * 0.05;
    let (cx, cy, cz) = (x / y, 1.0, (1.0 - x - y) / y);
    Vec3::new(
        0.7328 * cx + 0.4296 * cy - 0.1624 * cz,
        -0.7036 * cx + 1.6975 * cy + 0.0061 * cz,
        0.0030 * cx + 0.0136 * cy + 0.9834 * cz,
    )
}

/// The white-balance matrix (von Kries in LMS): a warmer `temperature` makes whites warmer, a
/// positive `tint` magenta. Identity at 0, 0.
pub(crate) fn white_balance(temperature: f32, tint: f32) -> Mat3 {
    let (temperature, tint) = (temperature.clamp(-1.0, 1.0), tint.clamp(-1.0, 1.0));
    if temperature == 0.0 && tint == 0.0 {
        return Mat3::IDENTITY;
    }
    let gain = white_lms(0.0, 0.0) / white_lms(temperature, tint);
    LMS_TO_LIN * Mat3::from_diagonal(gain) * LIN_TO_LMS
}

/// The saturation matrix: `luma + s · (c − luma)` (0 grey, 1 unchanged, > 1 more saturated).
pub(crate) fn saturation(s: f32) -> Mat3 {
    let grey = Mat3::from_cols(Vec3::splat(LUMA.x), Vec3::splat(LUMA.y), Vec3::splat(LUMA.z));
    Mat3::IDENTITY * s + grey * (1.0 - s)
}

/// Whether the HDR part (white balance, saturation, contrast) changes anything.
pub(crate) fn hdr_active(g: &ColorGrading) -> bool {
    g.temperature != 0.0 || g.tint != 0.0 || g.saturation != 1.0 || g.contrast != 1.0
}

/// Whether lift/gamma/gain change anything.
pub(crate) fn ldr_active(g: &ColorGrading) -> bool {
    g.lift != Vec3::ZERO || g.gamma != Vec3::ONE || g.gain != Vec3::ONE
}

/// The HDR grading matrix (white balance, then saturation).
pub(crate) fn hdr_matrix(g: &ColorGrading) -> Mat3 {
    let s = if g.saturation.is_finite() {
        g.saturation.max(0.0)
    } else {
        1.0
    };
    saturation(s) * white_balance(finite(g.temperature, 0.0), finite(g.tint, 0.0))
}

/// Contrast exponent (≥ 0; 1 for NaN).
pub(crate) fn contrast(g: &ColorGrading) -> f32 {
    finite(g.contrast, 1.0).max(0.0)
}

/// `1 / gamma` per channel (gamma clamped to ≥ 0.01; > 1 brightens the midtones).
pub(crate) fn inverse_gamma(g: &ColorGrading) -> Vec3 {
    let gamma = Vec3::new(finite(g.gamma.x, 1.0), finite(g.gamma.y, 1.0), finite(g.gamma.z, 1.0));
    Vec3::ONE / gamma.max(Vec3::splat(0.01))
}

fn finite(v: f32, or: f32) -> f32 {
    if v.is_finite() { v } else { or }
}

/// CPU mirror of the composite's HDR grading (`post_grade_hdr`): linear HDR in and out.
pub(crate) fn grade_hdr(c: Vec3, g: &ColorGrading) -> Vec3 {
    let c = (hdr_matrix(g) * c).max(Vec3::ZERO);
    let k = contrast(g);
    if k == 1.0 {
        return c;
    }
    let p = |v: f32| (v.max(1e-10) / PIVOT).powf(k) * PIVOT;
    Vec3::new(p(c.x), p(c.y), p(c.z))
}

/// CPU mirror of lift/gamma/gain on an sRGB-encoded display color (`post_grade_ldr` without the
/// LUT): `(e · gain + lift)^(1 / gamma)`.
#[cfg(test)]
pub(crate) fn grade_cdl(e: Vec3, g: &ColorGrading) -> Vec3 {
    let v = (e * g.gain + g.lift).max(Vec3::ZERO);
    let ig = inverse_gamma(g);
    Vec3::new(v.x.powf(ig.x), v.y.powf(ig.y), v.z.powf(ig.z))
}

/// The hooks' names and signatures (`fn(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32>`).
pub(crate) const HDR_HOOK: &str = "post_hdr_hook";
pub(crate) const LDR_HOOK: &str = "post_ldr_hook";
const DEFAULT_HDR_HOOK: &str = "fn post_hdr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> {\n    return c;\n}\n";
const DEFAULT_LDR_HOOK: &str = "fn post_ldr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> {\n    return c;\n}\n";

/// Whether `src` declares `fn <name>(` (whitespace allowed around the name).
pub(crate) fn declares(src: &str, name: &str) -> bool {
    src.match_indices("fn").any(|(i, _)| {
        let before_ok = src[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let rest = src[i + 2..].trim_start();
        before_ok
            && src[i + 2..].starts_with(char::is_whitespace)
            && rest.strip_prefix(name).is_some_and(|r| r.trim_start().starts_with('('))
    })
}

/// WGSL replacing the composite's `//#HOOKS` marker: the hook shader (if any) and identity
/// functions for the hooks it does not declare.
pub(crate) fn hooks_wgsl(shader: Option<&CustomShader>) -> String {
    let mut s = String::new();
    if let Some(c) = shader {
        s += &c.wgsl;
        s += "\n";
    }
    if !shader.is_some_and(|c| declares(&c.wgsl, HDR_HOOK)) {
        s += DEFAULT_HDR_HOOK;
    }
    if !shader.is_some_and(|c| declares(&c.wgsl, LDR_HOOK)) {
        s += DEFAULT_LDR_HOOK;
    }
    s
}

/// The composite's own module-scope resources (a hook may use them, never declare more).
const COMPOSITE_RESOURCES: [&str; 8] = [
    "post",
    "post_sampler",
    "post_seeds",
    "scene_tex",
    "bloom_tex",
    "exposure_tex",
    "lut_tex",
    "distortion_tex",
];

/// Validate a composite module with a grading hook (`src`, composed): WGSL errors, resources
/// the composite does not bind, workgroup memory, and pipeline constants without a default are
/// rejected with a readable message.
pub(crate) fn validate_hook_module(src: &str) -> Result<(), String> {
    use naga::AddressSpace as A;
    let (module, _) = crate::pipeline::parse_and_validate(src)?;
    for (_, g) in module.global_variables.iter() {
        let name = g.name.as_deref().unwrap_or("<unnamed>");
        match g.space {
            A::Function | A::Private => {}
            A::Uniform | A::Handle if COMPOSITE_RESOURCES.contains(&name) => {}
            space => {
                return Err(format!(
                    "grading hooks cannot declare `{name}` ({space:?}): only `var<private>` globals \
                     (and the composite's own resources) are allowed"
                ));
            }
        }
    }
    for (_, o) in module.overrides.iter() {
        if o.init.is_none() {
            let name = o.name.as_deref().unwrap_or("<unnamed>");
            return Err(format!(
                "grading hooks cannot declare pipeline constants without a default: `override {name}`"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_matrices_are_identity() {
        assert_eq!(white_balance(0.0, 0.0), Mat3::IDENTITY);
        assert!(saturation(1.0).abs_diff_eq(Mat3::IDENTITY, 1e-7));
        let g = ColorGrading::default();
        assert!(!hdr_active(&g) && !ldr_active(&g));
        assert!(hdr_matrix(&g).abs_diff_eq(Mat3::IDENTITY, 1e-7));
        assert_eq!(grade_hdr(Vec3::new(0.3, 2.0, 7.0), &g), Vec3::new(0.3, 2.0, 7.0));
        assert!((grade_cdl(Vec3::new(0.2, 0.5, 0.9), &g) - Vec3::new(0.2, 0.5, 0.9)).length() < 1e-6);
    }

    #[test]
    fn white_balance_shifts_whites() {
        let warm = white_balance(0.5, 0.0) * Vec3::ONE;
        assert!(warm.x > warm.y && warm.y > warm.z, "warm: {warm}");
        let cool = white_balance(-0.5, 0.0) * Vec3::ONE;
        assert!(cool.z > cool.y && cool.y > cool.x, "cool: {cool}");
        let magenta = white_balance(0.0, 0.5) * Vec3::ONE;
        assert!(magenta.y < magenta.x && magenta.y < magenta.z, "magenta: {magenta}");
        let green = white_balance(0.0, -0.5) * Vec3::ONE;
        assert!(green.y > green.x && green.y > green.z, "green: {green}");
        // Luminance stays about the same.
        for c in [warm, cool, magenta, green] {
            assert!((c.dot(LUMA) - 1.0).abs() < 0.15, "{c}");
        }
    }

    #[test]
    fn saturation_and_contrast() {
        let c = Vec3::new(0.8, 0.2, 0.1);
        let grey = grade_hdr(
            c,
            &ColorGrading {
                saturation: 0.0,
                ..Default::default()
            },
        );
        assert!((grey - Vec3::splat(c.dot(LUMA))).abs().max_element() < 1e-6);
        let vivid = grade_hdr(
            c,
            &ColorGrading {
                saturation: 1.5,
                ..Default::default()
            },
        );
        assert!(vivid.x > c.x && vivid.z < c.z, "{vivid}");
        // Contrast pivots on middle grey: 0.18 stays, 0.36 → 0.72 at contrast 2, 0.09 → 0.045.
        let g = ColorGrading {
            contrast: 2.0,
            ..Default::default()
        };
        let out = grade_hdr(Vec3::new(0.18, 0.36, 0.09), &g);
        assert!((out - Vec3::new(0.18, 0.72, 0.045)).abs().max_element() < 1e-5, "{out}");
    }

    #[test]
    fn cdl_lifts_gammas_and_gains() {
        let g = ColorGrading {
            lift: Vec3::splat(0.1),
            gamma: Vec3::splat(2.0),
            gain: Vec3::new(0.5, 1.0, 1.0),
            ..Default::default()
        };
        let out = grade_cdl(Vec3::new(0.4, 0.4, 0.0), &g);
        assert!((out.x - 0.3f32.sqrt()).abs() < 1e-6);
        assert!((out.y - 0.5f32.sqrt()).abs() < 1e-6);
        assert!((out.z - 0.1f32.sqrt()).abs() < 1e-6);
    }

    #[test]
    fn hooks_are_composed() {
        let base = hooks_wgsl(None);
        assert!(declares(&base, HDR_HOOK) && declares(&base, LDR_HOOK));
        let hdr = CustomShader::new(
            "warm",
            "fn post_hdr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> { return c * vec3(1.1, 1.0, 0.9); }",
        );
        let s = hooks_wgsl(Some(&hdr));
        assert_eq!(s.matches("fn post_hdr_hook").count(), 1);
        assert_eq!(s.matches("fn post_ldr_hook").count(), 1);
        assert!(!declares("fn post_hdr_hookx(", HDR_HOOK));
        assert!(declares("fn  post_ldr_hook (", LDR_HOOK));
    }
}
