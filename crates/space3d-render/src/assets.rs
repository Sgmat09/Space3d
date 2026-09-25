//! [`RenderAssets`]: CPU-side registration of meshes and textures, uploaded lazily.
//!
//! Handles are allocated immediately (no GPU needed), so games can create assets in plugin
//! `build` or `Startup` before a window/renderer exists. The renderer uploads pending
//! assets at the start of every frame (or on [`crate::Renderer::upload_assets`]) and drops the
//! CPU copies afterwards (bounds are kept for culling).

use crate::arena::{ArenaAlloc, MeshArena};
use crate::culling::{Aabb, Sphere};
use crate::handles::{MeshHandle, ShaderHandle, SlotMap, TextureHandle};
use crate::shader::CustomShader;
use glam::Vec3;
use space3d_core::{ImageData, MeshData};
use std::sync::Arc;

/// Texture sampling filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FilterMode {
    /// Bilinear + trilinear mips.
    #[default]
    Linear,
    /// Pixel-art style.
    Nearest,
}

/// Texture addressing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AddressMode {
    /// Tile.
    #[default]
    Repeat,
    /// Clamp to edge.
    Clamp,
}

/// Sampler settings stored with a texture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SamplerSettings {
    /// Filter.
    pub filter: FilterMode,
    /// Addressing (u and v).
    pub address: AddressMode,
}

/// Raw texture upload description (any wgpu format, incl. BC7/ASTC/ETC2 when supported).
#[derive(Clone, Debug, PartialEq)]
pub struct TextureSource {
    /// Width of mip 0 in texels.
    pub width: u32,
    /// Height of mip 0 in texels.
    pub height: u32,
    /// Array layers (atlas pages / animation frames). 1 for plain textures.
    pub layers: u32,
    /// Pixel format.
    pub format: wgpu::TextureFormat,
    /// Mips stored in `data` (layer-major: every mip of layer 0, then layer 1, ...).
    pub mip_level_count: u32,
    /// Tightly packed texel data.
    pub data: Vec<u8>,
    /// Generate the remaining mip chain on the GPU (uncompressed renderable formats only).
    pub generate_mipmaps: bool,
    /// Sampler.
    pub sampler: SamplerSettings,
}

impl TextureSource {
    /// sRGB RGBA8 image with GPU-generated mipmaps.
    pub fn from_image(image: ImageData) -> Self {
        Self {
            width: image.width,
            height: image.height,
            layers: 1,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            mip_level_count: 1,
            data: image.pixels,
            generate_mipmaps: true,
            sampler: SamplerSettings::default(),
        }
    }

    /// sRGB RGBA8 texture array (all layers must share the size).
    pub fn from_layers(layers: &[ImageData]) -> Option<Self> {
        let first = layers.first()?;
        if layers
            .iter()
            .any(|l| l.width != first.width || l.height != first.height)
        {
            return None;
        }
        let mut data = Vec::with_capacity(first.pixels.len() * layers.len());
        for l in layers {
            data.extend_from_slice(&l.pixels);
        }
        Some(Self {
            width: first.width,
            height: first.height,
            layers: layers.len() as u32,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            mip_level_count: 1,
            data,
            generate_mipmaps: true,
            sampler: SamplerSettings::default(),
        })
    }

    /// Non-color RGBA8 data (normal / metallic-roughness / occlusion maps) as `Rgba8Unorm` with
    /// GPU mipmaps ([`Self::from_image`] stays sRGB).
    pub fn from_image_linear(image: ImageData) -> Self {
        Self {
            format: wgpu::TextureFormat::Rgba8Unorm,
            ..Self::from_image(image)
        }
    }

    /// A non-color RGBA8 texture array (`Rgba8Unorm`; all layers must share the size).
    pub fn from_layers_linear(layers: &[ImageData]) -> Option<Self> {
        Self::from_layers(layers).map(|s| Self {
            format: wgpu::TextureFormat::Rgba8Unorm,
            ..s
        })
    }

