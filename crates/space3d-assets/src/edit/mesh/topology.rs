//! Topology edits: merge, weld, winding, subdivision, extrusion, decimation.

use super::{face_cross, has_colors, has_normals, has_uvs, position_ids, push_vertex_copy, recalculate_smooth_normals};
use glam::{Vec2, Vec3, Vec4};
use space3d_core::MeshData;
use std::collections::{HashMap, HashSet};

/// Append `src` to `dst`, reconciling attributes: if only one side has normals the other
/// side gets smooth normals; missing UVs become `[0, 0]`; missing colors become white.
pub fn append(dst: &mut MeshData, src: &MeshData) {
    if src.positions.is_empty() {
        return;
    }
    if dst.positions.is_empty() {
        let base_indices = std::mem::take(&mut dst.indices);
        *dst = src.clone();
        debug_assert!(base_indices.is_empty(), "indices without positions");
        return;
    }
    let base = dst.positions.len() as u32;
    let n_dst = dst.positions.len();
    let n_src = src.positions.len();
    match (has_normals(dst), has_normals(src)) {
        (true, true) => dst.normals.extend_from_slice(&src.normals),
        (true, false) => {
            let mut tmp = src.clone();
            recalculate_smooth_normals(&mut tmp);
            dst.normals.extend(tmp.normals);
        }
        (false, true) => {
            recalculate_smooth_normals(dst);
            dst.normals.extend_from_slice(&src.normals);
        }
        (false, false) => dst.normals.clear(),
    }
    if has_uvs(dst) || has_uvs(src) {
        dst.uvs.resize(n_dst, [0.0; 2]);
        if has_uvs(src) {
            dst.uvs.extend_from_slice(&src.uvs);
        } else {
            dst.uvs.resize(n_dst + n_src, [0.0; 2]);
        }
    } else {
        dst.uvs.clear();
    }
    if has_colors(dst) || has_colors(src) {
        dst.colors.resize(n_dst, [1.0; 4]);
        if has_colors(src) {
            dst.colors.extend_from_slice(&src.colors);
        } else {
            dst.colors.resize(n_dst + n_src, [1.0; 4]);
        }
    } else {
        dst.colors.clear();
    }
    dst.positions.extend_from_slice(&src.positions);
    dst.indices.extend(src.indices.iter().map(|&i| i + base));
}

/// Combine several meshes into one (see [`append`] for attribute rules).
pub fn merge(meshes: &[MeshData]) -> MeshData {
    let mut out = MeshData::default();
    for m in meshes {
        append(&mut out, m);
    }
    out
}

/// Options for [`weld_vertices`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WeldOptions {
    /// Maximum distance between positions to merge.
    pub epsilon: f32,
    /// Only merge vertices whose normals also match. When `false`, merged normals are
    /// averaged.
    pub compare_normals: bool,
    /// Only merge vertices whose UVs also match.
    pub compare_uvs: bool,
    /// Only merge vertices whose colors also match.
    pub compare_colors: bool,
    /// Per-component tolerance for attribute comparisons.
    pub attribute_epsilon: f32,
}

impl Default for WeldOptions {
    fn default() -> Self {
        Self {
            epsilon: 1e-5,
            compare_normals: true,
            compare_uvs: true,
            compare_colors: true,
            attribute_epsilon: 1e-4,
        }
    }
}

impl WeldOptions {
    /// Merge purely by position (normals averaged, first UV/color kept).
    pub fn positions_only(epsilon: f32) -> Self {
        Self {
            epsilon,
            compare_normals: false,
            compare_uvs: false,
            compare_colors: false,
            ..Default::default()
        }
    }
}

fn close<const N: usize>(a: &[f32; N], b: &[f32; N], eps: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= eps)
}

