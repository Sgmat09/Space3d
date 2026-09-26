//! Extraction: ECS entities and explicit instance buffers → batched, sorted sources; material
//! interning (one uniform slot per distinct material, one bind group per texture set), the
//! per-instance transparent sort and the materials' shadow participation.

use super::FrameState;
use crate::assets::{GpuMesh, RenderAssets};
use crate::components::{Material, Mesh3d, Visibility};
use crate::gpu::{GrowBuffer, TrackedBuffer, Uploader};
use crate::handles::FastMap;
use crate::handles::{MeshHandle, ShaderHandle, TextureHandle};
use crate::instance::{BufferUsage, CullSource, ErasedInstanceBuffer, InstanceBuffers, InstancedMesh};
use crate::lighting::api::{LightingSettings, TransparentSort};
use crate::material::{
    BlendMode, CLASS_EXT, FLAG_EMISSIVE, FLAG_FIXUP_SHIFT, FLAG_MAPS, FLAG_METALLIC_ROUGHNESS, FLAG_NORMAL,
    FLAG_OCCLUSION, FLAG_ORM_PACKED, FLAG_UV_TRANSFORM, MapMask, ShadowMode,
};
use crate::pipeline::{
    CLASS_BLEND, CLASS_DOUBLE_SIDED, GpuMaterial, KIND_COLOR_LAYERED, KIND_DATA_LAYERED, Pipelines, lighting_globals,
};
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

/// The texture maps of a material, by slot: base color, normal, metallic-roughness, occlusion,
/// emissive (the bindings 1, 3, 5, 7, 9 of the material bind group).
pub(super) type TextureSet = [Option<TextureHandle>; 5];

/// Slot of the base color map in a [`TextureSet`].
pub(super) const SLOT_BASE: usize = 0;
/// Slot of the emissive map.
pub(super) const SLOT_EMISSIVE: usize = 4;
/// Slots of the data maps (normal, metallic-roughness, occlusion): linear textures sharing one
/// texture kind.
const DATA_SLOTS: std::ops::RangeInclusive<usize> = 1..=3;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct MaterialKey {
    /// Base color, metallic, roughness, emissive, normal scale, occlusion strength, alpha cutoff,
    /// UV transform (offset, rotation, scale), as bits.
    words: [u32; 17],
    /// Unlit, double-sided, legacy alpha blend.
    flags: u8,
    blend: BlendMode,
    uv1_maps: u8,
    shadows: ShadowMode,
    render_order: i16,
    maps: TextureSet,
    shader: Option<ShaderHandle>,
}

impl MaterialKey {
    pub(super) fn new(m: &Material) -> Self {
        let b = m.base_color;
        let e = m.emissive;
        let t = m.uv_transform;
        Self {
            words: [
                b.x,
                b.y,
                b.z,
                b.w,
                m.metallic,
                m.roughness,
                e.x,
                e.y,
                e.z,
                m.normal_scale,
                m.occlusion_strength,
                m.alpha_cutoff,
                t.offset.x,
                t.offset.y,
                t.rotation,
                t.scale.x,
                t.scale.y,
            ]
            .map(f32::to_bits),
            flags: (m.unlit as u8) | (m.double_sided as u8) << 1 | (m.alpha_blend as u8) << 2,
            blend: m.blend,
            uv1_maps: m.uv1_maps.0,
            shadows: m.shadows,
            render_order: m.render_order,
            maps: [
                m.base_color_texture,
                m.normal_texture,
                m.metallic_roughness_texture,
                m.occlusion_texture,
                m.emissive_texture,
            ],
            shader: m.shader,
        }
    }

