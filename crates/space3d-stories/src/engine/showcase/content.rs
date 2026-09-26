//! Showcase content, generated in code (deterministic, CPU only: run it off the frame in a
//! game): three hull meshes, one tiling set of hull maps, asteroids and a nebula sky.

use space3d_core::HdrImageData;
use space3d_core::prelude::*;
use space3d_render::{
    CubemapHandle, CubemapSource, GraphicsQuality, Material, MeshExtras, MeshHandle, RenderAssets, SamplerSettings,
    TextureSource, f32_to_f16, linear_to_srgb,
};

// ---------------------------------------------------------------------------------------------
// Noise
// ---------------------------------------------------------------------------------------------

pub(super) fn hash(x: i32, y: i32, z: i32, seed: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343)
        ^ (y as u32).wrapping_mul(0xd816_3841)
        ^ (z as u32).wrapping_mul(0xcb1a_b31f)
        ^ seed.wrapping_mul(0x9e37_79b9);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^ (h >> 16)
}

/// 0..1 from a hash.
pub(super) fn unit(h: u32) -> f32 {
    (h >> 8) as f32 / (1u32 << 24) as f32
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Trilinear value noise in 0..1 with smoothstep weights.
fn noise3(p: Vec3, seed: u32) -> f32 {
    let i = p.floor();
    let f = p - i;
    let w = f * f * (Vec3::splat(3.0) - 2.0 * f);
    let (x, y, z) = (i.x as i32, i.y as i32, i.z as i32);
    let v = |dx: i32, dy: i32, dz: i32| unit(hash(x + dx, y + dy, z + dz, seed));
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let x00 = lerp(v(0, 0, 0), v(1, 0, 0), w.x);
    let x10 = lerp(v(0, 1, 0), v(1, 1, 0), w.x);
    let x01 = lerp(v(0, 0, 1), v(1, 0, 1), w.x);
    let x11 = lerp(v(0, 1, 1), v(1, 1, 1), w.x);
    lerp(lerp(x00, x10, w.y), lerp(x01, x11, w.y), w.z)
}

/// Fractal value noise in 0..1.
fn fbm3(p: Vec3, seed: u32, octaves: u32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 0.5, 1.0, 0.0);
    for o in 0..octaves {
        sum += amp * noise3(p * freq, seed.wrapping_add(o));
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    sum / norm
}

/// Tiling 2D value noise in 0..1 (period `period` cells).
fn noise2(x: f32, y: f32, period: i32, seed: u32) -> f32 {
    let (fx, fy) = (x.floor(), y.floor());
    let (tx, ty) = (x - fx, y - fy);
    let (ux, uy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let w = |i: i32| i.rem_euclid(period);
    let (x0, y0) = (fx as i32, fy as i32);
    let v = |dx: i32, dy: i32| unit(hash(w(x0 + dx), w(y0 + dy), 0, seed));
    let top = v(0, 0) + (v(1, 0) - v(0, 0)) * ux;
    let bottom = v(0, 1) + (v(1, 1) - v(0, 1)) * ux;
    top + (bottom - top) * uy
}

/// Tiling fractal noise over the unit square (`base` cells at the first octave).
fn fbm2(u: f32, v: f32, base: i32, seed: u32, octaves: u32) -> f32 {
    let (mut sum, mut amp, mut cells, mut norm) = (0.0, 0.5, base, 0.0);
    for o in 0..octaves {
        sum += amp * noise2(u * cells as f32, v * cells as f32, cells, seed.wrapping_add(o));
        norm += amp;
        amp *= 0.5;
        cells *= 2;
    }
    sum / norm
}

// ---------------------------------------------------------------------------------------------
// Hull meshes
// ---------------------------------------------------------------------------------------------

/// World units per hull-map tile (the maps repeat).
pub const HULL_TILE: f32 = 18.0;

/// A flat-shaded mesh built from polygons, lofts and boxes; UVs are projected along each
/// face's dominant axis (top faces get the hull maps in plan view).
struct Builder {
    m: MeshData,
}

impl Builder {
    fn new() -> Self {
        Self {
            m: MeshData {
                positions: Vec::new(),
                normals: Vec::new(),
                uvs: Vec::new(),
                colors: Vec::new(),
                indices: Vec::new(),
            },
        }
    }

    fn uv(v: Vec3, n: Vec3) -> [f32; 2] {
        let a = n.abs();
        let s = 1.0 / HULL_TILE;
        if a.y >= a.x && a.y >= a.z {
            [v.x * s + 0.5, v.z * s]
        } else if a.x >= a.z {
            [v.z * s + 0.37, -v.y * s + 0.21]
        } else {
            [v.x * s + 0.61, -v.y * s + 0.43]
        }
    }

    /// A planar polygon (fan), wound so its normal points along `outward`.
    fn polygon(&mut self, points: &[Vec3], outward: Vec3) {
        if points.len() < 3 {
            return;
        }
        let mut n = Vec3::ZERO;
        for i in 1..points.len() - 1 {
            n += (points[i] - points[0]).cross(points[i + 1] - points[0]);
        }
        if n.length_squared() < 1e-10 {
            return;
        }
        let flip = n.dot(outward) < 0.0;
        let n = if flip { -n } else { n }.normalize();
        let base = self.m.positions.len() as u32;
        for &p in points {
            self.m.positions.push(p.to_array());
            self.m.normals.push(n.to_array());
            self.m.uvs.push(Self::uv(p, n));
        }
        for i in 1..points.len() as u32 - 1 {
            if flip {
                self.m.indices.extend_from_slice(&[base, base + i + 1, base + i]);
            } else {
                self.m.indices.extend_from_slice(&[base, base + i, base + i + 1]);
            }
        }
    }

    /// Rings of equal point counts joined by quads (outward from the rings' centers), capped
    /// at both ends.
    fn loft(&mut self, rings: &[Vec<Vec3>]) {
        let center = |r: &[Vec3]| r.iter().copied().sum::<Vec3>() / r.len() as f32;
        let k = rings[0].len();
        for pair in rings.windows(2) {
            let mid = (center(&pair[0]) + center(&pair[1])) * 0.5;
            for j in 0..k {
                let q = [pair[0][j], pair[0][(j + 1) % k], pair[1][(j + 1) % k], pair[1][j]];
                let c = q.iter().copied().sum::<Vec3>() * 0.25;
                self.polygon(&q, c - mid);
            }
        }
        let (first, last) = (&rings[0], &rings[rings.len() - 1]);
        self.polygon(first, center(first) - center(&rings[1]));
        self.polygon(last, center(last) - center(&rings[rings.len() - 2]));
    }

    /// A box of half extents `h` at `c`.
    fn cuboid(&mut self, c: Vec3, h: Vec3) {
        let ring = |z: f32| {
            vec![
                c + Vec3::new(-h.x, h.y, z),
                c + Vec3::new(h.x, h.y, z),
                c + Vec3::new(h.x, -h.y, z),
                c + Vec3::new(-h.x, -h.y, z),
            ]
        };
        self.loft(&[ring(-h.z), ring(h.z)]);
    }
}

/// A chamfered octagon cross-section of half width `w`, half height `h` at `(x, y, z)`.
fn section(x: f32, y: f32, z: f32, w: f32, h: f32) -> Vec<Vec3> {
    let p = |px: f32, py: f32| Vec3::new(x + px, y + py, z);
    vec![
        p(-0.55 * w, h),
        p(0.55 * w, h),
        p(w, 0.3 * h),
        p(w, -0.35 * h),
        p(0.6 * w, -h),
        p(-0.6 * w, -h),
        p(-w, -0.35 * h),
        p(-w, 0.3 * h),
    ]
}

/// A lofted body along z at `x`: `(z, half width, half height, y)` stations.
fn body(b: &mut Builder, x: f32, stations: &[(f32, f32, f32, f32)]) {
    let rings: Vec<Vec<Vec3>> = stations
        .iter()
        .map(|&(z, w, h, y)| section(x, y, z, w.max(0.05), h.max(0.05)))
        .collect();
    b.loft(&rings);
}

/// An airfoil-like wing cross-section in the y-z plane at `x`: chord `z0..z1`, thickness `t`.
fn wing_section(x: f32, y: f32, z0: f32, z1: f32, t: f32) -> Vec<Vec3> {
    let c = z1 - z0;
    vec![
        Vec3::new(x, y, z0),
        Vec3::new(x, y + 0.5 * t, z0 + 0.25 * c),
        Vec3::new(x, y + 0.3 * t, z1),
        Vec3::new(x, y - 0.3 * t, z1),
        Vec3::new(x, y - 0.5 * t, z0 + 0.25 * c),
    ]
}

/// A mirrored pair of wings from `root` to `tip`, each `(x, y, z0, z1, thickness)`.
fn wings(b: &mut Builder, root: (f32, f32, f32, f32, f32), tip: (f32, f32, f32, f32, f32)) {
    for s in [-1.0, 1.0] {
        b.loft(&[
            wing_section(s * root.0, root.1, root.2, root.3, root.4),
            wing_section(s * tip.0, tip.1, tip.2, tip.3, tip.4),
        ]);
    }
}

/// Greebles: boxes `(x, z, width, length, height)` on a deck at height `y`, mirrored when
/// `x ≠ 0`.
fn greebles(b: &mut Builder, y: f32, items: &[(f32, f32, f32, f32, f32)]) {
    for &(x, z, w, l, h) in items {
        let xs: &[f32] = if x == 0.0 { &[0.0] } else { &[-x, x] };
        for &x in xs {
            b.cuboid(Vec3::new(x, y + h * 0.5, z), Vec3::new(w * 0.5, h * 0.5, l * 0.5));
        }
    }
}

/// Hull kinds: an interceptor, a frigate, a gunship.
pub const HULL_NAMES: [&str; 3] = ["interceptor", "frigate", "gunship"];

/// Hull `kind` (0 interceptor, 1 frigate, 2 gunship): about 40 units long, nose toward −Z,
/// engine nozzles at z ≈ +21 and x ∈ {−8, 0, 8} (where `ENGINE_EXHAUST` emitters sit), the
/// flight plane through its middle. Flat-shaded, UVs for the tiling [`HullMaps`].
pub fn hull_mesh(kind: usize) -> MeshData {
    let mut b = Builder::new();
    match kind % 3 {
        // Interceptor: an arrowhead fuselage, a canopy, swept wings with anhedral, two
        // nacelles.
        0 => {
            body(
                &mut b,
                0.0,
                &[
                    (-23.0, 0.3, 0.3, 0.3),
                    (-18.0, 2.4, 1.3, 0.3),
                    (-9.0, 4.0, 2.1, 0.3),
                    (1.0, 4.8, 2.4, 0.2),
                    (11.0, 5.0, 2.4, 0.0),
                    (17.0, 4.2, 2.0, 0.0),
                    (20.0, 3.0, 1.6, 0.0),
                ],
            );
            body(
                &mut b,
                0.0,
                &[
                    (-14.0, 0.3, 0.2, 2.1),
                    (-11.0, 1.5, 0.8, 2.4),
                    (-5.0, 1.8, 0.9, 2.5),
                    (-1.0, 1.1, 0.5, 2.3),
                ],
            );
            wings(&mut b, (4.0, 0.0, -5.0, 13.0, 1.4), (19.0, -0.8, 8.0, 15.0, 0.4));
            for x in [-8.0, 8.0] {
                body(
                    &mut b,
                    x,
                    &[
                        (5.0, 1.0, 1.0, 0.4),
                        (8.0, 2.0, 1.8, 0.4),
                        (18.0, 2.2, 1.9, 0.4),
                        (21.0, 1.8, 1.5, 0.4),
                    ],
                );
            }
            greebles(
                &mut b,
                2.2,
                &[
                    (0.0, 6.0, 3.0, 5.0, 0.6),
                    (2.2, 12.0, 1.2, 3.0, 0.5),
                    (0.0, 16.0, 2.0, 2.0, 0.4),
                ],
            );
        }
        // Frigate: a broad armoured hull, twin nose prongs, sponsons, a bridge tower, three
        // engines.
        1 => {
            body(
                &mut b,
                0.0,
                &[
                    (-20.0, 3.0, 1.2, 0.0),
                    (-15.0, 6.0, 2.4, 0.0),
                    (-5.0, 8.0, 3.0, 0.0),
                    (6.0, 9.0, 3.2, 0.0),
                    (15.0, 8.6, 3.0, 0.0),
                    (19.0, 7.0, 2.6, 0.0),
                ],
            );
            for x in [-5.0, 5.0] {
                body(
                    &mut b,
                    x,
                    &[
                        (-27.0, 0.4, 0.3, 0.0),
                        (-24.0, 1.4, 1.0, 0.0),
                        (-14.0, 1.8, 1.3, 0.0),
                        (-12.0, 1.2, 0.9, 0.0),
                    ],
                );
            }
            for x in [-13.0, 13.0] {
                body(
                    &mut b,
                    x,
                    &[
                        (-6.0, 0.8, 0.8, 0.0),
                        (-3.0, 2.4, 2.0, 0.0),
                        (12.0, 2.6, 2.0, 0.0),
                        (15.0, 1.6, 1.4, 0.0),
                    ],
                );
            }
            wings(&mut b, (8.0, 0.0, -1.0, 10.0, 1.6), (12.0, 0.0, -1.0, 10.0, 1.4));
            body(
                &mut b,
                0.0,
                &[
                    (2.0, 1.5, 0.6, 3.6),
                    (4.0, 3.2, 1.3, 4.3),
                    (10.0, 3.4, 1.4, 4.3),
                    (12.0, 2.4, 0.9, 3.8),
                ],
            );
            for x in [-8.0, 0.0, 8.0] {
                body(
                    &mut b,
                    x,
                    &[(14.0, 1.8, 1.6, 0.2), (16.0, 2.5, 2.2, 0.2), (21.0, 2.4, 2.0, 0.2)],
                );
            }
            greebles(
                &mut b,
                3.0,
                &[
                    (4.0, -8.0, 2.0, 4.0, 0.7),
                    (6.0, 0.0, 1.5, 3.0, 0.5),
                    (0.0, -12.0, 3.0, 2.5, 0.5),
                    (5.0, 12.0, 2.5, 3.0, 0.6),
                ],
            );
        }
        // Gunship: a long spine, a delta wing with engine pods, two turrets with barrels.
        _ => {
            body(
                &mut b,
                0.0,
                &[
                    (-25.0, 0.3, 0.3, 0.2),
                    (-19.0, 1.8, 1.1, 0.2),
                    (-8.0, 3.0, 1.8, 0.2),
                    (4.0, 3.8, 2.2, 0.2),
                    (14.0, 4.2, 2.4, 0.2),
                    (21.0, 3.2, 1.9, 0.2),
                ],
            );
            wings(&mut b, (2.5, 0.0, -11.0, 16.0, 1.8), (18.0, -0.4, 9.0, 16.0, 0.4));
            for x in [-8.0, 8.0] {
                body(
                    &mut b,
                    x,
                    &[
                        (6.0, 0.8, 0.8, 0.9),
                        (9.0, 1.8, 1.6, 0.9),
                        (18.0, 1.9, 1.7, 0.9),
                        (21.0, 1.5, 1.3, 0.9),
                    ],
                );
            }
            for z in [-9.0, 3.0] {
                b.cuboid(Vec3::new(0.0, 2.6, z), Vec3::new(1.4, 0.7, 1.6));
                for x in [-0.5, 0.5] {
                    b.cuboid(Vec3::new(x, 2.7, z - 3.2), Vec3::new(0.18, 0.18, 1.8));
                }
            }
            greebles(
                &mut b,
                2.0,
                &[
                    (2.4, -3.0, 1.2, 3.5, 0.5),
                    (0.0, 11.0, 2.5, 4.0, 0.6),
                    (2.0, 16.0, 1.0, 2.0, 0.4),
                ],
            );
        }
    }
    b.m
}

/// An asteroid of unit radius: a sphere displaced by value noise with craters, smooth normals.
pub fn asteroid_mesh(seed: u32) -> MeshData {
    let mut m = MeshData::sphere(1.0, 32, 20);
    for p in &mut m.positions {
        let d = Vec3::from_array(*p);
        let bumps = fbm3(d * 1.7 + Vec3::splat(seed as f32 * 3.7), seed, 5) - 0.5;
        let craters = (noise3(d * 3.1, seed + 40) - 0.62).max(0.0) * 1.2;
        *p = (d * (1.0 + 0.6 * bumps - craters)).to_array();
    }
    space3d_assets::edit::mesh::recalculate_smooth_normals(&mut m);
    m
}

// ---------------------------------------------------------------------------------------------
// Hull maps
// ---------------------------------------------------------------------------------------------

/// A panel of the hull maps' layout: `[x0, x1) × [y0, y1)` texels.
#[derive(Clone, Copy)]
struct Panel {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    id: u32,
}

/// The panel containing texel `(x, y)` of an `n`² tile: the square split recursively.
fn panel_at(x: u32, y: u32, n: u32) -> Panel {
    let mut p = Panel {
        x0: 0,
        y0: 0,
        x1: n,
        y1: n,
        id: 1,
    };
    let min = (n / 20).max(3);
    for depth in 0..7 {
        let (w, h) = (p.x1 - p.x0, p.y1 - p.y0);
        let r = hash(p.id as i32, depth, 0, 77);
        if depth >= 3 && r % 100 < 30 {
            break;
        }
        let vertical = if w > h * 2 {
            true
        } else if h > w * 2 {
            false
        } else {
            r & 1 == 0
        };
        let span = if vertical { w } else { h };
        if span < min * 2 {
            break;
        }
        let t = 0.3 + 0.4 * unit(hash(p.id as i32, depth, 1, 78));
        let cut = ((span as f32 * t) as u32).clamp(min, span - min);
        if vertical {
            if x < p.x0 + cut {
                p.x1 = p.x0 + cut;
                p.id *= 2;
            } else {
                p.x0 += cut;
                p.id = p.id * 2 + 1;
            }
        } else if y < p.y0 + cut {
            p.y1 = p.y0 + cut;
            p.id *= 2;
        } else {
            p.y0 += cut;
            p.id = p.id * 2 + 1;
        }
    }
    p
}

/// The hull maps: one tiling layout of panels, `n`² texels each.
#[derive(Clone, Debug, PartialEq)]
pub struct HullMaps {
    /// Base color (sRGB): light grey paint with per-panel shades and grime, dark trim, bare
    /// metal.
    pub base_color: ImageData,
    /// Packed ORM (linear): R occlusion (dark seams), G roughness, B metallic.
    pub orm: ImageData,
    /// Tangent-space normals (linear, +Y green up): bevelled seams, rivets, raised trim.
    pub normal: ImageData,
    /// Emissive mask (sRGB, white): window rows, running lights, dimmer faction stripes. The
    /// material's emissive factor colors it.
    pub emissive: ImageData,
}

/// Generate the tiling hull maps at `n`² texels (at least 64).
pub fn hull_maps(n: u32) -> HullMaps {
    let n = n.max(64);
    let px = n as f32 / 256.0; // texels per texel of a 256² tile
    let groove = (1.2 * px).max(1.0);
    let mut height = vec![0f32; (n * n) as usize];
    let mut base_color = ImageData::new(n, n);
    let mut orm = ImageData::new(n, n);
    let mut emissive = ImageData::new(n, n);
    let srgb = |c: f32| (linear_to_srgb(c.clamp(0.0, 1.0)) * 255.0).round() as u8;
    for y in 0..n {
        for x in 0..n {
            let p = panel_at(x, y, n);
            let r = |k: i32| unit(hash(p.id as i32, k, 3, 91));
            let (lx, ly) = ((x - p.x0) as f32, (y - p.y0) as f32);
            let (pw, ph) = ((p.x1 - p.x0) as f32, (p.y1 - p.y0) as f32);
            // Distance to the panel's edge in texels.
            let d = lx.min(pw - 1.0 - lx).min(ly).min(ph - 1.0 - ly);
            let (u, v) = ((x as f32 + 0.5) / n as f32, (y as f32 + 0.5) / n as f32);
            let grime = fbm2(u, v, 6, 5, 5);
            let streaks = fbm2(u * 0.5, v, 24, 9, 3);
            // Small panels are often dark trim or bare metal; big ones mostly paint.
            let kind = r(0);
            let small = pw.max(ph) < n as f32 / 7.0;
            let bare = kind < if small { 0.2 } else { 0.1 };
            let trim = small && (0.2..0.5).contains(&kind);
            let mut h = smoothstep(groove * 0.4, groove * 1.8, d);
            // Rivet rows along the edges of paint panels.
            let inset = 3.5 * px;
            if !trim && !bare && (d - inset).abs() < 2.0 * px {
                let along = if (lx - inset).abs() < 2.0 * px || (pw - 1.0 - lx - inset).abs() < 2.0 * px {
                    ly
                } else {
                    lx
                };
                let pitch = 8.0 * px;
                let a = ((along / pitch).fract() - 0.5).abs() * pitch;
                let dd = ((d - inset).powi(2) + a * a).sqrt();
                h += 0.4 * (1.0 - (dd / (1.2 * px)).min(1.0)).max(0.0).powi(2);
            }
            // Raised trim panels with a bevel.
            if trim {
                h += 0.5 * smoothstep(groove * 2.0, groove * 4.5, d);
            }
            h += 0.05 * (grime - 0.5);
            height[(y * n + x) as usize] = h;

            let ao = 0.3 + 0.7 * smoothstep(0.0, groove * 4.0, d);
            let dirt = (grime - 0.45).max(0.0) * 1.4 + 0.25 * (streaks - 0.5).max(0.0);
            // Bare metal: a light F0 and a moderately rough lobe, so from the top-down camera it
            // mirrors the sky's glow and the sun's highlight instead of reading as a hole.
            let color = if bare {
                Vec3::new(0.72, 0.74, 0.78) * (0.85 + 0.2 * r(2))
            } else if trim {
                Vec3::new(0.1, 0.105, 0.115) * (0.85 + 0.3 * r(2))
            } else {
                let shade = 0.26 + 0.12 * r(1);
                Vec3::new(shade, shade * 1.01, shade * 1.04)
            } * (1.0 - 0.45 * dirt)
                * (0.6 + 0.4 * ao);
            let roughness = if d < groove {
                0.9
            } else if bare {
                0.3 + 0.25 * streaks
            } else if trim {
                0.35 + 0.15 * r(3)
            } else {
                0.4 + 0.2 * r(3) + 0.35 * dirt
            };
            let metallic: f32 = if bare {
                1.0
            } else if trim {
                0.7
            } else {
                0.05
            };
            base_color.set(x, y, [srgb(color.x), srgb(color.y), srgb(color.z), 255]);
            orm.set(
                x,
                y,
                [
                    (ao * 255.0).round() as u8,
                    (roughness.clamp(0.04, 1.0) * 255.0).round() as u8,
                    (metallic * 255.0).round() as u8,
                    255,
                ],
            );

            // Emissive: window rows, faction stripes, running lights.
            let mut e = 0.0f32;
            let long = pw >= ph;
            let (a, b) = if long { (lx, ly) } else { (ly, lx) };
            let (la, lb) = if long { (pw, ph) } else { (ph, pw) };
            if !bare && !trim && d > 3.0 * px {
                if r(4) < 0.22 && lb > 10.0 * px {
                    // A row of 3 × 2 windows every 6 texels along the panel's long side.
                    let cell = 6.0 * px;
                    let row = b - 4.0 * px;
                    let k = (a / cell) as i32;
                    if (0.0..2.0 * px).contains(&row) && (a / cell).fract() < 0.55 && r(20 + k) > 0.2 {
                        e = 1.0;
                    }
                } else if (0.22..0.3).contains(&r(4)) && lb > 8.0 * px {
                    // A faction stripe along the middle of the panel.
                    if (b - lb * 0.5).abs() < 1.8 * px && a > 4.0 * px && a < la - 4.0 * px {
                        e = 0.6;
                    }
                }
            }
            // Running lights: bright dots at some panels' corners.
            if r(6) < 0.07 && (lx - 3.0 * px).abs() < 1.1 * px && (ly - 3.0 * px).abs() < 1.1 * px {
                e = 1.0;
            }
            let eb = srgb(e);
            emissive.set(x, y, [eb, eb, eb, 255]);
        }
    }
    // Normals from the height field (wrapping: the tile repeats).
    let mut normal = ImageData::new(n, n);
    let strength = 2.2 / px;
    let at = |x: i64, y: i64| {
        let (n, x, y) = (n as i64, x.rem_euclid(n as i64), y.rem_euclid(n as i64));
        height[(y * n + x) as usize]
    };
    let e = |c: f32| ((c * 0.5 + 0.5) * 255.0).round() as u8;
    for y in 0..n as i64 {
        for x in 0..n as i64 {
            // +X right, +Y up (rows go down).
            let dx = (at(x + 1, y) - at(x - 1, y)) * 0.5 * strength;
            let dy = -(at(x, y + 1) - at(x, y - 1)) * 0.5 * strength;
            let nrm = Vec3::new(-dx, -dy, 1.0).normalize();
            normal.set(x as u32, y as u32, [e(nrm.x), e(nrm.y), e(nrm.z), 255]);
        }
    }
    HullMaps {
        base_color,
        orm,
        normal,
        emissive,
    }
}

// ---------------------------------------------------------------------------------------------
// Sky
// ---------------------------------------------------------------------------------------------

/// Direction the showcase sun's light travels: down, toward −X and −Z (the shadows fall up
/// and to the left in the top-down view).
pub fn sun_direction() -> Vec3 {
    Vec3::new(-0.38, -0.82, -0.43).normalize()
}

/// Nebula radiance toward the unit direction `d`: clouds, the galactic band, dust lanes and
/// the sun's wide glow (no stars, no sun disc: smooth enough to evaluate coarsely).
fn nebula(d: Vec3) -> Vec3 {
    // The flight plane is seen from above: the richest clouds lie below it (−Y); a tilted
    // band (the galactic plane) crosses the whole sky.
    let band_axis = Vec3::new(0.3, 0.35, 0.89).normalize();
    let band = (-(d.dot(band_axis) / 0.38).powi(2)).exp();
    let below = smoothstep(0.2, -0.7, d.y);
    let q = d * 2.1;
    let warp = Vec3::new(
        fbm3(q + Vec3::splat(1.7), 11, 4),
        fbm3(q + Vec3::splat(9.2), 12, 4),
        fbm3(q + Vec3::splat(4.4), 13, 4),
    );
    let n = fbm3(q + warp * 1.8, 7, 5);
    let density = smoothstep(0.38, 0.78, n) * (0.25 + 0.45 * band + 0.55 * below);
    let filaments = smoothstep(0.6, 0.8, n) * (0.35 + 0.65 * band) * (0.4 + 0.6 * below);
    let mix_t = smoothstep(0.35, 0.68, fbm3(q * 0.7 + Vec3::splat(3.1), 21, 3));
    let violet = Vec3::new(0.42, 0.07, 0.5);
    let teal = Vec3::new(0.03, 0.26, 0.42);
    let rose = Vec3::new(1.0, 0.3, 0.34);
    let tint = violet.lerp(teal, mix_t);
    let dust = smoothstep(0.52, 0.72, fbm3(q * 1.6 + warp * 0.8 + Vec3::splat(7.7), 31, 4));
    let mut c = Vec3::new(0.0015, 0.002, 0.005)
        + tint * density * 0.55
        + rose * filaments * 0.4
        + Vec3::new(0.05, 0.035, 0.08) * band * 0.4;
    c *= 1.0 - 0.8 * dust;
    let sun = d.dot(-sun_direction()).max(0.0);
    c + Vec3::new(1.0, 0.8, 0.55) * sun.powf(48.0) * 0.5
}

/// The sun: a white-hot disc and a warm corona.
fn sun_disc(d: Vec3) -> Vec3 {
    let sun = d.dot(-sun_direction()).max(0.0);
    Vec3::new(1.0, 0.8, 0.55) * sun.powf(900.0) * 4.0 + Vec3::splat(40.0) * smoothstep(0.99985, 0.99995, sun)
}

/// Stars: at most one per cell of two 3D grids on the sphere (bright and faint), gaussian, at
/// least `texel` (radians) wide so every star survives the face resolution.
fn stars(d: Vec3, texel: f32) -> Vec3 {
    let mut c = Vec3::ZERO;
    // (grid cells per unit, seed, per mille of cells with a star, base, peak brightness)
    for (g, seed, density, base, peak) in [(80.0f32, 99u32, 12u32, 0.3f32, 12.0f32), (160.0, 57, 14, 0.06, 0.5)] {
        let cell = (d * g).floor();
        let h = hash(cell.x as i32, cell.y as i32, cell.z as i32, seed);
        if h % 1000 < density {
            let r = |i: i32| unit(hash(h as i32, i, 0, 3));
            let star = ((cell + Vec3::new(r(1), r(2), r(3))) / g).normalize();
            let dist = (d - star).length();
            let brightness = base + peak * r(4).powi(10);
            let color = Vec3::new(1.0, 0.78, 0.6).lerp(Vec3::new(0.65, 0.8, 1.0), r(5));
            let sigma = (texel * 0.6).max(0.0008);
            c += color * brightness * (-(dist / sigma).powi(2)).exp();
        }
    }
    c
}

/// Direction through `(s, t)` (−1..1) of cube face `face` (wgpu order +X, −X, +Y, −Y, +Z, −Z).
fn face_dir(face: usize, s: f32, t: f32) -> Vec3 {
    match face {
        0 => Vec3::new(1.0, -t, -s),
        1 => Vec3::new(-1.0, -t, s),
        2 => Vec3::new(s, 1.0, t),
        3 => Vec3::new(s, -1.0, -t),
        4 => Vec3::new(s, -t, 1.0),
        _ => Vec3::new(-s, -t, -1.0),
    }
    .normalize()
}

/// The showcase sky: linear HDR radiance on six `size`² `Rgba16Float` faces (wgpu order). The
/// clouds are evaluated on a quarter-resolution grid and interpolated (they are smooth); the
/// stars and the sun at every texel.
pub fn sky_faces(size: u32) -> CubemapSource {
    let size = size.max(16);
    let coarse = (size / 4).max(8);
    let texel = 2.0 / size as f32;
    let mut data = Vec::with_capacity((size * size * 6 * 8) as usize);
    for face in 0..6 {
        let grid: Vec<Vec3> = (0..=coarse)
            .flat_map(|y| (0..=coarse).map(move |x| (x, y)))
            .map(|(x, y)| {
                let s = x as f32 / coarse as f32 * 2.0 - 1.0;
                let t = y as f32 / coarse as f32 * 2.0 - 1.0;
                nebula(face_dir(face, s, t))
            })
            .collect();
        let at = |x: u32, y: u32| grid[(y * (coarse + 1) + x) as usize];
        for y in 0..size {
            for x in 0..size {
                let s = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let t = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let (gx, gy) = ((s + 1.0) * 0.5 * coarse as f32, (t + 1.0) * 0.5 * coarse as f32);
                let (x0, y0) = (gx.floor().min(coarse as f32 - 1.0), gy.floor().min(coarse as f32 - 1.0));
                let (fx, fy) = (gx - x0, gy - y0);
                let (x0, y0) = (x0 as u32, y0 as u32);
                let top = at(x0, y0).lerp(at(x0 + 1, y0), fx);
                let bottom = at(x0, y0 + 1).lerp(at(x0 + 1, y0 + 1), fx);
                let d = face_dir(face, s, t);
                let c = top.lerp(bottom, fy) + stars(d, texel) + sun_disc(d);
                for v in c.extend(1.0).to_array() {
                    data.extend_from_slice(&f32_to_f16(v).to_le_bytes());
                }
            }
        }
    }
    CubemapSource::Faces(TextureSource {
        width: size,
        height: size,
        layers: 6,
        format: space3d_render::wgpu::TextureFormat::Rgba16Float,
        mip_level_count: 1,
        data,
        generate_mipmaps: true,
        sampler: SamplerSettings::default(),
    })
}

/// The sky as an equirect panorama (`width` × `width / 2`; longitude 0 = −Z), for tools.
pub fn sky_panorama(width: u32) -> HdrImageData {
    let width = width.max(4);
    let height = width / 2;
    let texel = std::f32::consts::PI / height as f32;
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        let lat = (y as f32 + 0.5) / height as f32 * std::f32::consts::PI;
        for x in 0..width {
            let lon = ((x as f32 + 0.5) / width as f32 - 0.5) * std::f32::consts::TAU;
            let d = Vec3::new(lat.sin() * lon.sin(), lat.cos(), -lat.sin() * lon.cos());
            pixels.push((nebula(d) + stars(d, texel) + sun_disc(d)).extend(1.0).to_array());
        }
    }
    HdrImageData::new(width, height, pixels)
}

// ---------------------------------------------------------------------------------------------
// Content and assets
// ---------------------------------------------------------------------------------------------

/// A faction's colors: hull paint (the base color factor), the glow of its windows and stripes
/// (the emissive factor), its engines and its lasers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Faction {
    /// Name.
    pub name: &'static str,
    /// Linear base color factor of its hulls.
    pub paint: Vec3,
    /// Linear emissive factor (HDR: above 1 blooms).
    pub glow: Vec3,
    /// Engine exhaust tint (the `testing::battle` tints, in order).
    pub engine: Vec4,
    /// Laser tint.
    pub laser: Vec4,
}