/// Merge vertices closer than `epsilon` (and with matching attributes, per `options`),
/// drop triangles that collapse, and return how many vertices were removed.
pub fn weld_vertices(mesh: &mut MeshData, options: &WeldOptions) -> usize {
    let n = mesh.positions.len();
    if n == 0 {
        return 0;
    }
    let (hn, hu, hc) = (has_normals(mesh), has_uvs(mesh), has_colors(mesh));
    let eps = options.epsilon.max(0.0);
    let cell = eps as f64;
    // Grid cells of size `eps` (neighbours searched); exact bit keys when `eps == 0`.
    let key = |p: [f32; 3]| -> [i64; 3] {
        if eps > 0.0 {
            p.map(|v| (v as f64 / cell).floor() as i64)
        } else {
            p.map(|v| (v + 0.0).to_bits() as i64)
        }
    };
    let reach: i64 = if eps > 0.0 { 1 } else { 0 };
    let aeps = options.attribute_epsilon;

    let mut grid: HashMap<[i64; 3], Vec<u32>> = HashMap::new();
    let mut keep: Vec<usize> = Vec::new();
    let mut normal_acc: Vec<Vec3> = Vec::new();
    let mut remap = vec![0u32; n];
    for (i, slot) in remap.iter_mut().enumerate() {
        let p = mesh.positions[i];
        let k = key(p);
        let mut found = None;
        'search: for dx in -reach..=reach {
            for dy in -reach..=reach {
                for dz in -reach..=reach {
                    let Some(list) = grid.get(&[k[0] + dx, k[1] + dy, k[2] + dz]) else {
                        continue;
                    };
                    for &cand in list {
                        let j = keep[cand as usize];
                        let q = mesh.positions[j];
                        if (Vec3::from(p) - Vec3::from(q)).length_squared() > eps * eps {
                            continue;
                        }
                        if hn && options.compare_normals && !close(&mesh.normals[i], &mesh.normals[j], aeps) {
                            continue;
                        }
                        if hu && options.compare_uvs && !close(&mesh.uvs[i], &mesh.uvs[j], aeps) {
                            continue;
                        }
                        if hc && options.compare_colors && !close(&mesh.colors[i], &mesh.colors[j], aeps) {
                            continue;
                        }
                        found = Some(cand);
                        break 'search;
                    }
                }
            }
        }
        let normal = if hn { Vec3::from(mesh.normals[i]) } else { Vec3::ZERO };
        *slot = match found {
            Some(c) => {
                normal_acc[c as usize] += normal;
                c
            }
            None => {
                let c = keep.len() as u32;
                keep.push(i);
                normal_acc.push(normal);
                grid.entry(k).or_default().push(c);
                c
            }
        };
    }
    let removed = n - keep.len();
    mesh.positions = keep.iter().map(|&i| mesh.positions[i]).collect();
    if hn {
        mesh.normals = if options.compare_normals {
            keep.iter().map(|&i| mesh.normals[i]).collect()
        } else {
            normal_acc
                .iter()
                .map(|v| v.try_normalize().unwrap_or(Vec3::Y).into())
                .collect()
        };
    }
    if hu {
        mesh.uvs = keep.iter().map(|&i| mesh.uvs[i]).collect();
    }
    if hc {
        mesh.colors = keep.iter().map(|&i| mesh.colors[i]).collect();
    }
    for i in &mut mesh.indices {
        *i = remap[*i as usize];
    }
    remove_degenerate_triangles(mesh, 0.0);
    removed
}

/// Remove triangles that reuse a vertex index or whose area is `<= area_epsilon`.
/// Returns the number removed.
pub fn remove_degenerate_triangles(mesh: &mut MeshData, area_epsilon: f32) -> usize {
    let before = mesh.triangle_count();
    let mut out = Vec::with_capacity(mesh.indices.len());
    for t in 0..before {
        let tri = &mesh.indices[t * 3..t * 3 + 3];
        if tri[0] == tri[1] || tri[1] == tri[2] || tri[0] == tri[2] {
            continue;
        }
        if area_epsilon > 0.0 && face_cross(mesh, t).length() * 0.5 <= area_epsilon {
            continue;
        }
        out.extend_from_slice(tri);
    }
    mesh.indices = out;
    before - mesh.triangle_count()
}