    /// Slot in [`MaterialTable`]'s direct-mapped cache (a cheap multiplicative hash).
    pub(super) fn cache_slot(&self) -> usize {
        let handle = |t: Option<TextureHandle>| t.map_or(0, |t| (t.index() + 1) ^ t.generation().rotate_left(16));
        let sh = self
            .shader
            .map_or(0, |s| (s.index() + 1) ^ s.generation().rotate_left(16));
        let bits = self.flags as u32
            | (self.blend.class_bits() as u32) << 8
            | (self.uv1_maps as u32) << 16
            | (self.render_order as u16 as u32) << 8;
        let mut h = (bits ^ sh.rotate_left(8)).wrapping_mul(0x9E37_79B1);
        for t in self.maps {
            h = (h.rotate_left(5) ^ handle(t)).wrapping_mul(0x9E37_79B1);
        }
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
    /// Texture set (index in [`MaterialTable::sets`]).
    pub(super) set: u32,
    /// `FLAG_*` bits the material asks for; the uniform gets those its set can honour
    /// ([`TextureSetEntry::flags`]).
    pub(super) wanted: u32,
    /// The uniform as written into its slot.
    pub(super) gpu: GpuMaterial,
}

/// A texture set in use: its maps, what the GPU textures behind them are (resolved in
/// `extract`, once per frame of use) and its material bind group (built in
/// `prepare_material_bind_groups`, after the frame's uploads).
pub(crate) struct TextureSetEntry {
    pub(super) maps: TextureSet,
    pub(super) live: bool,
    /// Last frame the set was resolved against the uploaded textures.
    pub(super) checked: u64,
    /// `TextureEntry::version` of every map it was resolved for (`u32::MAX`: no GPU texture).
    pub(super) versions: [u32; 5],
    /// Resolved once at least.
    pub(super) resolved: bool,
    /// `PipelineKey::kinds`: color maps (base, emissive) and data maps are arrays.
    pub(super) kinds: u8,
    /// Slots bound to their texture (bit `slot`); the others get the default of their kind.
    pub(super) available: u8,
    /// Data slots whose texture is sRGB-encoded (bit `slot`): the shader re-encodes them.
    pub(super) srgb: u8,
    /// A map of the other texture kind than its group was reported.
    pub(super) warned: bool,
    /// The bind group, with the materials buffer generation and texture versions it binds.
    pub(super) bind_group: Option<(wgpu::BindGroup, u32, [u32; 5])>,
    /// Variant for views drawing into a render target the set samples: that render target is
    /// replaced by its copy (`feedback`) or the default.
    pub(super) own_target: Option<OwnTargetBindGroup>,
}

/// A texture set's bind group for views drawing into one of its maps: (that render target, the
/// copy bound in its place (`None`: the default), materials buffer generation, texture versions,
/// bind group).
pub(super) type OwnTargetBindGroup = (TextureHandle, Option<wgpu::TextureView>, u32, [u32; 5], wgpu::BindGroup);

impl TextureSetEntry {
    fn new(maps: TextureSet) -> Self {
        Self {
            maps,
            live: true,
            checked: u64::MAX,
            versions: [u32::MAX; 5],
            resolved: false,
            kinds: 0,
            available: 0,
            srgb: 0,
            warned: false,
            bind_group: None,
            own_target: None,
        }
    }