    /// A linear float image as `Rgba16Float` (f32 → f16, round to nearest even), without mips.
    pub fn from_hdr(image: &space3d_core::HdrImageData) -> Self {
        let mut data = Vec::with_capacity(image.pixels.len() * 8);
        for px in &image.pixels {
            for c in px {
                data.extend_from_slice(&crate::instance::f32_to_f16(*c).to_le_bytes());
            }
        }
        Self {
            width: image.width,
            height: image.height,
            layers: 1,
            format: wgpu::TextureFormat::Rgba16Float,
            mip_level_count: 1,
            data,
            generate_mipmaps: false,
            sampler: SamplerSettings::default(),
        }
    }

    /// Full mip count for this size.
    pub fn full_mip_count(&self) -> u32 {
        32 - self.width.max(self.height).max(1).leading_zeros()
    }
}

/// GPU mesh placement + culling data.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GpuMesh {
    pub alloc: ArenaAlloc,
    pub sphere: Sphere,
}

pub(crate) struct MeshEntry {
    pub cpu: Option<MeshData>,
    pub gpu: Option<GpuMesh>,
    pub aabb: Aabb,
    pub sphere: Sphere,
    pub triangles: u32,
}

pub(crate) struct GpuTexture {
    /// View for sampling: `D2` for single-layer textures, `D2Array` for arrays (`layered`).
    pub view: wgpu::TextureView,
    /// More than one layer: sampled as `texture_2d_array` (see `pipeline::sampled_view_dimension`).
    pub layered: bool,
    pub sampler: wgpu::Sampler,
    /// Single-layer D2 view (render targets).
    pub target_view: Option<wgpu::TextureView>,
    /// Depth for render targets.
    pub depth_view: Option<wgpu::TextureView>,
    /// Color format of the GPU texture.
    pub format: wgpu::TextureFormat,
    /// Array layers (as registered; GL may allocate one more, see `texture.rs`).
    pub layers: u32,
}

pub(crate) struct TextureEntry {
    pub source: Option<TextureSource>,
    pub gpu: Option<GpuTexture>,
    pub width: u32,
    pub height: u32,
    pub render_target: bool,
    /// Bumped whenever the GPU texture is replaced.
    pub version: u32,
    pub sampler: SamplerSettings,
    /// Render targets: the description they were created with.
    pub target_desc: Option<crate::settings::RenderTargetDesc>,
}

/// A registered [`CustomShader`]; `version` is bumped by every update (pipelines are rebuilt).
pub(crate) struct ShaderEntry {
    pub shader: Arc<CustomShader>,
    pub version: u32,
}

/// Resource holding all meshes, textures and custom shaders (CPU registration + GPU storage).
#[derive(Default)]
pub struct RenderAssets {
    pub(crate) meshes: SlotMap<MeshEntry>,
    pub(crate) textures: SlotMap<TextureEntry>,
    pub(crate) shaders: SlotMap<ShaderEntry>,
    pub(crate) pending_meshes: Vec<MeshHandle>,
    pub(crate) pending_textures: Vec<TextureHandle>,
    pub(crate) freed_allocs: Vec<ArenaAlloc>,
    pub(crate) arena: MeshArena,
    pub(crate) cubemaps: SlotMap<crate::lighting::cubemap::CubemapEntry>,
    pub(crate) pending_cubemaps: Vec<crate::handles::CubemapHandle>,
}

fn mesh_entry(data: MeshData) -> MeshEntry {
    let (min, max) = if data.positions.is_empty() {
        (Vec3::ZERO, Vec3::ZERO)
    } else {
        data.aabb()
    };
    let aabb = Aabb::new(min, max);
    MeshEntry {
        triangles: if data.indices.is_empty() {
            data.positions.len() as u32 / 3
        } else {
            data.triangle_count() as u32
        },
        sphere: Sphere::from_aabb(&aabb),
        aabb,
        cpu: Some(data),
        gpu: None,
    }
}

impl RenderAssets {
    /// Register a mesh; it is uploaded into the mesh arena on the next frame.
    pub fn add_mesh(&mut self, data: MeshData) -> MeshHandle {
        let h = MeshHandle::from_raw(self.meshes.insert(mesh_entry(data)));
        self.pending_meshes.push(h);
        h
    }

