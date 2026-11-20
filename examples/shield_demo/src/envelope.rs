//! Smooth hull-hugging shield envelopes, computed at load time for models that come without a
//! baked `.sbe` (the engine's test ship, when the SpaceCorps assets are not installed).
//!
//! The shell is not the hull inflated along its normals (that keeps facets, folds on sharp edges
//! and lets parts poke through). Instead:
//!   1. unsigned distance to the hull on a 3D grid (dense surface samples + two-pass 3D chamfer),
//!   2. interior flood-filled as solid (everything not connected to the grid border),
//!   3. field blurred (binomial 1-2-1, two passes per axis),
//!   4. iso-surface at `offset × hull length` extracted with surface nets, normals = field gradient.
//!
//! The same algorithm bakes the `.sbe` files of the game's models at build time
//! (outside this repository); this is its distance-field part.

use std::collections::VecDeque;

// ---------------------------------------------------------------- settings

/// Envelope parameters. `cells` is the grid resolution along the hull's longest dimension.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// Distance from the hull, as a fraction of the hull's longest dimension (prototype: 0.035).
    pub offset: f32,
    /// Grid cells along the longest dimension (48: a few milliseconds for a small ship).
    pub cells: u32,
}

/// Triangle mesh of the envelope, in the model's own space (same frame as the glTF scene).
#[derive(Clone, Debug, Default)]
pub struct Envelope {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

// ---------------------------------------------------------------- distance field

pub struct Field {
    pub d: Vec<f32>,
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub cell: f32,
    pub min: [f32; 3],
    pub hull_len: f32,
}

/// Builds the blurred distance field around a triangle soup (positions + triangle indices).
/// The grid is padded so any offset up to 0.14 × hull length can be extracted.
pub fn build_field(pos: &[[f32; 3]], idx: &[u32], cells: u32) -> Field {
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    for p in pos {
        for k in 0..3 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    let len = (0..3).map(|k| hi[k] - lo[k]).fold(0.0f32, f32::max).max(1e-6);
    let cell = len / cells as f32;
    let pad = 0.14 * len + 4.0 * cell;
    let min = [lo[0] - pad, lo[1] - pad, lo[2] - pad];
    let dims: Vec<usize> = (0..3)
        .map(|k| ((hi[k] + pad - min[k]) / cell).ceil() as usize + 1)
        .collect();
    let (nx, ny, nz) = (dims[0], dims[1], dims[2]);
    let sxy = nx * ny;
    let n = sxy * nz;
    let mut d = vec![1e9f32; n];

    // 1) exact distance next to the surface, from dense samples
    let splat = |p: [f32; 3], d: &mut Vec<f32>| {
        let g = [(p[0] - min[0]) / cell, (p[1] - min[1]) / cell, (p[2] - min[2]) / cell];
        let r = |k: usize, size: usize| {
            let a = (g[k].floor() as i64 - 1).max(0) as usize;
            let b = ((g[k].ceil() as i64 + 1) as usize).min(size - 1);
            (a, b)
        };
        let (x0, x1) = r(0, nx);
        let (y0, y1) = r(1, ny);
        let (z0, z1) = r(2, nz);
        for z in z0..=z1 {
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let dist = ((x as f32 - g[0]).powi(2) + (y as f32 - g[1]).powi(2) + (z as f32 - g[2]).powi(2))
                        .sqrt()
                        * cell;
                    let i = x + nx * y + sxy * z;
                    if dist < d[i] {
                        d[i] = dist;
                    }
                }
            }
        }
    };
    let step = cell * 0.9;
    for t in idx.as_chunks::<3>().0 {
        let (a, b, c) = (pos[t[0] as usize], pos[t[1] as usize], pos[t[2] as usize]);
        let e1 = sub(b, a);
        let e2 = sub(c, a);
        let longest = norm(e1).max(norm(e2)).max(norm(sub(c, b)));
        let m = ((longest / step).ceil() as usize).max(1);
        for u in 0..=m {
            for v in 0..=(m - u) {
                let (fa, fb) = (u as f32 / m as f32, v as f32 / m as f32);
                splat(
                    [
                        a[0] + e1[0] * fa + e2[0] * fb,
                        a[1] + e1[1] * fa + e2[1] * fb,
                        a[2] + e1[2] * fa + e2[2] * fb,
                    ],
                    &mut d,
                );
            }
        }
    }