    /// The uniform flags (`MaterialParams::flags.x`) of a material asking for `wanted` with this
    /// set: maps the set cannot bind are dropped (the shader then keeps the factors), ORM
    /// packing needs both slots, and sRGB data maps get their fixup bits.
    pub(super) fn flags(&self, wanted: u32) -> u32 {
        let available = self.available as u32;
        let mut f = wanted & !(FLAG_MAPS | FLAG_ORM_PACKED) | wanted & FLAG_MAPS & available;
        let orm = FLAG_METALLIC_ROUGHNESS | FLAG_OCCLUSION;
        if wanted & FLAG_ORM_PACKED != 0 && f & orm == orm {
            f |= FLAG_ORM_PACKED;
        }
        for slot in DATA_SLOTS {
            if f & (1 << slot) != 0 && self.srgb & (1 << slot) != 0 {
                f |= 1 << (FLAG_FIXUP_SHIFT + slot as u32);
            }
        }
        f
    }
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

/// Interns equal materials to one id / uniform slot so they batch together, and their texture
/// sets to one bind group each.
///
/// Every ECS entity carries its `Material` by value, so interning runs once per entity per
/// frame. The last material seen answers most of those with one comparison of the material
/// (neighbouring entities mostly share materials); a small direct-mapped cache of keys answers
/// almost all the others (a scene rarely uses more materials at once than the cache has slots);
/// only misses hash and probe the map.
pub(crate) struct MaterialTable {
    pub(super) map: FastMap<MaterialKey, u32>,
    pub(super) entries: Vec<MatEntry>,
    pub(super) free: Vec<u32>,
    pub(super) cache: [Option<CachedMaterial>; MATERIAL_CACHE],
    pub(super) last: Option<CachedMaterial>,
    /// The material `last` was interned for (the per-entity fast path compares it).
    pub(super) last_material: Option<Material>,
    /// Interns that missed the cache (tests).
    pub(super) misses: u64,
    /// Texture sets by maps, and the sets.
    pub(super) set_map: FastMap<TextureSet, u32>,
    pub(super) sets: Vec<TextureSetEntry>,
    pub(super) free_sets: Vec<u32>,
    /// Scratch of the set garbage collection: sets referenced by live materials.
    set_refs: Vec<bool>,
    /// Frame-wide override bits of the lighting settings (`PipelineKey::globals`).
    pub(super) globals: u8,
    /// Transparent sorting of the lighting settings.
    pub(super) transparent_sort: TransparentSort,
    /// Scratch of [`Self::sort_back_to_front`]: keys, then the sorted instances.
    sort_keys: Vec<u64>,
    sort_order: Vec<u32>,
    /// The default of array-kind maps: a 2-layer white `D2Array` (a 1-layer array would be a
    /// plain 2D texture on GL, which a `sampler2DArray` reads as black). Single-layer maps
    /// default to the renderer's 1×1 white texture.
    pub(super) white_array: Option<wgpu::TextureView>,
    /// The extension stream bound to slot 2 this frame: the arena's, or a zero-filled stand-in
    /// (capacity in vertices) while no mesh has extension data.
    pub(super) ext_buffer: Option<wgpu::Buffer>,
    zero_ext: Option<(wgpu::Buffer, u32)>,
    /// Instance layouts (ids) whose ext-less pipelines were reported.
    ext_warned: Vec<u16>,
}

impl Default for MaterialTable {
    fn default() -> Self {
        Self {
            map: FastMap::default(),
            entries: Vec::new(),
            free: Vec::new(),
            cache: [None; MATERIAL_CACHE],
            last: None,
            last_material: None,
            misses: 0,
            set_map: FastMap::default(),
            sets: Vec::new(),
            free_sets: Vec::new(),
            set_refs: Vec::new(),
            globals: 0,
            transparent_sort: TransparentSort::None,
            sort_keys: Vec::new(),
            sort_order: Vec::new(),
            white_array: None,
            ext_buffer: None,
            zero_ext: None,
            ext_warned: Vec::new(),
        }
    }
}

/// The `FLAG_*` bits a material asks for (before its set's availability).
fn wanted_flags(m: &Material) -> u32 {
    let mut f = 0;
    if m.normal_texture.is_some() {
        f |= FLAG_NORMAL;
    }
    if m.metallic_roughness_texture.is_some() {
        f |= FLAG_METALLIC_ROUGHNESS;
    }
    if m.occlusion_texture.is_some() {
        f |= FLAG_OCCLUSION;
        if m.occlusion_texture == m.metallic_roughness_texture {
            f |= FLAG_ORM_PACKED;
        }
    }
    if m.emissive_texture.is_some() {
        f |= FLAG_EMISSIVE;
    }
    if !m.uv_transform.is_identity() {
        f |= FLAG_UV_TRANSFORM;
    }
    f
}

/// The uniform of a material (`flags.x` from `flags`).
fn gpu_material(m: &Material, flags: u32) -> GpuMaterial {
    let t = m.uv_transform.matrix();
    GpuMaterial {
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
        maps: [m.normal_scale, m.occlusion_strength, m.alpha_cutoff, 0.0],
        flags: [flags, m.uv1_maps.0 as u32, 0, 0],
        uv_row0: [t.x_axis.x, t.y_axis.x, t.z_axis.x, 0.0],
        uv_row1: [t.x_axis.y, t.y_axis.y, t.z_axis.y, 0.0],
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
        match &self.last {
            Some(l) if l.frame == frame && self.last_material.as_ref() == Some(m) => (l.id, l.class),
            _ => self.intern_cached(m, frame, buf, stride, cpu_reallocs),
        }
    }

    /// [`Self::intern`] when the material differs from the previous one (kept out of line so
    /// the per-entity fast path stays small).
    #[inline(never)]
    fn intern_cached(
        &mut self,
        m: &Material,
        frame: u64,
        buf: &mut TrackedBuffer,
        stride: usize,
        cpu_reallocs: &mut u32,
    ) -> (u32, u8) {
        let key = MaterialKey::new(m);
        self.last_material = Some(m.clone());
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
        let class = material_class(m);
        let set = self.intern_set(key.maps);
        let wanted = wanted_flags(m);
        let gpu = gpu_material(m, self.sets[set as usize].flags(wanted));
        let entry = MatEntry {
            key,
            class,
            last_used: frame,
            live: true,
            set,
            wanted,
            gpu,
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
        buf.write_at(id as usize * stride, bytemuck::bytes_of(&gpu), cpu_reallocs);
        (id, class)
    }

    /// Id of the texture set `maps` (created on first use).
    fn intern_set(&mut self, maps: TextureSet) -> u32 {
        if let Some(&id) = self.set_map.get(&maps) {
            return id;
        }
        let entry = TextureSetEntry::new(maps);
        let id = match self.free_sets.pop() {
            Some(id) => {
                self.sets[id as usize] = entry;
                id
            }
            None => {
                self.sets.push(entry);
                self.sets.len() as u32 - 1
            }
        };
        self.set_map.insert(maps, id);
        id
    }

    /// Drop materials unused for a while so the table does not grow without bound, and the
    /// texture sets no live material uses (with their bind groups).
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
            self.last_material = None;
            self.set_refs.clear();
            self.set_refs.resize(self.sets.len(), false);
            for e in self.entries.iter().filter(|e| e.live) {
                self.set_refs[e.set as usize] = true;
            }
            for (i, s) in self.sets.iter_mut().enumerate() {
                if s.live && !self.set_refs[i] {
                    s.live = false;
                    s.bind_group = None;
                    s.own_target = None;
                    self.set_map.remove(&s.maps);
                    self.free_sets.push(i as u32);
                }
            }
        }
    }