/// Drop vertices no triangle references. Returns the number removed.
pub fn remove_unused_vertices(mesh: &mut MeshData) -> usize {
    let n = mesh.positions.len();
    let (hn, hu, hc) = (has_normals(mesh), has_uvs(mesh), has_colors(mesh));
    let mut remap = vec![u32::MAX; n];
    let mut order = Vec::new();
    for &i in &mesh.indices {
        if remap[i as usize] == u32::MAX {
            remap[i as usize] = order.len() as u32;
            order.push(i as usize);
        }
    }
    mesh.positions = order.iter().map(|&i| mesh.positions[i]).collect();
    if hn {
        mesh.normals = order.iter().map(|&i| mesh.normals[i]).collect();
    }
    if hu {
        mesh.uvs = order.iter().map(|&i| mesh.uvs[i]).collect();
    }
    if hc {
        mesh.colors = order.iter().map(|&i| mesh.colors[i]).collect();
    }
    for i in &mut mesh.indices {
        *i = remap[*i as usize];
    }
    n - order.len()
}

/// Reverse the winding order of every triangle (normals untouched; see [`flip_normals`]).
pub fn flip_winding(mesh: &mut MeshData) {
    for tri in mesh.indices.as_chunks_mut::<3>().0 {
        tri.swap(1, 2);
    }
}

/// Negate all normals.
pub fn flip_normals(mesh: &mut MeshData) {
    for n in &mut mesh.normals {
        *n = n.map(|v| -v);
    }
}

/// One level of midpoint subdivision: every triangle becomes four. Edge midpoints are
/// shared between neighbouring triangles; all attributes are interpolated (normals
/// renormalized).
pub fn subdivide(mesh: &mut MeshData) {
    let flags = (has_normals(mesh), has_uvs(mesh), has_colors(mesh));
    let mut mids: HashMap<(u32, u32), u32> = HashMap::new();
    let mut midpoint = |m: &mut MeshData, a: u32, b: u32| -> u32 {
        let key = (a.min(b), a.max(b));
        if let Some(&v) = mids.get(&key) {
            return v;
        }
        let (a, b) = (a as usize, b as usize);
        let v = m.positions.len() as u32;
        m.positions
            .push(((Vec3::from(m.positions[a]) + Vec3::from(m.positions[b])) * 0.5).into());
        if flags.0 {
            let n = (Vec3::from(m.normals[a]) + Vec3::from(m.normals[b]))
                .try_normalize()
                .unwrap_or(Vec3::from(m.normals[a]));
            m.normals.push(n.into());
        }
        if flags.1 {
            m.uvs.push(((Vec2::from(m.uvs[a]) + Vec2::from(m.uvs[b])) * 0.5).into());
        }
        if flags.2 {
            m.colors
                .push(((Vec4::from(m.colors[a]) + Vec4::from(m.colors[b])) * 0.5).into());
        }
        mids.insert(key, v);
        v
    };
    let old = std::mem::take(&mut mesh.indices);
    let mut indices = Vec::with_capacity(old.len() * 4);
    for tri in old.as_chunks::<3>().0 {
        let (a, b, c) = (tri[0], tri[1], tri[2]);
        let ab = midpoint(mesh, a, b);
        let bc = midpoint(mesh, b, c);
        let ca = midpoint(mesh, c, a);
        indices.extend([a, ab, ca, ab, b, bc, ca, bc, c, ab, bc, ca]);
    }
    mesh.indices = indices;
}

