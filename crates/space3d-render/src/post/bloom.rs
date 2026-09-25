//! Bloom (graphics spec §5.2 PO1; design-post §5.3): chain sizing, the per-pass uniforms, the
//! pipelines and the CPU mirror of the soft threshold. `shaders/post/bloom.wgsl` holds the
//! passes; [`super::PostState`] plans the chain per post group and records it before the
//! composite, which adds level 0 after exposure (graphics spec §1.9).
//!
//! A post group's chain is one pooled texture (`TexturePool`, per-level views) of
//! `GraphicsCaps::bloom_format`: level 0 is the group's rect in the scene texture at half
//! resolution, capped at `Bloom::max_mip_height`; every further level halves it. Passes: one
//! prefilter (S → level 0: exposure, soft threshold, Karis average), `mips − 1` downsamples and
//! `mips − 1` upsamples blended by `scatter` — 11 render passes for the default 6 levels.

use super::PostEncode;
use super::api::Bloom;
use crate::frame::PoolDesc;
use crate::gpu::{TrackedBuffer, Uploader};
use crate::handles::FastMap;
use crate::node::FULLSCREEN_WGSL;
use crate::output::OUTPUT_WGSL;
use bytemuck::{Pod, Zeroable};
#[cfg(test)]
use glam::Vec3;
use std::borrow::Cow;

/// Source of the bloom passes (`shaders/post/bloom.wgsl`, after the output helpers and the
/// full-screen triangle).
pub(crate) const BLOOM_WGSL: &str = include_str!("../shaders/post/bloom.wgsl");

pub(crate) fn bloom_source() -> String {
    format!("{OUTPUT_WGSL}\n{FULLSCREEN_WGSL}\n{BLOOM_WGSL}")
}

/// Most levels a chain has (`Bloom::mip_count` is clamped to 1..=8).
pub(crate) const MAX_MIPS: u32 = 8;

/// Render passes of a chain with `mips` levels: the prefilter, the downsamples and the
/// upsamples.
pub(crate) const fn pass_count(mips: u32) -> u32 {
    2 * mips - 1
}

/// Matches `BloomPass` in bloom.wgsl: one aligned uniform slot per pass.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub(crate) struct GpuBloomPass {
    /// Source rect in pixels of the source texture (prefilter: the post group's rect in `S`;
    /// chain passes: the whole source level).
    pub src_rect: [f32; 4],
    /// 1 / source width, 1 / source height, tap step in source pixels, exposure multiplier.
    pub src_texel: [f32; 4],
    /// Soft threshold: t, t − k, 2k, 0.25 / k (k = t × soft_knee; all 0 but t for a hard cut).
    pub threshold: [f32; 4],
    /// x: multiply by 2^E (auto exposure).
    pub flags: [u32; 4],
}

/// A post group's bloom chain: level 0's size, the number of levels and how many of the
/// reference chain's finest levels level 0 stands for ([`chain_size`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChainSize {
    pub size: (u32, u32),
    pub mips: u32,
    /// Finest levels of the reference chain merged into level 0 (their weights too).
    pub merged: u32,
}

/// The chain of a post group whose rect in the scene texture is `rect` (pixels).
///
/// The reference chain has `mip_count` levels (1..=8) of heights `max_mip_height / 2^k`: what a
/// rect of height `2 × max_mip_height` or more gets (half its size, scaled down to the cap, so
/// the cost stays constant at 1440p and 4K). Level `k`'s blur spans the same fraction of the
/// rect at any resolution only if its height does not depend on the rect, so a smaller rect
/// keeps the reference heights too: it drops the `n` finest levels, where `2^n` is the power of
/// two nearest to `max_mip_height / (rect height / 2)`, and level 0 stands for them (resampled
/// to `max_mip_height / 2^n`: within √2 of half the rect, never above the rect's own size;
/// their weights merge into it, [`upsample_blend`]). The halo's size and energy relative to
/// the frame are then the same at 360p, 720p and 1080p (only the tight glow of the merged
/// levels widens), and smaller rects cost less. Levels are then dropped from the coarse end
/// while the smallest one has fewer than 2 pixels on its short side. Level `k` is
/// `max(1, level0 >> k)` (the texture's mip chain).
pub(crate) fn chain_size(rect: (u32, u32), bloom: &Bloom) -> ChainSize {
    let (rw, rh) = (rect.0.max(1), rect.1.max(1));
    let cap = bloom.max_mip_height.max(1);
    let levels = bloom.mip_count.clamp(1, MAX_MIPS);
    let half = rh as f64 / 2.0;
    let merged = if half >= cap as f64 {
        0
    } else {
        ((cap as f64 / half).log2().round().max(0.0) as u32).min(levels - 1)
    };
    let height = ((cap as f64 / (1u64 << merged) as f64).round() as u32).max(1);
    // Never above the rect's own resolution (tiny rects, extreme aspects).
    let size = if height >= rh {
        (rw, rh)
    } else {
        (((rw as f64 * height as f64 / rh as f64).round() as u32).max(1), height)
    };
    let mut mips = levels - merged;
    while mips > 1 && (size.0 >> (mips - 1)).min(size.1 >> (mips - 1)) < 2 {
        mips -= 1;
    }
    ChainSize { size, mips, merged }
}

