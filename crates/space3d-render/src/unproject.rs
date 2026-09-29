//! Unprojection that does not depend on where the camera is.
//!
//! The inverse of `proj * view` has the camera position in its translation column. In `f32`
//! that inverse is badly conditioned far from the origin (at 85 000 units its entries carry
//! ~0.01 of rounding), so a world direction or ray rebuilt from it lands several pixels off,
//! differently for every camera pose. A *direction* needs no translation at all, and a *ray*
//! needs the camera position only as the exact `f32` value it is: so both are built from the
//! inverse projection and the inverse of the view's rotation, computed in `f64` (the
//! rotation and projection entries do not grow with the camera's distance from the origin, so
//! the results are as accurate 430 000 units out as at the origin).
//!
//! The renderer's sky pass reads [`inverse_proj_rot`] as `View::inv_proj_rot`.

use glam::{DMat3, DMat4, DVec4, Mat4, Vec2, Vec3};

/// The upper-left 3×3 of a view matrix, in `f64`.
fn view_linear(view: Mat4) -> DMat3 {
    DMat3::from_cols(
        view.x_axis.truncate().as_dvec3(),
        view.y_axis.truncate().as_dvec3(),
        view.z_axis.truncate().as_dvec3(),
    )
}

/// `(proj · view without its translation)⁻¹`: clip space → the world-space *offset* from the
/// camera. Multiplying `(ndc.xy, ndc.z, 1)` by it and dividing by `w` gives a world-space
/// vector from the camera to the point of the pixel at that depth, so
/// `unproject(ndc, 1) − unproject(ndc, 0)` is the pixel's world direction, with no camera
/// position in it (`GpuView::inv_proj_rot`, `View::inv_proj_rot` in [`crate::VIEW_WGSL`]).
///
/// Computed as `R⁻¹ · proj⁻¹` in `f64`, `R` being the view's rotation (its upper-left 3×3), and
/// rounded to `f32` once: the entries are independent of the camera position.
pub fn inverse_proj_rot(view: Mat4, proj: Mat4) -> Mat4 {
    (DMat4::from_mat3(view_linear(view).inverse()) * proj.as_dmat4().inverse()).as_mat4()
}