/// Extrude the triangles `faces` along their area-weighted average normal by `distance`.
///
/// The selected faces move (as copies with new vertices), and side walls with flat normals
/// connect the region's boundary edges to the moved cap. Boundary detection works on
/// positions, so selections spanning split vertices (UV/normal seams) extrude as one
/// region. Returns the indices of the newly added side triangles; the cap keeps the
/// original triangle indices.
pub fn extrude_faces(mesh: &mut MeshData, faces: &[usize], distance: f32) -> Vec<usize> {
    let tri_count = mesh.triangle_count();
    let mut selected: Vec<usize> = faces.iter().copied().filter(|&f| f < tri_count).collect();
    selected.sort_unstable();
    selected.dedup();
    if selected.is_empty() {
        return Vec::new();
    }
    let flags = (has_normals(mesh), has_uvs(mesh), has_colors(mesh));
    let dir = selected
        .iter()
        .map(|&f| face_cross(mesh, f))
        .sum::<Vec3>()
        .try_normalize()
        .or_else(|| face_cross(mesh, selected[0]).try_normalize())
        .unwrap_or(Vec3::Y);
    let offset = dir * distance;

    // Boundary edges: edges used once within the selection (keyed by position ids).
    let pid = position_ids(mesh);
    let mut uses: HashMap<(u32, u32), u32> = HashMap::new();
    let edge_key = |a: u32, b: u32| {
        let (x, y) = (pid[a as usize], pid[b as usize]);
        (x.min(y), x.max(y))
    };
    for &f in &selected {
        let t = &mesh.indices[f * 3..f * 3 + 3];
        for k in 0..3 {
            *uses.entry(edge_key(t[k], t[(k + 1) % 3])).or_default() += 1;
        }
    }
    let mut boundary: Vec<(u32, u32)> = Vec::new();
    for &f in &selected {
        let t = &mesh.indices[f * 3..f * 3 + 3];
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            if uses[&edge_key(a, b)] == 1 {
                boundary.push((a, b));
            }
        }
    }

    // Cap: duplicate selected vertices, move them, re-point the selected triangles.
    let mut cap: HashMap<u32, u32> = HashMap::new();
    for &f in &selected {
        for k in 0..3 {
            let v = mesh.indices[f * 3 + k];
            let nv = *cap.entry(v).or_insert_with(|| {
                let nv = push_vertex_copy(mesh, v as usize, flags);
                let p = &mut mesh.positions[nv as usize];
                *p = (Vec3::from(*p) + offset).into();
                nv
            });
            mesh.indices[f * 3 + k] = nv;
        }
    }

    // Side walls: quad a → b → b' → a' per boundary edge, flat-shaded.
    let mut side = Vec::with_capacity(boundary.len() * 2);
    for (a, b) in boundary {
        let (a2, b2) = (cap[&a], cap[&b]);
        let quad = [a, b, b2, a2];
        let p: Vec<Vec3> = quad.iter().map(|&v| Vec3::from(mesh.positions[v as usize])).collect();
        let n = (p[1] - p[0]).cross(p[2] - p[0]).try_normalize().unwrap_or(Vec3::ZERO);
        let base = mesh.positions.len() as u32;
        for (k, &v) in quad.iter().enumerate() {
            // Bottom verts copy from the original, top from the cap copy (same attributes).
            let src = if k < 2 { v } else { quad[3 - k] };
            push_vertex_copy(mesh, src as usize, flags);
            mesh.positions[(base + k as u32) as usize] = p[k].into();
            if flags.0 {
                mesh.normals[(base + k as u32) as usize] = n.into();
            }
        }
        let t = mesh.triangle_count();
        mesh.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        side.extend([t, t + 1]);
    }
    side
}