/// The three factions: blue, orange, green (the order of the Battle's engine tints).
pub const FACTIONS: [Faction; 3] = [
    Faction {
        name: "blue",
        paint: Vec3::new(0.52, 0.64, 0.9),
        glow: Vec3::new(0.6, 2.4, 6.0),
        engine: Vec4::new(0.3, 0.9, 2.0, 1.0),
        laser: Vec4::new(0.2, 0.6, 1.4, 1.0),
    },
    Faction {
        name: "orange",
        paint: Vec3::new(0.9, 0.44, 0.3),
        glow: Vec3::new(6.0, 1.9, 0.4),
        engine: Vec4::new(2.0, 0.9, 0.2, 1.0),
        laser: Vec4::new(1.4, 0.25, 0.08, 1.0),
    },
    Faction {
        name: "green",
        paint: Vec3::new(0.52, 0.8, 0.56),
        glow: Vec3::new(0.7, 5.4, 1.6),
        engine: Vec4::new(0.4, 2.0, 0.6, 1.0),
        laser: Vec4::new(0.25, 1.4, 0.3, 1.0),
    },
];

/// Sizes of the generated content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentOptions {
    /// Edge of the hull maps in texels (a tile of [`HULL_TILE`] units). Default 512.
    pub texture_size: u32,
    /// Sky cubemap face size. Default 512.
    pub sky_face_size: u32,
}

