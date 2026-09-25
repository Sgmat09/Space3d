//! Frame-owned textures: the scene textures of intermediate target groups (graphics spec
//! §1.3), copies of render targets that views sample while drawing into them, and the texture
//! pool (§2.6).

use super::plan::GroupTarget;
use super::views::ViewTarget;
use super::{FrameState, uniform_binding};
use crate::assets::RenderAssets;
use crate::node::FrameMode;
use crate::pipeline::{DEPTH_FORMAT, GpuMaterial, Pipelines};
use crate::stats::RenderStats;

/// What an intermediate group's textures are made for: equal keys reuse the textures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GroupKey {
    pub(super) scene_size: (u32, u32),
    pub(super) format: wgpu::TextureFormat,
    pub(super) samples: u32,
    /// Size of the 1-sample depth of display views, if the group has any.
    pub(super) display: Option<(u32, u32)>,
}

/// Textures of an intermediate target group: the scene color `S` (1 sample, sampled by the
/// composite, `COPY_SRC` for `capture_scene_hdr`), its multisampled twin, the group depth
/// (scene size and samples) and the 1-sample depth display views draw with into `D`. They are
/// (re)created only while planning a frame whose key changed, so a capture between frames reads
/// the last frame's `S`.
pub(crate) struct GroupTextures {
    pub(super) key: GroupKey,
    pub(crate) scene: wgpu::Texture,
    pub(super) scene_view: wgpu::TextureView,
    pub(super) scene_msaa: Option<wgpu::TextureView>,
    pub(super) depth: wgpu::TextureView,
    pub(super) display_depth: Option<wgpu::TextureView>,
    /// Unique per creation: bind groups sampling `S` are cached by it.
    pub(super) generation: u64,
    pub(super) last_used: u64,
    /// Bytes of all these textures.
    pub(super) bytes: u64,
    /// Mode the group rendered in when last planned.
    pub(super) mode: FrameMode,
}

fn texture_bytes(format: wgpu::TextureFormat, (w, h): (u32, u32), samples: u32) -> u64 {
    let texel = format.block_copy_size(None).unwrap_or(4) as u64;
    texel * w as u64 * h as u64 * samples as u64
}

impl GroupTextures {
    fn new(device: &wgpu::Device, key: GroupKey, msaa_depth_binding: bool, generation: u64) -> Self {
        use wgpu::TextureUsages as U;
        let texture = |label, format, (width, height): (u32, u32), samples, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let size = key.scene_size;
        let scene = texture(
            "space3d scene color",
            key.format,
            size,
            1,
            U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | U::COPY_SRC,
        );
        let scene_msaa = (key.samples > 1).then(|| {
            texture(
                "space3d scene color (msaa)",
                key.format,
                size,
                key.samples,
                U::RENDER_ATTACHMENT,
            )
            .create_view(&Default::default())
        });
        // Sampleable at 1 sample, multisampled only where it can be resolved (spec §1.6).
        let depth_usage = if key.samples == 1 || msaa_depth_binding {
            U::RENDER_ATTACHMENT | U::TEXTURE_BINDING
        } else {
            U::RENDER_ATTACHMENT
        };
        let depth = texture("space3d scene depth", DEPTH_FORMAT, size, key.samples, depth_usage);
        let display_depth = key.display.map(|d| {
            texture(
                "space3d display depth",
                DEPTH_FORMAT,
                d,
                1,
                U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
            )
            .create_view(&Default::default())
        });
        let mut bytes = texture_bytes(key.format, size, 1) + texture_bytes(DEPTH_FORMAT, size, key.samples);
        if key.samples > 1 {
            bytes += texture_bytes(key.format, size, key.samples);
        }
        if let Some(d) = key.display {
            bytes += texture_bytes(DEPTH_FORMAT, d, 1);
        }
        Self {
            key,
            scene_view: scene.create_view(&Default::default()),
            scene,
            scene_msaa,
            depth: depth.create_view(&Default::default()),
            display_depth,
            generation,
            last_used: 0,
            bytes,
            mode: FrameMode::Direct,
        }
    }
}

