//! Mesh arena: every mesh lives in one shared vertex buffer and one shared index buffer.
//!
//! Batches never rebind vertex/index buffers — a draw selects its mesh purely through
//! `base_vertex` / `first_index`, which is also what makes multi-draw-indirect possible.
//! Sub-allocation is first-fit over a sorted free list with coalescing. When a buffer is full
//! it grows (2x, never beyond the device's `max_buffer_size`) with a GPU→GPU copy of the old
//! contents; mesh data is then written through the staging ring like any other upload. Vertex
//! and index buffers grow independently.
//!
//! The new buffers are deliberately *not* `mapped_at_creation`: they lack `MAP_WRITE`, so wgpu
//! would back the mapping with a hidden staging buffer of the full new size (zero-filled on the
//! CPU and copied whole on unmap), and on WebGPU it copies the whole mapped range into wasm
//! memory and back. Staging only the written meshes costs a fraction of that.

use crate::gpu::Uploader;
use bytemuck::{Pod, Zeroable};
use space3d_core::MeshData;
use std::ops::Range;

/// Interleaved vertex stored in the arena (36 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub(crate) struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    /// sRGB-encoded RGBA8 vertex color (decoded to linear in the vertex shader).
    pub color: [u8; 4],
}

pub(crate) const VERTEX_SIZE: u64 = std::mem::size_of::<Vertex>() as u64;
pub(crate) const INDEX_SIZE: u64 = 4;

pub(crate) const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
    0 => Float32x3,
    1 => Float32x3,
    2 => Float32x2,
    3 => Unorm8x4,
];

/// Convert CPU mesh data into arena vertices + indices.
pub(crate) fn build_vertices(mesh: &MeshData, out_v: &mut Vec<Vertex>, out_i: &mut Vec<u32>) {
    out_v.clear();
    out_i.clear();
    out_v.reserve(mesh.positions.len());
    for (i, p) in mesh.positions.iter().enumerate() {
        let color = mesh.colors.get(i).map_or([255; 4], |c| {
            [
                (crate::components::linear_to_srgb(c[0].clamp(0.0, 1.0)) * 255.0).round() as u8,
                (crate::components::linear_to_srgb(c[1].clamp(0.0, 1.0)) * 255.0).round() as u8,
                (crate::components::linear_to_srgb(c[2].clamp(0.0, 1.0)) * 255.0).round() as u8,
                (c[3].clamp(0.0, 1.0) * 255.0).round() as u8,
            ]
        });
        out_v.push(Vertex {
            position: *p,
            normal: mesh.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
            uv: mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
            color,
        });
    }
    if mesh.indices.is_empty() {
        out_i.extend(0..mesh.positions.len() as u32);
    } else {
        out_i.extend_from_slice(&mesh.indices);
    }
}

/// First-fit range allocator over `0..capacity` (element units).
#[derive(Default, Debug)]
pub(crate) struct RangeAllocator {
    free: Vec<Range<u32>>,
    capacity: u32,
}

impl RangeAllocator {
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    pub fn alloc(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        let i = self.free.iter().position(|r| r.end - r.start >= n)?;
        let start = self.free[i].start;
        self.free[i].start += n;
        if self.free[i].is_empty() {
            self.free.remove(i);
        }
        Some(start)
    }

    pub fn free(&mut self, range: Range<u32>) {
        if range.is_empty() {
            return;
        }
        let i = self.free.partition_point(|r| r.start < range.start);
        self.free.insert(i, range);
        // Coalesce with neighbours.
        if i + 1 < self.free.len() && self.free[i].end == self.free[i + 1].start {
            self.free[i].end = self.free[i + 1].end;
            self.free.remove(i + 1);
        }
        if i > 0 && self.free[i - 1].end == self.free[i].start {
            self.free[i - 1].end = self.free[i].end;
            self.free.remove(i);
        }
    }

    pub fn grow(&mut self, new_capacity: u32) {
        let old = self.capacity;
        if new_capacity > old {
            self.capacity = new_capacity;
            self.free(old..new_capacity);
        }
    }

    /// Elements currently allocated.
    pub fn used(&self) -> u32 {
        self.capacity - self.free.iter().map(|r| r.end - r.start).sum::<u32>()
    }
}

/// Where a mesh lives inside the arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArenaAlloc {
    pub base_vertex: u32,
    pub vertex_count: u32,
    pub first_index: u32,
    pub index_count: u32,
}

/// A pending write into the arena.
pub(crate) struct ArenaWrite<'a> {
    pub alloc: ArenaAlloc,
    pub vertices: &'a [Vertex],
    pub indices: &'a [u32],
}

/// Shared vertex + index buffers.
#[derive(Default)]
pub(crate) struct MeshArena {
    pub vertex: Option<wgpu::Buffer>,
    pub index: Option<wgpu::Buffer>,
    pub vertices: RangeAllocator,
    pub indices: RangeAllocator,
    /// Bumped when buffers are replaced.
    pub generation: u32,
}