    /// The base color texture of a material.
    pub(super) fn texture(&self, id: u32) -> Option<TextureHandle> {
        self.entries.get(id as usize).and_then(|e| e.key.maps[SLOT_BASE])
    }

    pub(super) fn shader(&self, id: u32) -> Option<ShaderHandle> {
        self.entries.get(id as usize).and_then(|e| e.key.shader)
    }

    /// Texture set of a material.
    pub(super) fn set_of(&self, id: u32) -> u32 {
        self.entries.get(id as usize).map_or(0, |e| e.set)
    }

    /// Transparent draw order of a material (`Material::render_order`).
    pub(super) fn render_order(&self, id: u32) -> i16 {
        self.entries.get(id as usize).map_or(0, |e| e.key.render_order)
    }

    /// Resolve the texture set of `material` against the uploaded textures (once per frame):
    /// texture kinds, which maps can be bound, which data maps are sRGB. When that changes, the
    /// uniforms of the set's materials are rewritten (their map flags).
    pub(super) fn resolve_set(
        &mut self,
        material: u32,
        assets: &RenderAssets,
        frame: u64,
        buf: &mut TrackedBuffer,
        stride: usize,
        cpu_reallocs: &mut u32,
    ) {
        let id = self.set_of(material);
        let Some(set) = self.sets.get_mut(id as usize) else {
            return;
        };
        if set.checked == frame {
            return;
        }
        set.checked = frame;
        let gpu = set
            .maps
            .map(|h| h.and_then(|h| assets.textures.get(h.raw())).filter(|e| e.gpu.is_some()));
        let versions = gpu.map(|e| e.map_or(u32::MAX, |e| e.version));
        if set.resolved && versions == set.versions {
            return;
        }
        set.versions = versions;
        let layered = gpu.map(|e| e.and_then(|e| e.gpu.as_ref()).map(|g| g.layered));
        // The color kind follows the base color map (else the emissive map), the data kind
        // the first data map present.
        let color = layered[SLOT_BASE].or(layered[SLOT_EMISSIVE]).unwrap_or(false);
        let data = DATA_SLOTS.filter_map(|s| layered[s]).next().unwrap_or(false);
        let mut available = 0u8;
        let mut srgb = 0u8;
        let mut mismatch = false;
        for (slot, l) in layered.iter().enumerate() {
            let Some(l) = *l else { continue };
            let kind = if DATA_SLOTS.contains(&slot) { data } else { color };
            if l != kind {
                mismatch = true;
                continue;
            }
            available |= 1 << slot;
            let format = gpu[slot].and_then(|e| e.gpu.as_ref()).map(|g| g.format);
            if DATA_SLOTS.contains(&slot) && format.is_some_and(|f| f.is_srgb()) {
                srgb |= 1 << slot;
            }
        }
        if mismatch && !set.warned {
            set.warned = true;
            tracing::warn!(
                maps = ?set.maps,
                "a material mixes texture arrays and single-layer textures within its color maps \
                 (base color, emissive) or its data maps (normal, metallic-roughness, occlusion); \
                 the maps of the other kind are not sampled"
            );
        }
        if srgb != 0 && set.srgb != srgb {
            tracing::warn!(
                maps = ?set.maps,
                "a normal, metallic-roughness or occlusion map is an sRGB texture; these are \
                 linear data (upload them with `add_image_linear`): the sample is re-encoded, \
                 losing precision"
            );
        }
        let changed = !set.resolved || (available, srgb) != (set.available, set.srgb);
        set.resolved = true;
        set.kinds = if color { KIND_COLOR_LAYERED } else { 0 } | if data { KIND_DATA_LAYERED } else { 0 };
        set.available = available;
        set.srgb = srgb;
        if !changed {
            return;
        }
        let set = &self.sets[id as usize];
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.live && e.set == id {
                let flags = set.flags(e.wanted);
                if flags != e.gpu.flags[0] {
                    e.gpu.flags[0] = flags;
                    buf.write_at(i * stride, bytemuck::bytes_of(&e.gpu), cpu_reallocs);
                }
            }
        }
    }

