//! Per-frame work: extract → batch → cull (CPU or GPU) → upload → encode.
//!
//! Every scratch `Vec` here lives in [`FrameState`] and is reused, so the steady state performs
//! no heap allocation in our code (`RenderStats::cpu_reallocations` counts the exceptions).

use crate::arena::{ArenaAlloc, Vertex};
use crate::assets::RenderAssets;
use crate::components::AmbientLight;
use crate::gpu::{GrowBuffer, TrackedBuffer, Uploader};
use crate::handles::FastMap;
use crate::handles::{MeshHandle, TextureHandle};
use crate::instance::InstanceBuffers;
use crate::output::OutputEncoding;
use crate::pipeline::{
    GpuCompactParams, GpuCullJob, GpuCullParams, GpuFrustum, GpuMaterial, GpuPointLight, GpuView, Pipelines,
};
use crate::readback::CullReadback;
use crate::renderer::{RenderCapabilities, Renderer};
use crate::stats::DrawPath;
use space3d_core::{Resources, World};
use std::ops::Range;

mod draws;
mod extract;
mod passes;
mod plan;
mod targets;
#[cfg(test)]
mod tests;
mod views;

use draws::{DrawCmd, DrawKind, VertexSource};
use extract::{GpuInstances, KeyBuckets, MaterialTable, Source, SourceBuffer};
use plan::CameraItem;
use targets::Feedback;
use views::{Dispatch, SceneLights, ViewData, ViewTarget};

/// Runtime render options (resource).
#[derive(Clone, Debug, PartialEq)]
pub struct RenderSettings {
    /// Force a draw path; clamped to [`crate::RenderCapabilities::best_draw_path`].
    /// `None` uses the best available.
    pub draw_path: Option<DrawPath>,
    /// Frustum culling on/off (CPU and GPU).
    pub frustum_culling: bool,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            draw_path: None,
            frustum_culling: true,
        }
    }
}

const ARGS_SIZE: u64 = 20;
const WORKGROUP: u32 = 64;

/// What to do after trying to acquire the next surface texture.
pub(crate) enum SurfaceAcquire {
    Present(wgpu::SurfaceTexture),
    /// The configuration is outdated (resize, display change): reconfigure, skip this frame.
    Reconfigure,
    /// The surface is gone: it must be recreated from the window with `Instance::create_surface`.
    /// Reconfiguring it would fail validation.
    Recreate,
    /// Timeout, occluded or a (captured) validation error: skip presenting this frame.
    Skip,
}

impl SurfaceAcquire {
    pub(crate) fn from_current(t: wgpu::CurrentSurfaceTexture) -> Self {
        use wgpu::CurrentSurfaceTexture as C;
        match t {
            C::Success(t) | C::Suboptimal(t) => Self::Present(t),
            C::Outdated => Self::Reconfigure,
            C::Lost => Self::Recreate,
            other => {
                tracing::debug!(?other, "skipping present");
                Self::Skip
            }
        }
    }
}

/// Pending mesh uploads: handle, arena placement (once allocated), converted vertices and
/// indices, and the CPU data (restored to the entry if the mesh does not fit the arena).
type MeshScratch = Vec<(
    MeshHandle,
    Option<ArenaAlloc>,
    Vec<Vertex>,
    Vec<u32>,
    space3d_core::MeshData,
)>;