/// Blend constant of the upsample into `target` (`dst = src × c + dst × (1 − c)`): `scatter`,
/// and `scatter^(merged + 1)` into level 0, so level 0 keeps the weights of the reference
/// chain's finest levels it stands for (Σ_{k ≤ merged} (1 − s)·s^k = 1 − s^(merged + 1)) and
/// every coarser level keeps its own, (1 − s)·s^k.
pub(crate) fn upsample_blend(scatter: f32, merged: u32, target: u32) -> f32 {
    let s = scatter.clamp(0.0, 1.0);
    if target == 0 { s.powi(merged as i32 + 1) } else { s }
}

/// Size of level `level` of a chain whose level 0 is `size`.
pub(crate) fn level_size(size: (u32, u32), level: u32) -> (u32, u32) {
    ((size.0 >> level).max(1), (size.1 >> level).max(1))
}

/// The soft threshold's uniform constants (t, t − k, 2k, 0.25 / k).
pub(crate) fn threshold_constants(threshold: f32, soft_knee: f32) -> [f32; 4] {
    let t = threshold.max(0.0);
    let k = t * soft_knee.clamp(0.0, 1.0);
    if k > 0.0 {
        [t, t - k, 2.0 * k, 0.25 / k]
    } else {
        [t, t, 0.0, 0.0]
    }
}

/// CPU mirror of `bloom_threshold` in bloom.wgsl: the part of the exposed color `c` that
/// blooms. Below `threshold − knee` nothing, above `threshold + knee` everything above the
/// threshold, a quadratic knee in between (continuous at both ends); scales the whole color by
/// the ratio on its brightest channel.
#[cfg(test)]
pub(crate) fn soft_threshold(c: Vec3, threshold: f32, soft_knee: f32) -> Vec3 {
    let [t, lo, knee2, quad] = threshold_constants(threshold, soft_knee);
    let br = c.max_element();
    let rq = (br - lo).clamp(0.0, knee2);
    let rq = quad * rq * rq;
    c * (rq.max(br - t) / br.max(1e-4))
}

/// Uniforms of every pass of a chain, in recording order: the prefilter, the downsamples
/// (into level 1, 2, …) and the upsamples (into level `mips − 2`, …, 0).
///
/// `rect` is the post group's rect in the scene texture of `scene_size`; `(size, mips)` the
/// chain ([`chain_size`]); `(exposure, auto)` the linear manual exposure multiplier (the
/// compensation with auto exposure) and whether the prefilter also multiplies by 2^E.
pub(crate) fn passes(
    scene_size: (u32, u32),
    rect: [u32; 4],
    (size, mips): ((u32, u32), u32),
    bloom: &Bloom,
    (exposure, auto_exposure): (f32, bool),
    mut emit: impl FnMut(GpuBloomPass),
) {
    let texel = |(w, h): (u32, u32)| [1.0 / w.max(1) as f32, 1.0 / h.max(1) as f32];
    let [sx, sy] = texel(scene_size);
    // Half a destination texel, in source pixels: the 13 taps sit ±1 and ±2 steps away.
    let step = |src_h: f32, dst_h: u32| 0.5 * src_h / dst_h.max(1) as f32;
    emit(GpuBloomPass {
        src_rect: [rect[0] as f32, rect[1] as f32, rect[2] as f32, rect[3] as f32],
        src_texel: [sx, sy, step(rect[3] as f32, size.1), exposure],
        threshold: threshold_constants(bloom.threshold, bloom.soft_knee),
        flags: [u32::from(auto_exposure), 0, 0, 0],
    });
    let level = |l: u32| {
        let (w, h) = level_size(size, l);
        let [tx, ty] = texel((w, h));
        ([0.0, 0.0, w as f32, h as f32], tx, ty, h)
    };
    for l in 1..mips {
        let (src_rect, tx, ty, src_h) = level(l - 1);
        emit(GpuBloomPass {
            src_rect,
            src_texel: [tx, ty, step(src_h as f32, level_size(size, l).1), 1.0],
            ..GpuBloomPass::zeroed()
        });
    }
    for l in (0..mips - 1).rev() {
        let (src_rect, tx, ty, _) = level(l + 1);
        emit(GpuBloomPass {
            src_rect,
            src_texel: [tx, ty, 1.0, 1.0],
            ..GpuBloomPass::zeroed()
        });
    }
}