/// The world-space ray through the point `ndc` of the view (x, y in −1..1, y up): its origin on
/// the near plane and its unit direction, from the `f64` inverse of the projection and of the
/// view (see the module docs; the result of `(proj * view).inverse()` in `f32` is off by
/// several units far from the origin). `(origin, direction)`; a zero direction for a
/// degenerate matrix.
pub fn unproject_ray(view: Mat4, proj: Mat4, ndc: Vec2) -> (Vec3, Vec3) {
    let inv_proj = proj.as_dmat4().inverse();
    let inv_rot = view_linear(view).inverse();
    let at = |z: f64| {
        let h = inv_proj * DVec4::new(ndc.x as f64, ndc.y as f64, z, 1.0);
        h.truncate() / h.w
    };
    let (near, far) = (at(0.0), at(1.0));
    // view = R · world + t, so world = R⁻¹ · (view point − t).
    let origin = inv_rot * (near - view.w_axis.truncate().as_dvec3());
    let direction = (inv_rot * (far - near)).normalize_or_zero();
    (origin.as_vec3(), direction.as_vec3())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{DVec3, Quat};

    /// A view matrix looking along a fixed direction from `eye`, in `f32` like the renderer's
    /// (`camera transform.inverse()`).
    fn view_at(eye: Vec3, rotation: Quat) -> Mat4 {
        Mat4::from_rotation_translation(rotation, eye).inverse()
    }

    fn proj(near: f32, far: f32, aspect: f32) -> Mat4 {
        glam::camera::rh::proj::directx::perspective(60f32.to_radians(), aspect, near, far)
    }

    /// The world direction of `ndc` through the shader's formula.
    fn direction(m: Mat4, ndc: Vec2) -> Vec3 {
        let near = m * ndc.extend(0.0).extend(1.0);
        let far = m * ndc.extend(1.0).extend(1.0);
        far.truncate() / far.w - near.truncate() / near.w
    }

    /// The direction of `ndc` in `f64`, from the exact rotation and projection.
    fn reference(rotation: Quat, p: Mat4, ndc: Vec2) -> DVec3 {
        let inv = p.as_dmat4().inverse();
        let at = |z: f64| {
            let h = inv * DVec4::new(ndc.x as f64, ndc.y as f64, z, 1.0);
            h.truncate() / h.w
        };
        (rotation.as_dquat() * (at(1.0) - at(0.0))).normalize()
    }

    fn angle(a: Vec3, b: DVec3) -> f64 {
        a.as_dvec3().normalize().dot(b).clamp(-1.0, 1.0).acos()
    }

    /// The field does not contain the camera position: identical bits at every position, and
    /// the direction of every pixel is within a few 1e-7 rad of the `f64` truth. The `f32`
    /// inverse of `proj * view` (what the sky used) is off by orders of magnitude there.
    #[test]
    fn inverse_proj_rot_is_independent_of_the_camera_position() {
        let rotation = Quat::from_euler(glam::EulerRot::YXZ, 0.4, 0.1, 0.0);
        let p = proj(2.0, 300_000.0, 16.0 / 9.0);
        let near_origin = inverse_proj_rot(view_at(Vec3::new(1.0, 2.0, 3.0), rotation), p);
        let mut worst_old = 0.0f64;
        for eye in [
            Vec3::new(85_000.0, 0.0, 85_000.0),
            Vec3::new(1_200.0, -430_000.0, 900.0),
            Vec3::new(-31_000.0, 40.0, 31_000.0),
        ] {
            let view = view_at(eye, rotation);
            let m = inverse_proj_rot(view, p);
            assert_eq!(m, near_origin, "inv_proj_rot changed with the camera position {eye}");
            let old = (p * view).inverse();
            for ndc in [
                Vec2::ZERO,
                Vec2::new(0.9, 0.9),
                Vec2::new(-0.9, 0.7),
                Vec2::new(0.5, -0.9),
            ] {
                let truth = reference(rotation, p, ndc);
                let new_err = angle(direction(m, ndc), truth);
                assert!(new_err < 5e-7, "{eye} {ndc}: {new_err} rad");
                worst_old = worst_old.max(angle(direction(old, ndc), truth));
            }
        }
        // Informative: the old formula's worst error (~1 px at 640 wide is 2e-3 rad).
        eprintln!("f32 inverse of proj * view, worst direction error: {worst_old:.2e} rad");
    }

    /// Orthographic projections keep one direction for every pixel.
    #[test]
    fn inverse_proj_rot_orthographic() {
        let rotation = Quat::from_euler(glam::EulerRot::YXZ, -0.7, 0.3, 0.0);
        let p = glam::camera::rh::proj::directx::orthographic(-10.0, 10.0, -5.0, 5.0, 0.1, 500.0);
        let m = inverse_proj_rot(view_at(Vec3::new(85_000.0, 5.0, -85_000.0), rotation), p);
        let forward = rotation * Vec3::NEG_Z;
        for ndc in [Vec2::ZERO, Vec2::new(1.0, 1.0), Vec2::new(-0.3, 0.8)] {
            let d = direction(m, ndc);
            assert!(angle(d, forward.as_dvec3()) < 5e-7, "{ndc}: {d}");
        }
    }

    /// The ray of `unproject_ray` starts on the near plane and matches the `f64` truth far from
    /// the origin, where the `f32` inverse of `proj * view` puts it several units off.
    #[test]
    fn unproject_ray_far_from_the_origin() {
        let rotation = Quat::from_euler(glam::EulerRot::YXZ, 0.4, 0.1, 0.0);
        let eye = Vec3::new(85_000.0, 3.0, 85_000.0);
        let (view, p) = (view_at(eye, rotation), proj(2.0, 300_000.0, 16.0 / 9.0));
        for ndc in [Vec2::ZERO, Vec2::new(0.8, -0.6)] {
            let (origin, dir) = unproject_ray(view, p, ndc);
            let truth = reference(rotation, p, ndc);
            assert!(angle(dir, truth) < 5e-7, "{ndc}: direction");
            // The origin is 2 units along the ray from the camera (to f32 rounding of 85 000).
            let along = (origin - eye).length();
            let expected = 2.0 / (rotation * Vec3::NEG_Z).dot(dir);
            assert!((along - expected).abs() < 0.02, "{ndc}: {along} vs {expected}");
        }
    }
}