    // 2) two-pass 3D chamfer over the whole grid
    let mut fw: Vec<(i64, i64, i64, isize, f32)> = Vec::new();
    for dz in -1i64..=1 {
        for dy in -1i64..=1 {
            for dx in -1i64..=1 {
                if dz < 0 || (dz == 0 && (dy < 0 || (dy == 0 && dx < 0))) {
                    let w = cell * ((dx * dx + dy * dy + dz * dz) as f32).sqrt();
                    fw.push((
                        dx,
                        dy,
                        dz,
                        dx as isize + nx as isize * dy as isize + sxy as isize * dz as isize,
                        w,
                    ));
                }
            }
        }
    }
    let bw: Vec<_> = fw.iter().map(|&(dx, dy, dz, o, w)| (-dx, -dy, -dz, -o, w)).collect();
    let sweep = |offs: &[(i64, i64, i64, isize, f32)], forward: bool, d: &mut Vec<f32>| {
        for zi in 0..nz {
            for yi in 0..ny {
                for xi in 0..nx {
                    let (x, y, z) = if forward {
                        (xi, yi, zi)
                    } else {
                        (nx - 1 - xi, ny - 1 - yi, nz - 1 - zi)
                    };
                    let i = x + nx * y + sxy * z;
                    let mut v = d[i];
                    for &(dx, dy, dz, o, w) in offs {
                        let (xx, yy, zz) = (x as i64 + dx, y as i64 + dy, z as i64 + dz);
                        if xx < 0 || yy < 0 || zz < 0 || xx >= nx as i64 || yy >= ny as i64 || zz >= nz as i64 {
                            continue;
                        }
                        let c = d[(i as isize + o) as usize] + w;
                        if c < v {
                            v = c;
                        }
                    }
                    d[i] = v;
                }
            }
        }
    };
    sweep(&fw, true, &mut d);
    sweep(&bw, false, &mut d);

    // 3) interior = not connected to the grid border -> solid
    let wall = cell * 0.9;
    let mut seen = vec![false; n];
    let mut q = VecDeque::new();
    for z in 0..nz {
        for y in 0..ny {
            for x in 0..nx {
                if x == 0 || y == 0 || z == 0 || x == nx - 1 || y == ny - 1 || z == nz - 1 {
                    let i = x + nx * y + sxy * z;
                    if !seen[i] && d[i] > wall {
                        seen[i] = true;
                        q.push_back(i);
                    }
                }
            }
        }
    }
    while let Some(i) = q.pop_front() {
        let z = i / sxy;
        let r = i - z * sxy;
        let y = r / nx;
        let x = r - y * nx;
        let mut visit = |j: usize, q: &mut VecDeque<usize>| {
            if !seen[j] && d[j] > wall {
                seen[j] = true;
                q.push_back(j);
            }
        };
        if x > 0 {
            visit(i - 1, &mut q);
        }
        if x + 1 < nx {
            visit(i + 1, &mut q);
        }
        if y > 0 {
            visit(i - nx, &mut q);
        }
        if y + 1 < ny {
            visit(i + nx, &mut q);
        }
        if z > 0 {
            visit(i - sxy, &mut q);
        }
        if z + 1 < nz {
            visit(i + sxy, &mut q);
        }
    }
    for i in 0..n {
        if !seen[i] {
            d[i] = 0.0;
        }
    }

    // 4) binomial 1-2-1 blur, two passes per axis
    let mut t = vec![0f32; n];
    for _ in 0..2 {
        for (stride, size, axis) in [(1usize, nx, 0usize), (nx, ny, 1), (sxy, nz, 2)] {
            for i in 0..n {
                let c = match axis {
                    0 => i % nx,
                    1 => (i / nx) % ny,
                    _ => i / sxy,
                };
                let a = if c > 0 { d[i - stride] } else { d[i] };
                let b = if c + 1 < size { d[i + stride] } else { d[i] };
                t[i] = 0.25 * a + 0.5 * d[i] + 0.25 * b;
            }
            d.copy_from_slice(&t);
        }
    }
    Field {
        d,
        nx,
        ny,
        nz,
        cell,
        min,
        hull_len: len,
    }
}