/// Which pass a bloom pipeline draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BloomPassKind {
    Prefilter,
    Down,
    Up,
}

/// The three bloom pipelines of one chain format.
pub(crate) struct BloomPipelines {
    pub prefilter: wgpu::RenderPipeline,
    pub down: wgpu::RenderPipeline,
    pub up: wgpu::RenderPipeline,
}

impl BloomPipelines {
    pub(crate) fn new(
        device: &wgpu::Device,
        module: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
        format: wgpu::TextureFormat,
    ) -> Self {
        let pipeline = |kind: BloomPassKind| {
            let (entry, blend) = match kind {
                BloomPassKind::Prefilter => ("fs_prefilter", None),
                BloomPassKind::Down => ("fs_down", None),
                // dst = src × scatter + dst × (1 − scatter) (scatter = the blend constant).
                BloomPassKind::Up => (
                    "fs_up",
                    Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Constant,
                            dst_factor: wgpu::BlendFactor::OneMinusConstant,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent::REPLACE,
                    }),
                ),
            };
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("space3d bloom"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        Self {
            prefilter: pipeline(BloomPassKind::Prefilter),
            down: pipeline(BloomPassKind::Down),
            up: pipeline(BloomPassKind::Up),
        }
    }

    pub(crate) fn get(&self, kind: BloomPassKind) -> &wgpu::RenderPipeline {
        match kind {
            BloomPassKind::Prefilter => &self.prefilter,
            BloomPassKind::Down => &self.down,
            BloomPassKind::Up => &self.up,
        }
    }
}

/// The bloom shader module.
pub(crate) fn module(device: &wgpu::Device) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("space3d bloom"),
        source: wgpu::ShaderSource::Wgsl(Cow::Owned(bloom_source())),
    })
}

/// A post group's bloom chain this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct BloomPlan {
    pub format: wgpu::TextureFormat,
    /// Level 0 size and level count.
    pub size: (u32, u32),
    pub mips: u32,
    /// Reference levels merged into level 0 ([`ChainSize::merged`]).
    pub merged: u32,
    /// Uniform slot of the first pass (the others follow in recording order).
    pub first_slot: u32,
    pub scatter: f32,
}

impl BloomPlan {
    /// Bytes of the chain (every level).
    pub fn bytes(&self) -> u64 {
        let texel = self.format.block_copy_size(None).unwrap_or(8) as u64;
        (0..self.mips)
            .map(|l| {
                let (w, h) = level_size(self.size, l);
                texel * w as u64 * h as u64
            })
            .sum()
    }
}

/// GPU timestamps of a chain: query `begin` at the start of its first pass, `end` at the end of
/// its last one.
pub(crate) struct ChainTimestamps<'a> {
    pub query_set: &'a wgpu::QuerySet,
    pub begin: Option<u32>,
    pub end: Option<u32>,
}

/// Bloom resources shared by every post group.
pub(super) struct BloomState {
    /// `GpuBloomPass` of every pass planned this frame, one aligned slot each.
    pub params: TrackedBuffer,
    pub slot: usize,
    /// Slots written this frame.
    pub slots: u32,
    /// Group 0: pass uniforms (dynamic offset) + linear-clamp sampler; group 1: the source
    /// level (or `S`) and the exposure texture.
    params_bgl: wgpu::BindGroupLayout,
    source_bgl: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    pipelines: FastMap<wgpu::TextureFormat, BloomPipelines>,
    params_bg: Option<(wgpu::BindGroup, u32)>,
    /// Source bind groups by (0 = scene texture / 1 = pooled chain, its generation, level),
    /// with the frame they were last used.
    pub sources: FastMap<(u8, u64, u32), (wgpu::BindGroup, u64)>,
}

