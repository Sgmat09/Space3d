//! Bounding volumes.

use glam::Vec3;
use serde::{Deserialize, Serialize};
use space3d_core::MeshData;

/// Axis-aligned bounding box.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Aabb {
    /// Minimum corner.
    pub min: Vec3,
    /// Maximum corner.
    pub max: Vec3,
}

impl Aabb {
    /// Centre point.
    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }
    /// Edge lengths.
    pub fn size(&self) -> Vec3 {
        self.max - self.min
    }
    /// Half edge lengths.
    pub fn half_extents(&self) -> Vec3 {
        self.size() * 0.5
    }
    /// Whether `p` is inside (inclusive).
    pub fn contains(&self, p: Vec3) -> bool {
        p.cmpge(self.min).all() && p.cmple(self.max).all()
    }
    /// Smallest box containing both.
    pub fn union(&self, other: &Aabb) -> Aabb {
        Aabb {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }
}

/// Bounding sphere.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoundingSphere {
    /// Centre.
    pub center: Vec3,
    /// Radius.
    pub radius: f32,
}

impl BoundingSphere {
    /// Whether `p` is inside (with a small tolerance).
    pub fn contains(&self, p: Vec3) -> bool {
        p.distance(self.center) <= self.radius * (1.0 + 1e-5) + 1e-6
    }
}

/// Axis-aligned bounds of all positions (`None` for an empty mesh).
pub fn bounding_box(mesh: &MeshData) -> Option<Aabb> {
    if mesh.positions.is_empty() {
        return None;
    }
    let (min, max) = mesh.aabb();
    Some(Aabb { min, max })
}

/// Near-minimal bounding sphere (Ritter's algorithm; `None` for an empty mesh).
pub fn bounding_sphere(mesh: &MeshData) -> Option<BoundingSphere> {
    let pts: Vec<Vec3> = mesh.positions.iter().map(|&p| Vec3::from(p)).collect();
    let first = *pts.first()?;
    let farthest = |from: Vec3| {
        pts.iter()
            .copied()
            .max_by(|a, b| a.distance_squared(from).total_cmp(&b.distance_squared(from)))
            .unwrap_or(from)
    };
    let a = farthest(first);
    let b = farthest(a);
    let mut center = (a + b) * 0.5;
    let mut radius = a.distance(b) * 0.5;
    for &p in &pts {
        let d = p.distance(center);
        if d > radius {
            let new_r = (radius + d) * 0.5;
            center += (p - center) * ((new_r - radius) / d);
            radius = new_r;
        }
    }
    Some(BoundingSphere { center, radius })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_and_spheres() {
        let c = MeshData::cube(2.0);
        let bb = bounding_box(&c).unwrap();
        assert_eq!(
            (bb.min, bb.max, bb.center()),
            (Vec3::splat(-1.0), Vec3::ONE, Vec3::ZERO)
        );
        assert!(bb.contains(Vec3::splat(0.5)) && !bb.contains(Vec3::splat(1.5)));
        let s = bounding_sphere(&c).unwrap();
        assert!(s.center.length() < 1e-4);
        assert!((s.radius - 3f32.sqrt()).abs() < 1e-3);
        let sph = MeshData::sphere(2.0, 16, 8);
        let bs = bounding_sphere(&sph).unwrap();
        assert!(bs.radius < 2.05 && bs.radius >= 2.0 - 1e-3);
        assert!(sph.positions.iter().all(|&p| bs.contains(Vec3::from(p))));
        assert!(bounding_box(&MeshData::default()).is_none());
        assert!(bounding_sphere(&MeshData::default()).is_none());
    }
}
