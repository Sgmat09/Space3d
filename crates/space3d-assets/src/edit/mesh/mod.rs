//! Mesh editing operations on [`MeshData`] (indexed triangle lists), OBJ/glTF export and
//! [`MeshEditHistory`].
//!
//! Attribute conventions: `normals`, `uvs` and `colors` are used when their length equals
//! `positions.len()`; otherwise they are treated as absent. Operations keep present
//! attributes in sync with positions.

mod bounds;
mod export;
mod normals;
mod paint;
pub(crate) mod tangents;
mod topology;
mod transform;
mod uv;

pub use bounds::{Aabb, BoundingSphere, bounding_box, bounding_sphere};
pub use export::{to_gltf, to_obj};
pub use normals::{compute_tangents, recalculate_flat_normals, recalculate_smooth_normals};
pub use paint::{fill_vertex_colors, paint_vertex_colors};
pub use tangents::compute_mikktspace_tangents;
pub use topology::{
    WeldOptions, append, decimate_to_grid, decimate_vertex_clustering, extrude_faces, flip_normals, flip_winding,
    merge, remove_degenerate_triangles, remove_unused_vertices, subdivide, weld_vertices,
};
pub use transform::{center_on_origin, rotate, rotate_around, scale, transform, translate};
pub use uv::{normalize_uvs, uv_box_projection, uv_planar_projection};

use glam::Vec3;
use space3d_core::MeshData;

/// Undo/redo history for mesh editing (full snapshots with a memory cap).
///
/// ```
/// use space3d_assets::edit::mesh::{MeshEditHistory, subdivide};
/// use space3d_core::MeshData;
/// let mut h = MeshEditHistory::new(MeshData::cube(1.0));
/// h.apply("subdivide", subdivide);
/// assert_eq!(h.current().triangle_count(), 48);
/// h.undo();
/// assert_eq!(h.current().triangle_count(), 12);
/// ```
pub type MeshEditHistory = crate::edit::SnapshotHistory<MeshData>;

pub(crate) fn has_normals(m: &MeshData) -> bool {
    !m.positions.is_empty() && m.normals.len() == m.positions.len()
}
pub(crate) fn has_uvs(m: &MeshData) -> bool {
    !m.positions.is_empty() && m.uvs.len() == m.positions.len()
}
pub(crate) fn has_colors(m: &MeshData) -> bool {
    !m.positions.is_empty() && m.colors.len() == m.positions.len()
}

/// Unnormalized face normal (length = 2 × area) of triangle `t`.
pub(crate) fn face_cross(m: &MeshData, t: usize) -> Vec3 {
    let i = &m.indices[t * 3..t * 3 + 3];
    let a = Vec3::from(m.positions[i[0] as usize]);
    let b = Vec3::from(m.positions[i[1] as usize]);
    let c = Vec3::from(m.positions[i[2] as usize]);
    (b - a).cross(c - a)
}

/// Append a copy of vertex `v` (all present attributes) and return its index.
pub(crate) fn push_vertex_copy(m: &mut MeshData, v: usize, flags: (bool, bool, bool)) -> u32 {
    let idx = m.positions.len() as u32;
    m.positions.push(m.positions[v]);
    if flags.0 {
        m.normals.push(m.normals[v]);
    }
    if flags.1 {
        m.uvs.push(m.uvs[v]);
    }
    if flags.2 {
        m.colors.push(m.colors[v]);
    }
    idx
}

/// Map each vertex to a canonical id shared by all vertices at (nearly) the same position:
/// within `1e-6 ×` the mesh extent, which absorbs float noise at UV seams and poles.
pub(crate) fn position_ids(m: &MeshData) -> Vec<u32> {
    if m.positions.is_empty() {
        return Vec::new();
    }
    let (min, max) = m.aabb();
    let extent = (max - min).max_element();
    let eps = if extent.is_finite() {
        (extent * 1e-6).max(f32::MIN_POSITIVE)
    } else {
        f32::MIN_POSITIVE
    };
    let cell = eps as f64;
    let key = |p: [f32; 3]| p.map(|v| (v as f64 / cell).floor() as i64);
    let mut grid: std::collections::HashMap<[i64; 3], Vec<(u32, usize)>> = std::collections::HashMap::new();
    let mut ids = Vec::with_capacity(m.positions.len());
    let mut next = 0u32;
    for (i, &p) in m.positions.iter().enumerate() {
        let k = key(p);
        let mut found = None;
        'search: for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    if let Some(list) = grid.get(&[k[0] + dx, k[1] + dy, k[2] + dz]) {
                        for &(id, j) in list {
                            if Vec3::from(p).distance_squared(Vec3::from(m.positions[j])) <= eps * eps {
                                found = Some(id);
                                break 'search;
                            }
                        }
                    }
                }
            }
        }
        let id = found.unwrap_or_else(|| {
            let id = next;
            next += 1;
            grid.entry(k).or_default().push((id, i));
            id
        });
        ids.push(id);
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_roundtrip() {
        let mut h = MeshEditHistory::new(MeshData::cube(2.0));
        h.apply("move", |m| translate(m, Vec3::X));
        h.apply("scale", |m| scale(m, Vec3::splat(2.0)));
        assert_eq!(h.current().aabb().1, Vec3::new(4.0, 2.0, 2.0));
        assert!(h.undo());
        assert_eq!(h.current().aabb().1, Vec3::new(2.0, 1.0, 1.0));
        assert!(h.undo());
        assert_eq!(h.current(), &MeshData::cube(2.0));
        assert!(!h.undo());
        assert!(h.redo());
        assert_eq!(h.redo_label(), Some("scale"));
        assert!(h.redo());
        assert_eq!(h.current().aabb().1, Vec3::new(4.0, 2.0, 2.0));
        assert_eq!(h.undo_len(), 2);
        let mut capped = MeshEditHistory::new(MeshData::cube(1.0)).with_max_steps(1);
        capped.apply("a", subdivide);
        capped.apply("b", subdivide);
        assert_eq!(capped.undo_len(), 1);
    }
}
