//! Vertex color painting.

use super::has_colors;
use glam::{Vec3, Vec4};
use space3d_core::MeshData;

/// Set every vertex color to `color` (linear RGBA).
pub fn fill_vertex_colors(mesh: &mut MeshData, color: [f32; 4]) {
    mesh.colors = vec![color; mesh.positions.len()];
}

/// Paint a spherical brush of `radius` at `center`: vertex colors blend toward `color` by
/// `strength × falloff`, where falloff is 1 inside `hardness × radius` and eases to 0 at the
/// radius. Missing colors are initialized to white. Returns how many vertices changed.
pub fn paint_vertex_colors(
    mesh: &mut MeshData,
    center: Vec3,
    radius: f32,
    color: [f32; 4],
    strength: f32,
    hardness: f32,
) -> usize {
    if !has_colors(mesh) {
        fill_vertex_colors(mesh, [1.0; 4]);
    }
    let radius = radius.max(f32::EPSILON);
    let hard = hardness.clamp(0.0, 1.0);
    let target = Vec4::from(color);
    let mut changed = 0;
    for (p, c) in mesh.positions.iter().zip(mesh.colors.iter_mut()) {
        let d = Vec3::from(*p).distance(center) / radius;
        if d >= 1.0 {
            continue;
        }
        let falloff = if d <= hard {
            1.0
        } else {
            let t = (d - hard) / (1.0 - hard).max(1e-6);
            1.0 - t * t * (3.0 - 2.0 * t)
        };
        let w = (strength * falloff).clamp(0.0, 1.0);
        if w > 0.0 {
            *c = Vec4::from(*c).lerp(target, w).into();
            changed += 1;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paints_within_radius() {
        let mut m = MeshData::plane(2.0);
        let n = paint_vertex_colors(&mut m, Vec3::new(1.0, 0.0, 1.0), 0.5, [1.0, 0.0, 0.0, 1.0], 1.0, 1.0);
        assert_eq!(n, 1);
        assert_eq!(m.colors[1], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(m.colors[0], [1.0; 4]);
        let n = paint_vertex_colors(&mut m, Vec3::ZERO, 10.0, [0.0, 0.0, 1.0, 1.0], 0.5, 1.0);
        assert_eq!(n, 4);
        assert_eq!(m.colors[0], [0.5, 0.5, 1.0, 1.0]);
        fill_vertex_colors(&mut m, [0.0; 4]);
        assert!(m.colors.iter().all(|c| *c == [0.0; 4]));
    }
}
