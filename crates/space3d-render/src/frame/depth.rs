//! Depth-only views (graphics spec §1.4 steps 5, 9 and 11, §1.12; milestone F7) and the views'
//! group-0 bind groups.
//!
//! A *depth view* is a view the lighting requests (`Lighting::depth_views`: a shadow map) with
//! its own `GpuView` slot (after the camera views'; `view_proj` = the light matrix, every other
//! field zero), its own frustum slot, culling jobs and output regions — so on GPU paths it
//! shares the camera views' culling dispatches (one mark + scatter per source buffer and window
//! of 64 frustums), and on `CpuDirect` it is culled on the CPU like a camera. It records the
//! opaque sources whose material casts ([`super::extract::source_casts`]) and is drawn in a
//! depth-only pass (clear 1.0, store) before the first camera view, with the depth pipelines of
//! `PassKind::Depth` / `DepthMask` ([`crate::pipeline::Pipelines::depth`]). Depth views never
//! take part in color, overlay or post passes, node phases or capture replays.
//!
//! Group 0 of every view (`FrameLayouts::view`) binds the view uniform and the lighting
//! resources of `Lighting::view_resources`: camera views those of their camera, depth views
//! the neutral ones (never the shadow map a depth pass is writing). The bind groups are cached
//! by the lighting's resource key and the views buffer's generation.

use super::FrameState;
use super::draws::{DrawKind, VertexSource, direct_vertex_base};
use super::views::SourceCounts;
use super::{ARGS_SIZE, uniform_binding};
use crate::arena::VERTEX_SIZE;
use crate::instance::InstanceBuffers;
use crate::lighting::{DepthViewRequest, Lighting, ViewResources};
use crate::pipeline::{GpuView, PassKind, PipelineKey, Pipelines};
use crate::renderer::Renderer;
use crate::stats::RenderStats;
use bytemuck::Zeroable;
use space3d_core::World;
use std::ops::Range;

/// One depth-only view of this frame.
#[derive(Clone)]
pub(crate) struct DepthView {
    /// `Depth32Float`, square.
    pub target: wgpu::TextureView,
    pub bias: wgpu::DepthBiasState,
    /// Dynamic offset of its `GpuView` slot.
    pub view_offset: u32,
    /// Its draws in `FrameState::draws`.
    pub draws: Range<usize>,
    /// Its group-0 bind group (`ViewBindGroups`).
    pub bind_group: usize,
}

/// A view bind group: the lighting resource key it binds and the views buffer generation.
struct ViewBindGroup {
    key: u64,
    views_generation: u32,
    bind_group: wgpu::BindGroup,
    last_used: u64,
}

/// Group-0 bind groups of the frame's views (camera and depth views), one per lighting
/// resource key. Entries unused for [`Self::MAX_IDLE`] frames are dropped at the start of a
/// frame, before any index is handed out: an index stays valid until the next frame's
/// `begin` (capture replays of the frame included).
#[derive(Default)]
pub(crate) struct ViewBindGroups {
    entries: Vec<ViewBindGroup>,
}

impl ViewBindGroups {
    const MAX_IDLE: u64 = 3;

    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// Drop entries unused for a while (their resources may be gone).
    fn begin(&mut self, frame: u64) {
        self.entries.retain(|e| e.last_used + Self::MAX_IDLE >= frame);
    }

