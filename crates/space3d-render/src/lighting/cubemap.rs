//! Cubemaps: registration in [`RenderAssets`] (handles, sources, face sizes) and their upload
//! by the lighting (graphics spec §2.2.9, §5.2 PM3; design-materials §3.4, §6.4).
//!
//! On the GPU a cubemap is one *radiance cube*: a square 6-layer texture with a full mip chain
//! (a `TEXTURE_CUBE_MAP` on GL, which picks the target from the layer count), sampled through a
//! `Cube` view by the skybox and by environment bakes.
//! * Format: HDR sources (`Rgba16Float` and other float formats) stay `Rgba16Float` where it can
//!   be rendered (`GraphicsCaps::hdr_format`), else become `Rgba8UnormSrgb` (values clamp at 1,
//!   warned once); `Rgba8Unorm` stays linear; everything else becomes `Rgba8UnormSrgb`.
//! * Faces already in that format are uploaded straight into it; others (compressed, other
//!   float formats, or HDR without float targets) are uploaded into a temporary cube and copied
//!   face by face. A panorama is uploaded as a 2D texture (with GPU mips where it can be
//!   rendered) and converted face by face ([`Entry::Equirect`]: longitude 0 looks down −Z, +X
//!   is to its right, the top row is +Y), sampled at the mip matching the face resolution.
//! * The remaining mips are rendered face by face from the previous one (2×2 average).
//!
//! Every pass is recorded in the frame the cubemap is uploaded (never budgeted: the sky needs
//! the whole cube). Re-uploads (`update_cubemap`) replace the texture in the frame they are
//! uploaded; the old one stays in use until then.

use super::api::{CubemapHandle, CubemapSource, EnvironmentState};
use super::environment::{BakeParams, Baker, Entry, Job, Plan, Target, full_mips};
use crate::assets::{RenderAssets, TextureSource};

/// A registered cubemap.
pub(crate) struct CubemapEntry {
    /// Pixel data waiting for upload.
    pub source: Option<CubemapSource>,
    /// Face edge in texels.
    pub face_size: u32,
    /// Bumped by every update.
    pub version: u32,
    /// `Uploading` until the renderer uploaded it, then `Ready` (on the GPU; environments have
    /// their own state), or `Failed`.
    pub state: EnvironmentState,
    /// The radiance cube, once uploaded.
    pub gpu: Option<GpuCubemap>,
}

/// A cubemap on the GPU.
pub(crate) struct GpuCubemap {
    /// `Cube` view of every mip.
    pub view: wgpu::TextureView,
    /// Face edge of mip 0.
    pub size: u32,
    pub mips: u32,
    /// Unique per upload (a re-upload is a new id): environments and bind groups key on it.
    pub id: u64,
    /// GPU bytes of the cube (every mip; `RenderStats::lighting_bytes`).
    pub bytes: u64,
}

impl RenderAssets {
    /// Register a cubemap (uploaded, and converted from a panorama, by the renderer).
    pub fn add_cubemap(&mut self, source: CubemapSource) -> CubemapHandle {
        let h = CubemapHandle::from_raw(self.cubemaps.insert(CubemapEntry {
            face_size: source.face_size(),
            source: Some(source),
            version: 0,
            state: EnvironmentState::Uploading,
            gpu: None,
        }));
        self.pending_cubemaps.push(h);
        h
    }

    /// Replace a cubemap's contents (re-uploaded, environments re-baked). False for stale
    /// handles.
    pub fn update_cubemap(&mut self, h: CubemapHandle, source: CubemapSource) -> bool {
        let Some(e) = self.cubemaps.get_mut(h.raw()) else {
            return false;
        };
        e.face_size = source.face_size();
        e.source = Some(source);
        e.version = e.version.wrapping_add(1);
        e.state = EnvironmentState::Uploading;
        self.pending_cubemaps.push(h);
        true
    }

    /// Unregister a cubemap (its GPU textures are freed once no frame uses them). False for
    /// stale handles.
    pub fn remove_cubemap(&mut self, h: CubemapHandle) -> bool {
        self.cubemaps.remove(h.raw()).is_some()
    }

    /// Whether `h` is a live cubemap.
    pub fn contains_cubemap(&self, h: CubemapHandle) -> bool {
        self.cubemaps.get(h.raw()).is_some()
    }

    /// Face edge of a cubemap in texels.
    pub fn cubemap_face_size(&self, h: CubemapHandle) -> Option<u32> {
        self.cubemaps.get(h.raw()).map(|e| e.face_size)
    }

    /// The uploaded radiance cube of `h`.
    pub(crate) fn cubemap_gpu(&self, h: CubemapHandle) -> Option<&GpuCubemap> {
        self.cubemaps.get(h.raw())?.gpu.as_ref()
    }
}

/// Float (HDR) formats: kept as `Rgba16Float` where it renders.
pub(super) fn is_hdr(format: wgpu::TextureFormat) -> bool {
    use wgpu::TextureFormat as F;
    matches!(
        format,
        F::Rgba16Float | F::Rgba32Float | F::Rg11b10Ufloat | F::Rgb9e5Ufloat | F::Bc6hRgbUfloat | F::Bc6hRgbFloat
    )
}

