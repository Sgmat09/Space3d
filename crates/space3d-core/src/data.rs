//! CPU-side asset data shared by assets, render, editor and tests.
use glam::{Vec2, Vec3};
use serde::{Deserialize, Serialize};

/// RGBA8 (sRGB) image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl ImageData {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
        }
    }
    pub fn filled(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        Self {
            width,
            height,
            pixels: rgba.repeat((width * height) as usize),
        }
    }
    pub fn get(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        self.pixels[i..i + 4].try_into().unwrap()
    }
    pub fn set(&mut self, x: u32, y: u32, rgba: [u8; 4]) {
        let i = ((y * self.width + x) * 4) as usize;
        self.pixels[i..i + 4].copy_from_slice(&rgba);
    }
    /// Checkerboard, handy default/missing texture.
    pub fn checker(size: u32, cell: u32, a: [u8; 4], b: [u8; 4]) -> Self {
        let mut img = Self::new(size, size);
        for y in 0..size {
            for x in 0..size {
                img.set(x, y, if ((x / cell) + (y / cell)) % 2 == 0 { a } else { b });
            }
        }
        img
    }
}

/// Indexed triangle mesh.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    /// Optional per-vertex linear RGBA colors (empty = white).
    pub colors: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
}

impl MeshData {
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    pub fn aabb(&self) -> (Vec3, Vec3) {
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for p in &self.positions {
            let p = Vec3::from(*p);
            min = min.min(p);
            max = max.max(p);
        }
        (min, max)
    }

    /// Axis-aligned cube centred at origin with side `size`.
    pub fn cube(size: f32) -> Self {
        let h = size / 2.0;
        let faces: [(Vec3, Vec3, Vec3); 6] = [
            (Vec3::X, Vec3::NEG_Z, Vec3::Y),
            (Vec3::NEG_X, Vec3::Z, Vec3::Y),
            (Vec3::Y, Vec3::X, Vec3::NEG_Z),
            (Vec3::NEG_Y, Vec3::X, Vec3::Z),
            (Vec3::Z, Vec3::X, Vec3::Y),
            (Vec3::NEG_Z, Vec3::NEG_X, Vec3::Y),
        ];
        let mut m = Self::default();
        for (n, u, v) in faces {
            let base = m.positions.len() as u32;
            for (du, dv, uv) in [
                (-1.0, -1.0, [0.0, 1.0]),
                (1.0, -1.0, [1.0, 1.0]),
                (1.0, 1.0, [1.0, 0.0]),
                (-1.0, 1.0, [0.0, 0.0]),
            ] {
                m.positions.push(((n + u * du + v * dv) * h).into());
                m.normals.push(n.into());
                m.uvs.push(uv);
            }
            m.indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        m
    }

    /// XZ plane facing +Y.
    pub fn plane(size: f32) -> Self {
        let h = size / 2.0;
        Self {
            positions: vec![[-h, 0.0, h], [h, 0.0, h], [h, 0.0, -h], [-h, 0.0, -h]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
            colors: vec![],
            indices: vec![0, 1, 2, 0, 2, 3],
        }
    }

    /// UV sphere.
    pub fn sphere(radius: f32, sectors: u32, stacks: u32) -> Self {
        let mut m = Self::default();
        for i in 0..=stacks {
            let phi = std::f32::consts::PI * i as f32 / stacks as f32;
            for j in 0..=sectors {
                let theta = std::f32::consts::TAU * j as f32 / sectors as f32;
                let n = Vec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin());
                m.positions.push((n * radius).into());
                m.normals.push(n.into());
                m.uvs
                    .push(Vec2::new(j as f32 / sectors as f32, i as f32 / stacks as f32).into());
            }
        }
        let row = sectors + 1;
        for i in 0..stacks {
            for j in 0..sectors {
                let a = i * row + j;
                let b = a + row;
                m.indices.extend([a, a + 1, b, a + 1, b + 1, b]);
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primitives_are_well_formed() {
        for m in [MeshData::cube(2.0), MeshData::plane(1.0), MeshData::sphere(1.0, 16, 8)] {
            assert_eq!(m.positions.len(), m.normals.len());
            assert_eq!(m.positions.len(), m.uvs.len());
            assert_eq!(m.indices.len() % 3, 0);
            assert!(m.indices.iter().all(|&i| (i as usize) < m.vertex_count()));
        }
        let (min, max) = MeshData::cube(2.0).aabb();
        assert_eq!((min, max), (Vec3::splat(-1.0), Vec3::splat(1.0)));
    }
    #[test]
    fn image_get_set() {
        let mut img = ImageData::checker(4, 2, [255; 4], [0, 0, 0, 255]);
        assert_eq!(img.get(0, 0), [255; 4]);
        assert_eq!(img.get(2, 0), [0, 0, 0, 255]);
        img.set(1, 1, [1, 2, 3, 4]);
        assert_eq!(img.get(1, 1), [1, 2, 3, 4]);
    }
}