impl BloomState {
    pub fn new(device: &wgpu::Device, align: usize) -> Self {
        let params_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d bloom params"),
            entries: &[
                crate::pipeline::uniform_entry(
                    0,
                    wgpu::ShaderStages::FRAGMENT,
                    true,
                    std::mem::size_of::<GpuBloomPass>() as u64,
                ),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let texture = |binding, filterable| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let source_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d bloom source"),
            entries: &[texture(0, true), texture(1, false)],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("space3d bloom"),
            bind_group_layouts: &[Some(&params_bgl), Some(&source_bgl)],
            immediate_size: 0,
        });
        Self {
            params: TrackedBuffer::new("space3d bloom params", wgpu::BufferUsages::UNIFORM),
            slot: std::mem::size_of::<GpuBloomPass>().next_multiple_of(align),
            slots: 0,
            params_bgl,
            source_bgl,
            layout,
            module: module(device),
            pipelines: FastMap::default(),
            params_bg: None,
            sources: FastMap::default(),
        }
    }

    /// The pipelines of a chain in `format` (created on first use).
    pub fn pipelines(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> &BloomPipelines {
        let (module, layout) = (&self.module, &self.layout);
        self.pipelines
            .entry(format)
            .or_insert_with(|| BloomPipelines::new(device, module, layout, format))
    }

    /// Upload this frame's pass uniforms (after every plan, before any encode).
    pub fn flush(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        up: &mut Uploader,
        sampler: &wgpu::Sampler,
    ) {
        if self.slots == 0 && self.params_bg.is_some() {
            return;
        }
        let mut reallocs = 0;
        self.params
            .ensure_len(std::mem::size_of::<GpuBloomPass>(), &mut reallocs);
        self.params.flush(device, encoder, up);
        let generation = self.params.gpu.generation;
        if self.params_bg.as_ref().is_none_or(|(_, g)| *g != generation) {
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d bloom params"),
                layout: &self.params_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: self.params.buffer(),
                            offset: 0,
                            size: wgpu::BufferSize::new(std::mem::size_of::<GpuBloomPass>() as u64),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            });
            self.params_bg = Some((bg, generation));
        }
    }

    /// Record a post group's chain (prefilter, downsamples, upsamples) from `scene` (the scene
    /// texture, bind groups cached by `scene_generation`) into a pooled texture. Returns level
    /// 0's view and the texture's generation for the composite.
    /// `timestamps`: GPU timestamps written at the start of the first pass and the end of the
    /// last one.
    pub fn encode(
        &mut self,
        cx: &mut PostEncode<'_>,
        (scene, scene_generation): (&wgpu::TextureView, u64),
        plan: &BloomPlan,
        exposure: &wgpu::TextureView,
        frame: u64,
        timestamps: Option<ChainTimestamps<'_>>,
    ) -> Option<(wgpu::TextureView, u64)> {
        use wgpu::TextureUsages as U;
        let (params_bg, _) = self.params_bg.as_ref()?;
        let pipelines = {
            let (module, layout) = (&self.module, &self.layout);
            self.pipelines
                .entry(plan.format)
                .or_insert_with(|| BloomPipelines::new(cx.device, module, layout, plan.format))
        };
        let index = cx.pool.acquire(
            cx.device,
            PoolDesc {
                size: plan.size,
                format: plan.format,
                mips: plan.mips,
                usage: U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
            },
        );
        let chain = cx.pool.get(index);
        let (sources, source_bgl) = (&mut self.sources, &self.source_bgl);
        let last = pass_count(plan.mips) - 1;
        for j in 0..=last {
            // Prefilter into level 0, downsample into 1..mips, upsample into mips-2..=0.
            let (kind, source, target) = if j == 0 {
                (BloomPassKind::Prefilter, None, 0)
            } else if j < plan.mips {
                (BloomPassKind::Down, Some(j - 1), j)
            } else {
                let target = 2 * plan.mips - 2 - j;
                (BloomPassKind::Up, Some(target + 1), target)
            };
            let key = match source {
                None => (0, scene_generation, 0),
                Some(level) => (1, chain.generation, level),
            };
            let view = source.map_or(scene, |level| &chain.mips[level as usize]);
            let bg = sources.entry(key).or_insert_with(|| {
                let bg = cx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("space3d bloom source"),
                    layout: source_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(exposure),
                        },
                    ],
                });
                (bg, frame)
            });
            bg.1 = frame;
            let load = match kind {
                BloomPassKind::Up => wgpu::LoadOp::Load,
                _ => wgpu::LoadOp::Clear(wgpu::Color::BLACK),
            };
            let mut pass = cx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("space3d bloom"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &chain.mips[target as usize],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                timestamp_writes: timestamps.as_ref().and_then(|t| {
                    let begin = t.begin.filter(|_| j == 0);
                    let end = t.end.filter(|_| j == last);
                    (begin.is_some() || end.is_some()).then_some(wgpu::RenderPassTimestampWrites {
                        query_set: t.query_set,
                        beginning_of_pass_write_index: begin,
                        end_of_pass_write_index: end,
                    })
                }),
                ..Default::default()
            });
            pass.set_pipeline(pipelines.get(kind));
            pass.set_bind_group(0, params_bg, &[((plan.first_slot + j) as usize * self.slot) as u32]);
            pass.set_bind_group(1, &bg.0, &[]);
            if kind == BloomPassKind::Up {
                let s = upsample_blend(plan.scatter, plan.merged, target) as f64;
                pass.set_blend_constant(wgpu::Color { r: s, g: s, b: s, a: s });
            }
            pass.draw(0..3, 0..1);
            drop(pass);
            cx.stats.render_passes += 1;
            cx.stats.fullscreen_passes += 1;
            cx.stats.post_passes += 1;
        }
        Some((chain.mips[0].clone(), chain.generation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_sizes() {
        let glow = Bloom::glow();
        let c = |rect| {
            let ChainSize { size, mips, merged } = chain_size(rect, &glow);
            (size, mips, merged)
        };
        let ChainSize { size, mips, merged } = chain_size((1920, 1080), &glow);
        assert_eq!((size, mips, merged), ((960, 540), 6, 0));
        let levels: Vec<_> = (0..mips).map(|l| level_size(size, l)).collect();
        assert_eq!(
            levels,
            [(960, 540), (480, 270), (240, 135), (120, 67), (60, 33), (30, 16)]
        );
        // 4K is capped: the same cost as 1080p.
        assert_eq!(c((3840, 2160)), ((960, 540), 6, 0));
        // 1440p: capped to 540 high.
        assert_eq!(c((2560, 1440)), ((960, 540), 6, 0));
        // Below 1080p the levels keep the reference heights (the halo keeps its size relative to
        // the frame): 720p drops the 540 level (270 is the power of two nearest to its 360),
        // 900p keeps it (resampled up from 450), 360p and the 180p goldens drop 2 and 3.
        assert_eq!(c((1280, 720)), ((480, 270), 5, 1));
        assert_eq!(c((1600, 900)), ((960, 540), 6, 0));
        assert_eq!(c((640, 360)), ((240, 135), 4, 2));
        assert_eq!(c((320, 180)), ((121, 68), 3, 3));
        for rect in [(1920, 1080), (1280, 720), (640, 360), (320, 180)] {
            let ChainSize { size, mips, .. } = chain_size(rect, &glow);
            let last = level_size(size, mips - 1).1;
            assert!((16..=17).contains(&last), "{rect:?}: smallest level {last} px high");
        }
        // Tiny and odd rects never produce 0-sized levels, and the last level keeps ≥ 2 px.
        for rect in [(1, 1), (3, 5), (17, 9), (64, 32), (101, 7), (4000, 3), (3, 4000)] {
            let ChainSize { size, mips, merged } = chain_size(rect, &glow);
            assert!(size.0 >= 1 && size.1 >= 1 && (1..=6).contains(&mips), "{rect:?}");
            assert!(merged + mips <= 6, "{rect:?}");
            let last = level_size(size, mips - 1);
            assert!(mips == 1 || last.0.min(last.1) >= 2, "{rect:?}: {size:?} × {mips}");
        }
        assert_eq!(c((64, 32)), ((34, 17), 1, 5));
        assert_eq!(c((1, 1)), ((1, 1), 1, 5));
        // mip_count 0 and 20 clamp to 1 and 8.
        let b = |mip_count| Bloom { mip_count, ..glow };
        assert_eq!(chain_size((4096, 4096), &b(0)).mips, 1);
        assert_eq!(chain_size((4096, 4096), &b(20)).mips, 8);
        // Low quality: 4 levels below 270.
        let low = Bloom {
            mip_count: 4,
            max_mip_height: 270,
            ..glow
        };
        let ChainSize { size, mips, merged } = chain_size((1440, 810), &low);
        assert_eq!((size, mips, merged), ((480, 270), 4, 0));
    }

    /// Level 0 of a chain that merged `n` reference levels carries their weights: the final
    /// weights of the levels equal the reference chain's (1 − s)·s^k, summed over the merged
    /// ones, and add up to 1.
    #[test]
    fn merged_levels_keep_their_weights() {
        let s = 0.7f32;
        let reference: Vec<f32> = (0..6)
            .map(|k| if k < 5 { (1.0 - s) * s.powi(k) } else { s.powi(5) })
            .collect();
        for merged in 0..6u32 {
            let mips = 6 - merged;
            // Level j's final weight: (1 − c_j) · Π_{i<j} c_i (the last level: Π c_i).
            let mut weights = Vec::new();
            let mut carried = 1.0;
            for j in 0..mips {
                if j + 1 == mips {
                    weights.push(carried);
                } else {
                    let c = upsample_blend(s, merged, j);
                    weights.push(carried * (1.0 - c));
                    carried *= c;
                }
            }
            let want0: f32 = reference[..=merged as usize].iter().sum();
            assert!((weights[0] - want0).abs() < 1e-6, "merged {merged}: {weights:?}");
            for j in 1..mips as usize {
                assert!(
                    (weights[j] - reference[j + merged as usize]).abs() < 1e-6,
                    "merged {merged}: {weights:?}"
                );
            }
            assert!((weights.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn soft_threshold_is_continuous_and_cuts() {
        let (t, knee) = (1.0, 0.5);
        let at = |v: f32| soft_threshold(Vec3::splat(v), t, knee).x;
        assert_eq!(at(0.4), 0.0, "below the knee");
        assert!(at(0.6) > 0.0 && at(0.6) < 0.01, "in the knee: {}", at(0.6));
        // Continuous at both ends of the knee, and ≥ c − t above it.
        let eps = 1e-3;
        assert!((at(0.5 - eps) - at(0.5 + eps)).abs() < 1e-4);
        assert!((at(1.5 - eps) - at(1.5 + eps)).abs() < 1e-2);
        assert!((at(8.0) - 7.0).abs() < 1e-5);
        // Monotonic.
        let mut last = 0.0;
        for i in 0..400 {
            let v = at(i as f32 * 0.01);
            assert!(v >= last - 1e-6, "not monotonic at {}", i as f32 * 0.01);
            last = v;
        }
        // Hard cut: nothing up to t, c − t above.
        assert_eq!(soft_threshold(Vec3::splat(0.99), 1.0, 0.0), Vec3::ZERO);
        assert!((soft_threshold(Vec3::splat(3.0), 1.0, 0.0).x - 2.0).abs() < 1e-6);
        // Threshold 0: everything blooms unchanged.
        let c = Vec3::new(0.2, 0.5, 0.1);
        assert!((soft_threshold(c, 0.0, 0.5) - c).abs().max_element() < 1e-6);
        // The ratio applies to the whole color (hue kept).
        let c = soft_threshold(Vec3::new(4.0, 2.0, 0.0), 1.0, 0.5);
        assert!((c.y / c.x - 0.5).abs() < 1e-6 && c.z == 0.0);
    }

    #[test]
    fn pass_uniforms() {
        let glow = Bloom::glow();
        let ChainSize { size, mips, .. } = chain_size((1920, 1080), &glow);
        let mut all = Vec::new();
        passes(
            (1920, 1080),
            [0, 0, 1920, 1080],
            (size, mips),
            &glow,
            (2.0, false),
            |p| all.push(p),
        );
        assert_eq!(all.len() as u32, pass_count(mips));
        assert_eq!(all.len(), 11);
        let pre = all[0];
        assert_eq!(pre.src_rect, [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(pre.src_texel, [1.0 / 1920.0, 1.0 / 1080.0, 1.0, 2.0]);
        assert_eq!(pre.threshold, [1.0, 0.5, 1.0, 0.5]);
        // Downsample into level 1 reads level 0.
        assert_eq!(all[1].src_rect, [0.0, 0.0, 960.0, 540.0]);
        assert_eq!(all[1].src_texel[2], 1.0);
        // The first upsample reads the last level.
        assert_eq!(all[mips as usize].src_rect, [0.0, 0.0, 30.0, 16.0]);
        // The last upsample reads level 1.
        assert_eq!(all[10].src_rect, [0.0, 0.0, 480.0, 270.0]);
    }

    /// GPU milliseconds of one bloom chain over a 1920 × 1080 scene texture in `format` with the
    /// GPU loaded: after a warm-up, `rounds` command buffers of `chains` chains are submitted back
    /// to back (no idle between them); each round is timed from the start of its first pass to
    /// the end of its last, and the best round counts (other processes' GPU work only ever adds).
    /// `None` without an adapter or `TIMESTAMP_QUERY`.
    fn gpu_ms_per_chain(bloom: &Bloom, format: wgpu::TextureFormat, chains: u32, rounds: u32) -> Option<f64> {
        gpu_ms_per_chain_at((1920, 1080), bloom, format, chains, rounds)
    }

    fn gpu_ms_per_chain_at(
        scene_size: (u32, u32),
        bloom: &Bloom,
        format: wgpu::TextureFormat,
        chains: u32,
        rounds: u32,
    ) -> Option<f64> {
        use wgpu::TextureUsages as U;
        let r = pollster::block_on(crate::Renderer::new_headless(64, 64)).ok()?;
        if !r.graphics_caps().timestamps {
            return None;
        }
        let (device, queue) = (r.device(), r.queue());
        let texture = |size: (u32, u32), format, usage| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("bloom gate"),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let scene = texture(
            scene_size,
            wgpu::TextureFormat::Rgba16Float,
            U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
        );
        let exposure = texture((1, 1), wgpu::TextureFormat::Rgba8Unorm, U::TEXTURE_BINDING);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let align = device.limits().min_uniform_buffer_offset_alignment as usize;
        let mut state = BloomState::new(device, align);
        let ChainSize { size, mips, merged } = chain_size(scene_size, bloom);
        let mut reallocs = 0;
        let mut n = 0;
        let slot = state.slot;
        let params = &mut state.params;
        let rect = [0, 0, scene_size.0, scene_size.1];
        passes(scene_size, rect, (size, mips), bloom, (1.0, false), |p| {
            params.write_at(n * slot, bytemuck::bytes_of(&p), &mut reallocs);
            n += 1;
        });
        state.slots = n as u32;
        let plan = BloomPlan {
            format,
            size,
            mips,
            merged,
            first_slot: 0,
            scatter: bloom.scatter,
        };
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("bloom gate"),
            ty: wgpu::QueryType::Timestamp,
            count: 2 * rounds,
        });
        let bytes = 16 * rounds as u64;
        let buffer = |usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bloom gate"),
                size: bytes,
                usage,
                mapped_at_creation: false,
            })
        };
        let resolve = buffer(wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC);
        let read = buffer(wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST);
        let mut pool = crate::frame::TexturePool::default();
        let mut stats = crate::RenderStats::default();
        let mut up = Uploader::new(device);
        // Scene content (the passes have no data-dependent branches) and uniforms.
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &scene,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 4.0,
                        g: 2.0,
                        b: 1.0,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        state.flush(device, &mut encoder, &mut up, &sampler);
        up.finish();
        queue.submit([encoder.finish()]);
        let mut record = |encoder: &mut wgpu::CommandEncoder, timed: Option<u32>| {
            for i in 0..chains {
                pool.begin_frame(0);
                let mut cx = PostEncode {
                    device,
                    queue,
                    encoder,
                    stats: &mut stats,
                    pool: &mut pool,
                };
                let timestamps = timed.map(|round| ChainTimestamps {
                    query_set: &queries,
                    begin: (i == 0).then_some(2 * round),
                    end: (i + 1 == chains).then_some(2 * round + 1),
                });
                state
                    .encode(&mut cx, (&scene, 1), &plan, &exposure, 0, timestamps)
                    .expect("params");
                pool.release_all();
            }
        };
        // Warm-up, so the GPU runs at its loaded clocks.
        for _ in 0..30 {
            let mut encoder = device.create_command_encoder(&Default::default());
            record(&mut encoder, None);
            queue.submit([encoder.finish()]);
        }
        for round in 0..rounds {
            let mut encoder = device.create_command_encoder(&Default::default());
            record(&mut encoder, Some(round));
            queue.submit([encoder.finish()]);
        }
        // Resolve once every pass finished (Metal writes end-of-pass samples late).
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.resolve_query_set(&queries, 0..2 * rounds, &resolve, 0);
        encoder.copy_buffer_to_buffer(&resolve, 0, &read, 0, bytes);
        queue.submit([encoder.finish()]);
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let t: Vec<u64> = bytemuck::pod_collect_to_vec(&read.slice(..).get_mapped_range().expect("mapped"));
        read.unmap();
        let period = queue.get_timestamp_period() as f64 * 1e-6;
        let best = t
            .chunks(2)
            .map(|p| p[1].saturating_sub(p[0]) as f64 * period / chains as f64)
            .filter(|ms| *ms > 0.0)
            .fold(f64::MAX, f64::min);
        assert_eq!(r.gpu_error_count(), 0);
        Some(best)
    }

    /// Gate (graphics spec §4.4): bloom with 6 levels at 1080p ≤ 0.20 ms of GPU time on the dev
    /// machine (Apple M4 Max), loaded (measured 0.14–0.17 ms). Skips without `TIMESTAMP_QUERY`;
    /// `SPACE3D_PERF_BUDGET_SCALE` relaxes it on slower machines. Each run is the best of 40
    /// rounds; a run disturbed by other processes' GPU work (this machine is shared: 30–70 %
    /// device utilization from other apps reads 0.19–0.30 ms) is retried, best of 3 runs, like
    /// `gpu_100k_within_budget`. Ignored by default: next to the other GPU tests of this binary
    /// (run in parallel) the timing is not the chain's alone, and hosted CI runners cannot
    /// measure Dev-machine budgets. Run it alone on an idle Dev machine: `cargo test -p
    /// space3d-render --lib bloom_gate -- --ignored --nocapture`.
    #[test]
    #[ignore = "perf gate: run alone with --ignored"]
    fn bloom_gate_1080p() {
        const GATE_MS: f64 = 0.20;
        let glow = Bloom::glow();
        assert_eq!(chain_size((1920, 1080), &glow).mips, 6);
        let scale = std::env::var("SPACE3D_PERF_BUDGET_SCALE")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(1.0);
        let caps = pollster::block_on(crate::Renderer::new_headless(4, 4)).map(|r| r.graphics_caps().clone());
        let Some(format) = caps.ok().and_then(|c| c.bloom_format) else {
            eprintln!("skipping GPU test: no adapter or no float targets");
            return;
        };
        let mut best = f64::MAX;
        for run in 0..3 {
            for f in [format, wgpu::TextureFormat::Rgba16Float] {
                let Some(ms) = gpu_ms_per_chain(&glow, f, 5, 40) else {
                    eprintln!("skipping GPU timing: no TIMESTAMP_QUERY");
                    return;
                };
                eprintln!("run {run}: bloom 6 mips @1080p, {f:?} chain: {ms:.3} ms GPU (loaded)");
                if f == format {
                    best = best.min(ms);
                }
            }
            if best <= GATE_MS * scale {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        assert!(
            best <= GATE_MS * scale,
            "bloom {best:.3} ms > {:.3} ms",
            GATE_MS * scale
        );
    }

    #[test]
    fn gpu_bloom_pass_matches_wgsl() {
        let src = bloom_source();
        let module = naga::front::wgsl::parse_str(&src).unwrap();
        let (_, pass) = module
            .types
            .iter()
            .find(|(_, t)| t.name.as_deref() == Some("BloomPass"))
            .unwrap();
        let naga::TypeInner::Struct { members, span } = &pass.inner else {
            panic!("BloomPass is a struct")
        };
        assert_eq!(*span as usize, std::mem::size_of::<GpuBloomPass>());
        let offset = |name: &str| members.iter().find(|m| m.name.as_deref() == Some(name)).unwrap().offset as usize;
        assert_eq!(offset("src_texel"), std::mem::offset_of!(GpuBloomPass, src_texel));
        assert_eq!(offset("threshold"), std::mem::offset_of!(GpuBloomPass, threshold));
        assert_eq!(offset("flags"), std::mem::offset_of!(GpuBloomPass, flags));
        crate::pipeline::validate_wgsl(&src).unwrap_or_else(|e| panic!("{e}"));
    }
}
