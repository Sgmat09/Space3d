//! Material types beyond the legacy [`crate::Material`] fields (graphics spec §2.2.8): blend
//! modes, shadow participation, map masks, UV transforms and extra per-vertex data.
//!
//! This is the materials area's API hub (re-exported from the crate root). Every default
//! reproduces the legacy look bit for bit: a material without maps, with `BlendMode::Opaque`
//! (or the legacy `alpha_blend`) renders exactly as before the graphics upgrade.
//!
//! It also holds the crate-internal encodings of materials shared by extraction, the pipelines
//! and the forward shader: the pipeline class bits (`CLASS_*`) and the flag bits of the material
//! uniform (`FLAG_*`, mirrored by the `S3D_*` constants of `forward.wgsl`).

/// Pipeline class bits 2..=4: the blend mode (`BlendMode::class_bits`).
pub(crate) const CLASS_MODE_SHIFT: u8 = 2;
/// Mask of the blend-mode bits of a pipeline class.
pub(crate) const CLASS_MODE_MASK: u8 = 0b111 << CLASS_MODE_SHIFT;
/// Pipeline class bit 5: the pipeline reads the extension vertex stream (tangents, second UV
/// set; vertex buffer slot 2).
pub(crate) const CLASS_EXT: u8 = 1 << 5;

/// Material uniform flags (`MaterialParams::flags.x`): the normal map is bound and used.
pub(crate) const FLAG_NORMAL: u32 = MapMask::NORMAL.0 as u32;
/// The metallic-roughness map is bound.
pub(crate) const FLAG_METALLIC_ROUGHNESS: u32 = MapMask::METALLIC_ROUGHNESS.0 as u32;
/// The occlusion map is bound.
pub(crate) const FLAG_OCCLUSION: u32 = MapMask::OCCLUSION.0 as u32;
/// The emissive map is bound.
pub(crate) const FLAG_EMISSIVE: u32 = MapMask::EMISSIVE.0 as u32;
/// Occlusion is the R channel of the metallic-roughness map (one sample).
pub(crate) const FLAG_ORM_PACKED: u32 = 1 << 5;
/// `MaterialParams::uv_row0/1` hold a non-identity UV transform.
pub(crate) const FLAG_UV_TRANSFORM: u32 = 1 << 6;
/// A data map (normal, metallic-roughness, occlusion) sits in an sRGB texture: the shader
/// re-encodes its sample (bit `FLAG_FIXUP_SHIFT + slot`: 8 normal, 9 metallic-roughness,
/// 10 occlusion).
pub(crate) const FLAG_FIXUP_SHIFT: u32 = 7;
/// Every map flag.
pub(crate) const FLAG_MAPS: u32 = FLAG_NORMAL | FLAG_METALLIC_ROUGHNESS | FLAG_OCCLUSION | FLAG_EMISSIVE;
/// The material does not receive shadows (`ShadowMode::CastOnly` / `None`).
pub(crate) const FLAG_NO_SHADOW: u32 = 1 << 11;

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

    /// The blend-mode bits of a pipeline class (`CLASS_MODE_MASK`).
    pub(crate) fn class_bits(self) -> u8 {
        let code = match self {
            Self::Opaque => 0,
            Self::AlphaMask => 1,
            Self::AlphaBlend => 2,
            Self::Premultiplied => 3,
            Self::Additive => 4,
        };
        code << CLASS_MODE_SHIFT
    }

    /// The blend mode of a pipeline class. A transparent class (`CLASS_BLEND`) without mode
    /// bits is `AlphaBlend` (the class of the legacy `alpha_blend` before blend modes).
    pub(crate) fn from_class(class: u8) -> Self {
        match (class & CLASS_MODE_MASK) >> CLASS_MODE_SHIFT {
            1 => Self::AlphaMask,
            2 => Self::AlphaBlend,
            3 => Self::Premultiplied,
            4 => Self::Additive,
            _ if class & crate::pipeline::CLASS_BLEND != 0 => Self::AlphaBlend,
            _ => Self::Opaque,
        }
    }
}

/// Shadow participation of a material (only matters while a light has a
/// [`crate::DirectionalShadow`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShadowMode {
    /// Lit opaque and alpha-masked materials cast and receive (masked ones cast their cut-out
    /// shape); lit transparent ones receive but do not cast; unlit ones do neither (sky quads,
    /// glows, particles).
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

    /// Whether this is exactly [`Self::IDENTITY`] (the shader then skips the transform).
    pub(crate) fn is_identity(&self) -> bool {
        self.offset == glam::Vec2::ZERO && self.rotation == 0.0 && self.scale == glam::Vec2::ONE
    }

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
