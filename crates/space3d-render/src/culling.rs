//! CPU frustum culling primitives (the GPU path mirrors these in `shaders/cull.wgsl`).

use glam::{Mat4, Vec3, Vec4};

/// Axis-aligned bounding box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    /// Minimum corner.
    pub min: Vec3,
    /// Maximum corner.
    pub max: Vec3,
}

impl Aabb {
    /// An "inverted" box that any `grow` call replaces.
    pub const EMPTY: Aabb = Aabb {
        min: Vec3::splat(f32::INFINITY),
        max: Vec3::splat(f32::NEG_INFINITY),
    };

    /// Box from corners.
    pub fn new(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }
    /// True if no point was ever added.
    pub fn is_empty(&self) -> bool {
        self.min.x > self.max.x
    }
    /// Expand to include `p`.
    pub fn grow(&mut self, p: Vec3) {
        self.min = self.min.min(p);
        self.max = self.max.max(p);
    }
    /// Expand to include `other`.
    pub fn union(&mut self, other: &Aabb) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }
    /// Center point.
    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }
    /// Half extents.
    pub fn half_extents(&self) -> Vec3 {
        (self.max - self.min) * 0.5
    }
    /// Box grown by `r` on every side.
    pub fn inflated(&self, r: f32) -> Aabb {
        Aabb::new(self.min - Vec3::splat(r), self.max + Vec3::splat(r))
    }
}

/// Bounding sphere.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sphere {
    /// Center.
    pub center: Vec3,
    /// Radius.
    pub radius: f32,
}

impl Sphere {
    /// Sphere enclosing an AABB.
    pub fn from_aabb(aabb: &Aabb) -> Self {
        if aabb.is_empty() {
            return Sphere::default();
        }
        Sphere {
            center: aabb.center(),
            radius: aabb.half_extents().length(),
        }
    }
    /// Transform by an affine matrix (radius scaled by the largest axis scale).
    pub fn transformed(&self, m: &Mat4) -> Sphere {
        Sphere {
            center: m.transform_point3(self.center),
            radius: self.radius * max_axis_scale(m),
        }
    }
}

/// Largest column length of the upper 3x3 (conservative scale for bounding spheres).
pub fn max_axis_scale(m: &Mat4) -> f32 {
    m.x_axis
        .truncate()
        .length_squared()
        .max(m.y_axis.truncate().length_squared())
        .max(m.z_axis.truncate().length_squared())
        .sqrt()
}

/// Six inward-facing planes (`xyz` = normal, `w` = distance) extracted from a view-projection
/// matrix with wgpu's 0..1 clip depth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frustum {
    /// Left, right, bottom, top, near, far.
    pub planes: [Vec4; 6],
}

impl Frustum {
    /// Gribb–Hartmann plane extraction.
    pub fn from_view_proj(m: &Mat4) -> Self {
        let r0 = m.row(0);
        let r1 = m.row(1);
        let r2 = m.row(2);
        let r3 = m.row(3);
        let mut planes = [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2];
        for p in &mut planes {
            let len = p.truncate().length();
            if len > 0.0 {
                *p /= len;
            }
        }
        Self { planes }
    }

    /// Sphere intersects or is inside the frustum.
    #[inline]
    pub fn intersects_sphere(&self, s: &Sphere) -> bool {
        let c = s.center.extend(1.0);
        self.planes.iter().all(|p| p.dot(c) >= -s.radius)
    }

    /// AABB intersects or is inside the frustum (conservative "p-vertex" test).
    pub fn intersects_aabb(&self, b: &Aabb) -> bool {
        if b.is_empty() {
            return false;
        }
        self.planes.iter().all(|p| {
            let v = Vec3::new(
                if p.x >= 0.0 { b.max.x } else { b.min.x },
                if p.y >= 0.0 { b.max.y } else { b.min.y },
                if p.z >= 0.0 { b.max.z } else { b.min.z },
            );
            p.truncate().dot(v) + p.w >= 0.0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frustum() -> Frustum {
        let proj = glam::camera::rh::proj::directx::perspective(60f32.to_radians(), 1.0, 0.1, 100.0);
        let view = glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
        Frustum::from_view_proj(&(proj * view))
    }

    #[test]
    fn sphere_culling() {
        let f = frustum();
        assert!(f.intersects_sphere(&Sphere {
            center: Vec3::ZERO,
            radius: 1.0
        }));
        assert!(!f.intersects_sphere(&Sphere {
            center: Vec3::new(0.0, 0.0, 10.0),
            radius: 1.0
        }));
        assert!(!f.intersects_sphere(&Sphere {
            center: Vec3::new(100.0, 0.0, 0.0),
            radius: 1.0
        }));
        assert!(!f.intersects_sphere(&Sphere {
            center: Vec3::new(0.0, 0.0, -200.0),
            radius: 1.0
        }));
        // Straddling the left plane still counts.
        assert!(f.intersects_sphere(&Sphere {
            center: Vec3::new(-3.4, 0.0, 0.0),
            radius: 1.0
        }));
    }

    #[test]
    fn aabb_culling() {
        let f = frustum();
        assert!(f.intersects_aabb(&Aabb::new(Vec3::splat(-1.0), Vec3::splat(1.0))));
        assert!(!f.intersects_aabb(&Aabb::new(Vec3::new(50.0, 0.0, 0.0), Vec3::new(51.0, 1.0, 1.0))));
        assert!(!f.intersects_aabb(&Aabb::EMPTY));
    }

    #[test]
    fn transformed_sphere_scales() {
        let m = Mat4::from_scale_rotation_translation(Vec3::new(1.0, 3.0, 2.0), glam::Quat::IDENTITY, Vec3::X);
        let s = Sphere {
            center: Vec3::ZERO,
            radius: 1.0,
        }
        .transformed(&m);
        assert_eq!(s.center, Vec3::X);
        assert!((s.radius - 3.0).abs() < 1e-6);
    }
}