impl Default for ContentOptions {
    fn default() -> Self {
        Self {
            texture_size: 512,
            sky_face_size: 512,
        }
    }
}

impl ContentOptions {
    /// Sizes for a quality tier: maps 256 / 512 / 1024 / 1024, the tier's recommended sky face
    /// size ([`GraphicsQuality::sky_face_size`]).
    pub fn for_quality(q: GraphicsQuality) -> Self {
        Self {
            texture_size: match q {
                GraphicsQuality::Low => 256,
                GraphicsQuality::Medium => 512,
                _ => 1024,
            },
            sky_face_size: q.sky_face_size(),
        }
    }
}

/// Everything the showcase scenes draw, generated on the CPU ([`ShowcaseContent::generate`]).
#[derive(Clone, Debug)]
pub struct ShowcaseContent {
    /// The three hulls ([`hull_mesh`]) with their MikkTSpace tangents.
    pub hulls: [(MeshData, Vec<[f32; 4]>); 3],
    /// The hull maps.
    pub maps: HullMaps,
    /// Three asteroids (unit radius).
    pub asteroids: [MeshData; 3],
    /// The sky ([`sky_faces`]).
    pub sky: CubemapSource,
}

impl ShowcaseContent {
    /// Generate the content (CPU only, deterministic; run it with `spawn_compute` in a game:
    /// about 0.3 s at the default sizes in an optimized build).
    pub fn generate(options: ContentOptions) -> Self {
        let hull = |k: usize| space3d_assets::edit::mesh::compute_mikktspace_tangents(&hull_mesh(k));
        Self {
            hulls: [hull(0), hull(1), hull(2)],
            maps: hull_maps(options.texture_size),
            asteroids: [asteroid_mesh(3), asteroid_mesh(8), asteroid_mesh(13)],
            sky: sky_faces(options.sky_face_size),
        }
    }