/// All persistent per-frame GPU buffers, caches and reusable scratch memory.
pub(crate) struct FrameState {
    pub mesh_scratch: MeshScratch,
    ecs: TrackedBuffer,
    cpu_visible: TrackedBuffer,
    views_buf: TrackedBuffer,
    materials_buf: TrackedBuffer,
    cull_params: TrackedBuffer,
    cull_jobs: TrackedBuffer,
    templates: TrackedBuffer,
    /// `(first args index, draw count)` per multi-draw-count run (compaction input).
    run_meta: TrackedBuffer,
    compact_params: TrackedBuffer,
    cull_out: GrowBuffer,
    args: GrowBuffer,
    compact_args: GrowBuffer,
    counts: GrowBuffer,
    explicit: Vec<Option<GpuInstances>>,
    materials: MaterialTable,
    view_bg: Option<(wgpu::BindGroup, u32)>,
    /// Material bind group per texture: (bind group, materials buffer generation, texture
    /// version, texture is an array — selects the pipeline variant).
    material_bgs: FastMap<Option<TextureHandle>, (wgpu::BindGroup, u32, u32, bool)>,
    /// Culling bind group per (source buffer, window of frustums).
    cull_bgs: FastMap<(SourceBuffer, u32), (wgpu::BindGroup, [u32; 5])>,
    compact_bg: Option<(wgpu::BindGroup, [u32; 5])>,
    arena_buffers: Option<(wgpu::Buffer, wgpu::Buffer)>,
    /// Visible ECS entities in query order: batch key (class | material | mesh), matrix, mesh.
    ecs_keys: Vec<u64>,
    ecs_matrices: Vec<[f32; 16]>,
    ecs_meshes: Vec<MeshHandle>,
    /// Row of each extracted entity in its archetype (gather source of `ecs_matrices`).
    ecs_rows: Vec<u32>,
    /// Last frame's `ecs_keys`: while the key sequence is unchanged, so is the batch order.
    ecs_prev_keys: Vec<u64>,
    /// Batch order (indices into the `ecs_*` vectors) and `(key, start, count)` per batch.
    ecs_order: Vec<u32>,
    ecs_runs: Vec<(u64, u32, u32)>,
    ecs_buckets: KeyBuckets,
    /// Byte ranges of `ecs` drawn from directly this frame (CPU path upload filter).
    ecs_needed: Vec<Range<usize>>,
    /// The ECS matrices exceed `max_buffer_size` (this frame once batched; logged once).
    ecs_overflow: bool,
    /// On overflow: instances kept per batch of `ecs_runs`, and the kept entries of
    /// `ecs_order` (what `ecs` holds and ECS sources index instead of `ecs_order`).
    ecs_kept: Vec<u32>,
    ecs_cut: Vec<u32>,
    /// Last frame's partially visible ECS matrices did not all fit `cpu_visible` (logged once).
    visible_overflow: bool,
    /// Per-instance visibility bits of the batch being CPU-culled.
    cull_mask: Vec<u64>,
    sources: Vec<Source>,
    cameras: Vec<CameraItem>,
    /// Every point light of the frame, in query order (see `views::select_point_lights`).
    point_lights: Vec<GpuPointLight>,
    /// Scratch of the per-view point-light selection: (squared distance, light index).
    light_order: Vec<(f32, u32)>,
    views: Vec<ViewData>,
    draws: Vec<DrawCmd>,
    jobs: Vec<(SourceBuffer, GpuCullJob)>,
    dispatches: Vec<Dispatch>,
    /// Grid of the draw-compaction dispatch (`GpuMultiDrawCount`: one workgroup per run).
    compact: Option<(u32, u32)>,
    /// Params offset and grid of the per-job prefix-sum dispatch (`cull_scan`).
    scan: Option<(u32, (u32, u32))>,
    /// Frustum planes of every view with GPU-culled sources, in view order (written to the
    /// start of `cull_params`, before the dispatch params).
    cull_planes: Vec<GpuFrustum>,
    feedback: FastMap<TextureHandle, Feedback>,
    args_count: u32,
    run_count: u32,
    /// Instances and (pre-cull) triangles of this frame's GPU-culled batches.
    gpu_submitted: u64,
    gpu_triangles: u64,
    /// Readback of GPU-culled visible counts for `RenderStats` (1–3 frames of latency).
    readback: CullReadback,
    /// Workgroup records of all culling jobs this frame.
    wg_total: u32,
    /// Primary target size the current `views` were prepared for (windowed capture).
    last_primary_size: (u32, u32),
    path: DrawPath,
    storage: bool,
    uniform_align: usize,
    /// Largest storage binding, a multiple of 4 and at most `max_buffer_size`.
    max_storage_binding: u64,
    max_buffer_size: u64,
    /// The device's `max_texture_dimension_2d` (bounds valid viewports).
    max_texture_2d: u32,
    /// Largest `base_vertex` passed to a direct draw since the last `prepare`.
    #[cfg(test)]
    pub(crate) max_direct_base_vertex: i32,
    /// Test stand-in for a window whose surface is lost at the next frame's acquire: the
    /// renderer leaves its offscreen target alone like a windowed one, and the next frame
    /// takes the lost-surface path after preparing its views.
    #[cfg(test)]
    pub(crate) simulate_surface_loss: bool,
    /// Captures take the windowed replay path (`encode_capture_replay`) although headless.
    #[cfg(test)]
    pub(crate) force_capture_replay: bool,
    /// Times the ECS batch order was rebuilt.
    #[cfg(test)]
    pub(crate) ecs_reorders: u64,
    /// Every view renders with this output encoding, linear exposure and tonemapper.
    #[cfg(test)]
    pub(crate) force_output: Option<(OutputEncoding, f32, crate::output::Tonemapper)>,
}