impl FrameState {
    /// The textures of every intermediate group of this frame's plan, created where missing or
    /// made for another key (only here, at the start of a frame). Textures of groups not
    /// planned for 3 frames are freed. Counts `textures_created` and `target_bytes`.
    pub(super) fn prepare_group_textures(
        &mut self,
        device: &wgpu::Device,
        frame_index: u64,
        msaa_depth_binding: bool,
        stats: &mut RenderStats,
    ) {
        let f = self;
        for g in &f.groups {
            if let Some(t) = f.group_textures.get_mut(&g.target)
                && !g.intermediate()
            {
                t.mode = FrameMode::Direct;
            }
            if !g.intermediate() {
                continue;
            }
            let key = GroupKey {
                scene_size: g.scene_size,
                format: g.scene_format,
                samples: g.samples,
                display: g.display_views.then_some(g.display_size),
            };
            let entry = f.group_textures.get(&g.target);
            if entry.is_none_or(|t| t.key != key) {
                f.texture_generation += 1;
                let t = GroupTextures::new(device, key, msaa_depth_binding, f.texture_generation);
                stats.textures_created += 2 + (key.samples > 1) as u32 + key.display.is_some() as u32;
                f.group_textures.insert(g.target, t);
            }
            let t = f.group_textures.get_mut(&g.target).expect("inserted");
            t.last_used = frame_index;
            t.mode = g.mode;
            stats.target_bytes += t.bytes;
        }
        f.group_textures.retain(|_, t| t.last_used + 3 > frame_index);
    }

    /// The last frame's textures of `target`'s group, if it rendered through a scene texture.
    pub(crate) fn intermediate_textures(&self, target: GroupTarget) -> Option<&GroupTextures> {
        self.group_textures.get(&target).filter(|t| t.mode != FrameMode::Direct)
    }
}

/// The single-sample depth a multisampled group's depth is resolved into for scene-depth
/// readers (graphics spec §1.6: `depth_1x`), by target. Created at the start of a frame that
/// needs it, freed 3 frames after the last.
pub(crate) struct ResolvedDepth {
    pub(super) view: wgpu::TextureView,
    pub(super) size: (u32, u32),
    pub(super) last_used: u64,
}

impl FrameState {
    /// The resolved depth of `target`'s group at `size`, (re)created only here (while
    /// preparing a frame). Counts `textures_created` and `target_bytes`.
    pub(super) fn ensure_resolved_depth(
        &mut self,
        device: &wgpu::Device,
        target: GroupTarget,
        size: (u32, u32),
        frame_index: u64,
        stats: &mut RenderStats,
    ) {
        let fresh = self.resolved_depth.get(&target).is_none_or(|d| d.size != size);
        if fresh {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("space3d resolved depth"),
                size: wgpu::Extent3d {
                    width: size.0.max(1),
                    height: size.1.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            stats.textures_created += 1;
            self.resolved_depth.insert(
                target,
                ResolvedDepth {
                    view: texture.create_view(&Default::default()),
                    size,
                    last_used: frame_index,
                },
            );
        }
        let d = self.resolved_depth.get_mut(&target).expect("inserted");
        if d.last_used != frame_index || fresh {
            stats.target_bytes += texture_bytes(DEPTH_FORMAT, size, 1);
        }
        d.last_used = frame_index;
    }

    /// The resolved depth of `target`'s group, if it has one of `size`.
    pub(super) fn resolved_depth(&self, target: GroupTarget, size: (u32, u32)) -> Option<&wgpu::TextureView> {
        self.resolved_depth
            .get(&target)
            .filter(|d| d.size == size)
            .map(|d| &d.view)
    }

    /// Free resolved depths unused for 3 frames.
    pub(super) fn evict_resolved_depth(&mut self, frame_index: u64) {
        self.resolved_depth.retain(|_, d| d.last_used + 3 > frame_index);
    }
}

/// Copy of a render target sampled by materials drawn into that same target.
pub(super) struct Feedback {
    /// `TextureEntry::version` of the render target it was created for.
    pub(super) version: u32,
    pub(super) size: (u32, u32),
    pub(super) format: wgpu::TextureFormat,
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
                    if o.get().version == e.version && o.get().size == size && o.get().format == g.format =>
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
                        format: g.format,
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
                        format: g.format,
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
