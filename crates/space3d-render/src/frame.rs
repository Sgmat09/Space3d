//! Per-frame work: extract → batch → cull (CPU or GPU) → upload → encode.
//!
//! Every scratch `Vec` here lives in [`FrameState`] and is reused, so the steady state performs
//! no heap allocation in our code (`RenderStats::cpu_reallocations` counts the exceptions).

use crate::arena::{ArenaAlloc, VERTEX_SIZE, Vertex};
use crate::assets::{GpuMesh, RenderAssets};
use crate::components::{
    AmbientLight, Camera3d, DirectionalLight, Material, Mesh3d, PointLight, RenderTarget, Visibility,
};
use crate::culling::{Aabb, Frustum, Sphere};
use crate::gpu::{GrowBuffer, TrackedBuffer, align_up};
use crate::handles::FastMap;
use crate::handles::{MeshHandle, ShaderHandle, TextureHandle};
use crate::instance::{
    BufferUsage, CULL_BLOCK, CullBounds, CullFloat, CullSource, ErasedInstanceBuffer, InstanceBuffers, InstancedMesh,
    ScaleKind,
};
use crate::pipeline::{
    CLASS_BLEND, CLASS_DOUBLE_SIDED, CULL_VIEWS, GpuCompactParams, GpuCullJob, GpuCullParams, GpuDirLight, GpuFrustum,
    GpuMaterial, GpuPointLight, GpuView, MAX_DIR_LIGHTS, MAX_POINT_LIGHTS, OFFSCREEN_FORMAT, PipelineKey,
};
use crate::readback::CullReadback;
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
    /// Custom shader of the material and its source version (resolved in `prepare`).
    shader: Option<(ShaderHandle, u32)>,
    first_index: u32,
    index_count: u32,
    base_vertex: i32,
    vb: VertexSource,
    vb_offset: u64,
    kind: DrawKind,
}

/// What selects a draw's forward pipeline besides the pass's target format and samples:
/// (instance layout, class, layered texture, custom shader and its version).
type PipelineState = (u16, u8, bool, Option<(ShaderHandle, u32)>);

/// Scratch of [`order_by_key`].
#[derive(Default)]
struct KeyBuckets {
    map: FastMap<u64, u32>,
    /// Direct-mapped `(key, bucket)` cache in front of `map` (`u32::MAX` = empty).
    cache: Vec<(u64, u32)>,
    /// Per bucket: its key, then its item count, then its write cursor.
    keys: Vec<u64>,
    counts: Vec<u32>,
    /// Bucket of each item.
    of: Vec<u32>,
    /// Buckets sorted by key.
    sorted: Vec<u32>,
}

/// Group item indices by key: `order` lists the items by ascending key and, within a key, in
/// their original order — exactly a sort by `(key, index)`, but a counting sort: O(items +
/// keys · log keys), no element moves. `runs` gets `(key, start, count)` per key.
fn order_by_key(keys: &[u64], order: &mut Vec<u32>, runs: &mut Vec<(u64, u32, u32)>, s: &mut KeyBuckets) {
    const CACHE: usize = 256;
    s.map.clear();
    s.keys.clear();
    s.counts.clear();
    s.of.clear();
    s.cache.clear();
    s.cache.resize(CACHE, (0, u32::MAX));
    let mut last = None;
    for &k in keys {
        let b = match last {
            Some((lk, lb)) if lk == k => lb,
            _ => {
                let slot = (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - CACHE.trailing_zeros())) as usize;
                let b = match s.cache[slot] {
                    (ck, cb) if cb != u32::MAX && ck == k => cb,
                    _ => {
                        let next = s.keys.len() as u32;
                        let b = *s.map.entry(k).or_insert(next);
                        if b == next {
                            s.keys.push(k);
                            s.counts.push(0);
                        }
                        s.cache[slot] = (k, b);
                        b
                    }
                };
                last = Some((k, b));
                b
            }
        };
        s.counts[b as usize] += 1;
        s.of.push(b);
    }
    s.sorted.clear();
    s.sorted.extend(0..s.keys.len() as u32);
    let bucket_keys = &s.keys;
    s.sorted.sort_unstable_by_key(|&b| bucket_keys[b as usize]);
    runs.clear();
    let mut at = 0u32;
    for &b in &s.sorted {
        let count = s.counts[b as usize];
        runs.push((s.keys[b as usize], at, count));
        s.counts[b as usize] = at;
        at += count;
    }
    order.clear();
    order.resize(keys.len(), 0);
    for (i, &b) in s.of.iter().enumerate() {
        let cursor = &mut s.counts[b as usize];
        order[*cursor as usize] = i as u32;
        *cursor += 1;
    }
}

/// ECS batches at or above this many instances are frustum-tested on all rayon workers.
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_CULL: usize = 16_384;

/// Extracted ECS entities at or above this count (per archetype) gather their matrices on all
/// rayon workers.
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_GATHER: usize = 32_768;

/// `dst[k] = transforms[rows[k]]` as column arrays (a strided 64-byte copy per entity).
fn gather_matrices(transforms: &[GlobalTransform], rows: &[u32], dst: &mut [[f32; 16]]) {
    debug_assert_eq!(rows.len(), dst.len());
    let copy = |(d, &r): (&mut [f32; 16], &u32)| *d = transforms[r as usize].0.to_cols_array();
    #[cfg(not(target_arch = "wasm32"))]
    if rows.len() >= PARALLEL_GATHER {
        use rayon::prelude::*;
        dst.par_iter_mut()
            .zip(rows.par_iter())
            .with_min_len(4096)
            .for_each(copy);
        return;
    }
    dst.iter_mut().zip(rows).for_each(copy);
}

/// Frustum-test one ECS batch (`order` indexes `matrices`), one bit per instance in `mask`;
/// returns the number visible.
fn cull_mask(matrices: &[[f32; 16]], order: &[u32], sphere: Sphere, frustum: &Frustum, mask: &mut Vec<u64>) -> usize {
    mask.clear();
    mask.resize(order.len().div_ceil(64), 0);
    let word = |w: &mut u64, items: &[u32]| {
        let mut bits = 0u64;
        for (k, &i) in items.iter().enumerate() {
            let m = Mat4::from_cols_array(&matrices[i as usize]);
            bits |= (frustum.intersects_sphere(&sphere.transformed(&m)) as u64) << k;
        }
        *w = bits;
        bits.count_ones() as usize
    };
    #[cfg(not(target_arch = "wasm32"))]
    if order.len() >= PARALLEL_CULL {
        use rayon::prelude::*;
        return mask
            .par_iter_mut()
            .zip(order.par_chunks(64))
            .with_min_len(16)
            .map(|(w, items)| word(w, items))
            .sum();
    }
    mask.iter_mut()
        .zip(order.chunks(64))
        .map(|(w, items)| word(w, items))
        .sum()
}

#[derive(Clone, Copy, Debug)]
struct Source {
    blend: bool,
    class: u8,
    layout: u16,
    /// Rest of the pipeline key besides class and layout: custom shader and texture kind of
    /// the material (see [`pipeline_rank`]).
    pipeline: u64,
    material: u32,
    mesh: GpuMesh,
    triangles: u32,
    buffer: SourceBuffer,
    first: u32,
    count: u32,
    stride: u32,
    cull: CullSource,
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
        handle: TextureHandle,
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
    /// Some draw of this view samples the view's own render target. A texture cannot be a
    /// color attachment and a sampled resource in the same pass, so such draws sample a copy
    /// of the target taken just before the pass (its previous content) instead.
    feedback: bool,
}

/// Copy of a render target sampled by materials drawn into that same target.
struct Feedback {
    /// `TextureEntry::version` of the render target it was created for.
    version: u32,
    size: (u32, u32),
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Material bind group sampling `view` (and the materials buffer generation it binds).
    bind_group: Option<(wgpu::BindGroup, u32)>,
    last_used: u64,
}

/// One mark and one scatter dispatch: the culling jobs of one source buffer, for the views of
/// one window of [`CULL_VIEWS`] frustums.
struct Dispatch {
    buffer: SourceBuffer,
    params: GpuCullParams,
    params_offset: u32,
    /// Window of frustums the dispatch's jobs read (bound at a fixed offset of
    /// `cull_params`: one bind group per source buffer and window).
    window: u32,
    wg: (u32, u32),
}

/// Bytes of one window of frustums (the `frusta` binding of cull.wgsl).
const FRUSTA_WINDOW: usize = CULL_VIEWS * std::mem::size_of::<GpuFrustum>();

/// Workgroup grid for `n` workgroups within the 65535-per-dimension limit.
fn dispatch_dims(n: u32) -> (u32, u32) {
    const MAX_DIM: u32 = 65535;
    if n <= MAX_DIM {
        (n, 1)
    } else {
        (MAX_DIM, n.div_ceil(MAX_DIM))
    }
}

pub(crate) struct GpuInstances {
    generation: u32,
    buf: GrowBuffer,
    synced: u64,
    len: u32,
    /// The buffer exceeds the device's `max_buffer_size` (logged once, not drawn).
    too_large: bool,
}

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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct MaterialKey {
    words: [u32; 9],
    flags: u8,
    texture: Option<TextureHandle>,
    shader: Option<ShaderHandle>,
}

impl MaterialKey {
    fn new(m: &Material) -> Self {
        let b = m.base_color;
        let e = m.emissive;
        Self {
            words: [b.x, b.y, b.z, b.w, m.metallic, m.roughness, e.x, e.y, e.z].map(f32::to_bits),
            flags: (m.unlit as u8) | (m.double_sided as u8) << 1 | (m.alpha_blend as u8) << 2,
            texture: m.base_color_texture,
            shader: m.shader,
        }
    }

    /// Slot in [`MaterialTable`]'s direct-mapped cache (a cheap multiplicative hash).
    fn cache_slot(&self) -> usize {
        let t = self.texture.map_or(0, |t| t.index() ^ t.generation().rotate_left(16));
        let sh = self
            .shader
            .map_or(0, |s| (s.index() + 1) ^ s.generation().rotate_left(16));
        let mut h = (self.flags as u32 ^ t ^ sh.rotate_left(8)).wrapping_mul(0x9E37_79B1);
        for w in self.words {
            h = (h.rotate_left(5) ^ w).wrapping_mul(0x9E37_79B1);
        }
        (h >> (32 - MATERIAL_CACHE.trailing_zeros())) as usize
    }
}

struct MatEntry {
    key: MaterialKey,
    class: u8,
    last_used: u64,
    live: bool,
}

/// Slots of the direct-mapped cache in front of the material map (a power of two).
const MATERIAL_CACHE: usize = 64;

#[derive(Clone, Copy)]
struct CachedMaterial {
    key: MaterialKey,
    id: u32,
    class: u8,
    /// Frame whose `last_used` this entry already refreshed.
    frame: u64,
}

/// Interns equal materials to one id / uniform slot so they batch together.
///
/// Every ECS entity carries its `Material` by value, so interning runs once per entity per
/// frame. The last material seen and a small direct-mapped cache answer almost all of those
/// with one key comparison (neighbouring entities mostly share materials, and a scene rarely
/// uses more materials at once than the cache has slots); only misses hash and probe the map.
pub(crate) struct MaterialTable {
    map: FastMap<MaterialKey, u32>,
    entries: Vec<MatEntry>,
    free: Vec<u32>,
    cache: [Option<CachedMaterial>; MATERIAL_CACHE],
    last: Option<CachedMaterial>,
    /// Interns that missed the cache (tests).
    misses: u64,
}

impl Default for MaterialTable {
    fn default() -> Self {
        Self {
            map: FastMap::default(),
            entries: Vec::new(),
            free: Vec::new(),
            cache: [None; MATERIAL_CACHE],
            last: None,
            misses: 0,
        }
    }
}

impl MaterialTable {
    /// Material id and pipeline class of `m`, allocating a uniform slot for new materials.
    #[inline]
    fn intern(
        &mut self,
        m: &Material,
        frame: u64,
        buf: &mut TrackedBuffer,
        stride: usize,
        cpu_reallocs: &mut u32,
    ) -> (u32, u8) {
        let key = MaterialKey::new(m);
        match &self.last {
            Some(l) if l.frame == frame && l.key == key => (l.id, l.class),
            _ => self.intern_cached(m, key, frame, buf, stride, cpu_reallocs),
        }
    }

    /// [`Self::intern`] when the material differs from the previous one (kept out of line so
    /// the per-entity fast path stays small).
    #[inline(never)]
    fn intern_cached(
        &mut self,
        m: &Material,
        key: MaterialKey,
        frame: u64,
        buf: &mut TrackedBuffer,
        stride: usize,
        cpu_reallocs: &mut u32,
    ) -> (u32, u8) {
        let slot = key.cache_slot();
        if let Some(c) = &mut self.cache[slot]
            && c.key == key
        {
            if c.frame != frame {
                c.frame = frame;
                self.entries[c.id as usize].last_used = frame;
            }
            self.last = Some(*c);
            return (c.id, c.class);
        }
        self.misses += 1;
        let (id, class) = self.intern_slow(m, key, frame, buf, stride, cpu_reallocs);
        let c = CachedMaterial { key, id, class, frame };
        self.cache[slot] = Some(c);
        self.last = Some(c);
        (id, class)
    }

    #[cold]
    fn intern_slow(
        &mut self,
        m: &Material,
        key: MaterialKey,
        frame: u64,
        buf: &mut TrackedBuffer,
        stride: usize,
        cpu_reallocs: &mut u32,
    ) -> (u32, u8) {
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
        let mut freed = false;
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.live && e.last_used + MAX_IDLE < frame {
                e.live = false;
                self.map.remove(&e.key);
                self.free.push(i as u32);
                freed = true;
            }
        }
        if freed {
            // Freed ids get reused for other materials: forget every cached key → id.
            self.cache = [None; MATERIAL_CACHE];
            self.last = None;
        }
    }

    fn texture(&self, id: u32) -> Option<TextureHandle> {
        self.entries.get(id as usize).and_then(|e| e.key.texture)
    }

    fn shader(&self, id: u32) -> Option<ShaderHandle> {
        self.entries.get(id as usize).and_then(|e| e.key.shader)
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
    /// Largest `base_vertex` passed to a direct draw since the last `prepare`.
    #[cfg(test)]
    pub(crate) max_direct_base_vertex: i32,
    /// Test stand-in for a window whose surface is lost at the next frame's acquire: the
    /// renderer leaves its offscreen target alone like a windowed one, and the next frame
    /// takes the lost-surface path after preparing its views.
    #[cfg(test)]
    pub(crate) simulate_surface_loss: bool,
    /// Times the ECS batch order was rebuilt.
    #[cfg(test)]
    pub(crate) ecs_reorders: u64,
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
            #[cfg(test)]
            max_direct_base_vertex: 0,
            #[cfg(test)]
            simulate_surface_loss: false,
            #[cfg(test)]
            ecs_reorders: 0,
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

    /// Capacity snapshot of scratch vectors (to count CPU reallocations).
    fn capacities(&self) -> [usize; 20] {
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
        ]
    }
}

