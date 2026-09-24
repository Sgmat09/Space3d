//! Normal and tangent generation.

use super::{face_cross, has_colors, has_normals, has_uvs, position_ids};
use glam::{Vec2, Vec3};
use space3d_core::MeshData;

/// Give every triangle its own three vertices with the face normal (hard edges).
/// Vertex count becomes `3 × triangle_count`.
pub fn recalculate_flat_normals(mesh: &mut MeshData) {
    let (uv, col) = (has_uvs(mesh), has_colors(mesh));
    let n = mesh.indices.len();
    let mut positions = Vec::with_capacity(n);
    let mut normals = Vec::with_capacity(n);
    let mut uvs = Vec::with_capacity(if uv { n } else { 0 });
    let mut colors = Vec::with_capacity(if col { n } else { 0 });
    for t in 0..mesh.triangle_count() {
        let fnrm: [f32; 3] = face_cross(mesh, t).normalize_or_zero().into();
        for &i in &mesh.indices[t * 3..t * 3 + 3] {
            let i = i as usize;
            positions.push(mesh.positions[i]);
            normals.push(fnrm);
            if uv {
                uvs.push(mesh.uvs[i]);
            }
            if col {
                colors.push(mesh.colors[i]);
            }
        }
    }
    mesh.positions = positions;
    mesh.normals = normals;
    mesh.uvs = uvs;
    mesh.colors = colors;
    mesh.indices = (0..n as u32).collect();
}

/// Angle-weighted smooth normals. Vertices that share a position (e.g. across UV seams)
/// share the normal, so seams don't show. Vertex count is unchanged.
pub fn recalculate_smooth_normals(mesh: &mut MeshData) {
    let ids = position_ids(mesh);
    let groups = ids.iter().copied().max().map_or(0, |m| m as usize + 1);
    let mut acc = vec![Vec3::ZERO; groups];
    for tri in mesh.indices.as_chunks::<3>().0 {
        let p = [0, 1, 2].map(|k| Vec3::from(mesh.positions[tri[k] as usize]));
        let cross = (p[1] - p[0]).cross(p[2] - p[0]);
        // Skip slivers: their direction is numerically meaningless but angle weights are large.
        let longest = (0..3)
            .map(|k| p[k].distance_squared(p[(k + 1) % 3]))
            .fold(0.0, f32::max);
        if cross.length() <= 1e-6 * longest {
            continue;
        }
        let n = cross.normalize();
        for k in 0..3 {
            let angle = (p[(k + 1) % 3] - p[k]).angle_between(p[(k + 2) % 3] - p[k]);
            if angle.is_finite() {
                acc[ids[tri[k] as usize] as usize] += n * angle;
            }
        }
    }
    mesh.normals = ids
        .iter()
        .map(|&g| acc[g as usize].try_normalize().unwrap_or(Vec3::Y).into())
        .collect();
}

/// Per-vertex tangents `[x, y, z, w]` (w = ±1 bitangent sign) from positions, normals and
/// UV 0, suitable for normal mapping. Missing normals are computed smooth; missing UVs
/// yield an arbitrary tangent perpendicular to the normal.
pub fn compute_tangents(mesh: &MeshData) -> Vec<[f32; 4]> {
    let n = mesh.positions.len();
    let normals: Vec<Vec3> = if has_normals(mesh) {
        mesh.normals.iter().map(|&v| Vec3::from(v)).collect()
    } else {
        let mut tmp = mesh.clone();
        recalculate_smooth_normals(&mut tmp);
        tmp.normals.iter().map(|&v| Vec3::from(v)).collect()
    };
    let mut tan = vec![Vec3::ZERO; n];
    let mut bit = vec![Vec3::ZERO; n];
    if has_uvs(mesh) {
        for tri in mesh.indices.as_chunks::<3>().0 {
            let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
            let (pa, pb, pc) = (
                Vec3::from(mesh.positions[a]),
                Vec3::from(mesh.positions[b]),
                Vec3::from(mesh.positions[c]),
            );
            let (ua, ub, uc) = (
                Vec2::from(mesh.uvs[a]),
                Vec2::from(mesh.uvs[b]),
                Vec2::from(mesh.uvs[c]),
            );
            let (e1, e2) = (pb - pa, pc - pa);
            let (d1, d2) = (ub - ua, uc - ua);
            let det = d1.x * d2.y - d2.x * d1.y;
            if det.abs() < 1e-12 {
                continue;
            }
            let r = 1.0 / det;
            let s = (e1 * d2.y - e2 * d1.y) * r;
            let t = (e2 * d1.x - e1 * d2.x) * r;
            for v in [a, b, c] {
                tan[v] += s;
                bit[v] += t;
            }
        }
    }
    (0..n)
        .map(|i| {
            let nrm = normals[i];
            let t = (tan[i] - nrm * nrm.dot(tan[i]))
                .try_normalize()
                .unwrap_or_else(|| nrm.any_orthonormal_vector());
            let w = if nrm.cross(t).dot(bit[i]) < 0.0 { -1.0 } else { 1.0 };
            [t.x, t.y, t.z, w]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_normals_unweld() {
        let mut m = MeshData::sphere(1.0, 8, 4);
        let tris = m.triangle_count();
        recalculate_flat_normals(&mut m);
        assert_eq!(m.vertex_count(), tris * 3);
        assert_eq!(m.uvs.len(), m.vertex_count());
        for t in 0..tris {
            let n = Vec3::from(m.normals[t * 3]);
            let c = face_cross(&m, t);
            if c.length() > 1e-6 {
                assert!(c.normalize().dot(n) > 0.999);
            }
        }
    }

    #[test]
    fn smooth_normals_match_sphere_and_cube_corners() {
        let reference = MeshData::sphere(1.0, 24, 12);
        let mut m = reference.clone();
        m.normals.clear();
        recalculate_smooth_normals(&mut m);
        for (a, b) in m.normals.iter().zip(&reference.normals) {
            assert!(Vec3::from(*a).dot(Vec3::from(*b)) > 0.98);
        }
        let mut c = MeshData::cube(2.0);
        recalculate_smooth_normals(&mut c);
        for (p, n) in c.positions.iter().zip(&c.normals) {
            let expected = Vec3::from(*p).normalize();
            assert!(expected.dot(Vec3::from(*n)) > 0.999, "corner normal should be diagonal");
        }
    }

    #[test]
    fn tangents_follow_u_direction() {
        let m = MeshData::plane(2.0);
        let t = compute_tangents(&m);
        assert_eq!(t.len(), 4);
        for v in &t {
            assert!((Vec3::new(v[0], v[1], v[2]) - Vec3::X).length() < 1e-5, "{v:?}");
            assert_eq!(v[3].abs(), 1.0);
        }
        let mut no_uv = MeshData::cube(1.0);
        no_uv.uvs.clear();
        for (v, n) in compute_tangents(&no_uv).iter().zip(&no_uv.normals) {
            assert!(Vec3::new(v[0], v[1], v[2]).dot(Vec3::from(*n)).abs() < 1e-5);
        }
    }
}