/// Which buffers of the arena lacked space for an allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Short {
    pub vertices: bool,
    pub indices: bool,
}

/// Grow `a` so that `extra` more elements fit in its fresh tail (2x amortized, at least
/// `min`), clamped to `max` elements. Allocators without capacity get `min` even for 0.
fn grow_side(a: &mut RangeAllocator, extra: u32, min: u32, max: u32) {
    let old = a.capacity();
    if extra == 0 && old > 0 {
        return;
    }
    let want = (old as u64 + extra as u64)
        .max(old as u64 * 2)
        .max(min as u64)
        .next_multiple_of(64);
    a.grow(want.min(max as u64) as u32);
}

impl MeshArena {
    /// Allocate space; `Err` says which buffer(s) must grow first.
    pub fn try_alloc(&mut self, vertex_count: u32, index_count: u32) -> Result<ArenaAlloc, Short> {
        let v = self.vertices.alloc(vertex_count);
        let i = self.indices.alloc(index_count);
        match (v, i) {
            (Some(base_vertex), Some(first_index)) => Ok(ArenaAlloc {
                base_vertex,
                vertex_count,
                first_index,
                index_count,
            }),
            (v, i) => {
                if let Some(v) = v {
                    self.vertices.free(v..v + vertex_count);
                }
                if let Some(i) = i {
                    self.indices.free(i..i + index_count);
                }
                Err(Short {
                    vertices: v.is_none(),
                    indices: i.is_none(),
                })
            }
        }
    }

    pub fn free(&mut self, a: ArenaAlloc) {
        self.vertices.free(a.base_vertex..a.base_vertex + a.vertex_count);
        self.indices.free(a.first_index..a.first_index + a.index_count);
    }

    /// Grow allocator capacities so `extra_*` more elements fit (2x amortized). Only a side
    /// with a non-zero request grows (plus any side that has no capacity yet), and neither
    /// grows beyond `max_buffer_size` bytes — a request that does not fit even then is left
    /// short, and the caller's next allocation attempt fails. CPU-only; follow with
    /// [`Self::realloc_gpu`]. Returns the previous capacities.
    pub fn grow_capacity(&mut self, extra_vertices: u32, extra_indices: u32, max_buffer_size: u64) -> (u32, u32) {
        let old = (self.vertices.capacity(), self.indices.capacity());
        let (max_v, max_i) = Self::max_capacity(max_buffer_size);
        grow_side(&mut self.vertices, extra_vertices, 4096, max_v);
        grow_side(&mut self.indices, extra_indices, 8192, max_i);
        old
    }

    /// Largest vertex and index capacities whose buffers stay within `max_buffer_size`.
    pub fn max_capacity(max_buffer_size: u64) -> (u32, u32) {
        let max = |size: u64| ((max_buffer_size / size).min(u32::MAX as u64) as u32) & !63;
        (max(VERTEX_SIZE), max(INDEX_SIZE))
    }