fn strided(size: usize, align: usize) -> usize {
    size.next_multiple_of(align)
}

fn binding(buffer: &wgpu::Buffer, max: u64) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
        buffer,
        offset: 0,
        size: wgpu::BufferSize::new(buffer.size().min(max)),
    })
}

fn uniform_binding(buffer: &wgpu::Buffer, size: usize) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
        buffer,
        offset: 0,
        size: wgpu::BufferSize::new(size as u64),
    })
}

impl FrameState {
    pub fn new(caps: &RenderCapabilities, device: &wgpu::Device) -> Self {
        use wgpu::BufferUsages as U;
        let storage = caps.compute && caps.indirect;
        let s = if storage { U::STORAGE } else { U::empty() };
        Self {
            mesh_scratch: Vec::new(),
            ecs: TrackedBuffer::new("space3d ecs instances", U::VERTEX | s),
            cpu_visible: TrackedBuffer::new("space3d visible instances", U::VERTEX),
            views_buf: TrackedBuffer::new("space3d views", U::UNIFORM),
            materials_buf: TrackedBuffer::new("space3d materials", U::UNIFORM),
            cull_params: TrackedBuffer::new("space3d cull params", U::UNIFORM),
            cull_jobs: TrackedBuffer::new("space3d cull jobs", s),
            templates: TrackedBuffer::new("space3d indirect templates", U::COPY_SRC),
            run_meta: TrackedBuffer::new("space3d run meta", s),
            compact_params: TrackedBuffer::new("space3d compact params", U::UNIFORM),
            cull_out: GrowBuffer::new("space3d culled instances", U::VERTEX | s),
            // COPY_SRC: the culling-stats readback copies the visible counts out.
            args: GrowBuffer::new("space3d indirect args", U::INDIRECT | U::COPY_DST | U::COPY_SRC | s),
            compact_args: GrowBuffer::new("space3d compacted args", U::INDIRECT | s),
            counts: GrowBuffer::new("space3d draw counts", U::INDIRECT | U::COPY_DST | s),
            explicit: Vec::new(),
            materials: MaterialTable::default(),
            view_bg: None,
            material_bgs: FastMap::default(),
            cull_bgs: FastMap::default(),
            compact_bg: None,
            arena_buffers: None,
            ecs_keys: Vec::new(),
            ecs_matrices: Vec::new(),
            ecs_meshes: Vec::new(),
            ecs_rows: Vec::new(),
            ecs_prev_keys: Vec::new(),
            ecs_order: Vec::new(),
            ecs_runs: Vec::new(),
            ecs_buckets: KeyBuckets::default(),
            ecs_needed: Vec::new(),
            ecs_overflow: false,
            ecs_kept: Vec::new(),
            ecs_cut: Vec::new(),
            visible_overflow: false,
            cull_mask: Vec::new(),
            sources: Vec::new(),
            cameras: Vec::new(),
            point_lights: Vec::new(),
            light_order: Vec::new(),
            views: Vec::new(),
            draws: Vec::new(),
            jobs: Vec::new(),
            dispatches: Vec::new(),
            compact: None,
            scan: None,
            cull_planes: Vec::new(),
            feedback: FastMap::default(),
            args_count: 0,
            run_count: 0,
            gpu_submitted: 0,
            gpu_triangles: 0,
            readback: CullReadback::default(),
            wg_total: 0,
            last_primary_size: (0, 0),
            path: DrawPath::CpuDirect,
            storage,
            uniform_align: device.limits().min_uniform_buffer_offset_alignment as usize,
            max_storage_binding: caps.max_storage_binding,
            max_buffer_size: caps.max_buffer_size,
            max_texture_2d: device.limits().max_texture_dimension_2d,
            #[cfg(test)]
            max_direct_base_vertex: 0,
            #[cfg(test)]
            simulate_surface_loss: false,
            #[cfg(test)]
            force_capture_replay: false,
            #[cfg(test)]
            ecs_reorders: 0,
            #[cfg(test)]
            force_output: None,
        }
    }