    /// Whether views draw the instances of `s` back to front (`TransparentSort::PerInstance`:
    /// ECS batches of alpha-blended or premultiplied materials up to the size limit).
    pub(super) fn sorts(&self, s: &Source) -> bool {
        match self.transparent_sort {
            TransparentSort::PerInstance { max_instances } => {
                s.blend
                    && s.buffer == SourceBuffer::Ecs
                    && s.count <= max_instances
                    && matches!(
                        BlendMode::from_class(s.class),
                        BlendMode::AlphaBlend | BlendMode::Premultiplied
                    )
            }
            _ => false,
        }
    }

    /// The instances of an ECS batch (`order`, those with their `mask` bit when given) sorted
    /// far to near by the view-space depth of their translation (`view`: world → view);
    /// equal depths keep batch order. Indices into the ECS matrices.
    pub(super) fn sort_back_to_front(
        &mut self,
        order: &[u32],
        mask: Option<&[u64]>,
        matrices: &[[f32; 16]],
        view: glam::Mat4,
        cpu_reallocs: &mut u32,
    ) -> &[u32] {
        let caps = (self.sort_keys.capacity(), self.sort_order.capacity());
        let row = view.row(2);
        self.sort_keys.clear();
        for (k, &i) in order.iter().enumerate() {
            if mask.is_some_and(|m| m[k / 64] >> (k % 64) & 1 == 0) {
                continue;
            }
            let m = &matrices[i as usize];
            let z = row.dot(glam::Vec4::new(m[12], m[13], m[14], 1.0));
            // Order-preserving f32 → u32: ascending view z = far (most negative) first.
            let bits = z.to_bits();
            let key = if bits >> 31 == 1 { !bits } else { bits | 0x8000_0000 };
            self.sort_keys.push((key as u64) << 32 | k as u64);
        }
        self.sort_keys.sort_unstable();
        self.sort_order.clear();
        self.sort_order
            .extend(self.sort_keys.iter().map(|&k| order[(k & 0xFFFF_FFFF) as usize]));
        if caps != (self.sort_keys.capacity(), self.sort_order.capacity()) {
            *cpu_reallocs += 1;
        }
        &self.sort_order
    }

    /// The pipeline class of a source of `material` drawn with an instance layout: the
    /// extension stream is dropped for layouts without room for it (warned once per layout).
    fn class_for_layout(&mut self, class: u8, layout_id: u16, layout: &crate::instance::InstanceLayout) -> u8 {
        if class & CLASS_EXT == 0 || crate::pipeline::ext_fits(layout) {
            return class;
        }
        if !self.ext_warned.contains(&layout_id) {
            self.ext_warned.push(layout_id);
            tracing::warn!(
                layout = layout.name,
                "instance layout uses more than 10 vertex locations: normal maps on it use \
                 per-pixel tangents and second-UV-set maps sample UV0"
            );
        }
        class & !CLASS_EXT
    }