/// Format of the radiance cube of a source in `format` (`hdr`: `Rgba16Float` renders).
pub(crate) fn radiance_format(format: wgpu::TextureFormat, hdr: bool) -> wgpu::TextureFormat {
    use wgpu::TextureFormat as F;
    if is_hdr(format) {
        if hdr { F::Rgba16Float } else { F::Rgba8UnormSrgb }
    } else if format == F::Rgba8Unorm {
        F::Rgba8Unorm
    } else {
        F::Rgba8UnormSrgb
    }
}

/// Mips of a source format can be rendered (uncompressed 8-bit, or `Rgba16Float` with `hdr`).
fn renderable(format: wgpu::TextureFormat, hdr: bool) -> bool {
    use wgpu::TextureFormat as F;
    matches!(format, F::Rgba8UnormSrgb | F::Rgba8Unorm) || (format == F::Rgba16Float && hdr)
}

/// The device can upload and filter-sample `format`.
fn sampleable(device: &wgpu::Device, format: wgpu::TextureFormat) -> bool {
    let features = device.features();
    features.contains(format.required_features())
        && matches!(
            format.sample_type(None, Some(features)),
            Some(wgpu::TextureSampleType::Float { filterable: true })
        )
        && format
            .guaranteed_format_features(features)
            .allowed_usages
            .contains(wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST)
}

/// A sampled texture for a source (`layers` 1 or 6), with `mips` levels.
fn source_texture(device: &wgpu::Device, src: &TextureSource, mips: u32, layers: u32, render: bool) -> wgpu::Texture {
    let mut usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
    if render {
        usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
    }
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d cubemap source"),
        size: wgpu::Extent3d {
            width: src.width,
            height: src.height,
            depth_or_array_layers: layers,
        },
        mip_level_count: mips,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: src.format,
        usage,
        view_formats: &[],
    })
}

/// Write the first `mips` provided mips of every layer of `src` (layer-major data). Returns the
/// bytes written.
fn write_layers(queue: &wgpu::Queue, texture: &wgpu::Texture, src: &TextureSource, mips: u32) -> Result<u64, String> {
    let provided = src.mip_level_count.max(1);
    let mut offset = 0usize;
    let mut bytes = 0u64;
    for layer in 0..src.layers {
        for mip in 0..provided {
            let (bpr, rows, pw, ph) =
                crate::texture::mip_layout(src.format, src.width, src.height, mip).ok_or("unsupported format")?;
            let size = (bpr * rows) as usize;
            let data = src
                .data
                .get(offset..offset + size)
                .ok_or_else(|| format!("cubemap data too short for layer {layer} mip {mip}"))?;
            offset += size;
            if mip >= mips {
                continue;
            }
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: mip,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(rows),
                },
                wgpu::Extent3d {
                    width: pw,
                    height: ph,
                    depth_or_array_layers: 1,
                },
            );
            bytes += size as u64;
        }
    }
    Ok(bytes)
}

/// A 2D view of mips `mips` of a single-layer texture.
fn flat_view(texture: &wgpu::Texture, mips: std::ops::Range<u32>) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("space3d panorama"),
        dimension: Some(wgpu::TextureViewDimension::D2),
        base_mip_level: mips.start,
        mip_level_count: Some(mips.end - mips.start),
        ..Default::default()
    })
}

/// An upload planned by [`plan_upload`]: the radiance cube and the steps that fill it.
pub(super) struct Upload {
    pub gpu: GpuCubemap,
    pub job: Job,
    pub bytes: u64,
    /// GPU textures created.
    pub textures: u32,
}

