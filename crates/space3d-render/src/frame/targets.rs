//! Frame-owned textures: copies of render targets that views sample while drawing into them.
//!
//! Grows into the per-group scene targets and the texture pool (graphics spec §2.6).

use super::views::ViewTarget;
use super::{FrameState, uniform_binding};
use crate::assets::RenderAssets;
use crate::pipeline::{GpuMaterial, OFFSCREEN_FORMAT, Pipelines};

/// Copy of a render target sampled by materials drawn into that same target.
pub(super) struct Feedback {
    /// `TextureEntry::version` of the render target it was created for.
    pub(super) version: u32,
    pub(super) size: (u32, u32),
    pub(super) texture: wgpu::Texture,
    pub(super) view: wgpu::TextureView,
    /// Material bind group sampling `view` (and the materials buffer generation it binds).
    pub(super) bind_group: Option<(wgpu::BindGroup, u32)>,
    pub(super) last_used: u64,
}

impl FrameState {
    /// Copies of render targets sampled by the views drawing into them (see
    /// `ViewData::feedback`), and their material bind groups.
    pub(super) fn prepare_feedback(
        &mut self,
        device: &wgpu::Device,
        assets: &RenderAssets,
        pipelines: &Pipelines,
        frame_index: u64,
    ) {
        let f = self;
        let mgen = f.materials_buf.gpu.generation;
        // Views sampling their own render target bind a copy of it (see `ViewData::feedback`).
        for v in 0..f.views.len() {
            let ViewTarget::Texture { handle, .. } = f.views[v].target else {
                continue;
            };
            if !f.views[v].feedback {
                continue;
            }
            let Some(e) = assets.textures.get(handle.raw()) else {
                continue;
            };
            let Some(g) = &e.gpu else {
                continue;
            };
            let size = (e.width, e.height);
            let fb = match f.feedback.entry(handle) {
                std::collections::hash_map::Entry::Occupied(o)
                    if o.get().version == e.version && o.get().size == size =>
                {
                    o.into_mut()
                }
                entry => {
                    let texture = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("space3d render target feedback"),
                        size: wgpu::Extent3d {
                            width: size.0,
                            height: size.1,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: OFFSCREEN_FORMAT,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    let view = texture.create_view(&wgpu::TextureViewDescriptor {
                        dimension: Some(wgpu::TextureViewDimension::D2),
                        ..Default::default()
                    });
                    let fresh = Feedback {
                        version: e.version,
                        size,
                        texture,
                        view,
                        bind_group: None,
                        last_used: frame_index,
                    };
                    match entry {
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            o.insert(fresh);
                            o.into_mut()
                        }
                        std::collections::hash_map::Entry::Vacant(v) => v.insert(fresh),
                    }
                }
            };
            fb.last_used = frame_index;
            if fb.bind_group.as_ref().is_none_or(|(_, gen_)| *gen_ != mgen) {
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("space3d material (render target feedback)"),
                    // Render targets (and their copies) are single-layer.
                    layout: pipelines.material_bgl(false),
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform_binding(f.materials_buf.buffer(), std::mem::size_of::<GpuMaterial>()),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&fb.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&g.sampler),
                        },
                    ],
                });
                fb.bind_group = Some((bg, mgen));
            }
        }
        // Free copies of targets that stopped sampling themselves (or were removed).
        f.feedback.retain(|_, fb| fb.last_used + 60 > frame_index);
    }
}