    /// Register a mesh with extra per-vertex data (tangents, a second UV set). The extra
    /// stream is uploaded by the materials milestone PM2; until then `extras` is ignored.
    pub fn add_mesh_with(&mut self, data: MeshData, extras: crate::material::MeshExtras) -> MeshHandle {
        let _ = extras; // STUB(P-MAT): ext vertex stream
        self.add_mesh(data)
    }

    /// [`Self::update_mesh`] with extra per-vertex data (see [`Self::add_mesh_with`]).
    pub fn update_mesh_with(
        &mut self,
        handle: MeshHandle,
        data: MeshData,
        extras: crate::material::MeshExtras,
    ) -> bool {
        let _ = extras; // STUB(P-MAT): ext vertex stream
        self.update_mesh(handle, data)
    }

    /// Replace a mesh's data (re-uploaded next frame). Returns false for stale handles.
    pub fn update_mesh(&mut self, handle: MeshHandle, data: MeshData) -> bool {
        let Some(e) = self.meshes.get_mut(handle.raw()) else {
            return false;
        };
        let gpu = e.gpu;
        *e = mesh_entry(data);
        e.gpu = gpu;
        self.pending_meshes.push(handle);
        true
    }

    /// Remove a mesh and free its arena space.
    pub fn remove_mesh(&mut self, handle: MeshHandle) -> bool {
        match self.meshes.remove(handle.raw()) {
            Some(e) => {
                if let Some(g) = e.gpu {
                    self.freed_allocs.push(g.alloc);
                }
                true
            }
            None => false,
        }
    }

    /// Whether the handle refers to a live mesh.
    pub fn contains_mesh(&self, handle: MeshHandle) -> bool {
        self.meshes.get(handle.raw()).is_some()
    }

    /// Local-space bounds of a mesh.
    pub fn mesh_aabb(&self, handle: MeshHandle) -> Option<Aabb> {
        self.meshes.get(handle.raw()).map(|e| e.aabb)
    }

    /// Triangle count of a mesh.
    pub fn mesh_triangles(&self, handle: MeshHandle) -> Option<u32> {
        self.meshes.get(handle.raw()).map(|e| e.triangles)
    }

    /// Whether the mesh is resident on the GPU.
    pub fn is_mesh_uploaded(&self, handle: MeshHandle) -> bool {
        self.meshes
            .get(handle.raw())
            .is_some_and(|e| e.gpu.is_some() && e.cpu.is_none())
    }

    /// Number of live meshes.
    pub fn mesh_count(&self) -> usize {
        self.meshes.len()
    }

    /// Register an sRGB RGBA8 image (mipmaps generated on the GPU).
    pub fn add_image(&mut self, image: ImageData) -> TextureHandle {
        self.add_texture(TextureSource::from_image(image))
    }

    /// Register any texture (compressed formats need matching device features; see
    /// [`crate::Renderer::supports_texture_format`] — unsupported ones render white).
    pub fn add_texture(&mut self, source: TextureSource) -> TextureHandle {
        let h = TextureHandle::from_raw(self.textures.insert(TextureEntry {
            width: source.width,
            height: source.height,
            render_target: false,
            version: 0,
            sampler: source.sampler,
            source: Some(source),
            gpu: None,
            target_desc: None,
        }));
        self.pending_textures.push(h);
        h
    }

    /// Create a color texture cameras can render into ([`crate::RenderTarget::Texture`]) and
    /// materials can sample. A size past the device's `max_texture_dimension_2d` is logged
    /// and the target is not created until resized: cameras targeting it are skipped and
    /// materials sample white.
    pub fn add_render_target(&mut self, width: u32, height: u32) -> TextureHandle {
        self.add_render_target_with(width, height, crate::settings::RenderTargetDesc::default())
    }