    fn view_stride(&self) -> usize {
        strided(std::mem::size_of::<GpuView>(), self.uniform_align)
    }
    fn material_stride(&self) -> usize {
        strided(std::mem::size_of::<GpuMaterial>(), self.uniform_align)
    }
    fn params_stride(&self) -> usize {
        strided(std::mem::size_of::<GpuCullParams>(), self.uniform_align)
    }

    /// Whether any prepared view of the last frame draws to the primary target.
    pub(crate) fn has_primary_view(&self) -> bool {
        self.views.iter().any(|v| matches!(v.target, ViewTarget::Primary))
    }

    /// Capacity snapshot of scratch vectors (to count CPU reallocations).
    fn capacities(&self) -> [usize; 22] {
        [
            self.ecs_keys.capacity(),
            self.ecs_rows.capacity(),
            self.ecs_prev_keys.capacity(),
            self.ecs_matrices.capacity(),
            self.ecs_meshes.capacity(),
            self.ecs_buckets.of.capacity(),
            self.ecs_buckets.sorted.capacity(),
            self.ecs_order.capacity(),
            self.ecs_runs.capacity(),
            self.ecs_needed.capacity(),
            self.ecs_kept.capacity(),
            self.ecs_cut.capacity(),
            self.cull_mask.capacity(),
            self.sources.capacity(),
            self.cameras.capacity(),
            self.views.capacity(),
            self.draws.capacity(),
            self.jobs.capacity(),
            self.dispatches.capacity(),
            self.cull_planes.capacity(),
            self.point_lights.capacity(),
            self.light_order.capacity(),
        ]
    }
}

impl FrameState {
    /// Flush this frame's CPU-side buffers (instances, views, materials, culling inputs) to
    /// the GPU through the staging ring, and size the GPU-written ones.
    pub(super) fn upload(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        up: &mut Uploader,
        out_cursor: u64,
        wg_word: u32,
        reallocs: &mut u32,
    ) {
        let f = self;
        let mut cpu_reallocs = 0u32;
        let gpu = f.path.gpu_culling();
        // Bindings cover one full element even when empty.
        f.views_buf
            .ensure_len(std::mem::size_of::<GpuView>(), &mut cpu_reallocs);
        f.materials_buf
            .ensure_len(std::mem::size_of::<GpuMaterial>(), &mut cpu_reallocs);
        f.cull_params
            .ensure_len(std::mem::size_of::<GpuCullParams>(), &mut cpu_reallocs);
        f.compact_params
            .ensure_len(std::mem::size_of::<GpuCompactParams>(), &mut cpu_reallocs);
        let inst_before = up.bytes;
        if gpu {
            f.ecs.flush(device, encoder, up);
        } else {
            // Only batches drawn straight from `ecs` read it on the CPU path: culled and
            // partially visible ones (drawn from `cpu_visible`) keep their changes pending
            // until they are drawn from it.
            f.ecs_needed.clear();
            for d in &f.draws {
                if let (VertexSource::Ecs, DrawKind::Direct { instances }) = (d.vb, d.kind) {
                    let start = d.vb_offset as usize;
                    f.ecs_needed.push(start..start + instances as usize * 64);
                }
            }
            f.ecs_needed.sort_unstable_by_key(|r| r.start);
            f.ecs_needed.dedup_by(|next, kept| {
                let overlaps = next.start <= kept.end;
                if overlaps {
                    kept.end = kept.end.max(next.end);
                }
                overlaps
            });
            f.ecs.flush_needed(device, encoder, up, &f.ecs_needed);
        }
        f.cpu_visible.flush(device, encoder, up);
        up.instance_bytes += up.bytes - inst_before;
        f.views_buf.flush(device, encoder, up);
        f.materials_buf.flush(device, encoder, up);
        if gpu {
            f.cull_params.flush(device, encoder, up);
            f.cull_jobs.flush(device, encoder, up);
            f.templates.flush(device, encoder, up);
            f.run_meta.ensure_len(8, &mut cpu_reallocs);
            f.run_meta.flush(device, encoder, up);
            f.compact_params.flush(device, encoder, up);
            f.cull_out.ensure(device, out_cursor, false, up);
            let args_bytes = f.args_count as u64 * ARGS_SIZE;
            let records = wg_word as u64 * 4 + f.wg_total as u64 * 16;
            debug_assert!(
                records <= f.max_storage_binding,
                "cull records exceed the storage binding"
            );
            f.args.ensure(device, records, false, up);
            f.compact_args.ensure(device, args_bytes, false, up);
            f.counts.ensure(device, f.run_count as u64 * 4, false, up);
        }
        *reallocs += cpu_reallocs;
    }