    /// Register the content: meshes (hulls with their tangents), the four hull maps (base
    /// color and emissive sRGB, ORM and normal linear), one material per faction and the sky.
    pub fn upload(&self, assets: &mut RenderAssets) -> ShowcaseAssets {
        let hulls = std::array::from_fn(|k| {
            let (mesh, tangents) = &self.hulls[k];
            assets.add_mesh_with(mesh.clone(), MeshExtras::new().with_tangents(tangents.clone()))
        });
        let base = assets.add_image(self.maps.base_color.clone());
        let orm = assets.add_image_linear(self.maps.orm.clone());
        let normal = assets.add_image_linear(self.maps.normal.clone());
        let emissive = assets.add_image(self.maps.emissive.clone());
        let hull_materials = FACTIONS.map(|f| {
            // The maps multiply the factors: metallic and roughness come from the ORM map.
            Material::color(f.paint.extend(1.0))
                .with_texture(base)
                .with_metallic_roughness(1.0, 1.0)
                .with_orm_map(orm)
                .with_normal_map(normal, 1.0)
                .with_emissive_map(emissive, f.glow)
        });
        let asteroids = std::array::from_fn(|k| assets.add_mesh(self.asteroids[k].clone()));
        ShowcaseAssets {
            hulls,
            hull_materials,
            asteroids,
            asteroid_material: Material::srgb(0.3, 0.27, 0.25).with_metallic_roughness(0.0, 0.88),
            sky: assets.add_cubemap(self.sky.clone()),
            quad: assets.add_mesh(MeshData::plane(1.0)),
        }
    }
}

