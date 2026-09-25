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

/// What a pooled texture is: textures with equal descriptions are interchangeable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PoolDesc {
    pub size: (u32, u32),
    pub format: wgpu::TextureFormat,
    pub mips: u32,
    pub usage: wgpu::TextureUsages,
}

/// A pooled texture: the texture, a view of every mip (created once) and of the whole chain.
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PoolTexture {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub mips: Vec<wgpu::TextureView>,
    /// Unique per created texture: bind groups sampling it are cached by it.
    pub generation: u64,
}

struct PoolEntry {
    desc: PoolDesc,
    texture: PoolTexture,
    in_use: bool,
    last_used: u64,
}

/// The one texture pool of the frame (post bloom chains, the LDR intermediate, the downlevel
/// log-luminance chain): textures are handed out in request order, released at the end of the
/// post group that acquired them and evicted after 3 frames without use. The steady state
/// creates none.
#[derive(Default)]
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct TexturePool {
    entries: Vec<PoolEntry>,
    frame: u64,
    generation: u64,
    created: u32,
}

#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
impl TexturePool {
    /// Start frame `frame` (eviction age).
    pub fn begin_frame(&mut self, frame: u64) {
        self.frame = frame;
    }

    /// A free texture matching `desc` (created if there is none); returns its index for
    /// [`Self::get`]. Held until [`Self::release_all`].
    pub fn acquire(&mut self, device: &wgpu::Device, desc: PoolDesc) -> usize {
        let frame = self.frame;
        if let Some(i) = self.entries.iter().position(|e| !e.in_use && e.desc == desc) {
            let e = &mut self.entries[i];
            e.in_use = true;
            e.last_used = frame;
            return i;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("space3d pooled texture"),
            size: wgpu::Extent3d {
                width: desc.size.0.max(1),
                height: desc.size.1.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: desc.mips.max(1),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: desc.format,
            usage: desc.usage,
            view_formats: &[],
        });
        let mips = (0..desc.mips.max(1))
            .map(|m| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: m,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        self.generation += 1;
        self.created += 1;
        self.entries.push(PoolEntry {
            desc,
            texture: PoolTexture {
                view: texture.create_view(&Default::default()),
                texture,
                mips,
                generation: self.generation,
            },
            in_use: true,
            last_used: frame,
        });
        self.entries.len() - 1
    }

    /// The texture acquired as `index`.
    pub fn get(&self, index: usize) -> &PoolTexture {
        &self.entries[index].texture
    }

    /// Release every acquired texture (end of a post group).
    pub fn release_all(&mut self) {
        for e in &mut self.entries {
            e.in_use = false;
        }
    }

    /// Drop textures unused for 3 frames.
    pub fn evict(&mut self) {
        let frame = self.frame;
        self.entries.retain(|e| e.in_use || e.last_used + 3 > frame);
    }

    /// Textures created since the last call (`RenderStats::textures_created`).
    pub fn take_created(&mut self) -> u32 {
        std::mem::take(&mut self.created)
    }
}