/// Create the radiance cube of `source` (`hdr`: `Rgba16Float` renders), write the pixels it
/// takes directly and plan the conversion and mip steps (compute dispatches with `compute`,
/// else fragment passes; panorama mips are always rendered, and only those the conversion
/// samples).
pub(super) fn plan_upload(
    baker: &mut Baker,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    source: &CubemapSource,
    hdr: bool,
    compute: bool,
    id: u64,
) -> Result<Upload, String> {
    let max = device.limits().max_texture_dimension_2d;
    let mut plan = Plan::default();
    let mut bytes = 0;
    let mut textures = 1;
    let (cube, first_generated) = match source {
        CubemapSource::Faces(src) => {
            let size = src.width;
            if src.layers != 6 || src.width != src.height || size == 0 {
                return Err(format!(
                    "cubemap faces must be 6 equal squares (got {} layers of {}x{})",
                    src.layers, src.width, src.height
                ));
            }
            if size > max {
                return Err(format!("cubemap faces of {size}² exceed the device limit {max}"));
            }
            if !sampleable(device, src.format) {
                return Err(format!(
                    "cubemap format {:?} cannot be sampled on this device",
                    src.format
                ));
            }
            let format = radiance_format(src.format, hdr);
            let mips = full_mips(size);
            let cube = Target::cube(device, "space3d cubemap", size, mips, format, compute);
            let provided = src.mip_level_count.clamp(1, mips);
            if format == src.format {
                bytes += write_layers(queue, &cube.texture, src, provided)?;
                (cube, provided)
            } else {
                // Copy through a temporary cube in the source's own format.
                let tmp = source_texture(device, src, provided, 6, false);
                textures += 1;
                bytes += write_layers(queue, &tmp, src, provided)?;
                let s = plan.source(tmp.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::Cube),
                    ..Default::default()
                }));
                plan.cube_mip(Entry::CubeCopy, &cube, 0, s, BakeParams::default());
                (cube, 1)
            }
        }
        CubemapSource::Equirect {
            texture: src,
            face_size,
        } => {
            let size = *face_size;
            if src.layers != 1 || src.width == 0 || src.height == 0 || size == 0 {
                return Err(format!(
                    "a panorama must be one non-empty layer (got {} of {}x{}) and a face size",
                    src.layers, src.width, src.height
                ));
            }
            if size > max || src.width > max || src.height > max {
                return Err(format!(
                    "cubemap of {size}² from {}x{} exceeds the device limit {max}",
                    src.width, src.height
                ));
            }
            if !sampleable(device, src.format) {
                return Err(format!(
                    "panorama format {:?} cannot be sampled on this device",
                    src.format
                ));
            }
            let format = radiance_format(src.format, hdr);
            let mips = full_mips(size);
            let cube = Target::cube(device, "space3d cubemap", size, mips, format, compute);
            // A face texel spans 90°/size, a panorama texel 360°/width: the conversion samples
            // the panorama mip where they match, so only the mips up to it are needed.
            let lod = (src.width as f32 / (4.0 * size as f32)).log2().max(0.0);
            let full = src.full_mip_count();
            let provided = src.mip_level_count.clamp(1, full);
            let needed = (lod.floor() as u32 + 2).min(full);
            let generate = renderable(src.format, hdr) && provided < needed;
            let pano_mips = if generate { needed } else { provided };
            let pano = source_texture(device, src, pano_mips, 1, generate);
            textures += 1;
            bytes += write_layers(queue, &pano, src, pano_mips)?;
            for m in provided..pano_mips {
                let s = plan.source(flat_view(&pano, m - 1..m));
                plan.render(
                    Entry::Copy2d,
                    src.format,
                    s,
                    BakeParams::default(),
                    flat_view(&pano, m..m + 1),
                );
            }
            let s = plan.source(flat_view(&pano, 0..pano_mips));
            let params = BakeParams {
                lod: lod.min((pano_mips - 1) as f32),
                ..Default::default()
            };
            plan.cube_mip(Entry::Equirect, &cube, 0, s, params);
            (cube, 1)
        }
    };
    for m in first_generated..cube.mips {
        let s = plan.source(cube.cube_view(m - 1..m));
        plan.cube_mip(Entry::CubeCopy, &cube, m, s, BakeParams::default());
    }
    Ok(Upload {
        gpu: GpuCubemap {
            view: cube.cube_view(0..cube.mips),
            size: cube.size,
            mips: cube.mips,
            id,
            bytes: cube.bytes(),
        },
        job: baker.finish(device, plan),
        bytes,
        textures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::ImageData;

    #[test]
    fn register_update_remove() {
        let mut a = RenderAssets::default();
        let faces = std::array::from_fn(|_| ImageData::new(8, 8));
        let h = a.add_cubemap(CubemapSource::from_faces(faces).unwrap());
        assert!(a.contains_cubemap(h));
        assert_eq!(a.cubemap_face_size(h), Some(8));
        assert!(a.cubemap_gpu(h).is_none());
        assert!(a.update_cubemap(h, CubemapSource::from_equirect_image(ImageData::new(64, 32))));
        assert_eq!(a.cubemap_face_size(h), Some(16));
        assert_eq!(a.pending_cubemaps.len(), 2);
        assert!(a.remove_cubemap(h) && !a.remove_cubemap(h));
        assert!(!a.contains_cubemap(h));
        assert!(!a.update_cubemap(h, CubemapSource::from_equirect_image(ImageData::new(4, 2))));
    }

    #[test]
    fn radiance_formats() {
        use wgpu::TextureFormat as F;
        assert_eq!(radiance_format(F::Rgba8UnormSrgb, true), F::Rgba8UnormSrgb);
        assert_eq!(radiance_format(F::Rgba8Unorm, false), F::Rgba8Unorm);
        assert_eq!(radiance_format(F::Rgba16Float, true), F::Rgba16Float);
        assert_eq!(radiance_format(F::Rgba16Float, false), F::Rgba8UnormSrgb);
        assert_eq!(radiance_format(F::Bc6hRgbUfloat, true), F::Rgba16Float);
        assert_eq!(radiance_format(F::Bc7RgbaUnormSrgb, true), F::Rgba8UnormSrgb);
        assert!(renderable(F::Rgba16Float, true) && !renderable(F::Rgba16Float, false));
    }
}
