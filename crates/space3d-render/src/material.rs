//! Material types beyond the legacy [`crate::Material`] fields (graphics spec §2.2.8): blend
//! modes, shadow participation, map masks, UV transforms and extra per-vertex data.
//!
//! This is the materials area's API hub (re-exported from the crate root). The renderer's use of
//! these fields lands with the materials milestones (PM1–PM5); until then they are stored and
//! compared but do not change rendering (every default reproduces the legacy look).

/// How a material's color combines with what is behind it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum BlendMode {
    /// Opaque, depth-writing.
    #[default]
    Opaque,
    /// Opaque with alpha-tested holes (`Material::alpha_cutoff`; alpha to coverage with MSAA).
    AlphaMask,
    /// Standard alpha blending (`Material::alpha_blend` maps here).
    AlphaBlend,
    /// Premultiplied alpha: rgb is added, alpha covers.
    Premultiplied,
    /// Additive glow: `dst + src × alpha`.
    Additive,
}

impl BlendMode {
    /// Drawn in the transparent phase (blended, sorted, no depth write).
    pub fn is_transparent(self) -> bool {
        matches!(self, Self::AlphaBlend | Self::Premultiplied | Self::Additive)
    }

    /// Writes depth (Opaque and AlphaMask).
    pub fn writes_depth(self) -> bool {
        !self.is_transparent()
    }

    /// Color target blend: Opaque/AlphaMask → `None`; AlphaBlend → `ALPHA_BLENDING`;
    /// Premultiplied → `PREMULTIPLIED_ALPHA_BLENDING`; Additive → color (SrcAlpha, One, Add),
    /// alpha (Zero, One, Add). Shared with particles.
    pub fn blend_state(self) -> Option<wgpu::BlendState> {
        match self {
            Self::Opaque | Self::AlphaMask => None,
            Self::AlphaBlend => Some(wgpu::BlendState::ALPHA_BLENDING),
            Self::Premultiplied => Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            Self::Additive => Some(wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::SrcAlpha,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::Zero,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
            }),
        }
    }
}

/// Shadow participation of a material.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShadowMode {
    /// Lit opaque and alpha-masked materials cast and receive; unlit and transparent ones do
    /// neither.
    #[default]
    Auto,
    /// Cast and receive.
    CastAndReceive,
    /// Cast only.
    CastOnly,
    /// Receive only.
    ReceiveOnly,
    /// Neither.
    None,
}

/// A set of material maps (bit set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MapMask(pub u8);

impl MapMask {
    /// No map.
    pub const NONE: Self = Self(0);
    /// `base_color_texture`.
    pub const BASE_COLOR: Self = Self(1);
    /// `normal_texture`.
    pub const NORMAL: Self = Self(1 << 1);
    /// `metallic_roughness_texture`.
    pub const METALLIC_ROUGHNESS: Self = Self(1 << 2);
    /// `occlusion_texture`.
    pub const OCCLUSION: Self = Self(1 << 3);
    /// `emissive_texture`.
    pub const EMISSIVE: Self = Self(1 << 4);
    /// Every map.
    pub const ALL: Self = Self(0b1_1111);

    /// Both sets.
    pub const fn union(self, o: Self) -> Self {
        Self(self.0 | o.0)
    }
    /// Every map of `o` is in `self`.
    pub const fn contains(self, o: Self) -> bool {
        self.0 & o.0 == o.0
    }
}

impl std::ops::BitOr for MapMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// UV transform of every map of a material (`KHR_texture_transform`):
/// `uv' = T(offset) · R(rotation) · S(scale) · uv`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextureTransform {
    /// Translation.
    pub offset: glam::Vec2,
    /// Rotation in radians.
    pub rotation: f32,
    /// Scale.
    pub scale: glam::Vec2,
}

impl TextureTransform {
    /// No transform.
    pub const IDENTITY: Self = Self {
        offset: glam::Vec2::ZERO,
        rotation: 0.0,
        scale: glam::Vec2::ONE,
    };

    /// The transform as a 3×3 matrix on `(u, v, 1)`: rotation by `r` maps `(u, v)` to
    /// `(u cos r + v sin r, −u sin r + v cos r)` before the offset.
    pub fn matrix(&self) -> glam::Mat3 {
        let (s, c) = self.rotation.sin_cos();
        let t = glam::Mat3::from_translation(self.offset);
        let r = glam::Mat3::from_cols(glam::Vec3::new(c, -s, 0.0), glam::Vec3::new(s, c, 0.0), glam::Vec3::Z);
        t * r * glam::Mat3::from_scale(self.scale)
    }
}

impl Default for TextureTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Per-vertex data beyond `MeshData` (which stays frozen), for
/// [`crate::RenderAssets::add_mesh_with`].
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct MeshExtras {
    /// MikkTSpace tangents `[x, y, z, w]` (w = ±1 bitangent sign); empty = per-pixel tangents.
    pub tangents: Vec<[f32; 4]>,
    /// Second UV set (glTF `TEXCOORD_1`); empty = none.
    pub uvs1: Vec<[f32; 2]>,
}

impl MeshExtras {
    /// No extras.
    pub fn new() -> Self {
        Self::default()
    }
    /// With tangents.
    pub fn with_tangents(mut self, t: Vec<[f32; 4]>) -> Self {
        self.tangents = t;
        self
    }
    /// With a second UV set.
    pub fn with_uvs1(mut self, uv: Vec<[f32; 2]>) -> Self {
        self.uvs1 = uv;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{Vec2, Vec3};

    #[test]
    fn blend_modes() {
        assert!(!BlendMode::Opaque.is_transparent() && BlendMode::AlphaMask.writes_depth());
        for m in [BlendMode::AlphaBlend, BlendMode::Premultiplied, BlendMode::Additive] {
            assert!(m.is_transparent() && !m.writes_depth() && m.blend_state().is_some());
        }
        assert_eq!(BlendMode::Opaque.blend_state(), None);
        let add = BlendMode::Additive.blend_state().unwrap();
        assert_eq!(add.color.dst_factor, wgpu::BlendFactor::One);
        assert_eq!(add.alpha.src_factor, wgpu::BlendFactor::Zero);
    }

    #[test]
    fn map_masks_and_transforms() {
        let m = MapMask::NORMAL | MapMask::EMISSIVE;
        assert!(m.contains(MapMask::NORMAL) && !m.contains(MapMask::BASE_COLOR));
        assert!(MapMask::ALL.contains(m) && MapMask::NONE.contains(MapMask::NONE));
        assert_eq!(TextureTransform::default().matrix(), glam::Mat3::IDENTITY);
        let t = TextureTransform {
            offset: Vec2::new(0.5, 0.25),
            rotation: std::f32::consts::FRAC_PI_2,
            scale: Vec2::new(2.0, 1.0),
        };
        let uv = t.matrix() * Vec3::new(1.0, 0.0, 1.0);
        // scale (2, 0) → rotate → (0, −2) → offset.
        assert!((uv - Vec3::new(0.5, -1.75, 1.0)).abs().max_element() < 1e-6, "{uv}");
        let e = MeshExtras::new()
            .with_tangents(vec![[1.0, 0.0, 0.0, 1.0]])
            .with_uvs1(vec![[0.0; 2]]);
        assert_eq!((e.tangents.len(), e.uvs1.len()), (1, 1));
    }
}
