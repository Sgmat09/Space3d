//! Per-frame work: extract → batch → cull (CPU or GPU) → upload → encode.
//!
//! Every scratch `Vec` here lives in [`FrameState`] and is reused, so the steady state performs
//! no heap allocation in our code (`RenderStats::cpu_reallocations` counts the exceptions).

use crate::arena::{ArenaAlloc, Vertex};
use crate::assets::{GpuMesh, RenderAssets};
use crate::components::{
    AmbientLight, Camera3d, DirectionalLight, Material, Mesh3d, PointLight, RenderTarget, Visibility,
};
#[cfg(test)]
use crate::culling::Sphere;
use crate::culling::{Aabb, Frustum};
use crate::gpu::{GrowBuffer, TrackedBuffer, align_up};
use crate::handles::FastMap;
use crate::handles::{MeshHandle, TextureHandle};
use crate::instance::{BufferUsage, CullSource, ErasedInstanceBuffer, InstanceBuffers, InstancedMesh, ScaleKind};
use crate::pipeline::{
    CLASS_BLEND, CLASS_DOUBLE_SIDED, GpuCompactParams, GpuCullJob, GpuCullParams, GpuDirLight, GpuMaterial,
    GpuPointLight, GpuView, MAX_DIR_LIGHTS, MAX_POINT_LIGHTS, OFFSCREEN_FORMAT, PipelineKey,
};
use crate::renderer::{RenderCapabilities, Renderer};
use crate::stats::DrawPath;
use bytemuck::Zeroable;
use glam::{Mat4, Vec3, Vec4};
use space3d_core::{Entity, GlobalTransform, Resources, World};
use std::ops::Range;

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