    /// The view bind group (rebuilt when the views buffer was reallocated).
    pub(super) fn prepare_view_bind_group(&mut self, device: &wgpu::Device, pipelines: &Pipelines) {
        let f = self;
        let vgen = f.views_buf.gpu.generation;
        if f.view_bg.as_ref().is_none_or(|(_, g)| *g != vgen) {
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d view"),
                layout: &pipelines.view_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_binding(f.views_buf.buffer(), std::mem::size_of::<GpuView>()),
                }],
            });
            f.view_bg = Some((bg, vgen));
        }
    }
}

impl Renderer {
    /// Extract the world, cull, upload and draw one frame. Normally called by the
    /// `Stage::Render` system of [`crate::RenderPlugin`].
    pub fn render_frame(&mut self, world: &World, resources: &mut Resources) {
        self.frame_index += 1;
        self.ensure_offscreen();
        // GPU-culling counts of earlier frames whose copies the GPU has finished.
        self.frame.readback.collect();
        let settings = resources.get::<RenderSettings>().cloned().unwrap_or_default();
        let ambient = resources.get::<AmbientLight>().copied().unwrap_or_default();
        let best = self.caps.best_draw_path;
        let path = settings.draw_path.map_or(best, |p| p.min(best));
        self.stats.reset(self.frame_index, path);
        self.uploader.reset_counters();
        self.frame.path = path;

        // Move the asset/instance registries out without re-boxing resources.
        let mut assets = resources
            .get_mut::<RenderAssets>()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut buffers = resources
            .get_mut::<InstanceBuffers>()
            .map(std::mem::take)
            .unwrap_or_default();

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("space3d frame"),
        });
        self.upload_assets_into(&mut encoder, &mut assets);
        for h in buffers.removed.drain(..) {
            if let Some(slot) = self.frame.explicit.get_mut(h.index() as usize) {
                *slot = None;
            }
            let gone = SourceBuffer::Explicit(h.index());
            self.frame.cull_bgs.retain(|(b, _), _| *b != gone);
        }
        self.prepare(world, &assets, &mut buffers, ambient, &settings, &mut encoder);
        self.encode_compute(&mut encoder);
        self.apply_gpu_cull_counts();

        // Acquire the surface only when something presents.
        let mut surface_tex = None;
        let mut lost = false;
        if let Some(s) = &mut self.surface {
            match SurfaceAcquire::from_current(s.surface.get_current_texture()) {
                SurfaceAcquire::Present(t) => {
                    s.presented = true;
                    surface_tex = Some(t);
                }
                SurfaceAcquire::Reconfigure => s.surface.configure(&self.device, &s.config),
                SurfaceAcquire::Recreate
                    if crate::renderer::retry_without_view_formats(
                        cfg!(target_arch = "wasm32"),
                        s.presented,
                        &s.config,
                    ) =>
                {
                    tracing::warn!("the canvas rejected its sRGB view format; encoding sRGB in the shader instead");
                    s.drop_srgb_view(&self.device);
                }
                SurfaceAcquire::Recreate => lost = true,
                SurfaceAcquire::Skip => {}
            }
        }
        #[cfg(test)]
        {
            lost |= std::mem::take(&mut self.frame.simulate_surface_loss);
        }
        if lost {
            self.surface_lost();
        }
        let surface_view = surface_tex.as_ref().map(|t| {
            t.texture.create_view(&wgpu::TextureViewDescriptor {
                format: self.surface.as_ref().map(|s| s.view_format),
                ..Default::default()
            })
        });
        self.encode_views(&mut encoder, surface_view.as_ref(), false);
        self.overlays.sync(resources.get::<crate::overlay::RenderOverlays>());
        self.encode_frame_overlays(&mut encoder, surface_view.as_ref());

        self.uploader.finish();
        self.queue.submit([encoder.finish()]);
        self.uploader.recall();
        if let Some(t) = surface_tex {
            self.queue.present(t);
        }
        // Non-blocking: fires staging-chunk recycling and capture callbacks.
        let _ = self.device.poll(wgpu::PollType::Poll);

        self.stats.bytes_uploaded = self.uploader.bytes;
        self.stats.instance_bytes_uploaded = self.uploader.instance_bytes;
        self.stats.buffer_reallocations = self.uploader.reallocations;

        if let Some(r) = resources.get_mut::<RenderAssets>() {
            *r = assets;
        } else {
            resources.insert(assets);
        }
        if let Some(r) = resources.get_mut::<InstanceBuffers>() {
            *r = buffers;
        } else {
            resources.insert(buffers);
        }
        match resources.get_mut::<crate::RenderStats>() {
            Some(s) => s.clone_from(&self.stats),
            None => {
                resources.insert(self.stats.clone());
            }
        }
    }

    /// The presentation surface is gone (`SurfaceAcquire::Recreate`) after this frame's views
    /// were prepared for it. `has_surface()` turns false: the platform layer attaches a new
    /// surface with `set_surface`. Until then frames render offscreen, this one included — into
    /// an offscreen target of the size its primary views were prepared for. A windowed
    /// renderer sizes that target only for captures, so it may still have an older window
    /// size, and a viewport's scissor rect outside it would fail validation, losing the
    /// frame's commands and with them its uploads.
    fn surface_lost(&mut self) {
        tracing::warn!("presentation surface lost; dropping it until a new one is attached");
        self.surface = None;
        self.size_offscreen(self.frame.last_primary_size);
    }

    fn primary_size(&self) -> (u32, u32) {
        match &self.surface {
            Some(s) => (s.config.width, s.config.height),
            None => (self.config.width, self.config.height),
        }
    }

    fn prepare(
        &mut self,
        world: &World,
        assets: &RenderAssets,
        buffers: &mut InstanceBuffers,
        ambient: AmbientLight,
        settings: &RenderSettings,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let primary_size = self.primary_size();
        let primary_output = match &self.surface {
            Some(s) if s.manual_srgb => OutputEncoding::ManualSrgb,
            _ => OutputEncoding::Linear,
        };
        self.frame.last_primary_size = primary_size;
        let frame_index = self.frame_index;
        let Renderer {
            device,
            frame: f,
            pipelines,
            uploader: up,
            stats,
            white,
            white_sampler,
            ..
        } = self;
        let caps_before = f.capacities();
        let mut cpu_reallocs = 0u32;

        f.arena_buffers = match (&assets.arena.vertex, &assets.arena.index) {
            (Some(v), Some(i)) => Some((v.clone(), i.clone())),
            _ => None,
        };

        let lights = SceneLights::collect(world, &mut f.point_lights);
        f.collect_cameras(world);
        f.extract(
            world,
            assets,
            buffers,
            device,
            encoder,
            up,
            pipelines,
            frame_index,
            stats,
            &mut cpu_reallocs,
        );
        let out_cursor = f.prepare_views(
            assets,
            buffers,
            &lights,
            ambient,
            primary_size,
            primary_output,
            settings.frustum_culling,
            stats,
            &mut cpu_reallocs,
        );
        let wg_word = f.prepare_dispatches(&mut cpu_reallocs);
        f.upload(device, encoder, up, out_cursor, wg_word, &mut cpu_reallocs);

        // Custom-shader pipelines whose (asynchronous, browser) validation failed since the
        // last frame fall back to the built-in shader before anything is drawn with them.
        pipelines.poll_pending(device);
        f.prepare_view_bind_group(device, pipelines);
        f.prepare_material_bind_groups(device, assets, pipelines, white, white_sampler);
        f.prepare_feedback(device, assets, pipelines, frame_index);
        f.prepare_cull_bind_groups(device, pipelines, frame_index);

        let caps_after = f.capacities();
        cpu_reallocs += caps_before
            .iter()
            .zip(caps_after.iter())
            .filter(|(a, b)| a != b)
            .count() as u32;
        stats.cpu_reallocations += cpu_reallocs;
    }
}
