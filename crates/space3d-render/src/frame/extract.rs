//! Extraction: ECS entities and explicit instance buffers → batched, sorted sources; material
//! interning (one uniform slot per distinct material).

use super::FrameState;
use crate::assets::{GpuMesh, RenderAssets};
use crate::components::{Material, Mesh3d, Visibility};
use crate::gpu::{GrowBuffer, TrackedBuffer, Uploader};
use crate::handles::FastMap;
use crate::handles::{MeshHandle, ShaderHandle, TextureHandle};
use crate::instance::{BufferUsage, CullSource, ErasedInstanceBuffer, InstanceBuffers, InstancedMesh};
use crate::pipeline::{CLASS_BLEND, CLASS_DOUBLE_SIDED, GpuMaterial, Pipelines};
use crate::stats::RenderStats;
use space3d_core::{GlobalTransform, World};

/// Which GPU buffer a batch reads instances from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum SourceBuffer {
    Ecs,
    Explicit(u32),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Source {
    pub(super) blend: bool,
    pub(super) class: u8,
    pub(super) layout: u16,
    /// Rest of the pipeline key besides class and layout: custom shader and texture kind of
    /// the material (see [`pipeline_rank`]).
    pub(super) pipeline: u64,
    pub(super) material: u32,
    pub(super) mesh: GpuMesh,
    pub(super) triangles: u32,
    pub(super) buffer: SourceBuffer,
    pub(super) first: u32,
    pub(super) count: u32,
    pub(super) stride: u32,
    pub(super) cull: CullSource,
}

pub(crate) struct GpuInstances {
    pub(super) generation: u32,
    pub(super) buf: GrowBuffer,
    pub(super) synced: u64,
    pub(super) len: u32,
    /// The buffer exceeds the device's `max_buffer_size` (logged once, not drawn).
    pub(super) too_large: bool,
}

/// Scratch of [`order_by_key`].
#[derive(Default)]
pub(super) struct KeyBuckets {
    pub(super) map: FastMap<u64, u32>,
    /// Direct-mapped `(key, bucket)` cache in front of `map` (`u32::MAX` = empty).
    pub(super) cache: Vec<(u64, u32)>,
    /// Per bucket: its key, then its item count, then its write cursor.
    pub(super) keys: Vec<u64>,
    pub(super) counts: Vec<u32>,
    /// Bucket of each item.
    pub(super) of: Vec<u32>,
    /// Buckets sorted by key.
    pub(super) sorted: Vec<u32>,
}

/// Group item indices by key: `order` lists the items by ascending key and, within a key, in
/// their original order — exactly a sort by `(key, index)`, but a counting sort: O(items +
/// keys · log keys), no element moves. `runs` gets `(key, start, count)` per key.
pub(super) fn order_by_key(keys: &[u64], order: &mut Vec<u32>, runs: &mut Vec<(u64, u32, u32)>, s: &mut KeyBuckets) {
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

/// Extracted ECS entities at or above this count (per archetype) gather their matrices on all
/// rayon workers.
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_GATHER: usize = 32_768;

/// `dst[k] = transforms[rows[k]]` as column arrays (a strided 64-byte copy per entity).
pub(super) fn gather_matrices(transforms: &[GlobalTransform], rows: &[u32], dst: &mut [[f32; 16]]) {
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct MaterialKey {
    words: [u32; 9],
    flags: u8,
    texture: Option<TextureHandle>,
    shader: Option<ShaderHandle>,
}

impl MaterialKey {
    pub(super) fn new(m: &Material) -> Self {
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
    pub(super) fn cache_slot(&self) -> usize {
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

pub(super) struct MatEntry {
    pub(super) key: MaterialKey,
    pub(super) class: u8,
    pub(super) last_used: u64,
    pub(super) live: bool,
}

/// Slots of the direct-mapped cache in front of the material map (a power of two).
const MATERIAL_CACHE: usize = 64;

#[derive(Clone, Copy)]
pub(super) struct CachedMaterial {
    pub(super) key: MaterialKey,
    pub(super) id: u32,
    pub(super) class: u8,
    /// Frame whose `last_used` this entry already refreshed.
    pub(super) frame: u64,
}

/// Interns equal materials to one id / uniform slot so they batch together.
///
/// Every ECS entity carries its `Material` by value, so interning runs once per entity per
/// frame. The last material seen and a small direct-mapped cache answer almost all of those
/// with one key comparison (neighbouring entities mostly share materials, and a scene rarely
/// uses more materials at once than the cache has slots); only misses hash and probe the map.
pub(crate) struct MaterialTable {
    pub(super) map: FastMap<MaterialKey, u32>,
    pub(super) entries: Vec<MatEntry>,
    pub(super) free: Vec<u32>,
    pub(super) cache: [Option<CachedMaterial>; MATERIAL_CACHE],
    pub(super) last: Option<CachedMaterial>,
    /// Interns that missed the cache (tests).
    pub(super) misses: u64,
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
    pub(super) fn intern(
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
    pub(super) fn gc(&mut self, frame: u64) {
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

    pub(super) fn texture(&self, id: u32) -> Option<TextureHandle> {
        self.entries.get(id as usize).and_then(|e| e.key.texture)
    }

    pub(super) fn shader(&self, id: u32) -> Option<ShaderHandle> {
        self.entries.get(id as usize).and_then(|e| e.key.shader)
    }
}

/// How many instances of each batch (`counts`) to keep so that at most `cap` are kept in
/// total, taking the overflow from the largest batches: every batch keeps `min(count, t)` for
/// the largest `t` that fits, and what remains below `cap` goes one instance each to the first
/// batches (in batch order) that are still cut. A small batch is never dropped because a large
/// one sorts before it. Deterministic; only runs when the ECS data outgrows `max_buffer_size`.
pub(super) fn fair_share(counts: impl Iterator<Item = u32> + Clone, cap: u64, kept: &mut Vec<u32>) {
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

/// The part of a material's pipeline key that is not its class: its custom shader (slot + 1,
/// 0 for none) and whether its texture is an array (`texture_2d_array` pipeline variant).
pub(super) fn pipeline_rank(materials: &MaterialTable, assets: &RenderAssets, material: u32) -> u64 {
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
pub(super) fn source_sort_key(s: &Source) -> (bool, u8, u16, u64, u32, u32, SourceBuffer, u32) {
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

/// Upload an explicit instance buffer's dirty ranges (or all of it after reallocation / when
/// more than half changed / for `Stream` buffers).
pub(super) fn sync_instances(
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

impl FrameState {
    /// ECS extraction and batching, then explicit instance buffers: this frame's sources, in
    /// draw order (see [`source_sort_key`]).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn extract(
        &mut self,
        world: &World,
        assets: &RenderAssets,
        buffers: &mut InstanceBuffers,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        up: &mut Uploader,
        pipelines: &mut Pipelines,
        frame_index: u64,
        stats: &mut RenderStats,
        reallocs: &mut u32,
    ) {
        let f = self;
        let mut cpu_reallocs = 0u32;
        let material_stride = f.material_stride();
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
        *reallocs += cpu_reallocs;
    }
}

/// Whether the batches of `material` cast shadows (the depth views' draw filter).
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) fn source_casts(materials: &MaterialTable, material: u32) -> bool {
    let _ = (materials, material); // STUB(P-MAT): ShadowMode / blend / unlit rules
    false
}