/// How many instances of each batch (`counts`) to keep so that at most `cap` are kept in
/// total, taking the overflow from the largest batches: every batch keeps `min(count, t)` for
/// the largest `t` that fits, and what remains below `cap` goes one instance each to the first
/// batches (in batch order) that are still cut. A small batch is never dropped because a large
/// one sorts before it. Deterministic; only runs when the ECS data outgrows `max_buffer_size`.
fn fair_share(counts: impl Iterator<Item = u32> + Clone, cap: u64, kept: &mut Vec<u32>) {
    let total = |t: u32| counts.clone().map(|c| c.min(t) as u64).sum::<u64>();
    let (mut lo, mut hi) = (0u32, counts.clone().max().unwrap_or(0));
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if total(mid) <= cap {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut spare = cap - total(lo);
    kept.clear();
    for c in counts {
        let extra = u32::from(c > lo && spare > 0);
        spare -= extra as u64;
        kept.push(c.min(lo) + extra);
    }
}

/// Transform a mesh-local sphere for culling instances of a given kind (mirror of cull.wgsl).
#[cfg(test)]
fn instance_sphere(mesh: &Sphere, cull: CullSource, bytes: &[u8]) -> Option<Sphere> {
    let f = |o: u32| -> f32 {
        let o = o as usize;
        f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
    };
    let h = |o: u32| -> f32 {
        let o = o as usize;
        crate::instance::f16_to_f32(u16::from_le_bytes([bytes[o], bytes[o + 1]]))
    };
    let v = |o: u32, format: CullFloat| match format {
        CullFloat::F32 => Vec3::new(f(o), f(o + 4), f(o + 8)),
        CullFloat::F16 => Vec3::new(h(o), h(o + 2), h(o + 4)),
    };
    match cull {
        CullSource::Matrix { offset } => {
            let m = Mat4::from_cols_array(&std::array::from_fn(|i| f(offset + i as u32 * 4)));
            Some(mesh.transformed(&m))
        }
        CullSource::TranslationScale {
            translation,
            format,
            scale,
        } => {
            let s = match scale {
                None => 1.0,
                Some((o, ScaleKind::Uniform, CullFloat::F32)) => f(o).abs(),
                Some((o, ScaleKind::Uniform, CullFloat::F16)) => h(o).abs(),
                Some((o, ScaleKind::Vector, sf)) => v(o, sf).abs().max_element(),
            };
            Some(Sphere {
                center: v(translation, format),
                radius: (mesh.center.length() + mesh.radius) * s,
            })
        }
        CullSource::None => None,
    }
}

/// The part of a material's pipeline key that is not its class: its custom shader (slot + 1,
/// 0 for none) and whether its texture is an array (`texture_2d_array` pipeline variant).
fn pipeline_rank(materials: &MaterialTable, assets: &RenderAssets, material: u32) -> u64 {
    let shader = materials.shader(material).map_or(0, |h| h.index() as u64 + 1);
    let layered = materials
        .texture(material)
        .and_then(|h| assets.textures.get(h.raw()))
        .and_then(|e| e.gpu.as_ref())
        .is_some_and(|g| g.layered);
    shader << 1 | layered as u64
}

/// Draw order of batches: opaque before blended, then pipeline (class, instance layout,
/// shader/texture kind), then material, then mesh — so each pipeline is bound once per view
/// and each material once per pipeline, and multi-draw runs (equal pipeline and material) are
/// contiguous whatever the spawn order. Buffer and first instance make the order total.
fn source_sort_key(s: &Source) -> (bool, u8, u16, u64, u32, u32, SourceBuffer, u32) {
    (
        s.blend,
        s.class,
        s.layout,
        s.pipeline,
        s.material,
        s.mesh.alloc.first_index,
        s.buffer,
        s.first,
    )
}

/// `(kind, t_off, s_off, s_kind)` of a [`GpuCullJob`] (see `Job` in cull.wgsl): translation
/// offset in words, scale offset in bytes (f16 scales may sit in the upper half of a word).
fn cull_job_encoding(cull: CullSource) -> (u32, u32, u32, u32) {
    match cull {
        CullSource::Matrix { offset } => (0, offset / 4, 0, 0),
        CullSource::TranslationScale {
            translation,
            format,
            scale,
        } => {
            let kind = match format {
                CullFloat::F32 => 1,
                CullFloat::F16 => 2,
            };
            let (s_off, s_kind) = match scale {
                None => (0, 0),
                Some((o, ScaleKind::Uniform, CullFloat::F32)) => (o, 1),
                Some((o, ScaleKind::Vector, CullFloat::F32)) => (o, 2),
                Some((o, ScaleKind::Uniform, CullFloat::F16)) => (o, 3),
                Some((o, ScaleKind::Vector, CullFloat::F16)) => (o, 4),
            };
            (kind, translation / 4, s_off, s_kind)
        }
        CullSource::None => unreachable!("not GPU-culled"),
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
            // Pipelines of removed custom shaders.
            pipelines.retain_custom(|h| assets.contains_shader(h));
        }
        // Extract visible entities in query order: archetype by archetype (the order hecs
        // queries use), so component columns are plain slices. Keys (material interning)
        // are computed serially; the matrices of the kept rows are gathered afterwards, on all
        // workers for large archetypes.
        f.ecs_keys.clear();
        f.ecs_meshes.clear();
        f.ecs_rows.clear();
        let mut last_mesh: Option<(MeshHandle, bool)> = None;
        let mut kept = 0usize;
        for arch in world.archetypes() {
            if arch.is_empty() || !(arch.has::<GlobalTransform>() && arch.has::<Mesh3d>() && arch.has::<Material>()) {
                continue;
            }
            let (Some(transforms), Some(meshes), Some(materials)) = (
                arch.get::<&GlobalTransform>(),
                arch.get::<&Mesh3d>(),
                arch.get::<&Material>(),
            ) else {
                continue;
            };
            let visibility = arch.get::<&Visibility>();
            let first = kept;
            for (row, (mesh, mat)) in meshes.iter().zip(materials.iter()).enumerate() {
                if visibility.as_ref().is_some_and(|v| v[row] == Visibility::Hidden) {
                    continue;
                }
                let uploaded = match last_mesh {
                    Some((m, uploaded)) if m == mesh.0 => uploaded,
                    _ => {
                        let uploaded = assets.is_mesh_uploaded(mesh.0);
                        last_mesh = Some((mesh.0, uploaded));
                        uploaded
                    }
                };
                if !uploaded {
                    continue;
                }
                let (material, class) = f.materials.intern(
                    mat,
                    frame_index,
                    &mut f.materials_buf,
                    material_stride,
                    &mut cpu_reallocs,
                );
                f.ecs_keys
                    .push((class as u64) << 56 | (material as u64) << 32 | mesh.0.index() as u64);
                f.ecs_meshes.push(mesh.0);
                f.ecs_rows.push(row as u32);
                kept += 1;
            }
            // `ecs_matrices` is only resized, never cleared: an unchanged entity count costs
            // no zero-fill, and every element up to `kept` is overwritten here.
            if f.ecs_matrices.len() < kept {
                f.ecs_matrices.resize(kept, [0.0; 16]);
            }
            gather_matrices(&transforms, &f.ecs_rows[first..kept], &mut f.ecs_matrices[first..kept]);
        }
        f.ecs_matrices.truncate(kept);
        // Batches: by key, in query (spawn) order within a key. The order depends only on the
        // key sequence, so while that is unchanged (a static scene, or only transforms
        // moving) last frame's order is reused as is.
        if f.ecs_keys != f.ecs_prev_keys || f.ecs_order.len() != f.ecs_keys.len() {
            order_by_key(&f.ecs_keys, &mut f.ecs_order, &mut f.ecs_runs, &mut f.ecs_buckets);
            f.ecs_prev_keys.clone_from(&f.ecs_keys);
            #[cfg(test)]
            {
                f.ecs_reorders += 1;
            }
        }
        // The `ecs` buffer cannot exceed the device's `max_buffer_size` (creating it would fail
        // validation and lose every frame). Past it the largest batches are cut (see
        // `fair_share`: smaller batches are kept whole), their kept entities are listed in
        // `ecs_cut` in place of `ecs_order`, and the overflow is logged once.
        let max_entities = f.max_buffer_size / 64;
        let overflow = f.ecs_order.len() as u64 > max_entities;
        if overflow {
            fair_share(f.ecs_runs.iter().map(|r| r.2), max_entities, &mut f.ecs_kept);
            f.ecs_cut.clear();
            for (&(_, first, _), &kept) in f.ecs_runs.iter().zip(&f.ecs_kept) {
                f.ecs_cut
                    .extend_from_slice(&f.ecs_order[first as usize..(first + kept) as usize]);
            }
            if !f.ecs_overflow {
                tracing::error!(
                    entities = f.ecs_order.len(),
                    drawn = f.ecs_cut.len(),
                    max_buffer_size = f.max_buffer_size,
                    "ECS instance data exceeds the device's max_buffer_size; the largest batches \
                     are not drawn whole (use InstanceBuffers for this many instances)"
                );
            }
        }
        f.ecs_overflow = overflow;
        let ecs_order = if overflow { &f.ecs_cut } else { &f.ecs_order };
        // Matrices in batch order, diffed against last frame's.
        f.ecs.begin();
        f.ecs.write_gather(0, ecs_order, &f.ecs_matrices, &mut cpu_reallocs);
        f.sources.clear();
        let mut cut_first = 0u32;
        for (i, &(key, first, count)) in f.ecs_runs.iter().enumerate() {
            let (first, count) = if overflow {
                let kept = f.ecs_kept[i];
                cut_first += kept;
                (cut_first - kept, kept)
            } else {
                (first, count)
            };
            if count == 0 {
                continue;
            }
            let Some((mesh, triangles)) = assets.gpu_mesh(f.ecs_meshes[ecs_order[first as usize] as usize]) else {
                continue;
            };
            // GPU culling reads the ECS buffer through one storage binding (from offset 0): a
            // batch ending past it is culled on the CPU instead.
            let cull = if (first as u64 + count as u64) * 64 <= f.max_storage_binding {
                CullSource::Matrix { offset: 0 }
            } else {
                CullSource::None
            };
            let class = (key >> 56) as u8;
            f.sources.push(Source {
                blend: class & CLASS_BLEND != 0,
                class,
                layout: 0,
                pipeline: 0,
                material: (key >> 32) as u32 & 0x00FF_FFFF,
                mesh,
                triangles,
                buffer: SourceBuffer::Ecs,
                first,
                count,
                stride: 64,
                cull,
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
                let reused = SourceBuffer::Explicit(slot as u32);
                f.cull_bgs.retain(|(b, _), _| *b != reused);
                use wgpu::BufferUsages as U;
                let s = if f.storage { U::STORAGE } else { U::empty() };
                f.explicit[slot] = Some(GpuInstances {
                    generation: im.instances.generation(),
                    buf: GrowBuffer::new("space3d instance buffer", U::VERTEX | U::COPY_DST | s),
                    synced: 0,
                    len: 0,
                    too_large: false,
                });
            }
            let g = f.explicit[slot].as_mut().expect("just ensured");
            let bytes = ib.len() as u64 * ib.layout().stride as u64;
            if bytes > f.max_buffer_size {
                // Creating the GPU buffer would fail validation: skip the buffer (its dirty
                // ranges stay pending) until it shrinks.
                if !g.too_large {
                    tracing::error!(
                        bytes,
                        max = f.max_buffer_size,
                        "instance buffer exceeds the device's max_buffer_size; not drawn"
                    );
                }
                g.too_large = true;
                continue;
            }
            g.too_large = false;
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
                pipeline: 0,
                material,
                mesh,
                triangles,
                buffer: SourceBuffer::Explicit(slot as u32),
                first: 0,
                count: g.len,
                stride: layout.stride,
                cull,
            });
        }
        for s in &mut f.sources {
            s.pipeline = pipeline_rank(&f.materials, assets, s.material);
        }
        f.sources.sort_unstable_by_key(source_sort_key);
        stats.batches = f.sources.len() as u32;

        // ---- views -------------------------------------------------------------------
        f.views.clear();
        f.draws.clear();
        #[cfg(test)]
        {
            f.max_direct_base_vertex = 0;
        }
        f.jobs.clear();
        f.dispatches.clear();
        f.compact = None;
        f.views_buf.begin();
        f.cpu_visible.begin();
        f.cull_params.begin();
        f.cull_jobs.begin();
        f.cull_planes.clear();
        f.templates.begin();
        f.run_meta.begin();
        f.compact_params.begin();
        f.args_count = 0;
        f.run_count = 0;
        f.gpu_submitted = 0;
        f.gpu_triangles = 0;
        f.wg_total = 0;
        f.scan = None;
        let mut out_cursor = 0u64;
        let mut visible_overflow = false;

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
                            handle: h,
                            color: color.clone(),
                            depth: depth.clone(),
                        },
                        (e.width, e.height),
                    )
                }
            };
            let (vw, vh) = match cam.camera.viewport {
                Some(v) => {
                    let r = viewport_rect(v, size);
                    (r.w as f32, r.h as f32)
                }
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
                    shader: None,
                    first_index: alloc.first_index,
                    index_count: alloc.index_count,
                    base_vertex: alloc.base_vertex as i32,
                    vb: VertexSource::Ecs,
                    vb_offset: 0,
                    kind: DrawKind::Direct { instances: 0 },
                };

                // GPU culling writes the visible instances into this view's region of
                // `cull_out`, which is bound as one storage binding: a region that would end
                // past it falls back to CPU culling for this view.
                let run_key = (s.layout, s.class, s.material);
                let same_run = multi && matches!(run, Some((l, c, m, ..)) if (l, c, m) == run_key);
                let region_start = if multi && !same_run {
                    align_up(out_cursor, 256)
                } else {
                    out_cursor
                };
                let fits = region_start + s.count as u64 * s.stride as u64 <= f.max_storage_binding;
                if gpu && s.cull != CullSource::None && fits {
                    // GPU culling: reserve an output region, emit a job and an indirect draw.
                    if multi && !same_run {
                        out_cursor = region_start;
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
                    if let Some((.., cmd, _)) = run
                        && multi
                    {
                        match &mut f.draws[cmd].kind {
                            DrawKind::Multi { count, .. } | DrawKind::MultiCount { count, .. } => *count += 1,
                            _ => {}
                        }
                    }
                    if !multi {
                        f.draws.push(DrawCmd {
                            vb: VertexSource::CullOut,
                            vb_offset: region,
                            kind: DrawKind::Indirect { args: draw },
                            ..base_cmd
                        });
                    }
                    let (kind, t_off, s_off, s_kind) = cull_job_encoding(s.cull);
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
                            // Set once the view's frustum slot is known (below).
                            view: 0,
                            pad: 0,
                        },
                    ));
                    // Upper bound until the readback of the visible count (see `render_frame`).
                    stats.instances_drawn += s.count as u64;
                    stats.triangles += s.count as u64 * tris;
                    f.gpu_submitted += s.count as u64;
                    f.gpu_triangles += s.count as u64 * tris;
                    continue;
                }

                // CPU path (or buffers the GPU culler cannot decode).
                run = None;
                let mut visible = 0u32;
                match s.buffer {
                    SourceBuffer::Ecs => {
                        let range = s.first as usize..(s.first + s.count) as usize;
                        let order = if f.ecs_overflow {
                            &f.ecs_cut[range]
                        } else {
                            &f.ecs_order[range]
                        };
                        visible = if culling {
                            cull_mask(&f.ecs_matrices, order, s.mesh.sphere, &frustum, &mut f.cull_mask) as u32
                        } else {
                            s.count
                        };
                        // `cpu_visible` holds every view's partially visible batches and
                        // cannot exceed `max_buffer_size`: a batch that no longer fits is
                        // drawn whole from `ecs` (unculled, so the image is the same).
                        if visible < s.count
                            && visible > 0
                            && f.cpu_visible.len() as u64 + visible as u64 * 64 > f.max_buffer_size
                        {
                            visible_overflow = true;
                            visible = s.count;
                        }
                        if visible == s.count {
                            f.draws.push(DrawCmd {
                                vb: VertexSource::Ecs,
                                vb_offset: s.first as u64 * 64,
                                kind: DrawKind::Direct { instances: visible },
                                ..base_cmd
                            });
                        } else if visible > 0 {
                            // Partially visible: copy the visible matrices (diffed) into
                            // `cpu_visible`, in batch order.
                            let at = f.cpu_visible.len();
                            let (mask, matrices) = (&f.cull_mask, &f.ecs_matrices);
                            f.cpu_visible.write_all(
                                at,
                                visible as usize,
                                order
                                    .iter()
                                    .enumerate()
                                    .filter(|(k, _)| mask[k / 64] >> (k % 64) & 1 != 0)
                                    .map(|(_, &i)| &matrices[i as usize]),
                                &mut cpu_reallocs,
                            );
                            f.draws.push(DrawCmd {
                                vb: VertexSource::CpuVisible,
                                vb_offset: at as u64,
                                kind: DrawKind::Direct { instances: visible },
                                ..base_cmd
                            });
                        }
                    }
                    SourceBuffer::Explicit(slot) => {
                        // Visible runs of culling blocks become one draw each, offset into
                        // the instance buffer (no `first_instance` needed).
                        let mut draw = |f: &mut FrameState, start: usize, end: usize| {
                            let end = end.min(s.count as usize);
                            f.draws.push(DrawCmd {
                                vb: VertexSource::Explicit(slot),
                                vb_offset: (start * s.stride as usize) as u64,
                                kind: DrawKind::Direct {
                                    instances: (end - start) as u32,
                                },
                                ..base_cmd
                            });
                            visible += (end - start) as u32;
                        };
                        // Bounds are brought up to date here, lazily: only CPU culling reads
                        // them, and only blocks edited since the last refresh are recomputed.
                        let bounds = if culling {
                            let generation = f.explicit[slot as usize].as_ref().map_or(0, |g| g.generation);
                            buffers
                                .buffers
                                .get_mut(crate::handles::RawHandle {
                                    index: slot,
                                    generation,
                                })
                                .map(|ib| ib.cull_bounds())
                        } else {
                            None
                        };
                        let r = s.mesh.sphere.center.length() + s.mesh.sphere.radius;
                        // Empty boxes: no decodable positions, never culled.
                        let seen = |aabb: &Aabb, scale: f32| {
                            aabb.is_empty() || frustum.intersects_aabb(&aabb.inflated(r * scale))
                        };
                        match bounds {
                            Some(CullBounds::User(aabb)) => {
                                if seen(&aabb, 1.0) {
                                    draw(f, 0, s.count as usize);
                                }
                            }
                            Some(CullBounds::Blocks { total, blocks }) => {
                                if seen(&total.0, total.1) {
                                    let mut start = None;
                                    for (b, (aabb, scale)) in blocks.iter().enumerate() {
                                        if seen(aabb, *scale) {
                                            start.get_or_insert(b);
                                        } else if let Some(a) = start.take() {
                                            draw(f, a * CULL_BLOCK, b * CULL_BLOCK);
                                        }
                                    }
                                    if let Some(a) = start {
                                        draw(f, a * CULL_BLOCK, s.count as usize);
                                    }
                                }
                            }
                            None => draw(f, 0, s.count as usize),
                        }
                    }
                }
                stats.culled += (s.count - visible) as u64;
                stats.instances_drawn += visible as u64;
                stats.triangles += visible as u64 * tris;
            }

            // This view's frustum, read by its culling jobs. The jobs of all views are grouped
            // into dispatches after the loop.
            if f.jobs.len() > jobs_start {
                let planes: GpuFrustum = if culling {
                    frustum.planes.map(|p| p.to_array())
                } else {
                    [[0.0, 0.0, 0.0, 1.0]; 6]
                };
                let slot = f.cull_planes.len() as u32;
                f.cull_planes.push(planes);
                for (_, job) in &mut f.jobs[jobs_start..] {
                    job.view = slot;
                }
            }

            let feedback = match &target {
                ViewTarget::Texture { handle, .. } => f.draws[draws_start..]
                    .iter()
                    .any(|d| f.materials.texture(d.material) == Some(*handle)),
                ViewTarget::Primary => false,
            };
            f.views.push(ViewData {
                target,
                clear: cam.camera.clear_color,
                viewport: cam.camera.viewport,
                size,
                view_offset: view_offset as u32,
                draws: draws_start..f.draws.len(),
                feedback,
            });
        }
        stats.views = f.views.len() as u32;

        // Culling dispatches: every job reading the same source buffer, over all views (in
        // windows of CULL_VIEWS frustums), is one mark and one scatter dispatch — the dispatch
        // count depends on the number of source buffers, not on buffers × views. Jobs keep
        // their output regions and draws, so their order here does not change the image.
        f.jobs
            .sort_unstable_by_key(|(b, j)| (j.view as usize / CULL_VIEWS, *b, j.draw));
        let mut j = 0;
        while j < f.jobs.len() {
            let buffer = f.jobs[j].0;
            let window = f.jobs[j].1.view as usize / CULL_VIEWS;
            let job_start = (f.cull_jobs.len() / std::mem::size_of::<GpuCullJob>()) as u32;
            let mut wg = 0u32;
            let mut n = 0u32;
            while j < f.jobs.len() && f.jobs[j].0 == buffer && f.jobs[j].1.view as usize / CULL_VIEWS == window {
                let job = &mut f.jobs[j].1;
                // Frame-global record index: jobs of all dispatches share the records.
                job.wg_start = f.wg_total + wg;
                wg += job.count.div_ceil(WORKGROUP);
                let gpu_job = GpuCullJob {
                    view: job.view % CULL_VIEWS as u32,
                    ..*job
                };
                f.cull_jobs.push(bytemuck::bytes_of(&gpu_job), &mut cpu_reallocs);
                n += 1;
                j += 1;
            }
            f.wg_total += wg;
            let wgs = dispatch_dims(wg);
            f.dispatches.push(Dispatch {
                buffer,
                // `wg_word` is set below, once the record offset is known.
                params: GpuCullParams {
                    job_start,
                    job_count: n,
                    total_workgroups: wg,
                    workgroups_x: wgs.0,
                    wg_word: 0,
                    pad: [0; 3],
                },
                params_offset: 0,
                window: window as u32,
                wg: wgs,
            });
        }
        if path == DrawPath::GpuMultiDrawCount && f.run_count > 0 {
            // Draw compaction of every view's runs: one workgroup per run.
            for d in &f.draws {
                if let DrawKind::MultiCount { args_first, count, run } = d.kind {
                    f.run_meta.write_at(
                        run as usize * 8,
                        bytemuck::cast_slice(&[args_first, count]),
                        &mut cpu_reallocs,
                    );
                }
            }
            let wg = dispatch_dims(f.run_count);
            let p = GpuCompactParams {
                run_start: 0,
                run_count: f.run_count,
                workgroups_x: wg.0,
                pad: 0,
            };
            f.compact_params.write_at(0, bytemuck::bytes_of(&p), &mut cpu_reallocs);
            f.compact = Some(wg);
        }

        // Cull params: the frustums first, in whole windows of CULL_VIEWS (bound at fixed
        // offsets, in the same uniform buffer so culling binds no extra buffer), then one
        // params block per dispatch and the scan's (dynamic offsets; all offsets are
        // multiples of FRUSTA_WINDOW or `params_stride`, hence aligned). The per-workgroup
        // compaction records follow the indirect args.
        let wg_word = align_up(f.args_count as u64 * 5, 4) as u32;
        f.cull_params
            .write_at(0, bytemuck::cast_slice(&f.cull_planes), &mut cpu_reallocs);
        let params_base = f.cull_planes.len().div_ceil(CULL_VIEWS) * FRUSTA_WINDOW;
        f.cull_params.ensure_len(params_base, &mut cpu_reallocs);
        for (i, d) in f.dispatches.iter_mut().enumerate() {
            d.params.wg_word = wg_word;
            d.params_offset = (params_base + i * params_stride) as u32;
            f.cull_params.write_at(
                d.params_offset as usize,
                bytemuck::bytes_of(&d.params),
                &mut cpu_reallocs,
            );
        }
        let job_total = (f.cull_jobs.len() / std::mem::size_of::<GpuCullJob>()) as u32;
        if job_total > 0 {
            // One prefix-sum workgroup per job, over the jobs of every view.
            let wg = dispatch_dims(job_total);
            let p = GpuCullParams {
                job_start: 0,
                job_count: job_total,
                total_workgroups: job_total,
                workgroups_x: wg.0,
                wg_word,
                pad: [0; 3],
            };
            let off = params_base + f.dispatches.len() * params_stride;
            f.cull_params.write_at(off, bytemuck::bytes_of(&p), &mut cpu_reallocs);
            f.scan = Some((off as u32, wg));
        }

        if visible_overflow && !f.visible_overflow {
            tracing::warn!(
                max_buffer_size = f.max_buffer_size,
                "the partially visible ECS instances of all views exceed the device's \
                 max_buffer_size; the batches past it are drawn unculled"
            );
        }
        f.visible_overflow = visible_overflow;

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

        // ---- bind groups ---------------------------------------------------------------
        // Custom-shader pipelines whose (asynchronous, browser) validation failed since the
        // last frame fall back to the built-in shader before anything is drawn with them.
        pipelines.poll_pending(device);
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
            // Custom shaders: resolve the current source version (stale handles: built-in).
            if let Some(h) = f.materials.shader(f.draws[d].material) {
                f.draws[d].shader = assets.shaders.get(h.raw()).map(|e| {
                    pipelines.sync_custom(h, e.version, &e.shader);
                    (h, e.version)
                });
            }
            let tex = f.materials.texture(f.draws[d].material);
            let (view, sampler, version, layered) = match tex.and_then(|h| assets.textures.get(h.raw())) {
                Some(e) => match &e.gpu {
                    Some(g) => (&g.view, &g.sampler, e.version, g.layered),
                    None => (&*white, &*white_sampler, u32::MAX, false),
                },
                None => (&*white, &*white_sampler, u32::MAX, false),
            };
            let valid = f
                .material_bgs
                .get(&tex)
                .is_some_and(|(_, g, v, _)| *g == mgen && *v == version);
            if !valid {
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("space3d material"),
                    layout: pipelines.material_bgl(layered),
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
                f.material_bgs.insert(tex, (bg, mgen, version, layered));
            }
        }
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

        if gpu && let Some(cull) = &pipelines.cull {
            if frame_index.is_multiple_of(256) {
                // Bind groups of windows no longer dispatched would keep reallocated buffers
                // alive: drop the outdated ones.
                let (params, jobs, out, args) = (
                    f.cull_params.gpu.generation,
                    f.cull_jobs.gpu.generation,
                    f.cull_out.generation,
                    f.args.generation,
                );
                let (ecs, explicit) = (f.ecs.gpu.generation, &f.explicit);
                f.cull_bgs.retain(|(b, _), (_, g)| {
                    let src = match b {
                        SourceBuffer::Ecs => Some(ecs),
                        SourceBuffer::Explicit(slot) => explicit
                            .get(*slot as usize)
                            .and_then(Option::as_ref)
                            .map(|e| e.buf.generation),
                    };
                    src.is_some_and(|src| *g == [params, jobs, src, out, args])
                });
            }
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
                let window = f.dispatches[d].window;
                if f.cull_bgs.get(&(buffer, window)).is_some_and(|(_, g)| *g == gens) {
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
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: f.cull_params.buffer(),
                                offset: window as u64 * FRUSTA_WINDOW as u64,
                                size: wgpu::BufferSize::new(FRUSTA_WINDOW as u64),
                            }),
                        },
                    ],
                });
                f.cull_bgs.insert((buffer, window), (bg, gens));
            }
            if f.compact.is_some() {
                let gens = [
                    f.compact_params.gpu.generation,
                    f.run_meta.gpu.generation,
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
                                resource: binding(f.run_meta.buffer(), max),
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

    /// Reset indirect args from templates, then run culling (mark → scan → scatter, see
    /// `shaders/cull.wgsl`) and, for `GpuMultiDrawCount`, draw compaction.
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
        if f.compact.is_some() {
            encoder.clear_buffer(f.counts.get(), 0, Some(align_up(f.run_count as u64 * 4, 4)));
        }
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("space3d cull"),
            timestamp_writes: None,
        });
        // Each dispatch is its own usage scope, so wgpu orders the passes' storage accesses.
        pass.set_pipeline(&cull.mark);
        for d in &f.dispatches {
            let (bg, _) = &f.cull_bgs[&(d.buffer, d.window)];
            pass.set_bind_group(0, bg, &[d.params_offset]);
            pass.dispatch_workgroups(d.wg.0, d.wg.1, 1);
            self.stats.compute_dispatches += 1;
        }
        if let Some((offset, wg)) = f.scan {
            // The scan only touches jobs and args, which every cull bind group shares.
            let d = &f.dispatches[0];
            let (bg, _) = &f.cull_bgs[&(d.buffer, d.window)];
            pass.set_pipeline(&cull.scan);
            pass.set_bind_group(0, bg, &[offset]);
            pass.dispatch_workgroups(wg.0, wg.1, 1);
            self.stats.compute_dispatches += 1;
        }
        pass.set_pipeline(&cull.scatter);
        for d in &f.dispatches {
            let (bg, _) = &f.cull_bgs[&(d.buffer, d.window)];
            pass.set_bind_group(0, bg, &[d.params_offset]);
            pass.dispatch_workgroups(d.wg.0, d.wg.1, 1);
            self.stats.compute_dispatches += 1;
        }
        if let Some((bg, _)) = &f.compact_bg
            && let Some(wg) = f.compact
        {
            pass.set_pipeline(&cull.compact);
            pass.set_bind_group(0, bg, &[0]);
            pass.dispatch_workgroups(wg.0, wg.1, 1);
            self.stats.compute_dispatches += 1;
        }
        drop(pass);
        // Visible counts for `RenderStats`, read back without waiting (see `readback.rs`).
        self.uploader.reallocations += f.readback.record(
            &self.device,
            encoder,
            f.args.get(),
            f.args_count,
            f.gpu_submitted,
            self.frame_index,
        );
    }

    /// Replace the upper bounds of GPU-culled batches in the stats with the newest read-back
    /// visible counts (of an earlier frame).
    fn apply_gpu_cull_counts(&mut self) {
        let f = &mut self.frame;
        if f.gpu_submitted == 0 {
            f.readback.clear_latest();
            return;
        }
        if let Some(c) = f.readback.latest() {
            let s = &mut self.stats;
            s.instances_drawn = s.instances_drawn - f.gpu_submitted + c.visible;
            s.culled += c.submitted.saturating_sub(c.visible);
            s.triangles = s.triangles - f.gpu_triangles + c.triangles;
            s.gpu_counts_frame = Some(c.frame);
        }
    }

    /// Record render passes for the prepared views. With `primary_to_offscreen`, only primary
    /// views are re-recorded into the offscreen target (windowed capture).
    ///
    /// Views sharing a target (split screen, picture-in-picture, overlays) draw in camera
    /// order into the same attachments: the first pass into a target in a frame clears it,
    /// later ones load what the earlier ones drew (see [`Self::encode_pass`]).
    pub(crate) fn encode_views(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        surface_view: Option<&wgpu::TextureView>,
        primary_to_offscreen: bool,
    ) {
        let views = std::mem::take(&mut self.frame.views);
        let mut presented = false;
        for (i, view) in views.iter().enumerate() {
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
                (ViewTarget::Texture { handle, color, depth }, _) => {
                    if view.feedback
                        && let Some(fb) = self.frame.feedback.get(handle)
                    {
                        // Snapshot the target before this pass draws into it; the view's
                        // materials sample the snapshot.
                        encoder.copy_texture_to_texture(
                            color.texture().as_image_copy(),
                            fb.texture.as_image_copy(),
                            wgpu::Extent3d {
                                width: fb.size.0,
                                height: fb.size.1,
                                depth_or_array_layers: 1,
                            },
                        );
                    }
                    (color.clone(), None, depth.clone(), OFFSCREEN_FORMAT, 1, false)
                }
            };
            // Texture views are never skipped, except all of them in `primary_to_offscreen`.
            let first = match &view.target {
                ViewTarget::Primary => !presented,
                ViewTarget::Texture { handle, .. } => !views[..i]
                    .iter()
                    .any(|v| matches!(&v.target, ViewTarget::Texture { handle: h, .. } if h == handle)),
            };
            presented |= is_primary;
            self.encode_pass(
                encoder,
                view,
                first,
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

    /// Record one view's render pass. `first`: no earlier pass of this frame drew into the
    /// view's target.
    ///
    /// Color: a view covering its whole target clears it with its clear color (a load op). The
    /// first pass into a target otherwise clears it to black — never leaving the previous
    /// frame's content, and never painting a viewport's clear color outside the viewport —
    /// and later passes load it. A view with a viewport and a clear color then clears only its
    /// viewport, with a triangle in that color ([`Pipelines::viewport_clear`]). The viewport,
    /// in whole pixels ([`viewport_rect`]), also sets the pass's scissor rect (its part within
    /// the attachment). Depth is per view:
    /// every pass clears the whole depth attachment (views never test against each other) and
    /// discards it.
    ///
    /// [`Pipelines::viewport_clear`]: crate::pipeline::Pipelines::viewport_clear
    #[allow(clippy::too_many_arguments)]
    fn encode_pass(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &ViewData,
        first: bool,
        color: &wgpu::TextureView,
        resolve: Option<&wgpu::TextureView>,
        depth: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        samples: u32,
        manual_srgb: bool,
    ) {
        let rect = view.viewport.map(|v| viewport_rect(v, view.size));
        let full = rect.is_none_or(|r| r.covers(view.size));
        // Scissor rects must lie within the attachment itself. It has the size the view was
        // prepared for on every path (see `surface_lost`); should that ever not hold, a scissor
        // rect outside it would fail validation and lose the whole frame's commands.
        let extent = (color.texture().width(), color.texture().height());
        let clear = view.clear.map(|c| {
            let c = if manual_srgb { linear_to_srgb_color(c) } else { c };
            wgpu::Color {
                r: c.x as f64,
                g: c.y as f64,
                b: c.z as f64,
                a: c.w as f64,
            }
        });
        let (load, rect_clear) = match clear {
            Some(c) if full => (wgpu::LoadOp::Clear(c), None),
            _ if first => (wgpu::LoadOp::Clear(wgpu::Color::BLACK), clear),
            _ => (wgpu::LoadOp::Load, clear),
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
        if let Some(r) = rect {
            pass.set_viewport(r.x as f32, r.y as f32, r.w as f32, r.h as f32, 0.0, 1.0);
            let [x, y, w, h] = r.scissor(extent);
            pass.set_scissor_rect(x, y, w, h);
        }
        if let Some(c) = rect_clear {
            pass.set_pipeline(self.pipelines.viewport_clear(&self.device, format, samples));
            pass.set_blend_constant(c);
            pass.draw(0..3, 0..1);
        }
        self.encode_draws(&mut pass, view, format, samples, manual_srgb);
        if rect.is_some() && resolve.is_some() {
            // wgpu's GL backend resolves MSAA with a framebuffer blit at the end of the pass,
            // and GL clips blits to the scissor rect: only the viewport would be resolved, and
            // the rest of the target would keep its previous content.
            pass.set_scissor_rect(0, 0, extent.0, extent.1);
        }
    }

    /// Record a view's draws into its pass.
    fn encode_draws(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        view: &ViewData,
        format: wgpu::TextureFormat,
        samples: u32,
        manual_srgb: bool,
    ) {
        let f = &self.frame;
        let (Some((vb, ib)), Some((view_bg, _))) = (&f.arena_buffers, &f.view_bg) else {
            return;
        };
        if view.draws.is_empty() {
            return;
        }
        pass.set_bind_group(0, view_bg, &[view.view_offset]);
        pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
        let material_stride = f.material_stride() as u32;
        let base_vertex_supported = self.caps.base_vertex;
        // The render target this pass draws into must not be sampled by it.
        let own_target = match &view.target {
            ViewTarget::Texture { handle, .. } if view.feedback => Some(*handle),
            _ => None,
        };
        let mut last_pipeline: Option<PipelineState> = None;
        let mut last_material = u32::MAX;
        let mut layered = false;
        let mut last_vb: Option<(VertexSource, u64)> = None;
        // First arena vertex bound to slot 0 (see `direct_vertex_base`).
        let mut slot0: Option<u32> = None;
        #[cfg(test)]
        let mut max_direct_base_vertex = self.frame.max_direct_base_vertex;
        for cmd in &f.draws[view.draws.clone()] {
            // (Indirect draws take their counts from the args buffer.)
            let direct = matches!(cmd.kind, DrawKind::Direct { .. });
            if direct && cmd.index_count == 0 {
                continue;
            }
            if cmd.material != last_material {
                let tex = f.materials.texture(cmd.material);
                let bg = match tex {
                    // Render targets and their feedback copies are single-layer.
                    Some(t) if Some(t) == own_target => f
                        .feedback
                        .get(&t)
                        .and_then(|fb| fb.bind_group.as_ref())
                        .map(|(bg, _)| (bg, false)),
                    _ => f.material_bgs.get(&tex).map(|(bg, _, _, l)| (bg, *l)),
                };
                let Some((bg, l)) = bg else {
                    continue;
                };
                pass.set_bind_group(1, bg, &[cmd.material * material_stride]);
                self.stats.material_switches += 1;
                last_material = cmd.material;
                layered = l;
            }
            if last_pipeline != Some((cmd.layout, cmd.class, layered, cmd.shader)) {
                let key = PipelineKey {
                    layout: cmd.layout,
                    class: cmd.class,
                    format,
                    samples,
                    manual_srgb,
                    layered,
                    shader: cmd.shader,
                };
                pass.set_pipeline(self.pipelines.forward(&self.device, key));
                self.stats.pipeline_switches += 1;
                last_pipeline = Some((cmd.layout, cmd.class, layered, cmd.shader));
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
                DrawKind::Direct { instances } => {
                    #[cfg(test)]
                    {
                        max_direct_base_vertex = max_direct_base_vertex.max(base_vertex);
                    }
                    pass.draw_indexed(indices, base_vertex, 0..instances);
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
        #[cfg(test)]
        {
            self.frame.max_direct_base_vertex = max_direct_base_vertex;
        }
    }

    /// Re-record the last frame's primary views into the offscreen target (for capture). The
    /// target gets the size those views were prepared for (their projection and viewports),
    /// even if the window was resized since.
    pub(crate) fn encode_primary_to_offscreen(&mut self, encoder: &mut wgpu::CommandEncoder) {
        self.size_offscreen(self.frame.last_primary_size);
        // Re-recorded passes are not new draws: keep the frame's pass counters.
        let stats = self.stats.clone();
        self.encode_views(encoder, None, true);
        self.stats = stats;
    }
}

/// A view's viewport in whole pixels of its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewportRect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl ViewportRect {
    /// The rect covers every pixel of a `size` target.
    fn covers(&self, size: (u32, u32)) -> bool {
        self.x <= 0
            && self.y <= 0
            && self.x as i64 + self.w as i64 >= size.0 as i64
            && self.y as i64 + self.h as i64 >= size.1 as i64
    }

    /// `[x, y, width, height]` of the rect's part within a `size` target (at least one pixel,
    /// as a scissor rect must lie within the target): the scissor rect of the view's pass.
    fn scissor(&self, size: (u32, u32)) -> [u32; 4] {
        let axis = |start: i32, len: u32, extent: u32| {
            let extent = extent.max(1) as i64;
            let a = (start as i64).clamp(0, extent - 1);
            let b = (start as i64 + len as i64).clamp(a + 1, extent);
            (a as u32, (b - a) as u32)
        };
        let (x, w) = axis(self.x, self.w, size.0);
        let (y, h) = axis(self.y, self.h, size.1);
        [x, y, w, h]
    }
}

/// Pixel rect of a normalized viewport `[x, y, width, height]` (top left origin) on a `size`
/// target: its edges rounded to the nearest pixel boundary, at least one pixel wide and high.
/// Rounded because GL takes integer viewports (wgpu's GL backend truncates position and size
/// separately, which would shift fractional viewports and open gaps between neighbours): every
/// backend draws the same pixels, and neighbouring viewports share their edges exactly.
///
/// A viewport whose edges round to the same boundary (thinner than about a pixel) gets the
/// pixel holding its center, kept within the target when the viewport overlaps the target:
/// growing its far edge instead would push a viewport in the last half pixel of the target
/// entirely outside it.
fn viewport_rect([x, y, w, h]: [f32; 4], size: (u32, u32)) -> ViewportRect {
    let axis = |start: f32, len: f32, extent: u32| {
        let e = extent as f32;
        let a = (start * e).round();
        let b = ((start + len) * e).round();
        if b > a {
            return (a as i32, (b - a) as u32);
        }
        let center = ((start + 0.5 * len) * e).floor();
        let overlaps = start.min(start + len) < 1.0 && start.max(start + len) > 0.0;
        let p = if overlaps {
            center.clamp(0.0, (e - 1.0).max(0.0))
        } else {
            center
        };
        (p as i32, 1)
    };
    let (x, w) = axis(x, w, size.0);
    let (y, h) = axis(y, h, size.1);
    ViewportRect { x, y, w, h }
}

/// Where vertex slot 0 starts in the mesh arena (in vertices) and the `base_vertex` to draw
/// with. Normally the whole arena is bound and draws select their mesh with `base_vertex`.
/// Without `DownlevelFlags::BASE_VERTEX` (WebGL2, GLES < 3.2) a non-zero base is not possible
/// in direct draws — wgpu does not reject it, and the GL backend then calls a missing entry
/// point (a panic on the web) — so the arena is bound from the mesh's first vertex instead
/// (`36 * n` bytes: always a valid 4-byte-aligned offset) and the draw uses base 0. Indirect
/// draws carry their base in the args; GPU paths are only chosen with `BASE_VERTEX`.
fn direct_vertex_base(base_vertex: i32, direct: bool, supported: bool) -> (u32, i32) {
    if direct && !supported && base_vertex > 0 {
        (base_vertex as u32, 0)
    } else {
        (0, base_vertex)
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
    // CPU culling bounds are refreshed lazily by the CPU path (`cull_bounds`): GPU culling
    // paths never pay for them.
    ib.clear_dirty();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{InstanceCompact, InstanceData};
    use crate::pipeline::GpuCompactParams;

    #[test]
    fn material_keys_intern() {
        let a = MaterialKey::new(&Material::default());
        let b = MaterialKey::new(&Material::default());
        let c = MaterialKey::new(&Material::srgb(1.0, 0.0, 0.0));
        assert!(a == b && a != c);
    }

    #[test]
    fn surface_lost_is_recreated_not_reconfigured() {
        use wgpu::CurrentSurfaceTexture as C;
        assert!(matches!(
            SurfaceAcquire::from_current(C::Lost),
            SurfaceAcquire::Recreate
        ));
        assert!(matches!(
            SurfaceAcquire::from_current(C::Outdated),
            SurfaceAcquire::Reconfigure
        ));
        assert!(matches!(SurfaceAcquire::from_current(C::Timeout), SurfaceAcquire::Skip));
        assert!(matches!(
            SurfaceAcquire::from_current(C::Occluded),
            SurfaceAcquire::Skip
        ));
    }

    fn headless_app(w: u32, h: u32) -> Option<space3d_core::App> {
        let mut app = space3d_core::App::new();
        app.add_plugin(crate::RenderPlugin::default());
        match pollster::block_on(Renderer::new_headless(w, h)) {
            Ok(r) => {
                app.insert_resource(r);
                Some(app)
            }
            Err(e) => {
                eprintln!("skipping GPU test: no adapter available ({e})");
                None
            }
        }
    }

    fn set_path(app: &mut space3d_core::App, path: DrawPath) {
        app.resources.expect_mut::<RenderSettings>().draw_path = Some(path);
    }

    fn differing_px(a: &space3d_core::ImageData, b: &space3d_core::ImageData) -> usize {
        a.pixels
            .chunks(4)
            .zip(b.pixels.chunks(4))
            .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 2))
            .count()
    }

    /// Regression: `cull_out` accumulates every GPU-culled source of every view but is bound as
    /// one storage binding; regions past `max_storage_buffer_binding_size` (128 MiB on many
    /// mobile GPUs and WebGPU) were silently dropped while the draws still counted them, and
    /// the ECS source was never checked. Such sources now fall back to CPU culling.
    #[test]
    fn sources_past_the_storage_binding_limit_fall_back_to_cpu_culling() {
        use crate::{Camera3d, InstanceBuffer, InstanceTransform, InstancedMesh, Mesh3d};
        use space3d_core::{MeshData, Transform};
        let Some(mut app) = headless_app(96, 64) else { return };
        let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
        if best == DrawPath::CpuDirect {
            return;
        }
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.3));
        // ECS: 1000 matrices = 64000 B.
        for i in 0..1000 {
            let p = Vec3::new((i % 40) as f32 * 0.25 - 5.0, (i / 40) as f32 * 0.25 - 3.0, 0.0);
            app.world.spawn((
                Transform::from_xyz(p.x, p.y, p.z),
                Mesh3d(mesh),
                Material::srgb(0.9, 0.4, 0.1),
            ));
        }
        // Two explicit buffers of 400 × 64 B = 25600 B each.
        for (k, y) in [(0, 1.0), (1, -1.5)] {
            let data = (0..400)
                .map(|i| {
                    InstanceTransform::from_trs(
                        Vec3::new((i % 40) as f32 * 0.25 + 0.5, y + (i / 40) as f32 * 0.2, -1.0),
                        glam::Quat::from_rotation_y(i as f32),
                        Vec3::ONE,
                    )
                })
                .collect();
            let instances = app
                .resources
                .expect_mut::<InstanceBuffers>()
                .insert(InstanceBuffer::from_vec(data, crate::BufferUsage::Static));
            app.world.spawn((InstancedMesh {
                mesh,
                material: Material::srgb(0.2, 0.5 + 0.3 * k as f32, 0.9),
                instances,
            },));
        }
        // Two views of the same scene (left and right half).
        for (x, order) in [(0.0, 0), (0.5, 1)] {
            app.world.spawn((
                Transform::from_xyz(0.0, 0.0, 12.0),
                Camera3d {
                    viewport: Some([x, 0.0, 0.5, 1.0]),
                    order,
                    ..Camera3d::default().with_clear_color(glam::Vec4::new(0.0, 0.0, 1.0, 1.0))
                },
            ));
        }
        set_path(&mut app, DrawPath::CpuDirect);
        let cpu = crate::render_to_image(&mut app).expect("capture");
        // Only 32 KiB per storage binding: each explicit buffer fits on its own, the ECS
        // batch does not, and neither does the sum of all regions across the two views.
        app.resources.expect_mut::<Renderer>().frame.max_storage_binding = 32 << 10;
        set_path(&mut app, best);
        let limited = crate::render_to_image(&mut app).expect("capture");
        let d = differing_px(&cpu, &limited);
        assert_eq!(d, 0, "{best:?} with a 32 KiB binding differs in {d} px");
        let f = &app.resources.expect::<Renderer>().frame;
        assert_eq!(
            f.jobs.len(),
            1,
            "only the first explicit buffer of view 1 is GPU-culled"
        );
        assert!(app.resources.expect::<crate::RenderStats>().compute_dispatches > 0);
    }

    /// Regression: culling ran one mark and one scatter dispatch per (view, source buffer), so
    /// a scene with 120 small buffers and 13 views took 3121 dispatches (and wgpu-hal's Metal
    /// backend allocates in each). The jobs of all views reading one buffer now share a
    /// dispatch, each job selecting its view's frustum: 70 views (two windows of frustums) ×
    /// 4 buffers take 2 × 2 × 4 + 1 dispatches.
    ///
    /// Every one of the 70 viewports is checked against what its own camera sees — its own
    /// clear color, its ECS cube and its own uniquely colored explicit instance, and nothing
    /// else — on every draw path. A view culled with another view's frustum (on the CPU or the
    /// GPU), drawn with another view's matrix or wiped by a later viewport fails.
    #[test]
    fn culling_dispatches_scale_with_buffers_not_views() {
        use crate::components::srgb_to_linear;
        use crate::{Camera3d, InstanceBuffer, InstanceTransformColor, InstancedMesh, Mesh3d, RenderStats};
        use space3d_core::{ImageData, MeshData, Transform};
        const VIEWS: usize = 70;
        const W: u32 = 140;
        const H: u32 = 100;
        let Some(mut app) = headless_app(W, H) else { return };
        let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
        if best == DrawPath::CpuDirect {
            return;
        }
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.2));
        // View v's camera looks at `center(v)` from 4 units away (its frustum spans ±2.3 units
        // there); the next view's point is 10 units away, far outside it.
        let center = |v: usize| Vec3::new((v % 10) as f32 * 10.0, (v / 10) as f32 * 10.0, 0.0);
        // Seen by no view.
        let nowhere = |i: usize| Vec3::new(-1000.0 - i as f32 * 2.0, 0.0, 0.0);
        // sRGB colors of view v: its clear color and its explicit instance's color.
        let clear_rgb = |v: usize| [(v % 10) as u8 * 6, (v / 10) as u8 * 6, 60];
        let own_rgb = |v: usize| [40 + (v % 10) as u8 * 20, 40 + (v / 10) as u8 * 30, 200];
        let linear = |c: [u8; 3]| c.map(|x| srgb_to_linear(x as f32 / 255.0));
        let linear4 = |c: [u8; 3]| Vec3::from(linear(c)).extend(1.0);
        // ECS: a white cube left of each view's point, and cubes no view sees.
        for i in 0..300 {
            let p = if i < VIEWS {
                center(i) - Vec3::X * 0.9
            } else {
                nowhere(i)
            };
            app.world.spawn((
                Transform::from_xyz(p.x, p.y, p.z),
                Mesh3d(mesh),
                Material::unlit(Vec4::ONE),
            ));
        }
        // Three explicit buffers of 200 instances: view v's cube, right of its point and in its
        // own color, is instance 8 × (v / 3) of buffer v % 3 (in several culling workgroups);
        // the other instances are seen by no view.
        for k in 0..3 {
            let data = (0..200)
                .map(|i| {
                    let v = i / 8 * 3 + k;
                    let (p, rgb) = if i % 8 == 0 && v < VIEWS {
                        (center(v) + Vec3::X * 0.9, own_rgb(v))
                    } else {
                        (nowhere(300 + 200 * k + i), [255, 255, 255])
                    };
                    InstanceTransformColor::new(Mat4::from_translation(p), linear4(rgb))
                })
                .collect();
            let instances = app
                .resources
                .expect_mut::<InstanceBuffers>()
                .insert(InstanceBuffer::from_vec(data, crate::BufferUsage::Static));
            app.world.spawn((InstancedMesh {
                mesh,
                material: Material::unlit(Vec4::ONE),
                instances,
            },));
        }
        // A 10 × 7 grid of viewports (14 × 14.3 px), each with its own clear color.
        for v in 0..VIEWS {
            let (col, row) = ((v % 10) as f32, (v / 10) as f32);
            let c = center(v);
            app.world.spawn((
                Transform::from_xyz(c.x, c.y, 4.0),
                Camera3d {
                    viewport: Some([col / 10.0, row / 7.0, 0.1, 1.0 / 7.0]),
                    order: v as i32,
                    ..Camera3d::default().with_clear_color(linear4(clear_rgb(v)))
                },
            ));
        }
        // Inside each viewport (1 px margin), every pixel is the view's clear color, its white
        // ECS cube or its own instance, and each of the three is visible.
        let check = |img: &ImageData, path: DrawPath| {
            for v in 0..VIEWS {
                let (col, row) = ((v % 10) as u32, (v / 10) as u32);
                let cell_h = H as f32 / 7.0;
                let ys = (row as f32 * cell_h).ceil() as u32 + 1..((row + 1) as f32 * cell_h).floor() as u32 - 1;
                let expected = [clear_rgb(v), [255, 255, 255], own_rgb(v)];
                let mut seen = [0usize; 3];
                for y in ys {
                    for x in col * 14 + 1..col * 14 + 13 {
                        let p = img.get(x, y);
                        let is = |c: &[u8; 3]| p.iter().zip(c).all(|(a, b)| a.abs_diff(*b) <= 2);
                        match expected.iter().position(is) {
                            Some(i) => seen[i] += 1,
                            None => {
                                panic!("{path:?}: viewport {v} shows {p:?} at ({x}, {y}), expected one of {expected:?}")
                            }
                        }
                    }
                }
                assert!(
                    seen[0] >= 40 && seen[1] >= 6 && seen[2] >= 6,
                    "{path:?}: viewport {v}: clear color / ECS cube / own instance cover {seen:?} px"
                );
            }
        };
        set_path(&mut app, DrawPath::CpuDirect);
        let cpu = crate::render_to_image(&mut app).expect("capture");
        assert!(app.resources.expect::<RenderStats>().culled > 0, "views cull");
        check(&cpu, DrawPath::CpuDirect);
        for path in paths_up_to(best).skip(1) {
            set_path(&mut app, path);
            let img = crate::render_to_image(&mut app).expect("capture");
            check(&img, path);
            let d = differing_px(&cpu, &img);
            assert_eq!(d, 0, "{path:?} differs from CpuDirect in {d} px");
            let r = app.resources.expect::<Renderer>();
            assert_eq!(r.gpu_error_count(), 0, "{path:?}");
            assert_eq!(r.frame.jobs.len(), VIEWS * 4, "{path:?}: every source GPU-culled");
            let compact = u32::from(path == DrawPath::GpuMultiDrawCount);
            assert_eq!(
                app.resources.expect::<RenderStats>().compute_dispatches,
                2 * 2 * 4 + 1 + compact,
                "{path:?}: mark + scatter per buffer and frustum window, one scan"
            );
        }
    }

    /// Viewport rects are whole pixels with rounded edges (neighbours share edges exactly);
    /// scissors are their part within the target, at least one pixel.
    #[test]
    fn viewport_rects_and_scissors() {
        let rect = |v, size| {
            let r = viewport_rect(v, size);
            (r.x, r.y, r.w, r.h)
        };
        assert_eq!(rect([0.25, 0.25, 0.375, 0.5], (8, 4)), (2, 1, 3, 2));
        // Thirds of 100 px: 0..33, 33..67, 67..100.
        let thirds: Vec<_> = (0..3)
            .map(|i| rect([i as f32 / 3.0, 0.0, 1.0 / 3.0, 1.0], (100, 10)))
            .collect();
        assert_eq!(thirds, [(0, 0, 33, 10), (33, 0, 34, 10), (67, 0, 33, 10)]);
        // Sevenths of 100 px (the rows of `culling_dispatches_scale_with_buffers_not_views`).
        let rows: Vec<_> = (0..7)
            .map(|i| rect([0.0, i as f32 / 7.0, 0.1, 1.0 / 7.0], (140, 100)))
            .map(|(_, y, _, h)| (y, h))
            .collect();
        assert_eq!(
            rows,
            [(0, 14), (14, 15), (29, 14), (43, 14), (57, 14), (71, 15), (86, 14)]
        );
        let r = viewport_rect([0.25, 0.25, 0.375, 0.5], (8, 4));
        assert_eq!(r.scissor((8, 4)), [2, 1, 3, 2]);
        assert!(!r.covers((8, 4)));
        assert!(viewport_rect([0.0, 0.0, 1.0, 1.0], (8, 4)).covers((8, 4)));
        assert!(viewport_rect([-0.1, 0.0, 1.2, 1.0], (8, 4)).covers((8, 4)));
        // Degenerate and out-of-range viewports: at least one pixel, scissor within the target.
        assert_eq!(rect([0.5, 0.5, 0.0, 0.0], (10, 10)), (5, 5, 1, 1));
        // Thinner than a pixel: the pixel holding the center, inside the target at its edges
        // (the far edge used to grow past the target: x = 97, nothing drawn).
        assert_eq!(rect([0.999, 0.0, 0.001, 1.0], (97, 61)), (96, 0, 1, 61));
        assert_eq!(rect([0.0, 0.999, 1.0, 0.001], (97, 61)), (0, 60, 97, 1));
        assert_eq!(rect([0.0, 0.0, 1e-6, 1e-6], (97, 61)), (0, 0, 1, 1));
        assert_eq!(rect([0.9999, 0.0, 0.001, 1.0], (97, 61)), (96, 0, 1, 61));
        assert_eq!(rect([0.52, 0.0, 0.001, 1.0], (10, 10)), (5, 0, 1, 10));
        assert_eq!(rect([0.3, 0.0, -0.1, 1.0], (10, 10)), (2, 0, 1, 10));
        // Entirely outside the target, it stays outside (nothing is drawn).
        assert_eq!(rect([1.0, 1.0, 0.0, 0.0], (10, 10)), (10, 10, 1, 1));
        assert_eq!(rect([-0.5, 0.0, 0.01, 1.0], (10, 10)), (-5, 0, 1, 10));
        // Viewports within 0..1 stay within the target.
        for size in [(1, 1), (2, 3), (97, 61), (1920, 1080)] {
            for x in [0.0f32, 0.1, 0.5, 0.97, 0.999, 0.9999, 1.0 - f32::EPSILON] {
                for w in [0.0f32, 1e-6, 1e-4, 1e-3, 0.01, 0.5, 1.0]
                    .into_iter()
                    .filter(|w| x + w <= 1.0)
                {
                    let r = viewport_rect([x, 0.0, w, 1.0], size);
                    let [sx, _, sw, _] = r.scissor(size);
                    assert!(r.x >= 0 && r.x as u32 + r.w <= size.0, "{x} {w} {size:?}: {r:?}");
                    assert_eq!((sx, sw), (r.x as u32, r.w), "{x} {w} {size:?}");
                }
            }
        }
        let outside = viewport_rect([1.0, 1.0, 0.5, 0.5], (10, 10));
        assert_eq!((outside.x, outside.w), (10, 5));
        assert_eq!(outside.scissor((10, 10)), [9, 9, 1, 1]);
        let wide = viewport_rect([-0.5, 0.0, 2.0, 1.0], (10, 10));
        assert_eq!((wide.x, wide.w), (-5, 20));
        assert_eq!(wide.scissor((10, 10)), [0, 0, 10, 10]);
    }

    /// Split screen for the surface-loss tests: a green cube in the left camera's view (red
    /// clear color), a magenta one in the right camera's (blue).
    fn split_screen_app(w: u32, h: u32, samples: u32) -> Option<space3d_core::App> {
        use crate::{Camera3d, Mesh3d};
        use space3d_core::{MeshData, Transform};
        let mut app = headless_app(w, h)?;
        app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
        let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(2.0));
        for (order, (x, color, vp, clear)) in [
            (-50.0, Vec4::new(0.0, 1.0, 0.0, 1.0), 0.0, Vec4::new(1.0, 0.0, 0.0, 1.0)),
            (50.0, Vec4::new(1.0, 0.0, 1.0, 1.0), 0.5, Vec4::new(0.0, 0.0, 1.0, 1.0)),
        ]
        .into_iter()
        .enumerate()
        {
            app.world
                .spawn((Transform::from_xyz(x, 0.0, 0.0), Mesh3d(cube), Material::unlit(color)));
            app.world.spawn((
                Transform::from_xyz(x, 0.0, 6.0),
                Camera3d {
                    order: order as i32,
                    viewport: Some([vp, 0.0, 0.5, 1.0]),
                    clear_color: Some(clear),
                    ..Default::default()
                },
            ));
        }
        Some(app)
    }

    /// Regression: when the presentation surface was lost after a frame's views had been
    /// prepared for it (`SurfaceAcquire::Recreate`), that frame drew its primary views into
    /// the offscreen target, which a windowed renderer sizes only for captures — it could
    /// still have the window's first size. After the window had grown, the right viewport's
    /// scissor rect lay outside it: a validation error that lost the frame's command encoder,
    /// uploads included (a panic in debug builds). The frame now renders offscreen at the size
    /// its views were prepared for, on every draw path, like the frames after it.
    #[test]
    fn a_surface_lost_mid_frame_renders_offscreen_at_the_prepared_size() {
        const RED: [u8; 3] = [255, 0, 0];
        const GREEN: [u8; 3] = [0, 255, 0];
        const BLUE: [u8; 3] = [0, 0, 255];
        const MAGENTA: [u8; 3] = [255, 0, 255];
        for samples in [1, 4] {
            let Some(mut app) = split_screen_app(64, 48, samples) else {
                return;
            };
            let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
            let mut reference = None;
            for path in [
                DrawPath::CpuDirect,
                DrawPath::GpuIndirect,
                DrawPath::GpuMultiDraw,
                DrawPath::GpuMultiDrawCount,
            ] {
                if path > best {
                    continue;
                }
                set_path(&mut app, path);
                {
                    let r = app.resources.expect_mut::<Renderer>();
                    r.frame.simulate_surface_loss = false;
                    r.resize(64, 48);
                }
                let img = crate::render_to_image(&mut app).expect("capture");
                assert_eq!((img.width, img.height), (64, 48));
                {
                    // The window grows; its surface is lost at the next frame's acquire.
                    let r = app.resources.expect_mut::<Renderer>();
                    r.frame.simulate_surface_loss = true;
                    r.resize(128, 96);
                    assert_eq!((r.offscreen.width, r.offscreen.height), (64, 48));
                }
                let img = crate::render_to_image(&mut app).expect("capture");
                let r = app.resources.expect::<Renderer>();
                assert!(!r.frame.simulate_surface_loss, "the lost-surface path ran");
                assert_eq!(r.gpu_error_count(), 0, "{path:?} ×{samples}");
                assert_eq!((img.width, img.height), (128, 96), "{path:?} ×{samples}");
                let n = |xs: std::ops::Range<u32>, rgb: [u8; 3]| {
                    xs.flat_map(|x| (0..96).map(move |y| (x, y)))
                        .filter(|&(x, y)| img.get(x, y)[..3] == rgb)
                        .count()
                };
                assert!(
                    n(0..64, RED) > 3000 && n(0..64, GREEN) > 300,
                    "{path:?} ×{samples}: left"
                );
                assert!(
                    n(64..128, BLUE) > 3000 && n(64..128, MAGENTA) > 300,
                    "{path:?} ×{samples}: right"
                );
                assert_eq!(
                    n(0..64, BLUE) + n(0..64, MAGENTA) + n(64..128, RED) + n(64..128, GREEN),
                    0
                );
                match &reference {
                    None => reference = Some(img),
                    Some(cpu) => assert_eq!(differing_px(cpu, &img), 0, "{path:?} ×{samples} differs from CpuDirect"),
                }
            }
        }
    }

    /// A pass's scissor rects stay within its attachment even if the attachment does not have
    /// the size the view was prepared for (which `surface_lost` prevents): a scissor rect
    /// outside it would fail validation and lose the frame's commands.
    #[test]
    fn scissor_rects_stay_within_the_attachment() {
        for samples in [1, 4] {
            let Some(mut app) = split_screen_app(64, 48, samples) else {
                return;
            };
            crate::render_to_image(&mut app).expect("capture");
            let r = app.resources.expect_mut::<Renderer>();
            assert_eq!(r.frame.last_primary_size, (64, 48));
            // Re-record the frame's primary views into a smaller target.
            let samples = r.config.msaa_samples;
            r.offscreen = crate::renderer::Attachments::new(&r.device, 40, 30, samples, OFFSCREEN_FORMAT, true);
            let scope = r.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let mut encoder = r.device.create_command_encoder(&Default::default());
            r.encode_views(&mut encoder, None, true);
            r.queue.submit([encoder.finish()]);
            let error = pollster::block_on(scope.pop());
            assert!(error.is_none(), "×{samples}: {error:?}");
            assert_eq!(r.gpu_error_count(), 0);
        }
    }

    /// The viewport clear writes its color — the pass's blend constant — into every color
    /// format a view can render to: sRGB (encoded on store), linear 8- and 10-bit, and HDR
    /// `Rgba16Float` (not clamped to 1), with 1 and 4 samples (resolved), only inside the
    /// viewport and its scissor rect.
    #[test]
    fn viewport_clear_writes_every_target_format() {
        use crate::components::linear_to_srgb;
        use crate::instance::f16_to_f32;
        use crate::pipeline::DEPTH_FORMAT;
        use wgpu::TextureFormat as F;
        const W: u32 = 8;
        const H: u32 = 4;
        let Some(mut app) = headless_app(8, 8) else { return };
        let r = app.resources.expect_mut::<Renderer>();
        let size = wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        };
        let rect = viewport_rect([0.25, 0.25, 0.375, 0.5], (W, H));
        let mut tested = 0;
        for format in [
            F::Rgba8UnormSrgb,
            F::Bgra8UnormSrgb,
            F::Rgba8Unorm,
            F::Bgra8Unorm,
            F::Rgb10a2Unorm,
            F::Rgba16Float,
        ] {
            let features = r.adapter.get_texture_format_features(format);
            let color = if format == F::Rgba16Float {
                [2.5, 0.5, 0.125, 1.0]
            } else {
                [0.25, 0.5, 0.75, 1.0]
            };
            for samples in [1, 4] {
                let depth_samples = r.adapter.get_texture_format_features(DEPTH_FORMAT).flags;
                if !features.allowed_usages.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
                    || !features.flags.contains(wgpu::TextureFormatFeatureFlags::BLENDABLE)
                    || !features.flags.sample_count_supported(samples)
                    || !depth_samples.sample_count_supported(samples)
                {
                    eprintln!("{format:?} ×{samples}: not a blendable render target here, skipped");
                    continue;
                }
                let texture = |format, samples, usage| {
                    r.device
                        .create_texture(&wgpu::TextureDescriptor {
                            label: None,
                            size,
                            mip_level_count: 1,
                            sample_count: samples,
                            dimension: wgpu::TextureDimension::D2,
                            format,
                            usage,
                            view_formats: &[],
                        })
                        .create_view(&Default::default())
                };
                use wgpu::TextureUsages as U;
                let target = texture(format, 1, U::RENDER_ATTACHMENT | U::COPY_SRC);
                let msaa = (samples > 1).then(|| texture(format, samples, U::RENDER_ATTACHMENT));
                let depth = texture(DEPTH_FORMAT, samples, U::RENDER_ATTACHMENT);
                let mut encoder = r.device.create_command_encoder(&Default::default());
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: msaa.as_ref().unwrap_or(&target),
                            depth_slice: None,
                            resolve_target: msaa.is_some().then_some(&target),
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: &depth,
                            depth_ops: Some(wgpu::Operations {
                                load: wgpu::LoadOp::Clear(1.0),
                                store: wgpu::StoreOp::Discard,
                            }),
                            stencil_ops: None,
                        }),
                        ..Default::default()
                    });
                    pass.set_viewport(rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32, 0.0, 1.0);
                    let [x, y, w, h] = rect.scissor((W, H));
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_pipeline(r.pipelines.viewport_clear(&r.device, format, samples));
                    let [cr, cg, cb, ca] = color.map(f64::from);
                    pass.set_blend_constant(wgpu::Color {
                        r: cr,
                        g: cg,
                        b: cb,
                        a: ca,
                    });
                    pass.draw(0..3, 0..1);
                    // As `encode_pass` does: GL clips the MSAA resolve to the scissor rect.
                    pass.set_scissor_rect(0, 0, W, H);
                }
                let texel = format.block_copy_size(None).expect("color format") as usize;
                let row = align_up((W as usize * texel) as u64, 256) as usize;
                let buffer = r.device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: (row * H as usize) as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                encoder.copy_texture_to_buffer(
                    target.texture().as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(row as u32),
                            rows_per_image: None,
                        },
                    },
                    size,
                );
                r.queue.submit([encoder.finish()]);
                buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
                r.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
                assert_eq!(r.gpu_error_count(), 0, "{format:?} ×{samples}");
                let data = buffer.slice(..).get_mapped_range().expect("mapped");
                // Stored values, normalized (sRGB formats store encoded values).
                let pixel = |x: usize, y: usize| -> [f32; 4] {
                    let p = &data[y * row + x * texel..][..texel];
                    let unorm8 = |i: usize| p[i] as f32 / 255.0;
                    match format {
                        F::Rgba8UnormSrgb | F::Rgba8Unorm => [0, 1, 2, 3].map(unorm8),
                        F::Bgra8UnormSrgb | F::Bgra8Unorm => [2, 1, 0, 3].map(unorm8),
                        F::Rgb10a2Unorm => {
                            let v = u32::from_le_bytes(p.try_into().unwrap());
                            let c = |shift: u32| ((v >> shift) & 1023) as f32 / 1023.0;
                            [c(0), c(10), c(20), (v >> 30) as f32 / 3.0]
                        }
                        F::Rgba16Float => [0, 2, 4, 6].map(|i| f16_to_f32(u16::from_le_bytes([p[i], p[i + 1]]))),
                        _ => unreachable!(),
                    }
                };
                let stored = color.map(|c| if format.is_srgb() { linear_to_srgb(c) } else { c });
                let expected = [stored[0], stored[1], stored[2], color[3]];
                let step = match format {
                    F::Rgb10a2Unorm => 1.0 / 1023.0,
                    F::Rgba16Float => 2e-3,
                    _ => 1.0 / 255.0,
                };
                for y in 0..H as usize {
                    for x in 0..W as usize {
                        let inside = (2..5).contains(&x) && (1..3).contains(&y);
                        let want = if inside { expected } else { [0.0, 0.0, 0.0, 1.0] };
                        let got = pixel(x, y);
                        let tolerance = |w: f32| step * w.abs().max(1.0) * 1.01;
                        assert!(
                            got.iter().zip(want).all(|(g, w)| (g - w).abs() <= tolerance(w)),
                            "{format:?} ×{samples} at ({x}, {y}): {got:?}, expected {want:?}"
                        );
                    }
                }
                drop(data);
                tested += 1;
            }
        }
        assert!(tested >= 6, "only {tested} format/sample combinations tested");
        assert_eq!(r.gpu_error_count(), 0);
    }

    /// Headless app on a device created with these limits, so wgpu's own validation fails past
    /// them (not only the renderer's copy of the limits). `None` without an adapter.
    fn limited_app(w: u32, h: u32, max_buffer: u64, max_binding: u64) -> Option<space3d_core::App> {
        crate::renderer::TEST_LIMITS.set(Some((max_buffer, max_binding)));
        let app = headless_app(w, h);
        crate::renderer::TEST_LIMITS.set(None);
        let app = app?;
        let caps = app.resources.expect::<Renderer>().capabilities();
        assert_eq!(caps.max_buffer_size, max_buffer);
        // GL (WebGL2 level) has no storage buffers: a binding limit of 0.
        assert!(caps.max_storage_binding <= max_binding);
        Some(app)
    }

    fn paths_up_to(best: DrawPath) -> impl Iterator<Item = DrawPath> {
        [
            DrawPath::CpuDirect,
            DrawPath::GpuIndirect,
            DrawPath::GpuMultiDraw,
            DrawPath::GpuMultiDrawCount,
        ]
        .into_iter()
        .filter(move |p| *p <= best)
    }

    fn count_px(img: &space3d_core::ImageData, rgb: [u8; 3]) -> usize {
        img.pixels.chunks(4).filter(|p| p[..3] == rgb).count()
    }

    /// `n` unlit cubes in a grid `side` wide, from `(x0, -3)` spanning 4 × 6 units.
    fn spawn_grid(app: &mut space3d_core::App, mesh: MeshHandle, n: usize, side: usize, x0: f32, rgb: [f32; 3]) {
        spawn_grid_part(app, mesh, n, n, side, x0, rgb);
    }

    /// The first `n` cubes of the [`spawn_grid`] of `total` cubes (at the same positions).
    fn spawn_grid_part(
        app: &mut space3d_core::App,
        mesh: MeshHandle,
        n: usize,
        total: usize,
        side: usize,
        x0: f32,
        rgb: [f32; 3],
    ) {
        use crate::Mesh3d;
        use space3d_core::Transform;
        let material = Material::unlit(glam::Vec4::new(rgb[0], rgb[1], rgb[2], 1.0));
        for i in 0..n {
            let x = x0 + (i % side) as f32 * 4.0 / side as f32;
            let y = (i / side) as f32 * 6.0 / (total.div_ceil(side)) as f32 - 3.0;
            app.world
                .spawn((Transform::from_xyz(x, y, 0.0), Mesh3d(mesh), material.clone()));
        }
    }

    #[test]
    fn fair_share_cuts_the_largest_batches() {
        let mut kept = Vec::new();
        let mut share = |counts: &[u32], cap: u64| {
            fair_share(counts.iter().copied(), cap, &mut kept);
            kept.clone()
        };
        assert_eq!(share(&[60_000, 10_000, 1], 65_536), [55_535, 10_000, 1]);
        assert_eq!(share(&[1, 60_000, 10_000], 65_536), [1, 55_535, 10_000]);
        // Equal batches share the cut; the remainder goes to the first ones.
        assert_eq!(share(&[5, 5, 5], 7), [3, 2, 2]);
        assert_eq!(share(&[9, 4, 9], 20), [8, 4, 8]);
        assert_eq!(share(&[9, 4, 9], 21), [9, 4, 8]);
        // Everything fits: unchanged.
        assert_eq!(share(&[10, 20], 30), [10, 20]);
        assert_eq!(share(&[10, 20], 1000), [10, 20]);
        assert_eq!(share(&[3, 3], 0), [0, 0]);
        assert_eq!(share(&[], 5), [] as [u32; 0]);
        for cap in 0..40 {
            let k = share(&[17, 2, 11, 0, 6], cap);
            assert_eq!(k.iter().map(|&x| x as u64).sum::<u64>(), cap.min(36), "cap {cap}");
        }
    }

    /// Regression: the `ecs` instance buffer grew to whatever the scene needed. Past the
    /// device's `max_buffer_size` creating it failed validation and every frame was lost,
    /// unrelated objects included (a panic in debug builds). Then the matrices past the limit
    /// were dropped in batch order, so a small batch sorting after a huge one vanished whole.
    /// Now the largest batch is cut (logged once): the frame equals the scene without the cut
    /// entities, and the small batches are all drawn.
    #[test]
    fn ecs_instances_past_max_buffer_size_are_cut_not_fatal() {
        use crate::{Camera3d, RenderStats};
        use space3d_core::{MeshData, Transform};
        // 4 MiB (one staging chunk): 65 536 matrices.
        const MAX: u64 = crate::gpu::STAGING_CHUNK;
        const FIT: usize = (MAX / 64) as usize;
        const RED: usize = 60_000;
        const GREEN: usize = 10_000;
        // A red grid on the left, a green one on the right and one cyan cube (its own
        // material, the last batch); `red` of the RED red cubes are spawned.
        let build = |red: usize| {
            let mut app = limited_app(96, 64, MAX, MAX)?;
            let mesh = app
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::cube(0.04));
            spawn_grid_part(&mut app, mesh, red, RED, 250, -5.0, [1.0, 0.0, 0.0]);
            for i in 0..GREEN {
                let x = 1.0 + (i % 100) as f32 * 0.04;
                let y = (i / 100) as f32 * 0.06 - 3.0;
                app.world.spawn((
                    Transform::from_xyz(x, y, 0.0),
                    crate::Mesh3d(mesh),
                    Material::unlit(glam::Vec4::new(0.0, 1.0, 0.0, 1.0)),
                ));
            }
            let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.6));
            app.world.spawn((
                Transform::from_xyz(-0.5, 2.0, 0.0),
                crate::Mesh3d(cube),
                Material::unlit(glam::Vec4::new(0.0, 1.0, 1.0, 1.0)),
            ));
            app.world.spawn((
                Transform::from_xyz(0.0, 0.0, 8.0),
                Camera3d::default().with_clear_color(glam::Vec4::new(0.0, 0.0, 0.0, 1.0)),
            ));
            Some(app)
        };
        let Some(probe) = limited_app(8, 8, MAX, MAX) else {
            return;
        };
        let best = probe.resources.expect::<Renderer>().capabilities().best_draw_path;
        drop(probe);
        for path in paths_up_to(best) {
            let mut app = build(RED).expect("adapter");
            set_path(&mut app, path);
            // Two frames: the first allocates, the second diffs against the shadow.
            crate::render_to_image(&mut app).expect("capture");
            let cut = crate::render_to_image(&mut app).expect("capture");
            let r = app.resources.expect::<Renderer>();
            assert_eq!(r.gpu_error_count(), 0, "{path:?}");
            assert!(r.frame.ecs_overflow, "{path:?}");
            assert!(r.frame.ecs.gpu.capacity() <= MAX, "{path:?}");
            assert_eq!(
                r.frame.ecs_kept,
                [(FIT - GREEN - 1) as u32, GREEN as u32, 1],
                "{path:?}"
            );
            let stats = app.resources.expect::<RenderStats>();
            assert_eq!((stats.instances, stats.batches), (FIT as u64, 3), "{path:?}");

            // The same scene without the red cubes that were cut.
            let mut app = build(FIT - GREEN - 1).expect("adapter");
            set_path(&mut app, path);
            let reference = crate::render_to_image(&mut app).expect("capture");
            assert!(
                !app.resources.expect::<Renderer>().frame.ecs_overflow,
                "exactly at the limit"
            );
            let d = differing_px(&cut, &reference);
            assert_eq!(
                d, 0,
                "{path:?}: differs from the scene without the cut entities in {d} px"
            );
            assert!(count_px(&cut, [255, 0, 0]) > 500, "{path:?}: red grid");
            assert!(count_px(&cut, [0, 255, 0]) > 300, "{path:?}: green grid");
            assert!(count_px(&cut, [0, 255, 255]) > 20, "{path:?}: the cyan cube");
        }
    }

    /// Regression: `cpu_visible` collects the partially visible ECS matrices of every view, so
    /// with several views it outgrew `max_buffer_size` even though the `ecs` buffer fit, and
    /// every frame was lost. A batch that no longer fits is now drawn whole from `ecs`
    /// (unculled): the image is unchanged.
    #[test]
    fn partially_visible_ecs_past_max_buffer_size_is_drawn_unculled() {
        use crate::{Camera3d, RenderTarget};
        use space3d_core::{MeshData, Transform};
        const MAX: u64 = crate::gpu::STAGING_CHUNK;
        // 50 000 matrices (3.2 MB) fit; two render-target views each partially see about two
        // thirds of them (2.1 MB each), the primary view about 90 %. A 1 MiB storage binding
        // keeps the batch on CPU culling on every path.
        let build = |app: &mut space3d_core::App| {
            let mesh = app
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::cube(0.04));
            spawn_grid(app, mesh, 50_000, 250, -6.0, [1.0, 0.5, 0.0]);
            for (order, x) in [(-2, -1.5), (-1, 1.5)] {
                let rt = app.resources.expect_mut::<RenderAssets>().add_render_target(64, 64);
                app.world.spawn((
                    Transform::from_xyz(x, 0.0, 7.0),
                    Camera3d {
                        order,
                        target: RenderTarget::Texture(rt),
                        ..Default::default()
                    },
                ));
            }
            app.world.spawn((
                Transform::from_xyz(1.0, 0.0, 7.0),
                Camera3d::default().with_clear_color(glam::Vec4::new(0.0, 0.0, 0.0, 1.0)),
            ));
        };
        let Some(mut reference) = headless_app(96, 64) else {
            return;
        };
        build(&mut reference);
        set_path(&mut reference, DrawPath::CpuDirect);
        let expected = crate::render_to_image(&mut reference).expect("capture");
        assert!(!reference.resources.expect::<Renderer>().frame.visible_overflow);
        assert!(count_px(&expected, [255, 188, 0]) > 500, "the grid is visible");
        let best = reference.resources.expect::<Renderer>().capabilities().best_draw_path;
        drop(reference);
        for path in paths_up_to(best) {
            let mut app = limited_app(96, 64, MAX, 1 << 20).expect("adapter");
            build(&mut app);
            set_path(&mut app, path);
            crate::render_to_image(&mut app).expect("capture");
            let img = crate::render_to_image(&mut app).expect("capture");
            let r = app.resources.expect::<Renderer>();
            assert_eq!(r.gpu_error_count(), 0, "{path:?}");
            let f = &r.frame;
            assert!(f.visible_overflow, "{path:?}");
            assert!(f.cpu_visible.len() > 0, "{path:?}: the first view still culls");
            assert!(f.cpu_visible.gpu.capacity() <= MAX, "{path:?}");
            let d = differing_px(&expected, &img);
            assert_eq!(d, 0, "{path:?} differs in {d} px");
        }
    }

    /// GPU culling binds the `ecs` buffer from offset 0 up to the storage-binding limit, so ECS
    /// batches that end inside it keep GPU culling when the whole buffer is larger (before,
    /// ECS data past the limit moved every ECS batch to CPU culling).
    #[test]
    fn ecs_batches_inside_the_storage_binding_keep_gpu_culling() {
        use crate::Camera3d;
        use space3d_core::{MeshData, Transform};
        const BINDING: u64 = 32 << 10;
        let build = |app: &mut space3d_core::App| {
            let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.1));
            // 200 matrices (12.8 KB) first, then 1000 (64 KB, past the binding).
            spawn_grid(app, mesh, 200, 20, -5.0, [1.0, 0.0, 0.0]);
            spawn_grid(app, mesh, 1000, 40, 1.0, [0.0, 0.0, 1.0]);
            app.world.spawn((
                Transform::from_xyz(-1.0, 0.0, 6.0),
                Camera3d::default().with_clear_color(glam::Vec4::new(0.0, 0.0, 0.0, 1.0)),
            ));
        };
        let Some(mut app) = limited_app(96, 64, 256 << 20, BINDING) else {
            return;
        };
        build(&mut app);
        let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
        if best == DrawPath::CpuDirect {
            return;
        }
        set_path(&mut app, DrawPath::CpuDirect);
        let cpu = crate::render_to_image(&mut app).expect("capture");
        for path in paths_up_to(best).skip(1) {
            set_path(&mut app, path);
            let img = crate::render_to_image(&mut app).expect("capture");
            let d = differing_px(&cpu, &img);
            assert_eq!(d, 0, "{path:?} differs from CpuDirect in {d} px");
            let r = app.resources.expect::<Renderer>();
            assert_eq!(r.gpu_error_count(), 0, "{path:?}");
            let f = &r.frame;
            let ecs: Vec<_> = f.sources.iter().filter(|s| s.buffer == SourceBuffer::Ecs).collect();
            assert_eq!(ecs.len(), 2);
            for s in ecs {
                let inside = (s.first + s.count) as u64 * 64 <= BINDING;
                assert_eq!(s.cull != CullSource::None, inside, "{path:?}: batch at {}", s.first);
            }
            assert_eq!(
                f.jobs.iter().filter(|(b, _)| *b == SourceBuffer::Ecs).count(),
                1,
                "{path:?}: the first batch is GPU-culled"
            );
        }
        assert!(count_px(&cpu, [255, 0, 0]) > 50 && count_px(&cpu, [0, 0, 255]) > 50);
    }

    /// `compact.wgsl` (GpuMultiDrawCount) keeps the non-empty draws of each run in order. Run
    /// directly so it is covered on adapters without MULTI_DRAW_INDIRECT_COUNT (e.g. Metal).
    #[test]
    fn draw_compaction_is_order_preserving() {
        use wgpu::util::DeviceExt;
        let Ok(r) = pollster::block_on(Renderer::new_headless(8, 8)) else {
            return;
        };
        let Some(cull) = &r.pipelines.cull else { return };
        let device = &r.device;
        // Two runs: 150 draws (every third empty) and 7 draws (all empty but one).
        let runs: [[u32; 2]; 2] = [[0, 150], [150, 7]];
        let mut args_in = Vec::new();
        for d in 0..157u32 {
            let instances = if d < 150 {
                if d % 3 == 0 { 0 } else { d }
            } else if d == 153 {
                5
            } else {
                0
            };
            args_in.extend_from_slice(&[1000 + d, instances, d, d * 2, d * 3]);
        }
        let params = GpuCompactParams {
            run_start: 0,
            run_count: 2,
            workgroups_x: 2,
            pad: 0,
        };
        use wgpu::BufferUsages as U;
        let init = |label, contents: &[u8], usage| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents,
                usage,
            })
        };
        let params_buf = init("params", bytemuck::bytes_of(&params), U::UNIFORM);
        let runs_buf = init("runs", bytemuck::cast_slice(&runs), U::STORAGE);
        let args_buf = init("args", bytemuck::cast_slice(&args_in), U::STORAGE);
        let counts = init("counts", &[0xff; 8], U::STORAGE | U::COPY_SRC);
        let out = init("out", &vec![0xff; args_in.len() * 4], U::STORAGE | U::COPY_SRC);
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &cull.compact_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_binding(&params_buf, 16),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: runs_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: args_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: counts.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: out.as_entire_binding(),
                },
            ],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 8 + out.size(),
            usage: U::MAP_READ | U::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&cull.compact);
            pass.set_bind_group(0, &bg, &[0]);
            pass.dispatch_workgroups(2, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&counts, 0, &readback, 0, 8);
        encoder.copy_buffer_to_buffer(&out, 0, &readback, 8, out.size());
        r.queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let words: Vec<u32> = bytemuck::cast_slice(&readback.slice(..).get_mapped_range().expect("mapped")).to_vec();
        let (counts, out) = words.split_at(2);
        for (r, [first, n]) in runs.iter().enumerate() {
            let expected: Vec<u32> = (*first..first + n)
                .filter(|d| args_in[*d as usize * 5 + 1] != 0)
                .collect();
            assert_eq!(counts[r], expected.len() as u32, "run {r}");
            for (slot, d) in expected.iter().enumerate() {
                let o = (*first as usize + slot) * 5;
                assert_eq!(
                    &out[o..o + 5],
                    &args_in[*d as usize * 5..*d as usize * 5 + 5],
                    "run {r} slot {slot}"
                );
            }
        }
    }

    /// Instances with an snorm16 translation: not decodable by the GPU culler
    /// (`CullSource::None`), so GPU paths draw them with direct draws too.
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    struct SnormInstance {
        translation: [i16; 4],
    }
    impl InstanceData for SnormInstance {
        const LAYOUT: &'static crate::InstanceLayout = &crate::InstanceLayout {
            name: "SnormInstance",
            stride: 8,
            attributes: &[crate::InstanceAttribute::new(
                crate::InstanceSemantic::Translation,
                wgpu::VertexFormat::Snorm16x4,
                0,
            )],
        };
    }

    fn snorm(v: f32) -> i16 {
        (v * i16::MAX as f32).round() as i16
    }

    /// Several meshes (so most draws have `base_vertex > 0`) through every source kind: ECS
    /// batches, a cullable explicit buffer and an uncullable one (direct even on GPU paths).
    fn many_mesh_scene(app: &mut space3d_core::App) {
        use crate::{Camera3d, InstanceBuffer, InstanceTransform, InstancedMesh, Mesh3d};
        use space3d_core::{MeshData, Transform};
        let (cube, sphere, plane, small) = {
            let a = app.resources.expect_mut::<RenderAssets>();
            (
                a.add_mesh(MeshData::cube(0.8)),
                a.add_mesh(MeshData::sphere(0.5, 16, 8)),
                a.add_mesh(MeshData::plane(1.0)),
                a.add_mesh(MeshData::cube(0.3)),
            )
        };
        let colors = [
            Vec4::new(1.0, 0.0, 0.0, 1.0),
            Vec4::new(0.0, 1.0, 0.0, 1.0),
            Vec4::new(1.0, 1.0, 0.0, 1.0),
            Vec4::new(1.0, 0.0, 1.0, 1.0),
        ];
        for (i, (mesh, color)) in [cube, sphere, plane, small].into_iter().zip(colors).enumerate() {
            app.world.spawn((
                Transform::from_xyz(i as f32 * 1.2 - 1.8, 1.0, 0.0).with_rotation(glam::Quat::from_rotation_x(0.6)),
                Mesh3d(mesh),
                Material::unlit(color),
            ));
        }
        let data = (0..4)
            .map(|i| {
                InstanceTransform::from_trs(
                    Vec3::new(i as f32 - 1.5, -0.2, 0.0),
                    glam::Quat::IDENTITY,
                    Vec3::splat(0.6),
                )
            })
            .collect();
        let instances = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, crate::BufferUsage::Static));
        app.world.spawn((InstancedMesh {
            mesh: sphere,
            material: Material::unlit(Vec4::new(0.0, 1.0, 1.0, 1.0)),
            instances,
        },));
        let data = (0..4)
            .map(|i| SnormInstance {
                translation: [snorm(i as f32 * 0.5 - 0.75), snorm(-0.9), 0, 0],
            })
            .collect();
        let instances = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, crate::BufferUsage::Static));
        app.world.spawn((InstancedMesh {
            mesh: small,
            material: Material::unlit(Vec4::new(1.0, 1.0, 1.0, 1.0)),
            instances,
        },));
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 5.0),
            Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
        ));
    }

    /// Regression: direct draws passed each mesh's arena offset as `base_vertex`. WebGL2 and
    /// GLES < 3.2 lack `DownlevelFlags::BASE_VERTEX`; wgpu does not reject it there and its GL
    /// backend calls a missing entry point (a panic on the web; on ANGLE ES 3.0 every mesh but
    /// the first vanished). Without the flag, direct draws now bind the arena from the mesh's
    /// first vertex and pass 0, and the image is unchanged. Also run on the best GPU path,
    /// whose uncullable sources are drawn directly between indirect draws. (On a device
    /// without the flag — e.g. `WGPU_BACKEND=gl` on ANGLE — only the emulated path runs.)
    #[test]
    fn direct_draws_without_base_vertex_bind_the_arena_at_the_mesh() {
        let Some(mut app) = headless_app(96, 64) else { return };
        many_mesh_scene(&mut app);
        let caps = app.resources.expect::<Renderer>().capabilities().clone();
        let mut reference = None;
        for path in [DrawPath::CpuDirect, caps.best_draw_path] {
            set_path(&mut app, path);
            let native = caps.base_vertex.then(|| {
                let image = crate::render_to_image(&mut app).expect("capture");
                let used = app.resources.expect::<Renderer>().frame.max_direct_base_vertex;
                assert!(used > 0, "{path:?}: the scene draws meshes at base_vertex > 0");
                image
            });

            app.resources.expect_mut::<Renderer>().caps.base_vertex = false;
            let emulated = crate::render_to_image(&mut app).expect("capture");
            let r = app.resources.expect_mut::<Renderer>();
            r.caps.base_vertex = caps.base_vertex;
            assert_eq!(r.frame.max_direct_base_vertex, 0, "{path:?}: direct draws use base 0");
            assert_eq!(r.gpu_error_count(), 0);
            if let Some(native) = &native {
                let d = differing_px(native, &emulated);
                assert_eq!(d, 0, "{path:?}: emulated base_vertex differs in {d} px");
            }
            let reference = reference.get_or_insert(emulated.clone());
            let d = differing_px(reference, &emulated);
            assert_eq!(d, 0, "{path:?} differs from CpuDirect in {d} px");
        }
        // Every mesh is visible: red cube, green sphere, yellow plane, magenta small cube,
        // cyan explicit spheres, white snorm-placed cubes.
        let img = reference.expect("rendered");
        for color in [
            [255, 0, 0],
            [0, 255, 0],
            [255, 255, 0],
            [255, 0, 255],
            [0, 255, 255],
            [255, 255, 255],
        ] {
            let n = img.pixels.chunks(4).filter(|p| p[..3] == color).count();
            assert!(n > 10, "color {color:?} covers {n} px");
        }
    }

    #[test]
    fn direct_vertex_base_rebinds_only_without_base_vertex() {
        assert_eq!(direct_vertex_base(24, true, true), (0, 24));
        assert_eq!(direct_vertex_base(24, true, false), (24, 0));
        assert_eq!(direct_vertex_base(0, true, false), (0, 0));
        // Indirect args carry their own base; slot 0 must start at vertex 0 for them.
        assert_eq!(direct_vertex_base(24, false, false), (0, 24));
    }

    /// Meshes uploaded before an arena growth, meshes whose placement straddles the old
    /// capacity (written through the staging ring after the GPU copy), and in-place /
    /// resizing `update_mesh` all render exactly like a fresh single-upload app — with and
    /// without `BASE_VERTEX`, on CpuDirect and the best GPU path.
    #[test]
    fn arena_growth_and_mesh_updates_render_like_fresh_uploads() {
        use crate::{Camera3d, Mesh3d, RenderStats};
        use space3d_core::{MeshData, Transform};
        let Some(probe) = headless_app(8, 8) else { return };
        let caps = probe.resources.expect::<Renderer>().capabilities().clone();
        drop(probe);
        let setup = |path: DrawPath, base_vertex: bool| {
            let mut app = headless_app(128, 64).expect("adapter");
            set_path(&mut app, path);
            app.resources.expect_mut::<Renderer>().caps.base_vertex = base_vertex;
            app.world.spawn((
                Transform::from_xyz(0.0, 0.0, 6.0),
                Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
            ));
            app
        };
        let white = Material::unlit(Vec4::ONE);
        let red = Material::unlit(Vec4::new(1.0, 0.0, 0.0, 1.0));
        let fresh = |path, base_vertex, a: Option<MeshData>, b: Option<MeshData>| {
            let mut app = setup(path, base_vertex);
            for (mesh, x, m) in [(a, -2.0, &white), (b, 2.0, &red)] {
                if let Some(mesh) = mesh {
                    let h = app.resources.expect_mut::<RenderAssets>().add_mesh(mesh);
                    app.world
                        .spawn((Transform::from_xyz(x, 0.0, 0.0), Mesh3d(h), m.clone()));
                }
            }
            crate::render_to_image(&mut app).expect("capture")
        };
        let native = caps.base_vertex;
        let runs = [
            (DrawPath::CpuDirect, false),
            (DrawPath::CpuDirect, true),
            (caps.best_draw_path, true),
        ];
        // Native base_vertex only where the device has it.
        for (path, base_vertex) in runs.into_iter().filter(|(_, b)| native || !b) {
            let ctx = format!("{path:?}, base_vertex {base_vertex}");
            let mut app = setup(path, base_vertex);
            let a = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
            app.world
                .spawn((Transform::from_xyz(-2.0, 0.0, 0.0), Mesh3d(a), white.clone()));
            let img = crate::render_to_image(&mut app).expect("capture");
            assert_eq!(
                differing_px(&img, &fresh(path, base_vertex, Some(MeshData::cube(1.0)), None)),
                0,
                "{ctx}"
            );

            // 8385 vertices: the arena grows and B is placed at base 24, straddling the old
            // 4096-vertex capacity.
            let big = || MeshData::sphere(1.0, 128, 64);
            let b = app.resources.expect_mut::<RenderAssets>().add_mesh(big());
            app.world
                .spawn((Transform::from_xyz(2.0, 0.0, 0.0), Mesh3d(b), red.clone()));
            let img = crate::render_to_image(&mut app).expect("capture");
            assert!(
                app.resources.expect::<RenderStats>().buffer_reallocations > 0,
                "{ctx}: arena grew"
            );
            let reference = fresh(path, base_vertex, Some(MeshData::cube(1.0)), Some(big()));
            assert_eq!(differing_px(&img, &reference), 0, "{ctx}: after growth");

            // Many more meshes: another growth, placed entirely in the fresh region.
            for i in 0..300 {
                app.resources
                    .expect_mut::<RenderAssets>()
                    .add_mesh(MeshData::sphere(0.3, 8 + i % 20, 6 + i % 10));
            }
            let img = crate::render_to_image(&mut app).expect("capture");
            assert_eq!(differing_px(&img, &reference), 0, "{ctx}: unrelated growth");

            // Same vertex/index counts: updated in place.
            assert!(
                app.resources
                    .expect_mut::<RenderAssets>()
                    .update_mesh(a, MeshData::cube(1.6))
            );
            let img = crate::render_to_image(&mut app).expect("capture");
            let reference = fresh(path, base_vertex, Some(MeshData::cube(1.6)), Some(big()));
            assert_eq!(differing_px(&img, &reference), 0, "{ctx}: in-place update");

            // Different size: freed and placed again; then large enough to grow the arena.
            for mesh in [MeshData::sphere(0.5, 16, 8), MeshData::sphere(0.5, 256, 128)] {
                assert!(app.resources.expect_mut::<RenderAssets>().update_mesh(a, mesh.clone()));
                let img = crate::render_to_image(&mut app).expect("capture");
                let reference = fresh(path, base_vertex, Some(mesh), Some(big()));
                assert_eq!(differing_px(&img, &reference), 0, "{ctx}: resizing update");
            }
            assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
        }
    }

    /// Batches sort opaque → blended, then by pipeline (class, layout, shader/texture kind),
    /// material and mesh: an interleaved spawn order groups every material's meshes together.
    #[test]
    fn sources_sort_pipeline_then_material_then_mesh() {
        let source = |blend: bool, layout: u16, pipeline: u64, material: u32, first_index: u32| Source {
            blend,
            class: if blend { CLASS_BLEND } else { 0 },
            layout,
            pipeline,
            material,
            mesh: GpuMesh {
                alloc: ArenaAlloc {
                    base_vertex: 0,
                    vertex_count: 0,
                    first_index,
                    index_count: 36,
                },
                sphere: Sphere {
                    center: Vec3::ZERO,
                    radius: 1.0,
                },
            },
            triangles: 12,
            buffer: SourceBuffer::Ecs,
            first: first_index,
            count: 1,
            stride: 64,
            cull: CullSource::Matrix { offset: 0 },
        };
        let mut sources = [
            source(true, 0, 0, 0, 0),   // blended: last
            source(false, 0, 0, 1, 36), // (mesh B, material 1)
            source(false, 0, 0, 0, 36), // (mesh B, material 0)
            source(false, 0, 2, 0, 0),  // custom shader: after the built-in pipeline
            source(false, 0, 0, 1, 0),  // (mesh A, material 1)
            source(false, 1, 0, 0, 0),  // another instance layout: after layout 0
            source(false, 0, 0, 0, 0),  // (mesh A, material 0)
        ];
        sources.sort_unstable_by_key(source_sort_key);
        let order: Vec<_> = sources
            .iter()
            .map(|s| (s.blend, s.layout, s.pipeline, s.material, s.mesh.alloc.first_index))
            .collect();
        assert_eq!(
            order,
            [
                (false, 0, 0, 0, 0),
                (false, 0, 0, 0, 36),
                (false, 0, 0, 1, 0),
                (false, 0, 0, 1, 36),
                (false, 0, 2, 0, 0),
                (false, 1, 0, 0, 0),
                (true, 0, 0, 0, 0),
            ]
        );
    }

    /// `order_by_key` is exactly a sort by (key, index).
    #[test]
    fn counting_sort_matches_a_key_index_sort() {
        let mut seed = 99u64;
        let mut scratch = KeyBuckets::default();
        let (mut order, mut runs) = (Vec::new(), Vec::new());
        for distinct in [1u64, 3, 40, 1000, 5000] {
            let keys: Vec<u64> = (0..5000)
                .map(|_| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    ((seed >> 20) % distinct) << 32 | (seed >> 60)
                })
                .collect();
            order_by_key(&keys, &mut order, &mut runs, &mut scratch);
            let mut reference: Vec<u32> = (0..keys.len() as u32).collect();
            reference.sort_by_key(|&i| (keys[i as usize], i));
            assert_eq!(order, reference, "{distinct} keys");
            let mut at = 0;
            for &(key, first, count) in &runs {
                assert_eq!(first, at);
                assert!(
                    order[at as usize..(at + count) as usize]
                        .iter()
                        .all(|&i| keys[i as usize] == key)
                );
                at += count;
            }
            assert_eq!(at as usize, keys.len());
        }
    }

    /// Regression: every frame re-sorted all ECS items (96-byte elements, O(n log n)) even when
    /// nothing changed — ~55 ms per frame for a static 1M-entity scene with interleaved
    /// meshes/materials. The order now depends only on the key sequence and is reused while
    /// that is unchanged; moving entities never reorders.
    /// Updating a custom shader drops the pipelines of its old version; removing it drops all
    /// of its cached state (checked every 256 frames), so an editor that creates and removes
    /// shaders does not accumulate pipelines.
    #[test]
    fn removed_custom_shaders_release_their_pipelines() {
        use crate::{Camera3d, CustomShader, Mesh3d};
        use space3d_core::{MeshData, Transform};
        let Some(mut app) = headless_app(16, 16) else { return };
        let shader = "fn fragment_hook(in: VsOut, c: vec4<f32>) -> vec4<f32> { return c.bgra; }";
        let (mesh, h) = {
            let a = app.resources.expect_mut::<RenderAssets>();
            (
                a.add_mesh(MeshData::cube(1.0)),
                a.add_shader(CustomShader::new("a", shader)),
            )
        };
        app.world
            .spawn((Transform::default(), Mesh3d(mesh), Material::default().with_shader(h)));
        app.world
            .spawn((Transform::from_xyz(0.0, 0.0, 3.0), Camera3d::default()));
        let counts = |app: &space3d_core::App| app.resources.expect::<Renderer>().pipelines.custom_counts();
        app.step_ticks(1);
        assert_eq!(counts(&app), (1, 1));
        let v2 = format!("{shader}\nfn my_unused() {{}}");
        app.resources
            .expect_mut::<RenderAssets>()
            .update_shader(h, CustomShader::new("a", v2));
        app.step_ticks(1);
        assert_eq!(counts(&app), (1, 1), "the old version's pipeline is dropped");
        app.resources.expect_mut::<RenderAssets>().remove_shader(h);
        app.step_ticks(256);
        assert_eq!(counts(&app), (0, 0));
    }

    /// Regression: on browser WebGPU the error scope around a custom-shader pipeline answers
    /// asynchronously and the answer was dropped, so an invalid pipeline stayed cached and
    /// every frame drawing with it was lost. Pending answers are now polled each frame and an
    /// invalid pipeline is replaced by the built-in one (reported through `shader_error`).
    /// Native backends answer at once, so the pending answer is injected here.
    #[test]
    fn custom_pipelines_rejected_asynchronously_fall_back() {
        use crate::{Camera3d, CustomShader, Mesh3d};
        use space3d_core::{MeshData, Transform};
        let Some(mut app) = headless_app(16, 16) else { return };
        let green = "fn fragment_hook(in: VsOut, c: vec4<f32>) -> vec4<f32> { return vec4<f32>(0.0, 1.0, 0.0, 1.0); }";
        let (mesh, h) = {
            let a = app.resources.expect_mut::<RenderAssets>();
            (
                a.add_mesh(MeshData::cube(1.0)),
                a.add_shader(CustomShader::new("green", green)),
            )
        };
        app.world.spawn((
            Transform::default(),
            Mesh3d(mesh),
            Material::unlit(Vec4::new(1.0, 0.0, 0.0, 1.0)).with_shader(h),
        ));
        app.world
            .spawn((Transform::from_xyz(0.0, 0.0, 3.0), Camera3d::default()));
        let center = |app: &mut space3d_core::App| crate::render_to_image(app).expect("capture").get(8, 8);
        assert_eq!(center(&mut app), [0, 255, 0, 255], "custom shader");

        // Every frame until the (injected) answer arrives draws with the custom pipeline; the
        // first frame after it draws with the built-in one.
        let answer = |app: &mut space3d_core::App, error: Option<&str>| {
            let p = &mut app.resources.expect_mut::<Renderer>().pipelines;
            for key in p.custom_keys() {
                p.defer_validation(key, error.map(str::to_owned));
            }
            for _ in 0..1000 {
                let img = crate::render_to_image(app).expect("capture");
                if app.resources.expect::<Renderer>().pipelines.pending_count() == 0 {
                    return img.get(8, 8);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            panic!("the injected answer never resolved");
        };
        assert_eq!(answer(&mut app, None), [0, 255, 0, 255], "valid: kept");
        assert_eq!(app.resources.expect::<Renderer>().shader_error(h), None);
        assert_eq!(answer(&mut app, Some("injected: invalid pipeline")), [255, 0, 0, 255]);
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.shader_error(h), Some("injected: invalid pipeline"));
        assert_eq!(r.pipelines.custom_keys().len(), 1, "the key maps to the fallback");
        assert_eq!(center(&mut app), [255, 0, 0, 255], "stays on the built-in shader");

        // Answers for a version replaced meanwhile are dropped with it.
        let p = &mut app.resources.expect_mut::<Renderer>().pipelines;
        for key in p.custom_keys() {
            p.defer_validation(key, Some("stale".into()));
        }
        app.resources
            .expect_mut::<RenderAssets>()
            .update_shader(h, CustomShader::new("green", format!("{green}\nfn v2() {{}}")));
        assert_eq!(center(&mut app), [0, 255, 0, 255], "new version");
        let r = app.resources.expect::<Renderer>();
        assert_eq!((r.pipelines.pending_count(), r.shader_error(h)), (0, None));
    }

    #[test]
    fn static_ecs_scenes_reuse_the_batch_order() {
        use crate::{Camera3d, Mesh3d};
        use space3d_core::{MeshData, Transform};
        let Some(mut app) = headless_app(64, 64) else { return };
        let meshes: Vec<_> = (0..3)
            .map(|k| {
                app.resources
                    .expect_mut::<RenderAssets>()
                    .add_mesh(MeshData::cube(0.1 + 0.05 * k as f32))
            })
            .collect();
        let materials: Vec<_> = (0..4).map(|k| Material::srgb(0.2 * k as f32, 0.6, 0.3)).collect();
        let mut entities = Vec::new();
        for i in 0..600 {
            let p = Vec3::new((i % 30) as f32 * 0.3 - 4.5, (i / 30) as f32 * 0.3 - 3.0, 0.0);
            entities.push(app.world.spawn((
                Transform::from_xyz(p.x, p.y, p.z),
                Mesh3d(meshes[i % 3]),
                materials[(i / 3) % 4].clone(),
            )));
        }
        app.world
            .spawn((Transform::from_xyz(0.0, 0.0, 10.0), Camera3d::default()));
        let reorders = |app: &space3d_core::App| app.resources.expect::<Renderer>().frame.ecs_reorders;
        app.step_ticks(1);
        assert_eq!(reorders(&app), 1);
        assert_eq!(app.resources.expect::<crate::RenderStats>().batches, 12);
        let misses = app.resources.expect::<Renderer>().frame.materials.misses;
        for frame in 0..4 {
            for (k, &e) in entities.iter().enumerate().step_by(7) {
                app.world.get::<&mut Transform>(e).unwrap().translation.z = (frame + k) as f32 * 0.01;
            }
            app.step_ticks(1);
        }
        assert_eq!(reorders(&app), 1, "moving entities keeps the batch order");
        assert_eq!(
            app.resources.expect::<Renderer>().frame.materials.misses,
            misses,
            "4 materials interleaved: every intern hits the cache"
        );
        // Changing one entity's material reorders once, and the result renders like a
        // freshly built scene.
        *app.world.get::<&mut Material>(entities[5]).unwrap() = materials[0].clone();
        let img = crate::render_to_image(&mut app).expect("capture");
        assert_eq!(reorders(&app), 2);
        crate::render_to_image(&mut app).expect("capture");
        assert_eq!(reorders(&app), 2);

        let Some(mut fresh) = headless_app(64, 64) else { return };
        let fresh_meshes: Vec<_> = (0..3)
            .map(|k| {
                fresh
                    .resources
                    .expect_mut::<RenderAssets>()
                    .add_mesh(MeshData::cube(0.1 + 0.05 * k as f32))
            })
            .collect();
        for &e in &entities {
            let t = *app.world.get::<&Transform>(e).unwrap();
            let m = app.world.get::<&Mesh3d>(e).unwrap().0;
            let k = meshes.iter().position(|&x| x == m).unwrap();
            let mat = Material::clone(&app.world.get::<&Material>(e).unwrap());
            fresh.world.spawn((t, Mesh3d(fresh_meshes[k]), mat));
        }
        fresh
            .world
            .spawn((Transform::from_xyz(0.0, 0.0, 10.0), Camera3d::default()));
        let expected = crate::render_to_image(&mut fresh).expect("capture");
        assert_eq!(differing_px(&img, &expected), 0);
    }

    /// Regression: interning hashed every entity's material; the cache must also never map a
    /// key to an id that the garbage collector freed and handed to another material.
    #[test]
    fn material_cache_survives_id_reuse() {
        let mut t = MaterialTable::default();
        let mut buf = TrackedBuffer::new("test", wgpu::BufferUsages::UNIFORM);
        let mut r = 0;
        let a = Material::srgb(1.0, 0.0, 0.0);
        let b = Material::srgb(0.0, 1.0, 0.0);
        let (ia, _) = t.intern(&a, 1, &mut buf, 256, &mut r);
        for frame in 2..10 {
            assert_eq!(t.intern(&a, frame, &mut buf, 256, &mut r).0, ia);
        }
        assert_eq!(t.misses, 1, "only the first intern misses");
        // `a` idle long enough to be collected; `b` takes over its id.
        t.gc(1024);
        let (ib, _) = t.intern(&b, 1024, &mut buf, 256, &mut r);
        assert_eq!(ib, ia, "freed id reused");
        let (ia2, _) = t.intern(&a, 1025, &mut buf, 256, &mut r);
        assert_ne!(ia2, ib, "a stale cache entry must not map `a` to `b`'s id");
        assert!(t.entries[ia2 as usize].key == MaterialKey::new(&a));
        assert!(t.entries[ib as usize].key == MaterialKey::new(&b));
    }

    /// Regression: explicit-buffer bounds were recomputed every frame after a single
    /// `swap_remove` — a full scan through a per-instance layout search — on every path,
    /// although only CPU culling reads them. GPU paths now never compute them; the CPU path
    /// recomputes only the edited block.
    #[test]
    fn explicit_bounds_are_computed_only_for_cpu_culling() {
        use crate::{BufferUsage, Camera3d, InstanceBuffer, InstanceCompact, InstancedMesh};
        use space3d_core::{MeshData, Transform};
        let Some(mut app) = headless_app(32, 32) else { return };
        let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.1));
        let n = 50_000;
        let data = (0..n)
            .map(|i| InstanceCompact::new(Vec3::new(i as f32 * 0.01, 0.0, 0.0), glam::Quat::IDENTITY, 1.0))
            .collect();
        let h = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
        app.world.spawn((InstancedMesh {
            mesh,
            material: Material::default(),
            instances: h,
        },));
        app.world
            .spawn((Transform::from_xyz(0.0, 0.0, 20.0), Camera3d::default()));
        let step = |app: &mut space3d_core::App, frame: usize| {
            let buffers = app.resources.expect_mut::<InstanceBuffers>();
            let b = buffers.get_mut::<InstanceCompact>(h).unwrap();
            let v = b.swap_remove(frame * 997 % b.len());
            b.push(v);
            app.step_ticks(1);
            assert_eq!(app.resources.expect::<crate::RenderStats>().views, 1);
            let buffers = app.resources.expect::<InstanceBuffers>();
            buffers.buffers.get(h.raw()).unwrap().bounds_scanned()
        };
        if best != DrawPath::CpuDirect {
            set_path(&mut app, best);
            for frame in 0..5 {
                assert_eq!(step(&mut app, frame), 0, "{best:?} frame {frame}");
            }
        }
        set_path(&mut app, DrawPath::CpuDirect);
        assert_eq!(step(&mut app, 5), n, "first CPU-culled frame computes every block");
        for frame in 6..10 {
            assert!(
                step(&mut app, frame) <= 2 * crate::instance::CULL_BLOCK,
                "frame {frame}"
            );
        }
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