    /// Replace the GPU buffer(s) whose allocator capacity changed since `old_*` (or that do
    /// not exist yet), copying the old contents GPU→GPU. Record this before the frame's
    /// [`Self::write`]s so they land after the copy.
    pub fn realloc_gpu(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        (old_v, old_i): (u32, u32),
        up: &mut Uploader,
    ) {
        let new_v = self.vertices.capacity();
        let new_i = self.indices.capacity();
        let grow_v = self.vertex.is_none() || new_v != old_v;
        let grow_i = self.index.is_none() || new_i != old_i;
        let make = |label, size: u64, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: usage | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        if grow_v {
            let vb = make(
                "space3d mesh arena vertices",
                new_v as u64 * VERTEX_SIZE,
                wgpu::BufferUsages::VERTEX,
            );
            if let Some(old) = &self.vertex
                && old_v > 0
            {
                encoder.copy_buffer_to_buffer(old, 0, &vb, 0, old_v as u64 * VERTEX_SIZE);
            }
            self.vertex = Some(vb);
            up.reallocations += 1;
        }
        if grow_i {
            let ib = make(
                "space3d mesh arena indices",
                new_i as u64 * INDEX_SIZE,
                wgpu::BufferUsages::INDEX,
            );
            if let Some(old) = &self.index
                && old_i > 0
            {
                encoder.copy_buffer_to_buffer(old, 0, &ib, 0, old_i as u64 * INDEX_SIZE);
            }
            self.index = Some(ib);
            up.reallocations += 1;
        }
        if grow_v || grow_i {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Upload the vertices and indices of `items` (`get` yields an item's write, `None` for
    /// items without a placement) through the staging ring. Meshes whose ranges follow each
    /// other — the usual case, since new meshes fill free space first-fit in order — share one
    /// staging copy per buffer instead of costing two copies each.
    pub fn write_all<T>(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        items: &[T],
        get: impl for<'x> Fn(&'x T) -> Option<ArenaWrite<'x>>,
        up: &mut Uploader,
    ) {
        if let Some(vb) = &self.vertex {
            write_runs(encoder, vb, items, &get, Side::Vertices, up);
        }
        if let Some(ib) = &self.index {
            write_runs(encoder, ib, items, &get, Side::Indices, up);
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Vertices,
    Indices,
}

impl Side {
    /// Byte offset in the side's buffer and the bytes of `w` for this side.
    fn of<'x>(self, w: &ArenaWrite<'x>) -> (u64, &'x [u8]) {
        match self {
            Side::Vertices => (
                w.alloc.base_vertex as u64 * VERTEX_SIZE,
                bytemuck::cast_slice(w.vertices),
            ),
            Side::Indices => (w.alloc.first_index as u64 * INDEX_SIZE, bytemuck::cast_slice(w.indices)),
        }
    }
}

/// One staging write per run of back-to-back ranges of `side` (see [`MeshArena::write_all`]).
fn write_runs<T>(
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::Buffer,
    items: &[T],
    get: &impl for<'x> Fn(&'x T) -> Option<ArenaWrite<'x>>,
    side: Side,
    up: &mut Uploader,
) {
    let span = |t: &T| {
        get(t)
            .map(|w| side.of(&w))
            .filter(|(_, bytes)| !bytes.is_empty())
            .map(|(at, bytes)| (at, bytes.len() as u64))
    };
    let mut i = 0;
    while i < items.len() {
        let Some((start, len)) = span(&items[i]) else {
            i += 1;
            continue;
        };
        let mut end = start + len;
        let mut j = i + 1;
        while let Some((at, len)) = items.get(j).and_then(span) {
            if at != end {
                break;
            }
            end += len;
            j += 1;
        }
        let parts = items[i..j].iter().filter_map(get).map(|w| side.of(&w).1);
        up.write_parts(encoder, target, start, (end - start) as usize, parts);
        i = j;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_first_fit_and_coalescing() {
        let mut a = RangeAllocator::default();
        a.grow(100);
        let x = a.alloc(10).unwrap();
        let y = a.alloc(20).unwrap();
        let z = a.alloc(30).unwrap();
        assert_eq!((x, y, z), (0, 10, 30));
        a.free(10..30);
        assert_eq!(a.alloc(5), Some(10), "reuses the hole");
        a.free(10..15);
        a.free(0..10);
        a.free(30..60);
        assert_eq!(a.used(), 0);
        assert_eq!(a.alloc(100), Some(0), "fully coalesced");
        assert_eq!(a.alloc(1), None);
        a.grow(150);
        assert_eq!(a.alloc(50), Some(100));
    }

    #[test]
    fn growth_is_per_buffer_and_clamped_to_the_device_limit() {
        let mut a = MeshArena::default();
        let max = 36 * 16_384; // 16384 vertices / 147456 indices
        assert_eq!(a.grow_capacity(0, 0, max), (0, 0));
        assert_eq!(
            (a.vertices.capacity(), a.indices.capacity()),
            (4096, 8192),
            "initial sizes"
        );
        // Only the index buffer is short: the vertex buffer keeps its capacity.
        let x = a.try_alloc(100, 9000).unwrap_err();
        assert_eq!((x.vertices, x.indices), (false, true));
        a.grow_capacity(0, 9000, max);
        assert_eq!(a.vertices.capacity(), 4096);
        assert_eq!(a.indices.capacity(), 17_216);
        assert!(a.try_alloc(100, 9000).is_ok());
        // Doubling stops at the limit instead of producing an oversized buffer.
        a.grow_capacity(8000, 0, max);
        assert_eq!(a.vertices.capacity(), 12_096);
        a.grow_capacity(8000, 0, max);
        assert_eq!(a.vertices.capacity(), 16_384);
        assert!(a.vertices.capacity() as u64 * VERTEX_SIZE <= max);
        a.grow_capacity(8000, 0, max);
        assert_eq!(a.vertices.capacity(), 16_384, "cannot grow further");
        assert_eq!(
            a.try_alloc(20_000, 0).unwrap_err(),
            Short {
                vertices: true,
                indices: false
            }
        );
    }

    #[test]
    fn vertex_conversion_defaults() {
        let mut m = MeshData::cube(1.0);
        m.normals.clear();
        m.indices.clear();
        let (mut v, mut i) = (Vec::new(), Vec::new());
        build_vertices(&m, &mut v, &mut i);
        assert_eq!(v.len(), 24);
        assert_eq!(i.len(), 24);
        assert_eq!(v[0].normal, [0.0, 1.0, 0.0]);
        assert_eq!(v[0].color, [255; 4]);
        assert_eq!(std::mem::size_of::<Vertex>(), 36);
    }
}