/// Which GPU buffer a batch reads instances from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum SourceBuffer {
    Ecs,
    Explicit(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VertexSource {
    Ecs,
    Explicit(u32),
    CpuVisible,
    CullOut,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DrawKind {
    Direct { instances: u32 },
    Indirect { args: u32 },
    Multi { args_first: u32, count: u32 },
    MultiCount { args_first: u32, count: u32, run: u32 },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DrawCmd {
    layout: u16,
    class: u8,
    material: u32,
    first_index: u32,
    index_count: u32,
    base_vertex: i32,
    vb: VertexSource,
    vb_offset: u64,
    kind: DrawKind,
}

#[derive(Clone, Copy)]
struct EcsItem {
    key: u64,
    seq: u32,
    mesh: MeshHandle,
    material: u32,
    class: u8,
    matrix: [f32; 16],
}

#[derive(Clone, Copy)]
struct Source {
    blend: bool,
    class: u8,
    layout: u16,
    material: u32,
    mesh: GpuMesh,
    triangles: u32,
    buffer: SourceBuffer,
    first: u32,
    count: u32,
    stride: u32,
    cull: CullSource,
    bounds: Option<(Aabb, f32)>,
}

#[derive(Clone, Copy)]
struct CameraItem {
    order: i32,
    entity: u64,
    camera: Camera3d,
    transform: Mat4,
}

#[derive(Clone)]
pub(crate) enum ViewTarget {
    Primary,
    Texture {
        color: wgpu::TextureView,
        depth: wgpu::TextureView,
    },
}

#[derive(Clone)]
pub(crate) struct ViewData {
    target: ViewTarget,
    clear: Option<Vec4>,
    viewport: Option<[f32; 4]>,
    size: (u32, u32),
    view_offset: u32,
    draws: Range<usize>,
}

struct Dispatch {
    buffer: SourceBuffer,
    params_offset: u32,
    wg: (u32, u32),
}

struct CompactDispatch {
    params_offset: u32,
    draw_count: u32,
}

pub(crate) struct GpuInstances {
    generation: u32,
    buf: GrowBuffer,
    synced: u64,
    len: u32,
    bounds: (Aabb, f32),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct MaterialKey {
    words: [u32; 9],
    flags: u8,
    texture: Option<TextureHandle>,
}

impl MaterialKey {
    fn new(m: &Material) -> Self {
        let b = m.base_color;
        let e = m.emissive;
        Self {
            words: [b.x, b.y, b.z, b.w, m.metallic, m.roughness, e.x, e.y, e.z].map(f32::to_bits),
            flags: (m.unlit as u8) | (m.double_sided as u8) << 1 | (m.alpha_blend as u8) << 2,
            texture: m.base_color_texture,
        }
    }
}

struct MatEntry {
    key: MaterialKey,
    class: u8,
    last_used: u64,
    live: bool,
}

/// Interns equal materials to one id / uniform slot so they batch together.
#[derive(Default)]
pub(crate) struct MaterialTable {
    map: FastMap<MaterialKey, u32>,
    entries: Vec<MatEntry>,
    free: Vec<u32>,
}

impl MaterialTable {
    fn intern(
        &mut self,
        m: &Material,
        frame: u64,
        buf: &mut TrackedBuffer,
        stride: usize,
        cpu_reallocs: &mut u32,
    ) -> (u32, u8) {
        let key = MaterialKey::new(m);
        if let Some(&id) = self.map.get(&key) {
            let e = &mut self.entries[id as usize];
            e.last_used = frame;
            return (id, e.class);
        }
        let class = if m.double_sided { CLASS_DOUBLE_SIDED } else { 0 } | if m.alpha_blend { CLASS_BLEND } else { 0 };
        let entry = MatEntry {
            key,
            class,
            last_used: frame,
            live: true,
        };
        let id = match self.free.pop() {
            Some(id) => {
                self.entries[id as usize] = entry;
                id
            }
            None => {
                self.entries.push(entry);
                self.entries.len() as u32 - 1
            }
        };
        self.map.insert(key, id);
        let gpu = GpuMaterial {
            base_color: m.base_color.to_array(),
            emissive: [
                m.emissive.x,
                m.emissive.y,
                m.emissive.z,
                if m.unlit { 1.0 } else { 0.0 },
            ],
            params: [
                m.metallic,
                m.roughness,
                if m.base_color_texture.is_some() { 1.0 } else { 0.0 },
                0.0,
            ],
        };
        buf.write_at(id as usize * stride, bytemuck::bytes_of(&gpu), cpu_reallocs);
        (id, class)
    }

    /// Drop materials unused for a while so the table does not grow without bound.
    fn gc(&mut self, frame: u64) {
        const MAX_IDLE: u64 = 600;
        if !frame.is_multiple_of(256) {
            return;
        }
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.live && e.last_used + MAX_IDLE < frame {
                e.live = false;
                self.map.remove(&e.key);
                self.free.push(i as u32);
            }
        }
    }

    fn texture(&self, id: u32) -> Option<TextureHandle> {
        self.entries.get(id as usize).and_then(|e| e.key.texture)
    }
}

type MeshScratch = Vec<(MeshHandle, Option<ArenaAlloc>, Vec<Vertex>, Vec<u32>)>;

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
    draw_meta: TrackedBuffer,
    compact_params: TrackedBuffer,
    cull_out: GrowBuffer,
    args: GrowBuffer,
    compact_args: GrowBuffer,
    counts: GrowBuffer,
    explicit: Vec<Option<GpuInstances>>,
    materials: MaterialTable,
    view_bg: Option<(wgpu::BindGroup, u32)>,
    material_bgs: FastMap<Option<TextureHandle>, (wgpu::BindGroup, u32, u32)>,
    cull_bgs: FastMap<SourceBuffer, (wgpu::BindGroup, [u32; 5])>,
    compact_bg: Option<(wgpu::BindGroup, [u32; 5])>,
    arena_buffers: Option<(wgpu::Buffer, wgpu::Buffer)>,
    items: Vec<EcsItem>,
    sources: Vec<Source>,
    cameras: Vec<CameraItem>,
    views: Vec<ViewData>,
    draws: Vec<DrawCmd>,
    jobs: Vec<(SourceBuffer, GpuCullJob)>,
    dispatches: Vec<Dispatch>,
    compacts: Vec<CompactDispatch>,
    args_count: u32,
    run_count: u32,
    path: DrawPath,
    storage: bool,
    uniform_align: usize,
    max_storage_binding: u64,
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
            draw_meta: TrackedBuffer::new("space3d draw meta", s),
            compact_params: TrackedBuffer::new("space3d compact params", U::UNIFORM),
            cull_out: GrowBuffer::new("space3d culled instances", U::VERTEX | s),
            args: GrowBuffer::new("space3d indirect args", U::INDIRECT | U::COPY_DST | s),
            compact_args: GrowBuffer::new("space3d compacted args", U::INDIRECT | s),
            counts: GrowBuffer::new("space3d draw counts", U::INDIRECT | U::COPY_DST | s),
            explicit: Vec::new(),
            materials: MaterialTable::default(),
            view_bg: None,
            material_bgs: FastMap::default(),
            cull_bgs: FastMap::default(),
            compact_bg: None,
            arena_buffers: None,
            items: Vec::new(),
            sources: Vec::new(),
            cameras: Vec::new(),
            views: Vec::new(),
            draws: Vec::new(),
            jobs: Vec::new(),
            dispatches: Vec::new(),
            compacts: Vec::new(),
            args_count: 0,
            run_count: 0,
            path: DrawPath::CpuDirect,
            storage,
            uniform_align: device.limits().min_uniform_buffer_offset_alignment as usize,
            max_storage_binding: caps.max_storage_binding,
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
    fn capacities(&self) -> [usize; 8] {
        [
            self.items.capacity(),
            self.sources.capacity(),
            self.cameras.capacity(),
            self.views.capacity(),
            self.draws.capacity(),
            self.jobs.capacity(),
            self.dispatches.capacity(),
            self.compacts.capacity(),
        ]
    }
}

/// Transform a mesh-local sphere for culling instances of a given kind (mirror of cull.wgsl).
#[cfg(test)]
fn instance_sphere(mesh: &Sphere, cull: CullSource, bytes: &[u8]) -> Option<Sphere> {
    let f = |o: u32| -> f32 {
        let o = o as usize;
        f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
    };
    match cull {
        CullSource::Matrix { offset } => {
            let m = Mat4::from_cols_array(&std::array::from_fn(|i| f(offset + i as u32 * 4)));
            Some(mesh.transformed(&m))
        }
        CullSource::TranslationScale { translation, scale } => {
            let s = match scale {
                None => 1.0,
                Some((o, ScaleKind::Uniform)) => f(o).abs(),
                Some((o, ScaleKind::Vector)) => f(o).abs().max(f(o + 4).abs()).max(f(o + 8).abs()),
            };
            Some(Sphere {
                center: Vec3::new(f(translation), f(translation + 4), f(translation + 8)),
                radius: (mesh.center.length() + mesh.radius) * s,
            })
        }
        CullSource::None => None,
    }
}

fn linear_to_srgb_color(c: Vec4) -> Vec4 {
    use crate::components::linear_to_srgb;
    Vec4::new(linear_to_srgb(c.x), linear_to_srgb(c.y), linear_to_srgb(c.z), c.w)
}

impl Renderer {
    /// Extract the world, cull, upload and draw one frame. Normally called by the
    /// `Stage::Render` system of [`crate::RenderPlugin`].
    pub fn render_frame(&mut self, world: &World, resources: &mut Resources) {
        self.frame_index += 1;
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
            self.frame.cull_bgs.remove(&SourceBuffer::Explicit(h.index()));
        }
        self.prepare(world, &assets, &mut buffers, ambient, &settings, &mut encoder);
        self.encode_compute(&mut encoder);

        // Acquire the surface only when something presents.
        let mut surface_tex = None;
        if let Some(s) = &self.surface {
            match s.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                    surface_tex = Some(t)
                }
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    s.surface.configure(&self.device, &s.config);
                }
                other => tracing::debug!(?other, "skipping present"),
            }
        }
        let surface_view = surface_tex.as_ref().map(|t| {
            t.texture.create_view(&wgpu::TextureViewDescriptor {
                format: self.surface.as_ref().map(|s| s.config.format),
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
        // Non-blocking: fires staging-belt recycling and capture callbacks.
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

    fn primary_size(&self) -> (u32, u32) {
        match &self.surface {
            Some(s) => (s.config.width, s.config.height),
            None => (self.offscreen.width, self.offscreen.height),
        }
    }

    #[allow(clippy::too_many_lines)]
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
        let path = f.path;
        let gpu = path.gpu_culling();
        let multi = matches!(path, DrawPath::GpuMultiDraw | DrawPath::GpuMultiDrawCount);
        let culling = settings.frustum_culling;
        let material_stride = f.material_stride();
        let view_stride = f.view_stride();
        let params_stride = f.params_stride();

        f.arena_buffers = match (&assets.arena.vertex, &assets.arena.index) {
            (Some(v), Some(i)) => Some((v.clone(), i.clone())),
            _ => None,
        };

        // ---- lights ------------------------------------------------------------------
        let mut dir = [GpuDirLight::zeroed(); MAX_DIR_LIGHTS];
        let mut nd = 0;
        for (gt, l) in world.query::<(&GlobalTransform, &DirectionalLight)>().iter() {
            if nd < MAX_DIR_LIGHTS {
                let d = (-gt.0.z_axis.truncate()).normalize_or(Vec3::NEG_Y);
                dir[nd] = GpuDirLight {
                    direction: d.extend(0.0).to_array(),
                    color: (l.color * l.intensity).extend(1.0).to_array(),
                };
                nd += 1;
            }
        }
        let mut point = [GpuPointLight::zeroed(); MAX_POINT_LIGHTS];
        let mut np = 0;
        for (gt, l) in world.query::<(&GlobalTransform, &PointLight)>().iter() {
            if np < MAX_POINT_LIGHTS {
                point[np] = GpuPointLight {
                    position_range: gt.0.w_axis.truncate().extend(l.range.max(1e-3)).to_array(),
                    color: (l.color * l.intensity).extend(1.0).to_array(),
                };
                np += 1;
            }
        }

        // ---- cameras -----------------------------------------------------------------
        f.cameras.clear();
        for (e, cam, gt) in world.query::<(Entity, &Camera3d, &GlobalTransform)>().iter() {
            if cam.active {
                f.cameras.push(CameraItem {
                    order: cam.order,
                    entity: e.to_bits().get(),
                    camera: *cam,
                    transform: gt.0,
                });
            }
        }
        f.cameras.sort_unstable_by_key(|c| (c.order, c.entity));

        // ---- ECS batching ------------------------------------------------------------
        f.materials.gc(frame_index);
        if frame_index.is_multiple_of(256) {
            // Drop bind groups of removed textures (they keep the GPU texture alive).
            f.material_bgs
                .retain(|t, _| t.is_none_or(|h| assets.contains_texture(h)));
        }
        f.items.clear();
        let mut seq = 0u32;
        for (gt, mesh, mat, vis) in world
            .query::<(&GlobalTransform, &Mesh3d, &Material, Option<&Visibility>)>()
            .iter()
        {
            if vis == Some(&Visibility::Hidden) || !assets.is_mesh_uploaded(mesh.0) {
                continue;
            }
            let (material, class) = f.materials.intern(
                mat,
                frame_index,
                &mut f.materials_buf,
                material_stride,
                &mut cpu_reallocs,
            );
            f.items.push(EcsItem {
                key: (class as u64) << 56 | (material as u64) << 32 | mesh.0.index() as u64,
                seq,
                mesh: mesh.0,
                material,
                class,
                matrix: gt.0.to_cols_array(),
            });
            seq += 1;
        }
        f.items.sort_unstable_by_key(|i| (i.key, i.seq));
        f.sources.clear();
        f.ecs.begin();
        let mut i = 0;
        while i < f.items.len() {
            let key = f.items[i].key;
            let start = i;
            while i < f.items.len() && f.items[i].key == key {
                f.ecs.push(bytemuck::cast_slice(&f.items[i].matrix), &mut cpu_reallocs);
                i += 1;
            }
            let it = f.items[start];
            let Some((mesh, triangles)) = assets.gpu_mesh(it.mesh) else {
                continue;
            };
            f.sources.push(Source {
                blend: it.class & CLASS_BLEND != 0,
                class: it.class,
                layout: 0,
                material: it.material,
                mesh,
                triangles,
                buffer: SourceBuffer::Ecs,
                first: start as u32,
                count: (i - start) as u32,
                stride: 64,
                cull: CullSource::Matrix { offset: 0 },
                bounds: None,
            });
        }

        // ---- explicit instance buffers -----------------------------------------------
        for (im, vis) in world.query::<(&InstancedMesh, Option<&Visibility>)>().iter() {
            if vis == Some(&Visibility::Hidden) {
                continue;
            }
            let Some((mesh, triangles)) = assets.gpu_mesh(im.mesh) else {
                continue;
            };
            let Some(ib) = buffers.buffers.get_mut(im.instances.raw()) else {
                continue;
            };
            let slot = im.instances.index() as usize;
            if f.explicit.len() <= slot {
                f.explicit.resize_with(slot + 1, || None);
            }
            let fresh = !matches!(&f.explicit[slot], Some(g) if g.generation == im.instances.generation());
            if fresh {
                // A reused slot must not pick up a bind group of the previous buffer.
                f.cull_bgs.remove(&SourceBuffer::Explicit(slot as u32));
                use wgpu::BufferUsages as U;
                let s = if f.storage { U::STORAGE } else { U::empty() };
                f.explicit[slot] = Some(GpuInstances {
                    generation: im.instances.generation(),
                    buf: GrowBuffer::new("space3d instance buffer", U::VERTEX | U::COPY_DST | s),
                    synced: 0,
                    len: 0,
                    bounds: (Aabb::EMPTY, 0.0),
                });
            }
            let g = f.explicit[slot].as_mut().expect("just ensured");
            if g.synced != frame_index {
                sync_instances(device, encoder, up, g, ib.as_mut());
                g.synced = frame_index;
            }
            if g.len == 0 {
                continue;
            }
            let layout = ib.layout();
            let layout_id = pipelines.layout_id(layout);
            let (material, class) = f.materials.intern(
                &im.material,
                frame_index,
                &mut f.materials_buf,
                material_stride,
                &mut cpu_reallocs,
            );
            let mut cull = layout.cull_source();
            if (g.len as u64) * layout.stride as u64 > f.max_storage_binding {
                cull = CullSource::None;
            }
            f.sources.push(Source {
                blend: class & CLASS_BLEND != 0,
                class,
                layout: layout_id,
                material,
                mesh,
                triangles,
                buffer: SourceBuffer::Explicit(slot as u32),
                first: 0,
                count: g.len,
                stride: layout.stride,
                cull,
                bounds: Some(g.bounds),
            });
        }
        f.sources.sort_unstable_by_key(|s| {
            (
                s.blend,
                s.class,
                s.layout,
                s.material,
                s.mesh.alloc.first_index,
                s.buffer,
                s.first,
            )
        });
        stats.batches = f.sources.len() as u32;

        // ---- views -------------------------------------------------------------------
        f.views.clear();
        f.draws.clear();
        f.jobs.clear();
        f.dispatches.clear();
        f.compacts.clear();
        f.views_buf.begin();
        f.cpu_visible.begin();
        f.cull_params.begin();
        f.cull_jobs.begin();
        f.templates.begin();
        f.draw_meta.begin();
        f.compact_params.begin();
        f.args_count = 0;
        f.run_count = 0;
        let mut out_cursor = 0u64;

        for ci in 0..f.cameras.len() {
            let cam = f.cameras[ci];
            let (target, size) = match cam.camera.target {
                RenderTarget::Primary => (ViewTarget::Primary, primary_size),
                RenderTarget::Texture(h) => {
                    let Some(e) = assets.textures.get(h.raw()) else {
                        continue;
                    };
                    let Some(gpu) = &e.gpu else {
                        continue;
                    };
                    let (Some(color), Some(depth)) = (&gpu.target_view, &gpu.depth_view) else {
                        continue;
                    };
                    (
                        ViewTarget::Texture {
                            color: color.clone(),
                            depth: depth.clone(),
                        },
                        (e.width, e.height),
                    )
                }
            };
            let (vw, vh) = match cam.camera.viewport {
                Some([_, _, w, h]) => (size.0 as f32 * w, size.1 as f32 * h),
                None => (size.0 as f32, size.1 as f32),
            };
            let aspect = (vw / vh.max(1.0)).max(1e-4);
            let view_proj = cam.camera.projection.matrix(aspect) * cam.transform.inverse();
            let frustum = Frustum::from_view_proj(&view_proj);
            let view_offset = f.views.len() * view_stride;
            let gv = GpuView {
                view_proj: view_proj.to_cols_array_2d(),
                camera_pos: cam.transform.w_axis.to_array(),
                ambient: (ambient.color * ambient.intensity).extend(1.0).to_array(),
                counts: [nd as u32, np as u32, 0, 0],
                dir_lights: dir,
                point_lights: point,
            };
            f.views_buf
                .write_at(view_offset, bytemuck::bytes_of(&gv), &mut cpu_reallocs);

            let draws_start = f.draws.len();
            let jobs_start = f.jobs.len();
            let args_start = f.args_count;
            // Current multi-draw run: (layout, class, material, run_base, draw cmd index, first args).
            let mut run: Option<(u16, u8, u32, u64, usize, u32)> = None;

            for si in 0..f.sources.len() {
                let s = f.sources[si];
                if s.count == 0 {
                    continue;
                }
                stats.instances += s.count as u64;
                let tris = s.triangles as u64;
                let alloc = s.mesh.alloc;
                let base_cmd = DrawCmd {
                    layout: s.layout,
                    class: s.class,
                    material: s.material,
                    first_index: alloc.first_index,
                    index_count: alloc.index_count,
                    base_vertex: alloc.base_vertex as i32,
                    vb: VertexSource::Ecs,
                    vb_offset: 0,
                    kind: DrawKind::Direct { instances: 0 },
                };

                if gpu && s.cull != CullSource::None {
                    // GPU culling: reserve an output region, emit a job and an indirect draw.
                    let run_key = (s.layout, s.class, s.material);
                    let same_run = multi && matches!(run, Some((l, c, m, ..)) if (l, c, m) == run_key);
                    if multi && !same_run {
                        out_cursor = align_up(out_cursor, 256);
                        let cmd_index = f.draws.len();
                        let kind = if path == DrawPath::GpuMultiDrawCount {
                            DrawKind::MultiCount {
                                args_first: f.args_count,
                                count: 0,
                                run: f.run_count,
                            }
                        } else {
                            DrawKind::Multi {
                                args_first: f.args_count,
                                count: 0,
                            }
                        };
                        f.draws.push(DrawCmd {
                            vb: VertexSource::CullOut,
                            vb_offset: out_cursor,
                            kind,
                            ..base_cmd
                        });
                        run = Some((s.layout, s.class, s.material, out_cursor, cmd_index, f.args_count));
                        f.run_count += 1;
                    }
                    let region = out_cursor;
                    out_cursor += s.count as u64 * s.stride as u64;
                    let draw = f.args_count;
                    f.args_count += 1;
                    let first_instance = match run {
                        Some((.., base, _, _)) if multi => ((region - base) / s.stride as u64) as u32,
                        _ => 0,
                    };
                    let template = wgpu::util::DrawIndexedIndirectArgs {
                        index_count: alloc.index_count,
                        instance_count: 0,
                        first_index: alloc.first_index,
                        base_vertex: alloc.base_vertex as i32,
                        first_instance,
                    };
                    f.templates.push(template.as_bytes(), &mut cpu_reallocs);
                    let (run_index, run_first) = match run {
                        Some((.., cmd, first)) if multi => {
                            match &mut f.draws[cmd].kind {
                                DrawKind::Multi { count, .. } => *count += 1,
                                DrawKind::MultiCount { count, .. } => *count += 1,
                                _ => {}
                            }
                            (f.run_count - 1, first)
                        }
                        _ => (0, draw),
                    };
                    f.draw_meta
                        .push(bytemuck::cast_slice(&[run_index, run_first]), &mut cpu_reallocs);
                    if !multi {
                        f.draws.push(DrawCmd {
                            vb: VertexSource::CullOut,
                            vb_offset: region,
                            kind: DrawKind::Indirect { args: draw },
                            ..base_cmd
                        });
                    }
                    let (kind, t_off, s_off, s_kind) = match s.cull {
                        CullSource::Matrix { offset } => (0, offset / 4, 0, 0),
                        CullSource::TranslationScale { translation, scale } => match scale {
                            None => (1, translation / 4, 0, 0),
                            Some((o, ScaleKind::Uniform)) => (1, translation / 4, o / 4, 1),
                            Some((o, ScaleKind::Vector)) => (1, translation / 4, o / 4, 2),
                        },
                        CullSource::None => unreachable!(),
                    };
                    let sp = s.mesh.sphere;
                    f.jobs.push((
                        s.buffer,
                        GpuCullJob {
                            sphere: [sp.center.x, sp.center.y, sp.center.z, sp.radius],
                            src_word: s.first * s.stride / 4,
                            count: s.count,
                            stride_words: s.stride / 4,
                            kind,
                            t_off,
                            s_off,
                            s_kind,
                            dst_word: (region / 4) as u32,
                            draw,
                            wg_start: 0,
                            pad: [0; 2],
                        },
                    ));
                    stats.instances_drawn += s.count as u64;
                    stats.triangles += s.count as u64 * tris;
                    continue;
                }

                // CPU path (or buffers the GPU culler cannot decode).
                run = None;
                match s.buffer {
                    SourceBuffer::Ecs => {
                        let range = s.first as usize..(s.first + s.count) as usize;
                        let visible = if culling {
                            f.items[range.clone()]
                                .iter()
                                .filter(|it| {
                                    frustum.intersects_sphere(
                                        &s.mesh.sphere.transformed(&Mat4::from_cols_array(&it.matrix)),
                                    )
                                })
                                .count()
                        } else {
                            s.count as usize
                        };
                        stats.culled += (s.count as usize - visible) as u64;
                        if visible == 0 {
                            continue;
                        }
                        let (vb, vb_offset) = if visible == s.count as usize {
                            (VertexSource::Ecs, s.first as u64 * 64)
                        } else {
                            let at = f.cpu_visible.len();
                            for it in &f.items[range] {
                                let m = Mat4::from_cols_array(&it.matrix);
                                if frustum.intersects_sphere(&s.mesh.sphere.transformed(&m)) {
                                    f.cpu_visible.push(bytemuck::cast_slice(&it.matrix), &mut cpu_reallocs);
                                }
                            }
                            (VertexSource::CpuVisible, at as u64)
                        };
                        f.draws.push(DrawCmd {
                            vb,
                            vb_offset,
                            kind: DrawKind::Direct {
                                instances: visible as u32,
                            },
                            ..base_cmd
                        });
                        stats.instances_drawn += visible as u64;
                        stats.triangles += visible as u64 * tris;
                    }
                    SourceBuffer::Explicit(slot) => {
                        let visible = !culling
                            || match s.bounds {
                                Some((aabb, scale)) if !aabb.is_empty() => {
                                    let r = (s.mesh.sphere.center.length() + s.mesh.sphere.radius) * scale;
                                    frustum.intersects_aabb(&aabb.inflated(r))
                                }
                                // Unknown bounds: never cull.
                                _ => true,
                            };
                        if !visible {
                            stats.culled += s.count as u64;
                            continue;
                        }
                        f.draws.push(DrawCmd {
                            vb: VertexSource::Explicit(slot),
                            vb_offset: 0,
                            kind: DrawKind::Direct { instances: s.count },
                            ..base_cmd
                        });
                        stats.instances_drawn += s.count as u64;
                        stats.triangles += s.count as u64 * tris;
                    }
                }
            }

            // Group this view's culling jobs by source buffer: one dispatch per buffer.
            if f.jobs.len() > jobs_start {
                let planes = if culling {
                    frustum.planes.map(|p| p.to_array())
                } else {
                    [[0.0, 0.0, 0.0, 1.0]; 6]
                };
                f.jobs[jobs_start..].sort_unstable_by_key(|(b, j)| (*b, j.draw));
                let mut j = jobs_start;
                while j < f.jobs.len() {
                    let buffer = f.jobs[j].0;
                    let job_start = (f.cull_jobs.len() / std::mem::size_of::<GpuCullJob>()) as u32;
                    let mut wg = 0u32;
                    let mut n = 0u32;
                    while j < f.jobs.len() && f.jobs[j].0 == buffer {
                        let job = &mut f.jobs[j].1;
                        job.wg_start = wg;
                        wg += job.count.div_ceil(WORKGROUP);
                        f.cull_jobs.push(bytemuck::bytes_of(job), &mut cpu_reallocs);
                        n += 1;
                        j += 1;
                    }
                    let max_dim = 65535;
                    let wgs = if wg <= max_dim {
                        (wg, 1)
                    } else {
                        (max_dim, wg.div_ceil(max_dim))
                    };
                    let params = GpuCullParams {
                        planes,
                        job_start,
                        job_count: n,
                        total_workgroups: wg,
                        workgroups_x: wgs.0,
                    };
                    let params_offset = f.dispatches.len() * params_stride;
                    f.cull_params
                        .write_at(params_offset, bytemuck::bytes_of(&params), &mut cpu_reallocs);
                    f.dispatches.push(Dispatch {
                        buffer,
                        params_offset: params_offset as u32,
                        wg: wgs,
                    });
                }
                if path == DrawPath::GpuMultiDrawCount {
                    let p = GpuCompactParams {
                        draw_start: args_start,
                        draw_count: f.args_count - args_start,
                        pad: [0; 2],
                    };
                    let off = f.compacts.len() * f.uniform_align.max(16);
                    f.compact_params
                        .write_at(off, bytemuck::bytes_of(&p), &mut cpu_reallocs);
                    f.compacts.push(CompactDispatch {
                        params_offset: off as u32,
                        draw_count: p.draw_count,
                    });
                }
            }

            f.views.push(ViewData {
                target,
                clear: cam.camera.clear_color,
                viewport: cam.camera.viewport,
                size,
                view_offset: view_offset as u32,
                draws: draws_start..f.draws.len(),
            });
        }
        stats.views = f.views.len() as u32;

        // ---- upload ------------------------------------------------------------------
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
        f.ecs.flush(device, encoder, up);
        f.cpu_visible.flush(device, encoder, up);
        up.instance_bytes += up.bytes - inst_before;
        f.views_buf.flush(device, encoder, up);
        f.materials_buf.flush(device, encoder, up);
        if gpu {
            f.cull_params.flush(device, encoder, up);
            f.cull_jobs.flush(device, encoder, up);
            f.templates.flush(device, encoder, up);
            f.draw_meta.flush(device, encoder, up);
            f.compact_params.flush(device, encoder, up);
            f.cull_out.ensure(device, out_cursor, false, up);
            let args_bytes = f.args_count as u64 * ARGS_SIZE;
            f.args.ensure(device, args_bytes, false, up);
            f.compact_args.ensure(device, args_bytes, false, up);
            f.counts.ensure(device, f.run_count as u64 * 4, false, up);
        }

        // ---- bind groups ---------------------------------------------------------------
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
        let mgen = f.materials_buf.gpu.generation;
        for d in 0..f.draws.len() {
            let tex = f.materials.texture(f.draws[d].material);
            let (view, sampler, version) = match tex.and_then(|h| assets.textures.get(h.raw())) {
                Some(e) => match &e.gpu {
                    Some(g) => (&g.view, &g.sampler, e.version),
                    None => (&*white, &*white_sampler, u32::MAX),
                },
                None => (&*white, &*white_sampler, u32::MAX),
            };
            let valid = f
                .material_bgs
                .get(&tex)
                .is_some_and(|(_, g, v)| *g == mgen && *v == version);
            if !valid {
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("space3d material"),
                    layout: &pipelines.material_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform_binding(f.materials_buf.buffer(), std::mem::size_of::<GpuMaterial>()),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                    ],
                });
                f.material_bgs.insert(tex, (bg, mgen, version));
            }
        }
        if gpu && let Some(cull) = &pipelines.cull {
            for d in 0..f.dispatches.len() {
                let buffer = f.dispatches[d].buffer;
                let src = match buffer {
                    SourceBuffer::Ecs => &f.ecs.gpu,
                    SourceBuffer::Explicit(slot) => &f.explicit[slot as usize].as_ref().expect("synced").buf,
                };
                let gens = [
                    f.cull_params.gpu.generation,
                    f.cull_jobs.gpu.generation,
                    src.generation,
                    f.cull_out.generation,
                    f.args.generation,
                ];
                if f.cull_bgs.get(&buffer).is_some_and(|(_, g)| *g == gens) {
                    continue;
                }
                let max = f.max_storage_binding;
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("space3d cull"),
                    layout: &cull.bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform_binding(f.cull_params.buffer(), std::mem::size_of::<GpuCullParams>()),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: binding(f.cull_jobs.buffer(), max),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: binding(src.get(), max),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: binding(f.cull_out.get(), max),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: binding(f.args.get(), max),
                        },
                    ],
                });
                f.cull_bgs.insert(buffer, (bg, gens));
            }
            if !f.compacts.is_empty() {
                let gens = [
                    f.compact_params.gpu.generation,
                    f.draw_meta.gpu.generation,
                    f.args.generation,
                    f.counts.generation,
                    f.compact_args.generation,
                ];
                if f.compact_bg.as_ref().is_none_or(|(_, g)| *g != gens) {
                    let max = f.max_storage_binding;
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("space3d compact"),
                        layout: &cull.compact_bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: uniform_binding(
                                    f.compact_params.buffer(),
                                    std::mem::size_of::<GpuCompactParams>(),
                                ),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: binding(f.draw_meta.buffer(), max),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: binding(f.args.get(), max),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: binding(f.counts.get(), max),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: binding(f.compact_args.get(), max),
                            },
                        ],
                    });
                    f.compact_bg = Some((bg, gens));
                }
            }
        }

        let caps_after = f.capacities();
        cpu_reallocs += caps_before
            .iter()
            .zip(caps_after.iter())
            .filter(|(a, b)| a != b)
            .count() as u32;
        stats.cpu_reallocations += cpu_reallocs;
    }

    /// Reset indirect args from templates, then run culling (+ draw compaction).
    fn encode_compute(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let f = &mut self.frame;
        if !f.path.gpu_culling() || f.dispatches.is_empty() {
            return;
        }
        let Some(cull) = &self.pipelines.cull else {
            return;
        };
        let args_bytes = f.args_count as u64 * ARGS_SIZE;
        encoder.copy_buffer_to_buffer(f.templates.buffer(), 0, f.args.get(), 0, args_bytes);
        if !f.compacts.is_empty() && f.run_count > 0 {
            encoder.clear_buffer(f.counts.get(), 0, Some(align_up(f.run_count as u64 * 4, 4)));
        }
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("space3d cull"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&cull.pipeline);
        for d in &f.dispatches {
            let (bg, _) = &f.cull_bgs[&d.buffer];
            pass.set_bind_group(0, bg, &[d.params_offset]);
            pass.dispatch_workgroups(d.wg.0, d.wg.1, 1);
            self.stats.compute_dispatches += 1;
        }
        if let Some((bg, _)) = &f.compact_bg
            && !f.compacts.is_empty()
        {
            pass.set_pipeline(&cull.compact);
            for c in &f.compacts {
                pass.set_bind_group(0, bg, &[c.params_offset]);
                pass.dispatch_workgroups(c.draw_count.div_ceil(WORKGROUP).max(1), 1, 1);
                self.stats.compute_dispatches += 1;
            }
        }
    }

    /// Record render passes for the prepared views. With `primary_to_offscreen`, only primary
    /// views are re-recorded into the offscreen target (windowed capture).
    pub(crate) fn encode_views(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        surface_view: Option<&wgpu::TextureView>,
        primary_to_offscreen: bool,
    ) {
        let views = std::mem::take(&mut self.frame.views);
        let mut presented = false;
        for view in &views {
            let is_primary = matches!(view.target, ViewTarget::Primary);
            if primary_to_offscreen && !is_primary {
                continue;
            }
            // Resolve attachments.
            let (color, resolve, depth, format, samples, manual_srgb) = match (&view.target, surface_view) {
                (ViewTarget::Primary, Some(sv)) if !primary_to_offscreen => {
                    let s = self.surface.as_ref().expect("surface");
                    let a = &s.attachments;
                    match &a.msaa_view {
                        Some(m) => (
                            m.clone(),
                            Some(sv.clone()),
                            a.depth_view.clone(),
                            a.format,
                            a.samples,
                            s.manual_srgb,
                        ),
                        None => (sv.clone(), None, a.depth_view.clone(), a.format, 1, s.manual_srgb),
                    }
                }
                (ViewTarget::Primary, _) => {
                    if !primary_to_offscreen && self.surface.is_some() {
                        // Surface frame unavailable (minimized/occluded): skip.
                        continue;
                    }
                    let a = &self.offscreen;
                    let target = a.color_view.clone().expect("offscreen color");
                    match &a.msaa_view {
                        Some(m) => (
                            m.clone(),
                            Some(target),
                            a.depth_view.clone(),
                            a.format,
                            a.samples,
                            false,
                        ),
                        None => (target, None, a.depth_view.clone(), a.format, 1, false),
                    }
                }
                (ViewTarget::Texture { color, depth }, _) => {
                    (color.clone(), None, depth.clone(), OFFSCREEN_FORMAT, 1, false)
                }
            };
            presented |= is_primary;
            self.encode_pass(
                encoder,
                view,
                &color,
                resolve.as_ref(),
                &depth,
                format,
                samples,
                manual_srgb,
            );
        }
        // Nothing drew to the window: still clear it so stale content is not shown.
        if !presented
            && !primary_to_offscreen
            && let Some(sv) = surface_view
        {
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("space3d clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: sv,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        self.frame.views = views;
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_pass(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &ViewData,
        color: &wgpu::TextureView,
        resolve: Option<&wgpu::TextureView>,
        depth: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        samples: u32,
        manual_srgb: bool,
    ) {
        let load = match view.clear {
            Some(c) => {
                let c = if manual_srgb { linear_to_srgb_color(c) } else { c };
                wgpu::LoadOp::Clear(wgpu::Color {
                    r: c.x as f64,
                    g: c.y as f64,
                    b: c.z as f64,
                    a: c.w as f64,
                })
            }
            None => wgpu::LoadOp::Load,
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("space3d forward"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color,
                depth_slice: None,
                resolve_target: resolve,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        if let Some([x, y, w, h]) = view.viewport {
            let (tw, th) = (view.size.0 as f32, view.size.1 as f32);
            pass.set_viewport(x * tw, y * th, (w * tw).max(1.0), (h * th).max(1.0), 0.0, 1.0);
        }
        let f = &self.frame;
        let (Some((vb, ib)), Some((view_bg, _))) = (&f.arena_buffers, &f.view_bg) else {
            return;
        };
        if view.draws.is_empty() {
            return;
        }
        pass.set_bind_group(0, view_bg, &[view.view_offset]);
        pass.set_vertex_buffer(0, vb.slice(..));
        pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
        let material_stride = f.material_stride() as u32;
        let mut last_pipeline: Option<(u16, u8)> = None;
        let mut last_material = u32::MAX;
        let mut last_vb: Option<(VertexSource, u64)> = None;
        for cmd in &f.draws[view.draws.clone()] {
            if last_pipeline != Some((cmd.layout, cmd.class)) {
                let key = PipelineKey {
                    layout: cmd.layout,
                    class: cmd.class,
                    format,
                    samples,
                    manual_srgb,
                };
                pass.set_pipeline(self.pipelines.forward(&self.device, key));
                last_pipeline = Some((cmd.layout, cmd.class));
            }
            if cmd.material != last_material {
                let tex = f.materials.texture(cmd.material);
                let Some((bg, ..)) = f.material_bgs.get(&tex) else {
                    continue;
                };
                pass.set_bind_group(1, bg, &[cmd.material * material_stride]);
                last_material = cmd.material;
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
                DrawKind::Direct { instances } => {
                    pass.draw_indexed(indices, cmd.base_vertex, 0..instances);
                }
                DrawKind::Indirect { args } => {
                    pass.draw_indexed_indirect(f.args.get(), args as u64 * ARGS_SIZE);
                    self.stats.indirect_draw_calls += 1;
                }
                DrawKind::Multi { args_first, count } => {
                    pass.multi_draw_indexed_indirect(f.args.get(), args_first as u64 * ARGS_SIZE, count);
                    self.stats.indirect_draw_calls += 1;
                    self.stats.multi_draw_calls += 1;
                }
                DrawKind::MultiCount { args_first, count, run } => {
                    pass.multi_draw_indexed_indirect_count(
                        f.compact_args.get(),
                        args_first as u64 * ARGS_SIZE,
                        f.counts.get(),
                        run as u64 * 4,
                        count,
                    );
                    self.stats.indirect_draw_calls += 1;
                    self.stats.multi_draw_calls += 1;
                }
            }
            self.stats.draw_calls += 1;
        }
    }

    /// Re-record the last frame's primary views into the offscreen target (for capture).
    pub(crate) fn encode_primary_to_offscreen(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let (w, h) = self.primary_size();
        if (self.offscreen.width, self.offscreen.height) != (w, h) {
            self.offscreen =
                crate::renderer::Attachments::new(&self.device, w, h, self.offscreen.samples, OFFSCREEN_FORMAT, true);
        }
        let draw_calls = self.stats.draw_calls;
        self.encode_views(encoder, None, true);
        self.stats.draw_calls = draw_calls;
    }
}

/// Upload an explicit instance buffer's dirty ranges (or all of it after reallocation / when
/// more than half changed / for `Stream` buffers).
fn sync_instances(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    up: &mut crate::gpu::Uploader,
    g: &mut GpuInstances,
    ib: &mut dyn ErasedInstanceBuffer,
) {
    let stride = ib.layout().stride as usize;
    let len = ib.len();
    let needed = len * stride;
    let realloc = g
        .buf
        .ensure(device, needed as u64, ib.usage() == BufferUsage::Static, up);
    let before = up.bytes;
    if needed > 0 {
        let bytes = ib.bytes();
        let full = realloc || ib.usage() == BufferUsage::Stream || ib.dirty().total() * 2 > len;
        if full {
            up.write(encoder, g.buf.get(), 0, &bytes[..needed]);
        } else {
            for r in ib.dirty().ranges() {
                let (a, b) = (r.start.min(len) * stride, r.end.min(len) * stride);
                if a < b {
                    up.write(encoder, g.buf.get(), a as u64, &bytes[a..b]);
                }
            }
        }
    }
    up.instance_bytes += up.bytes - before;
    g.len = len as u32;
    g.bounds = ib.refresh_bounds();
    ib.clear_dirty();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{InstanceCompact, InstanceData};

    #[test]
    fn material_keys_intern() {
        let a = MaterialKey::new(&Material::default());
        let b = MaterialKey::new(&Material::default());
        let c = MaterialKey::new(&Material::srgb(1.0, 0.0, 0.0));
        assert!(a == b && a != c);
    }

    #[test]
    fn instance_sphere_kinds() {
        let mesh = Sphere {
            center: Vec3::ZERO,
            radius: 1.0,
        };
        let c = InstanceCompact::new(Vec3::new(1.0, 2.0, 3.0), glam::Quat::IDENTITY, 2.0);
        let s = instance_sphere(&mesh, InstanceCompact::LAYOUT.cull_source(), bytemuck::bytes_of(&c)).unwrap();
        assert_eq!(s.center, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(s.radius, 2.0);
    }
}