    /// Create the default map textures once (the 2-layer white array), and the extension
    /// stream bound this frame: the arena's, or a zero-filled buffer of its capacity while no
    /// mesh has extension data but an ext pipeline draws.
    fn prepare_gpu(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        assets: &RenderAssets,
        ext_used: bool,
        up: &mut Uploader,
    ) {
        if self.white_array.is_none() {
            self.white_array = Some(white_array(device, encoder));
        }
        self.ext_buffer = match &assets.arena.ext {
            Some((b, _)) => {
                self.zero_ext = None;
                Some(b.clone())
            }
            None if ext_used => {
                let capacity = assets.arena.vertices.capacity().max(1);
                if self.zero_ext.as_ref().is_none_or(|(_, c)| *c != capacity) {
                    let b = device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("space3d empty extension vertices"),
                        size: capacity as u64 * crate::arena::EXT_VERTEX_SIZE,
                        usage: wgpu::BufferUsages::VERTEX,
                        mapped_at_creation: false,
                    });
                    up.reallocations += 1;
                    self.zero_ext = Some((b, capacity));
                }
                self.zero_ext.as_ref().map(|(b, _)| b.clone())
            }
            None => None,
        };
    }
}

/// The 2-layer white `Rgba8Unorm` array default of array-kind maps (copied from a small
/// staging buffer: rows of 256 bytes).
fn white_array(device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d white array"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 2,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("space3d white array staging"),
        size: 512,
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    // (Only a lost device leaves a new buffer unmapped: the copy is then invalid anyway.)
    if let Ok(mut bytes) = staging.get_mapped_range_mut(..) {
        for layer in 0..2 {
            crate::gpu::copy_to_staging(bytes.slice(layer * 256..layer * 256 + 4), &[255; 4]);
        }
    }
    staging.unmap();
    encoder.copy_buffer_to_texture(
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(1),
            },
        },
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 2,
        },
    );
    texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    })
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

/// The draw-order rank of a source beyond its class and layout: the render order of
/// transparent batches (bits 48..63, `Material::render_order` + 32768), the custom shader
/// (slot + 1, 0 for none; bits 2..) and the texture kinds of the material's maps (bits 0..1,
/// the texture-array pipeline variants).
pub(super) fn pipeline_rank(materials: &MaterialTable, class: u8, material: u32) -> u64 {
    let shader = materials.shader(material).map_or(0, |h| h.index() as u64 + 1);
    let kinds = materials
        .sets
        .get(materials.set_of(material) as usize)
        .map_or(0, |s| s.kinds);
    let order = if class & CLASS_BLEND != 0 {
        (materials.render_order(material) as i32 + 32768) as u64
    } else {
        0
    };
    order << 48 | shader << 2 | kinds as u64
}

/// Pipeline class bits of a material (`PipelineKey::class`): double-sided, transparent, the
/// blend mode (`BlendMode::class_bits`) and the extension vertex stream (a normal map, for
/// vertex tangents, or maps on the second UV set).
pub(crate) fn material_class(m: &Material) -> u8 {
    let blend = m.effective_blend();
    let mut class = blend.class_bits();
    if m.double_sided {
        class |= CLASS_DOUBLE_SIDED;
    }
    if blend.is_transparent() {
        class |= CLASS_BLEND;
    }
    if m.normal_texture.is_some() || m.uv1_maps != MapMask::NONE {
        class |= CLASS_EXT;
    }
    class
}

