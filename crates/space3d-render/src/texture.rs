//! Texture upload (`queue.write_texture` with block-aware row pitch), GPU mipmap generation,
//! render-target textures and sampler caching.

use crate::assets::{AddressMode, FilterMode, GpuTexture, SamplerSettings, TextureEntry, TextureSource};
use crate::handles::FastMap;
use crate::pipeline::{DEPTH_FORMAT, OFFSCREEN_FORMAT, Pipelines, sampled_view_dimension};

/// Samplers shared by all textures with equal settings.
#[derive(Default)]
pub(crate) struct SamplerCache {
    map: FastMap<SamplerSettings, wgpu::Sampler>,
}

impl SamplerCache {
    pub fn get(&mut self, device: &wgpu::Device, s: SamplerSettings) -> wgpu::Sampler {
        self.map
            .entry(s)
            .or_insert_with(|| {
                let filter = match s.filter {
                    FilterMode::Linear => wgpu::FilterMode::Linear,
                    FilterMode::Nearest => wgpu::FilterMode::Nearest,
                };
                let address = match s.address {
                    AddressMode::Repeat => wgpu::AddressMode::Repeat,
                    AddressMode::Clamp => wgpu::AddressMode::ClampToEdge,
                };
                device.create_sampler(&wgpu::SamplerDescriptor {
                    label: Some("space3d texture sampler"),
                    address_mode_u: address,
                    address_mode_v: address,
                    address_mode_w: address,
                    mag_filter: filter,
                    min_filter: filter,
                    mipmap_filter: match s.filter {
                        FilterMode::Linear => wgpu::MipmapFilterMode::Linear,
                        FilterMode::Nearest => wgpu::MipmapFilterMode::Nearest,
                    },
                    ..Default::default()
                })
            })
            .clone()
    }
}

/// Bytes of one mip level of one layer (and its row pitch / row count in blocks).
pub(crate) fn mip_layout(
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    mip: u32,
) -> Option<(u32, u32, u32, u32)> {
    let (bw, bh) = format.block_dimensions();
    let block_size = format.block_copy_size(None)?;
    let w = (width >> mip).max(1);
    let h = (height >> mip).max(1);
    let blocks_w = w.div_ceil(bw);
    let blocks_h = h.div_ceil(bh);
    let bytes_per_row = blocks_w * block_size;
    // Physical (block-aligned) extent required for compressed copies.
    Some((bytes_per_row, blocks_h, blocks_w * bw, blocks_h * bh))
}

/// Format capabilities wgpu-core validates against. It uses the WebGPU-guaranteed set, or the
/// adapter-specific set on downlevel backends (GLES/WebGL2); the intersection is valid in both.
fn format_features(
    adapter: &wgpu::Adapter,
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> wgpu::TextureFormatFeatures {
    let guaranteed = format.guaranteed_format_features(device.features());
    let specific = adapter.get_texture_format_features(format);
    wgpu::TextureFormatFeatures {
        allowed_usages: guaranteed.allowed_usages & specific.allowed_usages,
        flags: guaranteed.flags & specific.flags,
    }
}

/// Whether materials can use this format: the device has its features, it can be uploaded and
/// sampled, and it is filterable float — the material binding is `texture_2d_array<f32>` with a
/// filtering sampler, so integer, depth and (without `FLOAT32_FILTERABLE`) 32-bit float formats
/// would fail bind-group validation.
pub(crate) fn format_supported(adapter: &wgpu::Adapter, device: &wgpu::Device, format: wgpu::TextureFormat) -> bool {
    use wgpu::TextureUsages as U;
    if !device.features().contains(format.required_features()) {
        return false;
    }
    let filterable_float = match format.sample_type(None, Some(device.features())) {
        Some(wgpu::TextureSampleType::Float { filterable }) => filterable,
        _ => false,
    };
    let features = format_features(adapter, device, format);
    filterable_float
        && features.allowed_usages.contains(U::TEXTURE_BINDING | U::COPY_DST)
        && features.flags.contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
}

/// Whether mips of a [`format_supported`] format can be generated with the linear blit: it
/// must also be renderable (e.g. `Rgba16Float` is not on GLES without half-float support).
fn mip_renderable(adapter: &wgpu::Adapter, device: &wgpu::Device, format: wgpu::TextureFormat) -> bool {
    !format.is_compressed()
        && format_features(adapter, device, format)
            .allowed_usages
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
}

/// Create the GPU texture for an entry. Returns bytes uploaded.
pub(crate) fn create_gpu_texture(
    adapter: &wgpu::Adapter,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &mut Pipelines,
    samplers: &mut SamplerCache,
    entry: &mut TextureEntry,
) -> Result<u64, String> {
    let sampler = samplers.get(device, entry.sampler);
    if entry.render_target {
        let size = wgpu::Extent3d {
            width: entry.width,
            height: entry.height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("space3d render target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("space3d render target depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        // One D2 view serves as color attachment and for sampling.
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2),
            ..Default::default()
        });
        entry.gpu = Some(GpuTexture {
            view: view.clone(),
            layered: false,
            target_view: Some(view),
            depth_view: Some(depth.create_view(&Default::default())),
            sampler,
        });
        entry.version = entry.version.wrapping_add(1);
        return Ok(0);
    }

    let Some(src) = entry.source.take() else {
        return Ok(0);
    };
    let result = upload_source(adapter, device, queue, encoder, pipelines, &src, sampler);
    match result {
        Ok((gpu, bytes)) => {
            entry.gpu = Some(gpu);
            entry.version = entry.version.wrapping_add(1);
            Ok(bytes)
        }
        Err(e) => Err(e),
    }
}

