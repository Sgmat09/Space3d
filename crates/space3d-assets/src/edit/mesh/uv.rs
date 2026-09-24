//! UV projections.

use super::has_normals;
use glam::{Vec2, Vec3};
use space3d_core::MeshData;

/// Orthonormal `(u, v)` axes for projecting along `n`, oriented so a front view (+Z) maps
/// +X → +u and +Y → −v (image rows grow downward).
fn basis(n: Vec3) -> (Vec3, Vec3) {
    let up = if n.y.abs() < 0.999 {
        Vec3::Y
    } else if n.y > 0.0 {
        Vec3::NEG_Z
    } else {
        Vec3::Z
    };
    let u = up.cross(n).normalize();
    let v = n.cross(u);
    (u, -v)
}

/// Project positions onto the plane perpendicular to `normal`: `uv = (p·u, p·v) × scale`.
pub fn uv_planar_projection(mesh: &mut MeshData, normal: Vec3, scale: f32) {
    let (u, v) = basis(normal.try_normalize().unwrap_or(Vec3::Z));
    mesh.uvs = mesh
        .positions
        .iter()
        .map(|&p| {
            let p = Vec3::from(p);
            [p.dot(u) * scale, p.dot(v) * scale]
        })
        .collect();
}

/// Tri-planar "box" projection: each vertex is projected along the dominant axis of its
/// normal. Works best on meshes with split vertices at hard edges (e.g. after
/// [`super::recalculate_flat_normals`]).
pub fn uv_box_projection(mesh: &mut MeshData, scale: f32) {
    let normals: Vec<Vec3> = if has_normals(mesh) {
        mesh.normals.iter().map(|&n| Vec3::from(n)).collect()
    } else {
        let mut tmp = mesh.clone();
        super::recalculate_smooth_normals(&mut tmp);
        tmp.normals.iter().map(|&n| Vec3::from(n)).collect()
    };
    mesh.uvs = mesh
        .positions
        .iter()
        .zip(&normals)
        .map(|(&p, &n)| {
            let a = n.abs();
            let axis = if a.x >= a.y && a.x >= a.z {
                Vec3::X * n.x.signum()
            } else if a.y >= a.z {
                Vec3::Y * n.y.signum()
            } else {
                Vec3::Z * n.z.signum()
            };
            let (u, v) = basis(axis);
            let p = Vec3::from(p);
            [p.dot(u) * scale, p.dot(v) * scale]
        })
        .collect();
}

/// Remap UVs so their bounding rectangle becomes `[0, 1]²` (aspect not preserved).
pub fn normalize_uvs(mesh: &mut MeshData) {
    if mesh.uvs.is_empty() {
        return;
    }
    let (mut min, mut max) = (Vec2::splat(f32::INFINITY), Vec2::splat(f32::NEG_INFINITY));
    for &uv in &mesh.uvs {
        min = min.min(Vec2::from(uv));
        max = max.max(Vec2::from(uv));
    }
    let size = (max - min).max(Vec2::splat(f32::EPSILON));
    for uv in &mut mesh.uvs {
        *uv = ((Vec2::from(*uv) - min) / size).into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planar_front_and_top() {
        let mut m = MeshData::plane(2.0);
        uv_planar_projection(&mut m, Vec3::Y, 0.5);
        // Top view: +X → +u, +Z (toward viewer) → +v.
        assert_eq!(m.uvs[1], [0.5, 0.5]);
        normalize_uvs(&mut m);
        assert_eq!(m.uvs, MeshData::plane(2.0).uvs);
        let mut f = MeshData {
            positions: vec![[1.0, 2.0, 0.0]],
            ..Default::default()
        };
        uv_planar_projection(&mut f, Vec3::Z, 1.0);
        assert_eq!(f.uvs[0], [1.0, -2.0]);
    }

    #[test]
    fn box_projection_on_cube() {
        let mut c = MeshData::cube(2.0);
        uv_box_projection(&mut c, 0.5);
        normalize_uvs(&mut c);
        assert_eq!(c.uvs.len(), 24);
        // Every face gets a full 0..1 square.
        for face in c.uvs.chunks(4) {
            let us: Vec<f32> = face.iter().map(|uv| uv[0]).collect();
            assert!(us.contains(&0.0) && us.contains(&1.0), "{face:?}");
        }
    }
}