/// Draw order of batches (graphics spec §1.7): phase (opaque, then alpha-masked, then
/// transparent), the transparent render order (`Material::render_order`), alpha-blended and
/// premultiplied before additive, then pipeline (class, instance layout, shader/texture kinds),
/// then material, then mesh — so each pipeline is bound once per view and each material once
/// per pipeline, and multi-draw runs (equal pipeline and material) are contiguous whatever the
/// spawn order. Buffer and first instance make the order total. With every render order 0 and
/// no masks or additive materials, this is the order before blend modes existed.
#[allow(clippy::type_complexity)]
pub(super) fn source_sort_key(s: &Source) -> (u8, u16, bool, u8, u16, u64, u32, u32, SourceBuffer, u32) {
    let mode = BlendMode::from_class(s.class);
    let phase = if s.blend {
        2
    } else if mode == BlendMode::AlphaMask {
        1
    } else {
        0
    };
    (
        phase,
        (s.pipeline >> 48) as u16,
        mode == BlendMode::Additive,
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
        lighting: &LightingSettings,
        frame_index: u64,
        stats: &mut RenderStats,
        reallocs: &mut u32,
    ) {
        let f = self;
        let mut cpu_reallocs = 0u32;
        let material_stride = f.material_stride();
        f.materials.globals = lighting_globals(lighting);
        f.materials.transparent_sort = lighting.transparent_sort;
        // ---- ECS batching ------------------------------------------------------------
        // (Unused texture sets go with their materials, and with them their bind groups, which
        // keep GPU textures alive.)
        f.materials.gc(frame_index);
        if frame_index.is_multiple_of(256) {
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
            let class = f.materials.class_for_layout(class, layout_id, layout);
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
        // The texture sets drawn this frame, against the uploaded textures (texture kinds, map
        // flags), and the frame's default maps and extension stream.
        let mut ext_used = false;
        for s in &f.sources {
            f.materials.resolve_set(
                s.material,
                assets,
                frame_index,
                &mut f.materials_buf,
                material_stride,
                &mut cpu_reallocs,
            );
            ext_used |= s.class & CLASS_EXT != 0;
        }
        f.materials.prepare_gpu(device, encoder, assets, ext_used, up);
        for s in &mut f.sources {
            s.pipeline = pipeline_rank(&f.materials, s.class, s.material);
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
    let Some(e) = materials.entries.get(material as usize) else {
        return false;
    };
    match e.key.shadows {
        ShadowMode::CastAndReceive | ShadowMode::CastOnly => true,
        ShadowMode::ReceiveOnly | ShadowMode::None => false,
        // Auto: lit opaque and alpha-masked materials (unlit ones, like sky quads, never cast).
        _ => e.key.flags & 1 == 0 && !BlendMode::from_class(e.class).is_transparent(),
    }
}

#[cfg(test)]
mod material_tests {
    use super::*;
    use crate::material::TextureTransform;
    use glam::{Vec2, Vec3, Vec4};

    fn handle(i: u32) -> TextureHandle {
        TextureHandle::from_raw(crate::handles::RawHandle {
            index: i,
            generation: 0,
        })
    }

    /// A default material writes the legacy uniform prefix and class: nothing else is set.
    #[test]
    fn material_defaults_are_legacy() {
        let m = Material::default();
        let g = gpu_material(&m, wanted_flags(&m));
        assert_eq!(g.base_color, [1.0; 4]);
        assert_eq!(g.emissive, [0.0; 4]);
        assert_eq!(g.params, [0.0, 0.5, 0.0, 0.0]);
        assert_eq!(g.flags, [0; 4]);
        assert_eq!((g.uv_row0, g.uv_row1), ([1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]));
        assert_eq!(material_class(&m), 0);
        let legacy = Material {
            alpha_blend: true,
            double_sided: true,
            ..Material::default()
        };
        let class = material_class(&legacy);
        assert_eq!(
            class & (CLASS_BLEND | CLASS_DOUBLE_SIDED),
            CLASS_BLEND | CLASS_DOUBLE_SIDED
        );
        assert_eq!(BlendMode::from_class(class), BlendMode::AlphaBlend);
        let t = handle(1);
        assert_ne!(
            material_class(&Material::default().with_normal_map(t, 1.0)) & CLASS_EXT,
            0
        );
        assert_ne!(
            material_class(&Material::default().with_uv1_maps(MapMask::EMISSIVE)) & CLASS_EXT,
            0
        );
        assert_eq!(material_class(&Material::default().with_orm_map(t)) & CLASS_EXT, 0);
    }

    /// Materials differing in any new field intern to different ids; equal ones to one.
    #[test]
    fn material_key_covers_new_fields() {
        let t = handle(3);
        let base = Material::default();
        let variants = [
            base.clone().with_normal_map(t, 1.0),
            base.clone().with_normal_map(t, 0.5),
            base.clone().with_metallic_roughness_map(t),
            base.clone().with_occlusion_map(t, 1.0),
            base.clone().with_occlusion_map(t, 0.3),
            base.clone().with_emissive_map(t, Vec3::ZERO),
            base.clone().with_blend(BlendMode::Additive),
            base.clone().with_alpha_mask(0.5),
            base.clone().with_alpha_mask(0.2),
            base.clone().with_uv_transform(TextureTransform {
                offset: Vec2::new(0.5, 0.0),
                ..TextureTransform::IDENTITY
            }),
            base.clone().with_uv1_maps(MapMask::NORMAL),
            base.clone().with_shadows(ShadowMode::None),
            base.clone().with_render_order(2),
        ];
        let mut keys = vec![MaterialKey::new(&base)];
        for v in &variants {
            let k = MaterialKey::new(v);
            assert!(!keys.contains(&k), "{v:?}");
            keys.push(k);
        }
        let mut table = MaterialTable::default();
        let mut buf = TrackedBuffer::new("test", wgpu::BufferUsages::UNIFORM);
        let mut r = 0;
        let ids: Vec<u32> = variants
            .iter()
            .map(|m| table.intern(m, 1, &mut buf, 256, &mut r).0)
            .collect();
        let again: Vec<u32> = variants
            .iter()
            .map(|m| table.intern(&m.clone(), 1, &mut buf, 256, &mut r).0)
            .collect();
        assert_eq!(ids, again);
        // One texture set per distinct map combination: the normal-map variants share one.
        assert_eq!(table.set_of(ids[0]), table.set_of(ids[1]));
        assert_ne!(table.set_of(ids[0]), table.set_of(ids[2]));
    }

    /// The uniform's map flags are what the set can bind: unavailable maps are dropped, ORM
    /// packing needs both slots, sRGB data maps get fixups.
    #[test]
    fn set_flags_follow_availability() {
        let mut set = TextureSetEntry::new([None; 5]);
        let wanted = FLAG_NORMAL | FLAG_METALLIC_ROUGHNESS | FLAG_OCCLUSION | FLAG_ORM_PACKED | FLAG_UV_TRANSFORM;
        assert_eq!(set.flags(wanted), FLAG_UV_TRANSFORM, "nothing uploaded yet");
        set.available = 0b0_1110;
        assert_eq!(set.flags(wanted), wanted);
        set.available = 0b0_0110;
        assert_eq!(
            set.flags(wanted),
            FLAG_NORMAL | FLAG_METALLIC_ROUGHNESS | FLAG_UV_TRANSFORM,
            "no occlusion: no ORM"
        );
        set.available = 0b1_1110;
        set.srgb = 0b0_0010;
        assert_eq!(
            set.flags(FLAG_NORMAL | FLAG_EMISSIVE),
            FLAG_NORMAL | FLAG_EMISSIVE | 1 << (FLAG_FIXUP_SHIFT + 1)
        );
    }

    /// Draw order: opaque, then alpha-masked, then transparent by render order, alpha before
    /// additive at equal order.
    #[test]
    fn class_bits_and_sort_key() {
        let mut table = MaterialTable::default();
        let mut buf = TrackedBuffer::new("test", wgpu::BufferUsages::UNIFORM);
        let mut r = 0;
        let materials = [
            ("additive", Material::additive(Vec4::ONE)),
            (
                "late alpha",
                Material::color(Vec4::splat(0.5))
                    .with_blend(BlendMode::AlphaBlend)
                    .with_render_order(1),
            ),
            (
                "alpha",
                Material::color(Vec4::splat(0.5)).with_blend(BlendMode::AlphaBlend),
            ),
            ("early additive", Material::additive(Vec4::ONE).with_render_order(-3)),
            ("mask", Material::default().with_alpha_mask(0.5)),
            ("opaque", Material::default()),
        ];
        let mut sources: Vec<(Source, &str)> = materials
            .iter()
            .map(|(name, m)| {
                let (id, class) = table.intern(m, 1, &mut buf, 256, &mut r);
                let s = Source {
                    blend: class & CLASS_BLEND != 0,
                    class,
                    layout: 0,
                    pipeline: pipeline_rank(&table, class, id),
                    material: id,
                    mesh: GpuMesh {
                        alloc: crate::arena::ArenaAlloc {
                            base_vertex: 0,
                            vertex_count: 4,
                            first_index: 0,
                            index_count: 6,
                        },
                        sphere: crate::culling::Sphere {
                            center: Vec3::ZERO,
                            radius: 1.0,
                        },
                    },
                    triangles: 2,
                    buffer: SourceBuffer::Ecs,
                    first: 0,
                    count: 1,
                    stride: 64,
                    cull: CullSource::Matrix { offset: 0 },
                };
                (s, *name)
            })
            .collect();
        sources.sort_by_key(|(s, _)| source_sort_key(s));
        let order: Vec<&str> = sources.iter().map(|(_, n)| *n).collect();
        assert_eq!(
            order,
            ["opaque", "mask", "early additive", "alpha", "additive", "late alpha"]
        );
    }
}