/// Simple decimation by vertex clustering on a grid of `cell_size`: vertices in the same
/// cell merge (averaged position/normal/color, first UV), collapsed and duplicate
/// triangles are removed. Returns the number of triangles removed.
pub fn decimate_vertex_clustering(mesh: &mut MeshData, cell_size: f32) -> usize {
    let before = mesh.triangle_count();
    if cell_size <= 0.0 || mesh.positions.is_empty() {
        return 0;
    }
    let (hn, hu, hc) = (has_normals(mesh), has_uvs(mesh), has_colors(mesh));
    let (min, _) = mesh.aabb();
    let mut clusters: HashMap<[i64; 3], u32> = HashMap::new();
    let mut remap = Vec::with_capacity(mesh.positions.len());
    let mut sum_p: Vec<Vec3> = Vec::new();
    let mut sum_n: Vec<Vec3> = Vec::new();
    let mut sum_c: Vec<Vec4> = Vec::new();
    let mut count: Vec<f32> = Vec::new();
    let mut uv: Vec<[f32; 2]> = Vec::new();
    for i in 0..mesh.positions.len() {
        let p = Vec3::from(mesh.positions[i]);
        let k = ((p - min) / cell_size).floor();
        let key = [k.x as i64, k.y as i64, k.z as i64];
        let c = *clusters.entry(key).or_insert_with(|| {
            sum_p.push(Vec3::ZERO);
            sum_n.push(Vec3::ZERO);
            sum_c.push(Vec4::ZERO);
            count.push(0.0);
            uv.push(if hu { mesh.uvs[i] } else { [0.0; 2] });
            (sum_p.len() - 1) as u32
        });
        let ci = c as usize;
        sum_p[ci] += p;
        count[ci] += 1.0;
        if hn {
            sum_n[ci] += Vec3::from(mesh.normals[i]);
        }
        if hc {
            sum_c[ci] += Vec4::from(mesh.colors[i]);
        }
        remap.push(c);
    }
    mesh.positions = sum_p.iter().zip(&count).map(|(p, c)| (*p / *c).into()).collect();
    if hn {
        mesh.normals = sum_n
            .iter()
            .map(|n| n.try_normalize().unwrap_or(Vec3::Y).into())
            .collect();
    }
    if hu {
        mesh.uvs = uv;
    }
    if hc {
        mesh.colors = sum_c.iter().zip(&count).map(|(c, n)| (*c / *n).into()).collect();
    }
    let mut seen: HashSet<[u32; 3]> = HashSet::new();
    let mut indices = Vec::with_capacity(mesh.indices.len());
    for tri in mesh.indices.as_chunks::<3>().0 {
        let t = [remap[tri[0] as usize], remap[tri[1] as usize], remap[tri[2] as usize]];
        if t[0] == t[1] || t[1] == t[2] || t[0] == t[2] {
            continue;
        }
        // Canonical rotation keeps orientation but detects duplicates.
        let r = (0..3).min_by_key(|&k| t[k]).unwrap_or(0);
        let canon = [t[r], t[(r + 1) % 3], t[(r + 2) % 3]];
        if seen.insert(canon) {
            indices.extend(t);
        }
    }
    mesh.indices = indices;
    remove_unused_vertices(mesh);
    before - mesh.triangle_count()
}