    /// Index of the bind group of `resources` over the views buffer `views` (generation
    /// `views_generation`), created on first use or after the buffer was reallocated.
    fn index(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        (views, views_generation): (&wgpu::Buffer, u32),
        resources: &ViewResources<'_>,
        frame: u64,
    ) -> usize {
        let found = self.entries.iter().position(|e| e.key == resources.key);
        if let Some(i) = found
            && self.entries[i].views_generation == views_generation
        {
            self.entries[i].last_used = frame;
            return i;
        }
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("space3d view"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_binding(views, std::mem::size_of::<GpuView>()),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(resources.env_specular),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(resources.env_irradiance),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(resources.brdf_lut),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(resources.env_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(resources.shadow_map),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::Sampler(resources.shadow_sampler),
                },
            ],
        });
        let entry = ViewBindGroup {
            key: resources.key,
            views_generation,
            bind_group,
            last_used: frame,
        };
        match found {
            Some(i) => {
                self.entries[i] = entry;
                i
            }
            None => {
                self.entries.push(entry);
                self.entries.len() - 1
            }
        }
    }

    /// The bind group at `index` (from [`Self::index`] this frame).
    pub fn get(&self, index: usize) -> Option<&wgpu::BindGroup> {
        self.entries.get(index).map(|e| &e.bind_group)
    }

    /// Bind groups cached (tests).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl FrameState {
    /// Step 5 (graphics spec §1.4): one depth view per request (after the camera views, which
    /// must be recorded already): its `GpuView` slot, and its draws and culling jobs through
    /// [`FrameState::record_sources`] with the request's frustum. Adds to
    /// `RenderStats::{depth_views, shadow_casters}`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_depth_views<'r>(
        &mut self,
        requests: impl Iterator<Item = &'r DepthViewRequest>,
        buffers: &mut InstanceBuffers,
        culling: bool,
        stats: &mut RenderStats,
        out_cursor: &mut u64,
        visible_overflow: &mut bool,
        reallocs: &mut u32,
    ) {
        let f = self;
        f.depth_views.clear();
        let view_stride = f.view_stride();
        for r in requests {
            let view_offset = (f.views.len() + f.depth_views.len()) * view_stride;
            let mut gv = GpuView::zeroed();
            gv.view_proj = r.view_proj.to_cols_array_2d();
            f.views_buf.write_at(view_offset, bytemuck::bytes_of(&gv), reallocs);
            let draws_start = f.draws.len();
            let mut counts = SourceCounts::default();
            f.record_sources(
                &r.frustum,
                culling,
                true,
                None,
                buffers,
                out_cursor,
                visible_overflow,
                &mut counts,
                reallocs,
            );
            stats.shadow_casters += counts.instances;
            stats.depth_views += 1;
            f.depth_views.push(DepthView {
                target: r.target.clone(),
                bias: r.bias,
                view_offset: view_offset as u32,
                draws: draws_start..f.draws.len(),
                bind_group: 0,
            });
        }
    }

    /// Step 8: the camera views' `GpuView`s, with the lighting's environment, sky and shadow
    /// fields (`Lighting::fill_view`), into the views buffer — written once, so a still scene
    /// uploads nothing.
    pub(super) fn write_view_uniforms(&mut self, world: &World, lighting: &Lighting, reallocs: &mut u32) {
        let f = self;
        for v in 0..f.views.len() {
            let camera = f.cameras[f.views[v].camera].id;
            lighting.fill_view(world, camera, &mut f.gpu_views[v]);
            let offset = f.views[v].view_offset as usize;
            f.views_buf
                .write_at(offset, bytemuck::bytes_of(&f.gpu_views[v]), reallocs);
        }
    }

    /// Group-0 bind groups of this frame's views (after the views buffer is flushed, which may
    /// reallocate it): camera views bind their camera's lighting resources, depth views the
    /// neutral ones.
    pub(super) fn prepare_view_bind_groups(
        &mut self,
        device: &wgpu::Device,
        pipelines: &Pipelines,
        lighting: &Lighting,
        frame: u64,
    ) {
        let f = self;
        f.view_bgs.begin(frame);
        let views = (f.views_buf.buffer(), f.views_buf.gpu.generation);
        for v in 0..f.views.len() {
            let camera = f.cameras[f.views[v].camera].id;
            let resources = lighting.view_resources(Some(camera));
            f.views[v].bind_group = f.view_bgs.index(device, &pipelines.view_bgl, views, &resources, frame);
        }
        if !f.depth_views.is_empty() {
            let neutral = lighting.view_resources(None);
            let index = f.view_bgs.index(device, &pipelines.view_bgl, views, &neutral, frame);
            for d in &mut f.depth_views {
                d.bind_group = index;
            }
        }
    }

    /// Group 0 of a view (`ViewData::bind_group`, `DepthView::bind_group`).
    pub(super) fn view_bind_group(&self, index: usize) -> Option<&wgpu::BindGroup> {
        self.view_bgs.get(index)
    }

    /// Depth views of the last frame.
    pub(crate) fn depth_view_count(&self) -> usize {
        self.depth_views.len()
    }

    /// Draw commands of the last frame's depth view `index`.
    pub(crate) fn depth_view_draws(&self, index: usize) -> usize {
        self.depth_views[index].draws.len()
    }

    /// Instances the last frame drew into depth view `index` (`crate::testing`): direct draws
    /// count their instances, indirect ones the instance counts GPU culling wrote into their
    /// args, read back blocking.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn depth_view_instances(&self, device: &wgpu::Device, queue: &wgpu::Queue, index: usize) -> u64 {
        let mut total = 0u64;
        let mut args = Vec::new();
        for cmd in &self.draws[self.depth_views[index].draws.clone()] {
            match cmd.kind {
                DrawKind::Direct { instances } => total += instances as u64,
                DrawKind::Indirect { args: a } => args.push(a..a + 1),
                DrawKind::Multi { args_first, count } | DrawKind::MultiCount { args_first, count, .. } => {
                    args.push(args_first..args_first + count)
                }
            }
        }
        if args.is_empty() {
            return total;
        }
        let bytes = self.args_count as u64 * ARGS_SIZE;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("space3d test args readback"),
            size: bytes.next_multiple_of(4),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(self.args.get(), 0, &readback, 0, bytes);
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let data = readback.slice(..).get_mapped_range().expect("mapped");
        for r in args {
            for a in r {
                let at = a as usize * ARGS_SIZE as usize + 4;
                total += u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as u64;
            }
        }
        total
    }

    /// The depth pipeline kind of a caster of pipeline class `class`
    /// ([`super::extract::caster_pass`]; the `crate::testing` override first).
    fn caster_pass(&self, class: u8) -> PassKind {
        self.force_caster_pass
            .unwrap_or_else(|| super::extract::caster_pass(class))
    }
}

