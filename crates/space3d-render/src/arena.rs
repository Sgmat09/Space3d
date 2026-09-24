//! Mesh arena: every mesh lives in one shared vertex buffer and one shared index buffer.
//!
//! Batches never rebind vertex/index buffers — a draw selects its mesh purely through
//! `base_vertex` / `first_index`, which is also what makes multi-draw-indirect possible.
//! Sub-allocation is first-fit over a sorted free list with coalescing. When the arena is
//! full it grows (2x) with a GPU→GPU copy of the old contents; data for the new region is
//! written straight into the new buffer's `mapped_at_creation` memory.

use crate::gpu::{Uploader, copy_to_staging};
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

impl MeshArena {
    /// Allocate space; `None` if the arena must grow first.
    pub fn alloc(&mut self, vertex_count: u32, index_count: u32) -> Option<ArenaAlloc> {
        let v = self.vertices.alloc(vertex_count);
        let i = self.indices.alloc(index_count);
        match (v, i) {
            (Some(base_vertex), Some(first_index)) => Some(ArenaAlloc {
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
                None
            }
        }
    }

    pub fn free(&mut self, a: ArenaAlloc) {
        self.vertices.free(a.base_vertex..a.base_vertex + a.vertex_count);
        self.indices.free(a.first_index..a.first_index + a.index_count);
    }

    /// Grow allocator capacities so `extra_*` more elements fit (2x amortized). CPU-only;
    /// follow with [`Self::realloc_gpu`]. Returns the previous capacities.
    pub fn grow_capacity(&mut self, extra_vertices: u32, extra_indices: u32) -> (u32, u32) {
        let old_v = self.vertices.capacity();
        let old_i = self.indices.capacity();
        let need_v = self.vertices.used() + extra_vertices;
        let need_i = self.indices.used() + extra_indices;
        let new_v = need_v.max(old_v * 2).max(4096).next_multiple_of(64);
        let new_i = need_i.max(old_i * 2).max(8192).next_multiple_of(64);
        self.vertices.grow(new_v.max(old_v + extra_vertices));
        self.indices.grow(new_i.max(old_i + extra_indices));
        (old_v, old_i)
    }

    /// Replace the GPU buffers with ones matching the allocator capacities. Old contents are
    /// copied GPU→GPU; `writes` landing in the fresh region (at/after `old_*`) are written
    /// directly into `mapped_at_creation` memory (no staging copy).
    pub fn realloc_gpu(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        (old_v, old_i): (u32, u32),
        writes: &[ArenaWrite<'_>],
        up: &mut Uploader,
    ) {
        let new_v = self.vertices.capacity();
        let new_i = self.indices.capacity();
        let make = |label, size: u64, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: usage | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: true,
            })
        };
        let vb = make("space3d mesh arena vertices", new_v as u64 * VERTEX_SIZE, wgpu::BufferUsages::VERTEX);
        let ib = make("space3d mesh arena indices", new_i as u64 * INDEX_SIZE, wgpu::BufferUsages::INDEX);
        up.reallocations += 2;
        {
            let mut vmap = vb.get_mapped_range_mut(..).expect("arena map");
            let mut imap = ib.get_mapped_range_mut(..).expect("arena map");
            for w in writes {
                if w.alloc.base_vertex >= old_v && !w.vertices.is_empty() {
                    let o = (w.alloc.base_vertex as u64 * VERTEX_SIZE) as usize;
                    let bytes: &[u8] = bytemuck::cast_slice(w.vertices);
                    copy_to_staging(vmap.slice(o..o + bytes.len()), bytes);
                    up.bytes += bytes.len() as u64;
                }
                if w.alloc.first_index >= old_i && !w.indices.is_empty() {
                    let o = (w.alloc.first_index as u64 * INDEX_SIZE) as usize;
                    let bytes: &[u8] = bytemuck::cast_slice(w.indices);
                    copy_to_staging(imap.slice(o..o + bytes.len()), bytes);
                    up.bytes += bytes.len() as u64;
                }
            }
        }
        vb.unmap();
        ib.unmap();
        if let (Some(old_vb), Some(old_ib)) = (&self.vertex, &self.index) {
            if old_v > 0 {
                encoder.copy_buffer_to_buffer(old_vb, 0, &vb, 0, old_v as u64 * VERTEX_SIZE);
            }
            if old_i > 0 {
                encoder.copy_buffer_to_buffer(old_ib, 0, &ib, 0, old_i as u64 * INDEX_SIZE);
            }
        }
        self.vertex = Some(vb);
        self.index = Some(ib);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Upload writes that were not satisfied by a growth mapping (`fresh_*` = old capacities
    /// at the time of growth, or `u32::MAX` if no growth happened this frame) via staging.
    pub fn write(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        w: &ArenaWrite<'_>,
        fresh_v: u32,
        fresh_i: u32,
        up: &mut Uploader,
    ) {
        if w.alloc.base_vertex < fresh_v && !w.vertices.is_empty() {
            up.write(
                encoder,
                self.vertex.as_ref().expect("arena"),
                w.alloc.base_vertex as u64 * VERTEX_SIZE,
                bytemuck::cast_slice(w.vertices),
            );
        }
        if w.alloc.first_index < fresh_i && !w.indices.is_empty() {
            up.write(
                encoder,
                self.index.as_ref().expect("arena"),
                w.alloc.first_index as u64 * INDEX_SIZE,
                bytemuck::cast_slice(w.indices),
            );
        }
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
