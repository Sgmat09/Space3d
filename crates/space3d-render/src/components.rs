//! ECS components and resources understood by the renderer.

use crate::handles::{MeshHandle, TextureHandle};
use glam::{Mat4, Vec3, Vec4};

/// Renders the entity's mesh with its [`Material`] at its `GlobalTransform`.
/// Entities sharing mesh + material are drawn with a single instanced draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mesh3d(pub MeshHandle);

/// Optional visibility toggle; entities without it are visible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Drawn (subject to frustum culling).
    #[default]
    Visible,
    /// Skipped entirely.
    Hidden,
}

/// Convert one sRGB-encoded channel (0..1) to linear.
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Convert one linear channel (0..1) to sRGB encoding.
pub fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Surface description. All colors are **linear**; use [`Material::srgb`] for sRGB input.
///
/// Materials are plain values: the renderer interns identical materials, so many entities with
/// equal materials still batch into one draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    /// Linear RGBA multiplier (and alpha when `alpha_blend`).
    pub base_color: Vec4,
    /// Optional sRGB color texture (sampled as a 2D array; layer selected per instance).
    pub base_color_texture: Option<TextureHandle>,
    /// 0 = dielectric, 1 = metal.
    pub metallic: f32,
    /// 0 = mirror, 1 = fully rough.
    pub roughness: f32,
    /// Linear emitted radiance added after lighting.
    pub emissive: Vec3,
    /// Skip lighting: output `base_color * texture + emissive`.
    pub unlit: bool,
    /// Disable back-face culling.
    pub double_sided: bool,
    /// Alpha blending (drawn after opaque geometry, no depth writes).
    pub alpha_blend: bool,
}

impl Default for Material {
    fn default() -> Self {
        Self {
            base_color: Vec4::ONE,
            base_color_texture: None,
            metallic: 0.0,
            roughness: 0.5,
            emissive: Vec3::ZERO,
            unlit: false,
            double_sided: false,
            alpha_blend: false,
        }
    }
}

impl Material {
    /// Lit material with a linear RGBA color.
    pub fn color(base_color: Vec4) -> Self {
        Self {
            base_color,
            ..Default::default()
        }
    }
    /// Lit material from sRGB-encoded components in 0..1.
    pub fn srgb(r: f32, g: f32, b: f32) -> Self {
        Self::color(Vec4::new(srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0))
    }
    /// Unlit material with a linear RGBA color.
    pub fn unlit(base_color: Vec4) -> Self {
        Self {
            base_color,
            unlit: true,
            ..Default::default()
        }
    }
    /// Set the base color texture.
    pub fn with_texture(mut self, texture: TextureHandle) -> Self {
        self.base_color_texture = Some(texture);
        self
    }
    /// Set metallic / roughness.
    pub fn with_metallic_roughness(mut self, metallic: f32, roughness: f32) -> Self {
        self.metallic = metallic;
        self.roughness = roughness;
        self
    }
    /// Set emissive radiance (linear).
    pub fn with_emissive(mut self, emissive: Vec3) -> Self {
        self.emissive = emissive;
        self
    }
}

/// Camera projection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Projection {
    /// Perspective with vertical field of view in radians.
    Perspective { fov_y: f32, near: f32, far: f32 },
    /// Orthographic; `height` is the visible world-space height (width follows aspect).
    Orthographic { height: f32, near: f32, far: f32 },
}

impl Default for Projection {
    fn default() -> Self {
        Projection::Perspective {
            fov_y: 60f32.to_radians(),
            near: 0.1,
            far: 1000.0,
        }
    }
}

impl Projection {
    /// Right-handed projection matrix with wgpu's 0..1 depth range.
    pub fn matrix(&self, aspect: f32) -> Mat4 {
        match *self {
            Projection::Perspective { fov_y, near, far } => {
                glam::camera::rh::proj::directx::perspective(fov_y, aspect, near, far)
            }
            Projection::Orthographic { height, near, far } => {
                let h = height * 0.5;
                let w = h * aspect;
                glam::camera::rh::proj::directx::orthographic(-w, w, -h, h, near, far)
            }
        }
    }
}

/// Where a camera renders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RenderTarget {
    /// The window surface, or the offscreen capture target when headless.
    #[default]
    Primary,
    /// A texture created with [`crate::RenderAssets::add_render_target`].
    Texture(TextureHandle),
}

/// A 3D camera. Uses the entity's `GlobalTransform` as its world transform (looking down -Z).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera3d {
    /// Projection.
    pub projection: Projection,
    /// Linear clear color; `None` keeps the previous contents (for overlay cameras).
    pub clear_color: Option<Vec4>,
    /// Render target.
    pub target: RenderTarget,
    /// Cameras render in ascending `order`.
    pub order: i32,
    /// Normalized viewport `[x, y, width, height]` in 0..1; `None` = full target.
    pub viewport: Option<[f32; 4]>,
    /// Inactive cameras are skipped.
    pub active: bool,
}

impl Default for Camera3d {
    fn default() -> Self {
        Self {
            projection: Projection::default(),
            clear_color: Some(Vec4::new(0.02, 0.02, 0.03, 1.0)),
            target: RenderTarget::Primary,
            order: 0,
            viewport: None,
            active: true,
        }
    }
}

impl Camera3d {
    /// Camera with a given clear color.
    pub fn with_clear_color(mut self, color: Vec4) -> Self {
        self.clear_color = Some(color);
        self
    }
    /// Camera with a given projection.
    pub fn with_projection(mut self, projection: Projection) -> Self {
        self.projection = projection;
        self
    }
    /// Camera rendering to a texture target.
    pub fn with_target(mut self, target: RenderTarget) -> Self {
        self.target = target;
        self
    }
}

/// Sun-like light shining along the entity's forward (-Z) axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionalLight {
    /// Linear color.
    pub color: Vec3,
    /// Intensity multiplier.
    pub intensity: f32,
}

impl Default for DirectionalLight {
    fn default() -> Self {
        Self {
            color: Vec3::ONE,
            intensity: 1.0,
        }
    }
}

/// Omni light at the entity's position with smooth falloff to zero at `range`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointLight {
    /// Linear color.
    pub color: Vec3,
    /// Intensity multiplier.
    pub intensity: f32,
    /// Distance at which the light fades to zero.
    pub range: f32,
}

impl Default for PointLight {
    fn default() -> Self {
        Self {
            color: Vec3::ONE,
            intensity: 1.0,
            range: 10.0,
        }
    }
}

/// Global ambient term (resource). Defaults to a dim neutral light.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientLight {
    /// Linear color.
    pub color: Vec3,
    /// Intensity multiplier.
    pub intensity: f32,
}

impl Default for AmbientLight {
    fn default() -> Self {
        Self {
            color: Vec3::ONE,
            intensity: 0.05,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn srgb_roundtrip() {
        for v in [0.0, 0.001, 0.2, 0.5, 1.0] {
            assert!((linear_to_srgb(srgb_to_linear(v)) - v).abs() < 1e-5);
        }
    }
}
