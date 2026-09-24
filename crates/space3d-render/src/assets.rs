//! [`RenderAssets`]: CPU-side registration of meshes and textures, uploaded lazily.
//!
//! Handles are allocated immediately (no GPU needed), so games can create assets in plugin
//! `build` or `Startup` before a window/renderer exists. The renderer uploads pending
//! assets at the start of every frame (or on [`crate::Renderer::upload_assets`]) and drops the
//! CPU copies afterwards (bounds are kept for culling).

use crate::arena::{ArenaAlloc, MeshArena};
use crate::culling::{Aabb, Sphere};
use crate::handles::{MeshHandle, SlotMap, TextureHandle};
use glam::Vec3;
use space3d_core::{ImageData, MeshData};

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
    /// D2Array view for sampling.
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    /// Single-layer D2 view (render targets).
    pub target_view: Option<wgpu::TextureView>,
    /// Depth for render targets.
    pub depth_view: Option<wgpu::TextureView>,
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
}

/// Resource holding all meshes and textures (CPU registration + GPU storage).
#[derive(Default)]
pub struct RenderAssets {
    pub(crate) meshes: SlotMap<MeshEntry>,
    pub(crate) textures: SlotMap<TextureEntry>,
    pub(crate) pending_meshes: Vec<MeshHandle>,
    pub(crate) pending_textures: Vec<TextureHandle>,
    pub(crate) freed_allocs: Vec<ArenaAlloc>,
    pub(crate) arena: MeshArena,
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
        }));
        self.pending_textures.push(h);
        h
    }

    /// Create a color texture cameras can render into ([`crate::RenderTarget::Texture`]) and
    /// materials can sample.
    pub fn add_render_target(&mut self, width: u32, height: u32) -> TextureHandle {
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
        }));
        self.pending_textures.push(h);
        h
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
}