impl Renderer {
    /// Step 11 (graphics spec §1.4): each depth view's depth-only pass (clear 1.0, store) into
    /// its target, before any camera view. Never part of a capture replay.
    pub(super) fn encode_depth_views(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let depth_views = std::mem::take(&mut self.frame.depth_views);
        for d in &depth_views {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("space3d depth view"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &d.target,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: self.timer.render("shadow", None),
                ..Default::default()
            });
            self.encode_depth_draws(&mut pass, d);
            drop(pass);
            self.stats.render_passes += 1;
            #[cfg(test)]
            {
                self.frame.depth_passes_encoded += 1;
            }
        }
        self.frame.depth_views = depth_views;
    }

    /// Record a depth view's draws: the caster's depth pipeline (`Depth`: group 0 only;
    /// `DepthMask`: with the material, see [`super::extract::caster_pass`]), the mesh arena and
    /// the draw's instances, like [`Renderer::encode_draws`] but without color state.
    fn encode_depth_draws(&mut self, pass: &mut wgpu::RenderPass<'_>, d: &DepthView) {
        let f = &self.frame;
        let (Some((vb, ib)), Some(view_bg)) = (&f.arena_buffers, f.view_bind_group(d.bind_group)) else {
            return;
        };
        if d.draws.is_empty() {
            return;
        }
        pass.set_bind_group(0, view_bg, &[d.view_offset]);
        pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
        let material_stride = f.material_stride() as u32;
        let base_vertex_supported = self.caps.base_vertex;
        let mut last_pipeline: Option<(u16, u8, u8, PassKind)> = None;
        let mut last_material = u32::MAX;
        let mut last_vb: Option<(VertexSource, u64)> = None;
        let mut slot0: Option<u32> = None;
        for cmd in &f.draws[d.draws.clone()] {
            let direct = matches!(cmd.kind, DrawKind::Direct { .. });
            if direct && cmd.index_count == 0 {
                continue;
            }
            let kind = self.frame.caster_pass(cmd.class);
            let mut kinds = 0;
            if kind == PassKind::DepthMask {
                let Some((bg, k)) = f.material_bind_group(cmd.material) else {
                    continue;
                };
                if cmd.material != last_material {
                    pass.set_bind_group(1, bg, &[cmd.material * material_stride]);
                    last_material = cmd.material;
                }
                kinds = k;
            }
            if last_pipeline != Some((cmd.layout, cmd.class, kinds, kind)) {
                let key = PipelineKey {
                    layout: cmd.layout,
                    class: cmd.class,
                    format: crate::pipeline::DEPTH_FORMAT,
                    samples: 1,
                    output: crate::output::OutputEncoding::Linear,
                    kinds,
                    shader: None,
                    pass: kind,
                    globals: 0,
                };
                pass.set_pipeline(self.pipelines.depth(&self.device, key, d.bias));
                last_pipeline = Some((cmd.layout, cmd.class, kinds, kind));
            }
            let (first_vertex, base_vertex) = direct_vertex_base(cmd.base_vertex, direct, base_vertex_supported);
            if slot0 != Some(first_vertex) {
                pass.set_vertex_buffer(0, vb.slice(first_vertex as u64 * VERTEX_SIZE..));
                slot0 = Some(first_vertex);
            }
            if last_vb != Some((cmd.vb, cmd.vb_offset)) {
                let buffer = match cmd.vb {
                    VertexSource::Ecs => f.ecs.buffer(),
                    VertexSource::CpuVisible => f.cpu_visible.buffer(),
                    VertexSource::CullOut => f.cull_out.get(),
                    VertexSource::Explicit(slot) => match f.explicit.get(slot as usize) {
                        Some(Some(g)) => g.buf.get(),
                        _ => continue,
                    },
                };
                pass.set_vertex_buffer(1, buffer.slice(cmd.vb_offset..));
                last_vb = Some((cmd.vb, cmd.vb_offset));
            }
            let indices = cmd.first_index..cmd.first_index + cmd.index_count;
            match cmd.kind {
                DrawKind::Direct { instances } => pass.draw_indexed(indices, base_vertex, 0..instances),
                DrawKind::Indirect { args } => pass.draw_indexed_indirect(f.args.get(), args as u64 * ARGS_SIZE),
                DrawKind::Multi { args_first, count } => {
                    pass.multi_draw_indexed_indirect(f.args.get(), args_first as u64 * ARGS_SIZE, count)
                }
                DrawKind::MultiCount { args_first, count, run } => pass.multi_draw_indexed_indirect_count(
                    f.compact_args.get(),
                    args_first as u64 * ARGS_SIZE,
                    f.counts.get(),
                    run as u64 * 4,
                    count,
                ),
            }
            self.stats.shadow_draw_calls += 1;
        }
    }
}