/// The registered showcase content ([`ShowcaseContent::upload`]).
#[derive(Clone, Debug)]
pub struct ShowcaseAssets {
    /// Interceptor, frigate, gunship.
    pub hulls: [MeshHandle; 3],
    /// One hull material per faction ([`FACTIONS`]): all four maps.
    pub hull_materials: [Material; 3],
    /// Three asteroid meshes (unit radius).
    pub asteroids: [MeshHandle; 3],
    /// Rock.
    pub asteroid_material: Material,
    /// The sky cubemap (skybox and image-based lighting).
    pub sky: CubemapHandle,
    /// A unit quad in the XZ plane (render-target previews).
    pub quad: MeshHandle,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hulls_are_closed_and_sized() {
        for k in 0..3 {
            let m = hull_mesh(k);
            assert!(m.indices.len().is_multiple_of(3) && !m.indices.is_empty(), "{k}");
            assert_eq!(m.positions.len(), m.uvs.len());
            let (lo, hi) = m.positions.iter().fold((Vec3::MAX, Vec3::MIN), |(lo, hi), p| {
                let p = Vec3::from_array(*p);
                (lo.min(p), hi.max(p))
            });
            let size = hi - lo;
            assert!((40.0..=50.0).contains(&size.z), "{k}: {size}");
            assert!(hi.z >= 20.0 && hi.z <= 21.5, "{k}: nozzles at +21, {hi}");
            assert!(lo.z < -20.0, "{k}: nose toward -Z, {lo}");
            // Every triangle is wound like its face's normal (front faces point outward; the
            // lofts' quads are not all planar, so a triangle may lean off the quad's normal).
            let mut min = 1.0f32;
            for t in m.indices.chunks(3) {
                let [a, b, c] = [0, 1, 2].map(|i| Vec3::from_array(m.positions[t[i] as usize]));
                let n = Vec3::from_array(m.normals[t[0] as usize]);
                let face = (b - a).cross(c - a);
                if face.length() > 1e-6 {
                    min = min.min(face.normalize().dot(n));
                }
            }
            assert!(min > 0.5, "{k}: winding ({min})");
        }
    }

