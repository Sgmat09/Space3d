//! Custom shaders: user WGSL hooks spliced into the built-in forward shader.

/// User WGSL spliced into the built-in forward shader, selected per material with
/// [`crate::Material::with_shader`] (register it with [`crate::RenderAssets::add_shader`]).
///
/// The source defines one or both hooks (a missing one is the identity):
///
/// ```wgsl
/// // End of the vertex stage: adjust any output (position, uv, color, `out.custom`).
/// fn vertex_hook(v: VertexInput, inst: InstanceInput, out: VsOut) -> VsOut
/// // End of the fragment stage: `color` is the lit linear color (alpha = base alpha).
/// fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32>
/// ```
///
/// Everything the built-in shader declares is in scope: `VertexInput` (`position`, `normal`,
/// `uv`, `color`), the generated `InstanceInput` with its accessors (`instance_model`,
/// `instance_color`, …, and `instance_customN(inst) -> vec4<f32>` for every
/// [`crate::InstanceSemantic::Custom`] attribute `N` of the instance layout, zero-padded — slots
/// 0–3 always exist and return zero when the layout lacks them, and `Unorm8x4Bgra` is
/// swizzled to RGBA — plus the raw field `inst.customN` in the attribute's own type, as fetched
/// (memory order, B G R A, for `Unorm8x4Bgra`)), `VsOut`
/// (`clip`, `world_pos`, `normal`, `uv`, `color`, `layer`, and `custom: vec4<f32>`, an extra
/// interpolated varying for passing data from `vertex_hook` to `fragment_hook`), the `view`
/// and `material` uniforms, `sample_base(uv, layer)` and the helpers `srgb_to_linear`,
/// `linear_to_srgb`, `quat_to_mat3`, `trs`. Custom shaders cannot declare their own bindings.
/// Prefix your own helper functions (e.g. `my_`) to avoid clashes with the template; every name
/// the engine adds is prefixed `s3d_` / `S3D_`.
///
/// The `view` uniform is the shared `View` of [`crate::VIEW_WGSL`]: besides `view_proj`,
/// `camera_pos`, `ambient` and the lights it has `view`, `proj`, `inv_view_proj`, `inv_proj`,
/// `viewport` (pixels of the target), `target_size` (w, h, 1/w, 1/h), `depth` (near, far,
/// orthographic), `output` and `exposure`. The output helpers of [`crate::OUTPUT_WGSL`]
/// (`s3d_tonemap`, `s3d_luma`, `s3d_linear_to_srgb`, …) are in scope too. The color returned by
/// `fragment_hook` is linear: the template encodes it for the pass (`s3d_output` with the
/// pipeline override `S3D_OUTPUT`). `MANUAL_SRGB` — true when the target needs sRGB encoded in
/// the shader — is a pipeline-overridable constant (`override`), so it can be read in function
/// bodies but not in `const` declarations.
///
/// The same material/shader works with every instance layout and on every draw path; GPU
/// culling uses the instance's position and scale, so a `vertex_hook` that moves vertices far
/// beyond the mesh bounds can be culled while (partly) visible.
///
/// A shader that fails to compile for a layout is reported once through `tracing` and by
/// [`crate::Renderer::shader_error`]; its draws fall back to the built-in shader instead of
/// failing the frame.
///
/// **Portability.** Checked before any GPU work, with a message naming the construct.
/// Rejected on every backend (the shader cannot work in the forward pipeline):
///
/// | construct | why |
/// |---|---|
/// | own bindings (`@group`/`@binding` resources) | not in the pipeline layout |
/// | `var<workgroup>` | compute-only; Metal accepts it in these stages, GL and browsers do not |
/// | `override` without a default | pipeline creation fails |
///
/// What GLSL ES 3.00 (WebGL2, GLES 3.0) lacks is rejected **only on such devices** (a
/// [`crate::Renderer::shader_error`] there, drawn with the built-in shader). Everywhere else —
/// Metal, Vulkan, DX12, WebGPU, GLES 3.1+ — the shader builds and draws as written, and
/// [`crate::Renderer::shader_warning`] (plus one `tracing` warning) says it will not run on
/// WebGL2, so the problem shows up on the machine the shader is written on:
///
/// | construct | Metal / Vulkan / DX12 / WebGPU / GLES 3.1+ | WebGL2, GLES 3.0 (GLSL ES 3.00) |
/// |---|---|---|
/// | `countOneBits`, `reverseBits`, `extractBits`, `insertBits`, `firstLeadingBit`, `firstTrailingBit`, `countTrailingZeros`, `ldexp`, `dot4I8Packed`, `dot4U8Packed`, `unpack4xI8`, `unpack4xU8`, `textureGather*` | runs, warning | error (GLSL ES 3.10) |
/// | `textureNumLevels`, `textureNumSamples`, `pack4x8snorm`, `pack4x8unorm` | runs, warning | error |
///
/// The first row covers every function of the source, called or not. Available everywhere (naga
/// polyfills them for GLSL ES 3.00): `frexp`, `modf`, `fma`, `countLeadingZeros`,
/// `unpack4x8unorm`/`unpack4x8snorm`, `pack2x16unorm`/`pack2x16snorm` and their unpacks, the
/// `Fine`/`Coarse` derivatives (as the plain ones on GL), `textureDimensions`, `textureLoad`,
/// `textureSampleLevel`/`Grad`/`Bias`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomShader {
    /// Debug name (shader labels, error messages).
    pub name: String,
    /// WGSL defining `vertex_hook` and/or `fragment_hook` (and any helpers they use).
    pub wgsl: String,
}