    /// A render target with options ([`crate::RenderTargetDesc`]: HDR format, MSAA). Until the
    /// frame renders HDR targets (graphics milestone F4) every target is created as
    /// [`RENDER_TARGET_FORMAT`](crate::RENDER_TARGET_FORMAT) with one sample; the description is
    /// kept ([`Self::render_target_desc`]).
    pub fn add_render_target_with(
        &mut self,
        width: u32,
        height: u32,
        desc: crate::settings::RenderTargetDesc,
    ) -> TextureHandle {
        let h = TextureHandle::from_raw(self.textures.insert(TextureEntry {
            width: width.max(1),
            height: height.max(1),
            render_target: true,
            version: 0,
            sampler: SamplerSettings {
                filter: FilterMode::Linear,
                address: AddressMode::Clamp,
            },
            source: None,
            gpu: None,
            target_desc: Some(desc),
        }));
        self.pending_textures.push(h);
        h
    }

    /// The description a render target was created with (`None` for other textures and stale
    /// handles).
    pub fn render_target_desc(&self, handle: TextureHandle) -> Option<crate::settings::RenderTargetDesc> {
        self.textures.get(handle.raw())?.target_desc
    }

    /// Register a non-color RGBA8 image (normal, metallic-roughness, occlusion maps) as
    /// `Rgba8Unorm` with GPU mipmaps ([`TextureSource::from_image_linear`]).
    pub fn add_image_linear(&mut self, image: ImageData) -> TextureHandle {
        self.add_texture(TextureSource::from_image_linear(image))
    }

    /// Replace a sampled texture's contents (re-uploaded next frame). Returns false for stale
    /// handles and for render targets, whose contents come from their camera — resize those
    /// with [`Self::resize_render_target`].
    pub fn update_texture(&mut self, handle: TextureHandle, source: TextureSource) -> bool {
        let Some(e) = self.textures.get_mut(handle.raw()) else {
            return false;
        };
        if e.render_target {
            tracing::warn!(texture = ?handle, "update_texture on a render target is ignored; use resize_render_target");
            return false;
        }
        e.width = source.width;
        e.height = source.height;
        e.sampler = source.sampler;
        e.source = Some(source);
        self.pending_textures.push(handle);
        true
    }

    /// Replace an image texture (see [`Self::update_texture`]).
    pub fn update_image(&mut self, handle: TextureHandle, image: ImageData) -> bool {
        self.update_texture(handle, TextureSource::from_image(image))
    }

    /// Resize a render target created with [`Self::add_render_target`]. Its color and depth
    /// textures are recreated (contents cleared) on the next frame; until then cameras targeting
    /// it are skipped and materials sample white. Returns false for stale handles and for
    /// textures that are not render targets.
    pub fn resize_render_target(&mut self, handle: TextureHandle, width: u32, height: u32) -> bool {
        let Some(e) = self.textures.get_mut(handle.raw()) else {
            return false;
        };
        if !e.render_target {
            return false;
        }
        let (width, height) = (width.max(1), height.max(1));
        if (e.width, e.height) != (width, height) {
            e.width = width;
            e.height = height;
            e.gpu = None;
            self.pending_textures.push(handle);
        }
        true
    }

    /// Remove a texture.
    pub fn remove_texture(&mut self, handle: TextureHandle) -> bool {
        self.textures.remove(handle.raw()).is_some()
    }

    /// Whether the handle refers to a live texture.
    pub fn contains_texture(&self, handle: TextureHandle) -> bool {
        self.textures.get(handle.raw()).is_some()
    }

    /// Size of a texture.
    pub fn texture_size(&self, handle: TextureHandle) -> Option<(u32, u32)> {
        self.textures.get(handle.raw()).map(|e| (e.width, e.height))
    }

    /// Number of live textures.
    pub fn texture_count(&self) -> usize {
        self.textures.len()
    }

    /// Register a [`CustomShader`] for materials ([`crate::Material::with_shader`]). Its
    /// pipelines are built on first use.
    pub fn add_shader(&mut self, shader: CustomShader) -> ShaderHandle {
        ShaderHandle::from_raw(self.shaders.insert(ShaderEntry {
            shader: Arc::new(shader),
            version: 0,
        }))
    }