/// [`decimate_vertex_clustering`] with the cell size chosen so the longest bounding-box
/// side spans `resolution` cells.
pub fn decimate_to_grid(mesh: &mut MeshData, resolution: u32) -> usize {
    if mesh.positions.is_empty() || resolution == 0 {
        return 0;
    }
    let (min, max) = mesh.aabb();
    let extent = (max - min).max_element();
    if extent <= 0.0 {
        return 0;
    }
    decimate_vertex_clustering(mesh, extent / resolution as f32 * 1.0001)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::mesh::bounding_box;

    #[test]
    fn weld_cube_ignoring_attributes() {
        let mut c = MeshData::cube(2.0);
        assert_eq!(c.vertex_count(), 24);
        let removed = weld_vertices(&mut c, &WeldOptions::positions_only(1e-4));
        assert_eq!(removed, 16);
        assert_eq!(c.vertex_count(), 8);
        assert_eq!(c.triangle_count(), 12);
        assert!(c.indices.iter().all(|&i| i < 8));
        for (p, n) in c.positions.iter().zip(&c.normals) {
            assert!(Vec3::from(*p).normalize().dot(Vec3::from(*n)) > 0.99);
        }
        // With attribute comparison nothing merges (every corner copy has its own normal/UV).
        let mut strict = MeshData::cube(2.0);
        assert_eq!(weld_vertices(&mut strict, &WeldOptions::default()), 0);
        assert_eq!(strict.vertex_count(), 24);
    }

    #[test]
    fn weld_with_epsilon_merges_near_points() {
        let mut m = MeshData {
            positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0005, 0.0, 0.0]],
            indices: vec![0, 1, 2, 3, 1, 2],
            ..Default::default()
        };
        assert_eq!(weld_vertices(&mut m, &WeldOptions::positions_only(1e-3)), 1);
        assert_eq!(m.vertex_count(), 3);
        assert_eq!(m.indices, vec![0, 1, 2, 0, 1, 2]);
        let mut exact = MeshData {
            positions: vec![[0.0; 3], [0.0; 3], [1.0; 3]],
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        weld_vertices(&mut exact, &WeldOptions::positions_only(0.0));
        assert_eq!(exact.vertex_count(), 2);
        assert_eq!(exact.triangle_count(), 0, "collapsed triangle removed");
    }

    #[test]
    fn subdivide_quadruples_triangles() {
        let mut c = MeshData::cube(2.0);
        subdivide(&mut c);
        assert_eq!(c.triangle_count(), 48);
        assert_eq!(c.vertex_count(), 24 + 6 * 5);
        assert_eq!(c.normals.len(), c.vertex_count());
        assert_eq!(c.uvs.len(), c.vertex_count());
        assert_eq!(bounding_box(&c).unwrap().max, Vec3::ONE);
        subdivide(&mut c);
        assert_eq!(c.triangle_count(), 192);
    }

    #[test]
    fn flip_winding_twice_is_identity() {
        let c = MeshData::cube(1.0);
        let mut m = c.clone();
        flip_winding(&mut m);
        assert_ne!(m, c);
        flip_winding(&mut m);
        assert_eq!(m, c);
        flip_normals(&mut m);
        assert_eq!(m.normals[0], [-1.0, 0.0, 0.0]);
    }

    #[test]
    fn merge_reconciles_attributes() {
        let mut colored = MeshData::plane(1.0);
        colored.colors = vec![[1.0, 0.0, 0.0, 1.0]; 4];
        let mut bare = MeshData::cube(1.0);
        bare.normals.clear();
        bare.uvs.clear();
        let m = merge(&[colored, bare]);
        assert_eq!(m.vertex_count(), 28);
        assert_eq!(m.triangle_count(), 14);
        assert_eq!(m.normals.len(), 28);
        assert_eq!(m.uvs.len(), 28);
        assert_eq!(m.colors.len(), 28);
        assert_eq!(m.colors[27], [1.0; 4]);
        assert_eq!(m.indices[6], 4);
        assert!(m.indices.iter().all(|&i| i < 28));
    }

    #[test]
    fn extrude_plane_region() {
        let mut p = MeshData::plane(2.0);
        let side = extrude_faces(&mut p, &[0, 1], 1.0);
        assert_eq!(side.len(), 8);
        assert_eq!(p.triangle_count(), 10);
        assert_eq!(p.vertex_count(), 4 + 4 + 16);
        let bb = bounding_box(&p).unwrap();
        assert_eq!((bb.min.y, bb.max.y), (0.0, 1.0));
        // Walls face outward.
        for &t in &side {
            let tri = &p.indices[t * 3..t * 3 + 3];
            let centroid: Vec3 = tri.iter().map(|&i| Vec3::from(p.positions[i as usize])).sum::<Vec3>() / 3.0;
            let n = Vec3::from(p.normals[tri[0] as usize]);
            let fc = face_cross(&p, t).normalize();
            assert!(fc.dot(n) > 0.99);
            assert!(
                n.dot(Vec3::new(centroid.x, 0.0, centroid.z)) > 0.0,
                "wall normal points outward"
            );
            assert!(n.y.abs() < 1e-5);
        }
        // Cap moved up with its normal.
        let cap_v = p.indices[0] as usize;
        assert_eq!(p.positions[cap_v][1], 1.0);
        assert_eq!(p.normals[cap_v], [0.0, 1.0, 0.0]);
    }

    #[test]
    fn extrude_cube_face_across_split_vertices() {
        let mut c = MeshData::cube(2.0);
        let top: Vec<usize> = (0..c.triangle_count())
            .filter(|&t| face_cross(&c, t).normalize().y > 0.9)
            .collect();
        assert_eq!(top.len(), 2);
        let side = extrude_faces(&mut c, &top, 0.5);
        assert_eq!(side.len(), 8);
        assert_eq!(bounding_box(&c).unwrap().max.y, 1.5);
        assert!(extrude_faces(&mut c, &[999], 1.0).is_empty());
    }

    #[test]
    fn decimation_reduces_sphere() {
        let mut s = MeshData::sphere(1.0, 64, 32);
        let before = s.triangle_count();
        let removed = decimate_to_grid(&mut s, 8);
        assert!(removed > before / 2, "removed {removed} of {before}");
        assert_eq!(s.triangle_count(), before - removed);
        assert!(s.indices.iter().all(|&i| (i as usize) < s.vertex_count()));
        assert_eq!(s.normals.len(), s.vertex_count());
        let r = bounding_box(&s).unwrap().size().max_element();
        assert!(r > 1.5 && r <= 2.0 + 1e-4);
    }
}