/// A block-compressed texture's mip 0 must be a whole number of blocks (smaller mips may be
/// partial blocks). wgpu rejects anything else in `create_texture` — a validation error that
/// the default error handler turns into a panic — so such sources are refused up front.
pub(crate) fn check_block_alignment(format: wgpu::TextureFormat, width: u32, height: u32) -> Result<(), String> {
    let (bw, bh) = format.block_dimensions();
    if width.is_multiple_of(bw) && height.is_multiple_of(bh) {
        Ok(())
    } else {
        Err(format!(
            "texture {width}x{height} is not a multiple of {format:?}'s {bw}x{bh} blocks"
        ))
    }
}

fn upload_source(
    adapter: &wgpu::Adapter,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &mut Pipelines,
    src: &TextureSource,
    sampler: wgpu::Sampler,
) -> Result<(GpuTexture, u64), String> {
    let format = src.format;
    if !format_supported(adapter, device, format) {
        return Err(format!(
            "texture format {format:?} cannot be sampled by materials on this device"
        ));
    }
    if src.width == 0 || src.height == 0 || src.layers == 0 {
        return Err("texture has zero size".into());
    }
    check_block_alignment(format, src.width, src.height)?;
    let limits = device.limits();
    if src.width > limits.max_texture_dimension_2d || src.height > limits.max_texture_dimension_2d {
        return Err(format!("texture {}x{} exceeds device limit", src.width, src.height));
    }
    let gl = adapter.get_info().backend == wgpu::Backend::Gl;
    let allocated_layers = allocated_layers(src.layers, src.width, src.height, gl);
    if allocated_layers > limits.max_texture_array_layers {
        return Err(format!(
            "texture has {} layers; the device allows {}",
            src.layers, limits.max_texture_array_layers
        ));
    }
    let provided_mips = src.mip_level_count.max(1);
    let full = src.full_mip_count();
    let generate = src.generate_mipmaps && provided_mips < full && mip_renderable(adapter, device, format);
    let mip_count = if generate { full } else { provided_mips.min(full) };

    let mut usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
    if generate {
        usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d texture"),
        size: wgpu::Extent3d {
            width: src.width,
            height: src.height,
            depth_or_array_layers: allocated_layers,
        },
        mip_level_count: mip_count,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });

    // Layer-major data: every provided mip of layer 0, then layer 1, ...
    let mut offset = 0usize;
    let mut bytes = 0u64;
    for layer in 0..src.layers {
        for mip in 0..provided_mips.min(mip_count) {
            let (bpr, rows, pw, ph) = mip_layout(format, src.width, src.height, mip).ok_or("unsupported format")?;
            let size = (bpr * rows) as usize;
            let Some(data) = src.data.get(offset..offset + size) else {
                return Err(format!("texture data too short for layer {layer} mip {mip}"));
            };
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
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
            offset += size;
            bytes += size as u64;
        }
    }

    if generate {
        generate_mips(
            device,
            encoder,
            pipelines,
            &texture,
            format,
            src.layers,
            provided_mips,
            mip_count,
        );
    }

    let layered = src.layers > 1;
    Ok((
        GpuTexture {
            view: texture.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(sampled_view_dimension(layered)),
                ..Default::default()
            }),
            layered,
            sampler,
            target_view: None,
            depth_view: None,
        },
        bytes,
    ))
}