impl CustomShader {
    /// A named custom shader.
    pub fn new(name: impl Into<String>, wgsl: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            wgsl: wgsl.into(),
        }
    }

    /// Declares `vertex_hook`.
    pub(crate) fn has_vertex_hook(&self) -> bool {
        declares(&self.wgsl, "vertex_hook")
    }

    /// Declares `fragment_hook`.
    pub(crate) fn has_fragment_hook(&self) -> bool {
        declares(&self.wgsl, "fragment_hook")
    }
}

/// Whether `src` contains `fn <name>` followed by `(` (whitespace allowed around the name).
fn declares(src: &str, name: &str) -> bool {
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

pub(crate) const DEFAULT_VERTEX_HOOK: &str =
    "fn vertex_hook(v: VertexInput, inst: InstanceInput, out: VsOut) -> VsOut {\n    return out;\n}\n";
pub(crate) const DEFAULT_FRAGMENT_HOOK: &str =
    "fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {\n    return color;\n}\n";

/// WGSL replacing the template's HOOKS marker: the custom source (if any) plus identity
/// functions for the hooks it does not declare.
pub(crate) fn hooks_wgsl(shader: Option<&CustomShader>) -> String {
    let mut s = String::new();
    if let Some(c) = shader {
        s += &c.wgsl;
        s += "\n";
    }
    if !shader.is_some_and(CustomShader::has_vertex_hook) {
        s += DEFAULT_VERTEX_HOOK;
    }
    if !shader.is_some_and(CustomShader::has_fragment_hook) {
        s += DEFAULT_FRAGMENT_HOOK;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_detection() {
        let s = CustomShader::new(
            "t",
            "fn fragment_hook (in: VsOut, c: vec4<f32>) -> vec4<f32> { return c; }",
        );
        assert!(s.has_fragment_hook() && !s.has_vertex_hook());
        let s = CustomShader::new("t", "fn my_vertex_hook(x: f32) -> f32 { return x; }\nfn vertex_hook(");
        assert!(s.has_vertex_hook());
        assert!(!CustomShader::new("t", "fn my_vertex_hook(x: f32) {}").has_vertex_hook());
        assert!(!CustomShader::new("t", "// vertex_hook").has_vertex_hook());
        let h = hooks_wgsl(Some(&CustomShader::new("t", "fn vertex_hook(v: VertexInput) {}")));
        assert!(h.contains("fn fragment_hook") && h.matches("fn vertex_hook").count() == 1);
        assert_eq!(
            hooks_wgsl(None),
            format!("{DEFAULT_VERTEX_HOOK}{DEFAULT_FRAGMENT_HOOK}")
        );
    }
}