    /// Replace a custom shader's source (hot reload): its pipelines are rebuilt on next use.
    /// Returns false for stale handles.
    pub fn update_shader(&mut self, handle: ShaderHandle, shader: CustomShader) -> bool {
        let Some(e) = self.shaders.get_mut(handle.raw()) else {
            return false;
        };
        if *e.shader != shader {
            e.shader = Arc::new(shader);
            e.version = e.version.wrapping_add(1);
        }
        true
    }

    /// Remove a custom shader; materials still referencing it draw with the built-in shader.
    pub fn remove_shader(&mut self, handle: ShaderHandle) -> bool {
        self.shaders.remove(handle.raw()).is_some()
    }

    /// Whether the handle refers to a live custom shader.
    pub fn contains_shader(&self, handle: ShaderHandle) -> bool {
        self.shaders.get(handle.raw()).is_some()
    }

    /// Anything waiting for upload.
    pub fn has_pending(&self) -> bool {
        !self.pending_meshes.is_empty() || !self.pending_textures.is_empty() || !self.freed_allocs.is_empty()
    }

    /// Vertices / indices currently allocated in the mesh arena.
    pub fn arena_usage(&self) -> (u32, u32) {
        (self.arena.vertices.used(), self.arena.indices.used())
    }

    pub(crate) fn gpu_mesh(&self, handle: MeshHandle) -> Option<(GpuMesh, u32)> {
        let e = self.meshes.get(handle.raw())?;
        Some((e.gpu?, e.triangles))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_without_gpu() {
        let mut a = RenderAssets::default();
        let m = a.add_mesh(MeshData::cube(2.0));
        assert!(a.contains_mesh(m));
        assert_eq!(a.mesh_triangles(m), Some(12));
        assert!(!a.is_mesh_uploaded(m));
        assert!(a.has_pending());
        assert!(a.remove_mesh(m));
        assert!(!a.update_mesh(m, MeshData::plane(1.0)));
        let t = a.add_image(ImageData::filled(4, 4, [255; 4]));
        assert_eq!(a.texture_size(t), Some((4, 4)));
        let src = TextureSource::from_image(ImageData::new(256, 64));
        assert_eq!(src.full_mip_count(), 9);
        assert!(TextureSource::from_layers(&[ImageData::new(2, 2), ImageData::new(4, 4)]).is_none());
    }

    #[test]
    fn graphics_constructors() {
        let lin = TextureSource::from_image_linear(ImageData::new(2, 2));
        assert_eq!(
            (lin.format, lin.generate_mipmaps),
            (wgpu::TextureFormat::Rgba8Unorm, true)
        );
        let layers = TextureSource::from_layers_linear(&[ImageData::new(2, 2), ImageData::new(2, 2)]).unwrap();
        assert_eq!((layers.format, layers.layers), (wgpu::TextureFormat::Rgba8Unorm, 2));
        let hdr = TextureSource::from_hdr(&space3d_core::HdrImageData::filled(2, 1, [1.0, 2.0, 0.5, 1.0]));
        assert_eq!((hdr.format, hdr.data.len()), (wgpu::TextureFormat::Rgba16Float, 16));
        assert_eq!(&hdr.data[0..4], &[0x00, 0x3C, 0x00, 0x40], "1.0 and 2.0 as f16");
        let mut a = RenderAssets::default();
        let rt = a.add_render_target_with(8, 8, crate::RenderTargetDesc::hdr());
        assert_eq!(a.render_target_desc(rt), Some(crate::RenderTargetDesc::hdr()));
        let plain = a.add_render_target(8, 8);
        assert_eq!(a.render_target_desc(plain), Some(crate::RenderTargetDesc::default()));
        let lin = a.add_image_linear(ImageData::new(1, 1));
        assert_eq!(a.render_target_desc(lin), None);
        let m = a.add_mesh_with(MeshData::cube(1.0), crate::MeshExtras::new());
        assert!(a.update_mesh_with(m, MeshData::plane(1.0), crate::MeshExtras::new()));
    }
}