/// Layers to allocate for a sampled texture of `layers` layers. wgpu's GL backend picks a
/// texture's GL target from its descriptor alone and makes every square texture whose layer
/// count is a multiple of 6 a cube map (array), which the forward shader's `sampler2DArray`
/// reads as black. There such arrays get one extra, unused layer.
pub(crate) fn allocated_layers(layers: u32, width: u32, height: u32, gl: bool) -> u32 {
    if gl && width == height && layers.is_multiple_of(6) {
        layers + 1
    } else {
        layers
    }
}

/// Downsample mip `m-1` into mip `m` for every layer with a linear blit.
///
/// The source is sampled through a view of the texture's own kind — `D2` for single-layer
/// textures, `D2Array` over all layers for arrays (the layer is picked by the draw's vertex
/// range, see `blit.wgsl`). wgpu's GL backend binds a sampled texture with the target implied
/// by its layer count and ignores a view's base layer, so a `D2` view of one layer of an array
/// samples black there. Each destination is a single-layer `D2` render attachment.
#[allow(clippy::too_many_arguments)]
fn generate_mips(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &mut Pipelines,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
    layers: u32,
    first_generated: u32,
    mip_count: u32,
) {
    let layered = layers > 1;
    let sampler = pipelines.blit_sampler.clone();
    let (pipeline, bgl) = pipelines.blit(device, format, layered);
    let (pipeline, bgl) = (pipeline.clone(), bgl.clone());
    let source = |mip: u32| {
        texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("space3d mip source"),
            dimension: Some(sampled_view_dimension(layered)),
            base_mip_level: mip,
            mip_level_count: Some(1),
            ..Default::default()
        })
    };
    let target = |mip: u32, layer: u32| {
        texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("space3d mip target"),
            dimension: Some(wgpu::TextureViewDimension::D2),
            base_mip_level: mip,
            mip_level_count: Some(1),
            base_array_layer: layer,
            array_layer_count: Some(1),
            ..Default::default()
        })
    };
    for mip in first_generated.max(1)..mip_count {
        let src = source(mip - 1);
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("space3d mipgen"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        for layer in 0..layers {
            let dst = target(mip, layer);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("space3d mipgen"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.draw(layer * 3..layer * 3 + 3, 0..1);
        }
    }
}

/// 1x1 white texture (single-layer `D2` view) used when a material has no (or a missing)
/// texture.
pub(crate) fn white_texture(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d white"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: OFFSCREEN_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &[255, 255, 255, 255],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mip_layouts() {
        let f = wgpu::TextureFormat::Rgba8UnormSrgb;
        assert_eq!(mip_layout(f, 256, 128, 0), Some((1024, 128, 256, 128)));
        assert_eq!(mip_layout(f, 256, 128, 8), Some((4, 1, 1, 1)));
        let bc = wgpu::TextureFormat::Bc7RgbaUnormSrgb;
        // 4x4 blocks of 16 bytes; a 2x2 mip still occupies one block.
        assert_eq!(mip_layout(bc, 64, 64, 0), Some((256, 16, 64, 64)));
        assert_eq!(mip_layout(bc, 64, 64, 5), Some((16, 1, 4, 4)));
    }

    #[test]
    fn compressed_sizes_must_be_whole_blocks() {
        use wgpu::TextureFormat as F;
        assert!(check_block_alignment(F::Bc1RgbaUnormSrgb, 30, 30).is_err());
        assert!(check_block_alignment(F::Bc7RgbaUnormSrgb, 64, 6).is_err());
        assert!(check_block_alignment(F::Bc7RgbaUnormSrgb, 12, 64).is_ok());
        let astc = F::Astc {
            block: wgpu::AstcBlock::B6x5,
            channel: wgpu::AstcChannel::UnormSrgb,
        };
        assert!(check_block_alignment(astc, 12, 10).is_ok());
        assert!(check_block_alignment(astc, 10, 12).is_err());
        assert!(check_block_alignment(F::Rgba8UnormSrgb, 30, 7).is_ok());
    }

    /// Regression: on GL, square arrays of 6k layers became cube maps and sampled black.
    #[test]
    fn cube_compatible_arrays_are_padded_on_gl() {
        assert_eq!(allocated_layers(6, 16, 16, true), 7);
        assert_eq!(allocated_layers(12, 16, 16, true), 13);
        assert_eq!(allocated_layers(6, 16, 8, true), 6);
        assert_eq!(allocated_layers(5, 16, 16, true), 5);
        assert_eq!(allocated_layers(1, 16, 16, true), 1);
        assert_eq!(allocated_layers(6, 16, 16, false), 6);
    }
}
