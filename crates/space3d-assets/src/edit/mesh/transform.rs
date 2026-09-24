//! Affine transforms (normals follow via the inverse-transpose).

use super::has_normals;
use glam::{Mat3, Mat4, Quat, Vec3};
use space3d_core::MeshData;

/// Apply an arbitrary affine matrix. Normals use the inverse-transpose and are
/// renormalized; mirroring matrices (negative determinant) also flip the winding so faces
/// keep facing outward.
pub fn transform(mesh: &mut MeshData, matrix: Mat4) {
    for p in &mut mesh.positions {
        *p = matrix.transform_point3(Vec3::from(*p)).into();
    }
    let linear = Mat3::from_mat4(matrix);
    let det = linear.determinant();
    if has_normals(mesh) && det.abs() > f32::EPSILON {
        let nm = linear.inverse().transpose();
        for n in &mut mesh.normals {
            *n = (nm * Vec3::from(*n)).normalize_or_zero().into();
        }
    }
    if det < 0.0 {
        super::flip_winding(mesh);
    }
}

/// Move every vertex by `offset`.
pub fn translate(mesh: &mut MeshData, offset: Vec3) {
    for p in &mut mesh.positions {
        *p = (Vec3::from(*p) + offset).into();
    }
}

/// Rotate about the origin.
pub fn rotate(mesh: &mut MeshData, rotation: Quat) {
    for p in &mut mesh.positions {
        *p = (rotation * Vec3::from(*p)).into();
    }
    if has_normals(mesh) {
        for n in &mut mesh.normals {
            *n = (rotation * Vec3::from(*n)).into();
        }
    }
}

/// Rotate about `pivot`.
pub fn rotate_around(mesh: &mut MeshData, rotation: Quat, pivot: Vec3) {
    translate(mesh, -pivot);
    rotate(mesh, rotation);
    translate(mesh, pivot);
}

/// Non-uniform scale about the origin (normals corrected, mirroring handled).
pub fn scale(mesh: &mut MeshData, factor: Vec3) {
    transform(mesh, Mat4::from_scale(factor));
}

/// Translate so the bounding-box centre is at the origin; returns the applied offset.
pub fn center_on_origin(mesh: &mut MeshData) -> Vec3 {
    let Some(bb) = super::bounding_box(mesh) else {
        return Vec3::ZERO;
    };
    let offset = -bb.center();
    translate(mesh, offset);
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::mesh::face_cross;

    #[test]
    fn translate_rotate_scale() {
        let mut m = MeshData::cube(2.0);
        translate(&mut m, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(m.aabb(), (Vec3::new(0.0, 1.0, 2.0), Vec3::new(2.0, 3.0, 4.0)));
        assert_eq!(center_on_origin(&mut m), Vec3::new(-1.0, -2.0, -3.0));
        let mut r = MeshData::plane(2.0);
        rotate(&mut r, Quat::from_rotation_x(std::f32::consts::FRAC_PI_2));
        let n = Vec3::from(r.normals[0]);
        assert!((n - Vec3::Z).length() < 1e-5, "{n}");
        let mut s = MeshData::plane(2.0);
        rotate_around(
            &mut s,
            Quat::from_rotation_z(std::f32::consts::PI),
            Vec3::new(0.0, 1.0, 0.0),
        );
        let (min, max) = s.aabb();
        assert!((min.y - 2.0).abs() < 1e-5 && (max.y - 2.0).abs() < 1e-5);
    }

    #[test]
    fn nonuniform_scale_fixes_normals_and_mirroring_keeps_winding() {
        let mut m = MeshData::sphere(1.0, 12, 6);
        scale(&mut m, Vec3::new(4.0, 1.0, 1.0));
        for n in &m.normals {
            assert!((Vec3::from(*n).length() - 1.0).abs() < 1e-4);
        }
        let mut c = MeshData::cube(2.0);
        scale(&mut c, Vec3::new(-1.0, 1.0, 1.0));
        for t in 0..c.triangle_count() {
            let fc = face_cross(&c, t).normalize();
            let n = Vec3::from(c.normals[c.indices[t * 3] as usize]);
            assert!(fc.dot(n) > 0.99, "face {t} winding/normal mismatch");
        }
    }
}