/// Surface nets at distance `offset × hull_len`.
pub fn extract(field: &Field, offset: f32) -> Envelope {
    let Field {
        d,
        nx,
        ny,
        nz,
        cell,
        min,
        hull_len,
    } = field;
    let (nx, ny, nz, cell) = (*nx, *ny, *nz, *cell);
    let sxy = nx * ny;
    let iso = offset * hull_len;
    let f = |x: usize, y: usize, z: usize| d[x + nx * y + sxy * z] - iso;
    const CO: [[usize; 3]; 8] = [
        [0, 0, 0],
        [1, 0, 0],
        [0, 1, 0],
        [1, 1, 0],
        [0, 0, 1],
        [1, 0, 1],
        [0, 1, 1],
        [1, 1, 1],
    ];
    const E: [[usize; 2]; 12] = [
        [0, 1],
        [2, 3],
        [4, 5],
        [6, 7],
        [0, 2],
        [1, 3],
        [4, 6],
        [5, 7],
        [0, 4],
        [1, 5],
        [2, 6],
        [3, 7],
    ];
    let mut vid = vec![-1i32; nx * ny * nz];
    let mut out = Envelope::default();
    let mut v = [0f32; 8];
    for z in 0..nz - 1 {
        for y in 0..ny - 1 {
            for x in 0..nx - 1 {
                let mut neg = 0;
                for k in 0..8 {
                    v[k] = f(x + CO[k][0], y + CO[k][1], z + CO[k][2]);
                    if v[k] < 0.0 {
                        neg += 1;
                    }
                }
                if neg == 0 || neg == 8 {
                    continue;
                }
                let (mut s, mut c) = ([0f32; 3], 0f32);
                for [a, b] in E {
                    if (v[a] < 0.0) == (v[b] < 0.0) {
                        continue;
                    }
                    let t = v[a] / (v[a] - v[b]);
                    for k in 0..3 {
                        s[k] += CO[a][k] as f32 + (CO[b][k] as f32 - CO[a][k] as f32) * t;
                    }
                    c += 1.0;
                }
                let g = [
                    (v[1] + v[3] + v[5] + v[7]) - (v[0] + v[2] + v[4] + v[6]),
                    (v[2] + v[3] + v[6] + v[7]) - (v[0] + v[1] + v[4] + v[5]),
                    (v[4] + v[5] + v[6] + v[7]) - (v[0] + v[1] + v[2] + v[3]),
                ];
                let gl = norm(g).max(1e-12);
                vid[x + nx * y + sxy * z] = out.positions.len() as i32;
                out.positions.push([
                    min[0] + (x as f32 + s[0] / c) * cell,
                    min[1] + (y as f32 + s[1] / c) * cell,
                    min[2] + (z as f32 + s[2] / c) * cell,
                ]);
                out.normals.push([g[0] / gl, g[1] / gl, g[2] / gl]);
            }
        }
    }
    let id = |x: usize, y: usize, z: usize| vid[x + nx * y + sxy * z];
    let quad = |q: [i32; 4], out: &mut Envelope| {
        if q.iter().any(|&i| i < 0) {
            return;
        }
        let q = q.map(|i| i as u32);
        out.indices.extend_from_slice(&[q[0], q[1], q[2], q[0], q[2], q[3]]);
    };
    for z in 1..nz - 1 {
        for y in 1..ny - 1 {
            for x in 1..nx - 1 {
                let s0 = f(x, y, z) < 0.0;
                if (f(x + 1, y, z) < 0.0) != s0 {
                    quad(
                        [id(x, y - 1, z - 1), id(x, y, z - 1), id(x, y, z), id(x, y - 1, z)],
                        &mut out,
                    );
                }
                if (f(x, y + 1, z) < 0.0) != s0 {
                    quad(
                        [id(x - 1, y, z - 1), id(x - 1, y, z), id(x, y, z), id(x, y, z - 1)],
                        &mut out,
                    );
                }
                if (f(x, y, z + 1) < 0.0) != s0 {
                    quad(
                        [id(x - 1, y - 1, z), id(x, y - 1, z), id(x, y, z), id(x - 1, y, z)],
                        &mut out,
                    );
                }
            }
        }
    }
    // each triangle faces along the field gradient (outwards)
    for t in out.indices.as_chunks_mut::<3>().0 {
        let (a, b, c) = (
            out.positions[t[0] as usize],
            out.positions[t[1] as usize],
            out.positions[t[2] as usize],
        );
        if dot(cross(sub(b, a), sub(c, a)), out.normals[t[0] as usize]) < 0.0 {
            t.swap(1, 2);
        }
    }
    out
}

/// The envelope around a triangle soup (model space).
pub fn envelope(pos: &[[f32; 3]], idx: &[u32], s: Settings) -> Envelope {
    extract(&build_field(pos, idx, s.cells), s.offset)
}

// ---------------------------------------------------------------- small vector helpers

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn norm(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