    #[test]
    fn maps_tile_and_are_deterministic() {
        let a = hull_maps(64);
        assert_eq!(a, hull_maps(64));
        for img in [&a.base_color, &a.orm, &a.normal, &a.emissive] {
            assert_eq!((img.width, img.height), (64, 64));
        }
        // Some windows glow, some panels are bare metal.
        assert!(a.emissive.pixels.chunks(4).any(|p| p[0] > 200));
        assert!(a.orm.pixels.chunks(4).any(|p| p[2] == 255));
        let lit = a.emissive.pixels.chunks(4).filter(|p| p[0] > 0).count();
        assert!(lit < 64 * 64 / 8, "{lit} emissive texels");
    }

    #[test]
    fn sky_is_hdr_with_a_sun() {
        let pano = sky_panorama(256);
        let max = pano
            .pixels
            .iter()
            .map(|p| p[0].max(p[1]).max(p[2]))
            .fold(0.0f32, f32::max);
        assert!(max > 5.0, "stars and the sun bloom: {max}");
        let mean: f32 = pano.pixels.iter().map(|p| p[0] + p[1] + p[2]).sum::<f32>() / pano.pixels.len() as f32;
        assert!(mean > 0.01 && mean < 0.5, "{mean}");
        match sky_faces(32) {
            CubemapSource::Faces(t) => assert_eq!((t.width, t.layers, t.data.len()), (32, 6, 32 * 32 * 6 * 8)),
            other => panic!("{other:?}"),
        }
    }
}
