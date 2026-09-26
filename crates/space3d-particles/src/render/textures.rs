//! Particle textures: the procedural built-ins (`BuiltinTexture`: generated deterministically on
//! the CPU, with their mip chains, once per process when an effect using one is registered —
//! usually at startup, never in a render frame) and the bind groups of every texture a render
//! layer samples (group 3 of the sprite and strip pipelines).
//!
//! `Fireball16` and `Smoke16` are 16-frame flipbooks in a texture array (`D2Array`); `Sparkle`
//! and `SoftDot` are single textures (`D2`): an array is only ever bound with 2 or more layers,
//! the renderer's rule for WebGL2, where a texture's target is fixed at creation.

use crate::effect::BuiltinTexture;
use crate::rng::pcg;
use space3d_render::wgpu;

/// Size of the built-in textures (mip 0).
pub(crate) const BUILTIN_SIZE: u32 = 64;

/// A generated texture: `layers` layers of `size`², every mip level (each holding all layers,
/// layer after layer), sRGB RGBA8 with straight alpha.
pub(crate) struct Generated {
    pub size: u32,
    pub layers: u32,
    pub mips: Vec<Vec<u8>>,
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Value noise in 0..1 on the integer lattice, seeded.
fn noise(x: f32, y: f32, seed: u32) -> f32 {
    let (ix, iy) = (x.floor(), y.floor());
    let (fx, fy) = (x - ix, y - iy);
    let (wx, wy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let h = |dx: i32, dy: i32| {
        let k = pcg((ix as i32 + dx) as u32 ^ pcg((iy as i32 + dy) as u32 ^ seed));
        (k >> 8) as f32 * (1.0 / 16_777_216.0)
    };
    let a = h(0, 0) + (h(1, 0) - h(0, 0)) * wx;
    let b = h(0, 1) + (h(1, 1) - h(0, 1)) * wx;
    a + (b - a) * wy
}

/// Four octaves of value noise (0..1).
fn fbm(x: f32, y: f32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut f = 1.0;
    for o in 0..4 {
        sum += amp * noise(x * f, y * f, seed.wrapping_add(o * 0x9E37));
        f *= 2.03;
        amp *= 0.5;
    }
    sum / 0.9375
}

fn srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

fn linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Linear color and alpha of built-in `kind`, frame `frame` of `frames`, at `(x, y)` in −1..1.
fn texel(kind: BuiltinTexture, frame: u32, frames: u32, x: f32, y: f32) -> ([f32; 3], f32) {
    let r = (x * x + y * y).sqrt();
    let p = frame as f32 / (frames.max(2) - 1) as f32;
    match kind {
        BuiltinTexture::SoftDot => {
            let a = if r < 1.0 {
                (-r * r * 4.0).exp() * (1.0 - r * r)
            } else {
                0.0
            };
            ([1.0; 3], a)
        }
        BuiltinTexture::Sparkle => {
            // A four-point star with a softer diagonal cross and a round core.
            let (ax, ay) = (x.abs(), y.abs());
            let ray = |along: f32, across: f32| (-across * across * 900.0).exp() * (1.0 - along).max(0.0).powf(1.5);
            let (d1, d2) = (
                (x + y).abs() * std::f32::consts::FRAC_1_SQRT_2,
                (x - y).abs() * std::f32::consts::FRAC_1_SQRT_2,
            );
            let diag = 0.45 * (ray(d1, d2) + ray(d2, d1));
            let a = ((-r * r * 30.0).exp() + ray(ax, ay) + ray(ay, ax) + diag).min(1.0) * smoothstep(1.0, 0.9, r);
            ([1.0, 0.97, 0.92], a)
        }
        BuiltinTexture::Fireball16 => {
            // A billow of fire: it grows, its noise rolls, and it burns from white-hot through
            // orange and red into dark smoke over the 16 frames.
            let grow = 0.72 + 0.28 * p;
            let (sx, sy) = (x / grow, y / grow);
            let rs = (sx * sx + sy * sy).sqrt();
            let n = fbm(sx * 2.2 + p * 1.7, sy * 2.2 - p * 1.3, 0x51f1_5eed);
            let density = smoothstep(1.0, 0.45, rs + (n - 0.5) * (0.55 + 0.35 * p));
            let heat = ((1.0 - rs * rs).max(0.0) * (1.25 - 1.1 * p) + (n - 0.5) * 0.6).clamp(0.0, 1.0);
            let ramp = [
                (0.0, [0.06, 0.05, 0.05]),
                (0.25, [0.45, 0.07, 0.02]),
                (0.5, [1.0, 0.35, 0.06]),
                (0.75, [1.0, 0.72, 0.25]),
                (1.0, [1.0, 0.95, 0.8]),
            ];
            let mut c = ramp[0].1;
            for w in ramp.windows(2) {
                if heat >= w[0].0 {
                    c = lerp3(w[0].1, w[1].1, (heat - w[0].0) / (w[1].0 - w[0].0));
                }
            }
            (c, density * (1.0 - 0.45 * p * p))
        }
        BuiltinTexture::Smoke16 => {
            // A puff of smoke: it spreads and thins; the noise gives it billows lit from the
            // upper left.
            let grow = 0.6 + 0.4 * p;
            let (sx, sy) = (x / grow, y / grow);
            let rs = (sx * sx + sy * sy).sqrt();
            let n = fbm(sx * 2.0 + p * 0.9, sy * 2.0 + p * 0.6, 0x5a0c_e5ed);
            let lit = n - fbm(sx * 2.0 + p * 0.9 + 0.12, sy * 2.0 + p * 0.6 + 0.16, 0x5a0c_e5ed);
            let density = smoothstep(1.0, 0.3, rs + (n - 0.5) * 0.9);
            let shade = (0.62 + 0.5 * (n - 0.5) + lit * 2.2).clamp(0.3, 1.0);
            ([shade, shade, shade * 1.02], density * (1.0 - 0.6 * p))
        }
    }
}

/// The data of built-in `kind`, generated on first use and kept for the process.
pub(crate) fn builtin_data(kind: BuiltinTexture) -> &'static Generated {
    use std::sync::OnceLock;
    static CACHE: [OnceLock<Generated>; 4] = [OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let i = match kind {
        BuiltinTexture::Fireball16 => 0,
        BuiltinTexture::Smoke16 => 1,
        BuiltinTexture::Sparkle => 2,
        BuiltinTexture::SoftDot => 3,
    };
    CACHE[i].get_or_init(|| generate(kind))
}

/// Generate the built-in textures `effect` uses (called when it is registered, so a render frame
/// never pays for it: about 4 ms for a 16-frame one on the Dev machine).
pub(crate) fn prepare_effect(effect: &crate::effect::ParticleEffect) {
    use crate::effect::{SpriteShape, SpriteTexture};
    let builtin = |t: Option<&SpriteTexture>| match t {
        Some(SpriteTexture::Builtin(b)) => Some(*b),
        _ => None,
    };
    let sprites = effect.layers.iter().filter_map(|l| match &l.render.shape {
        SpriteShape::Texture(t) => builtin(Some(t)),
        _ => None,
    });
    let ribbons = effect.ribbons.iter().filter_map(|r| builtin(r.texture.as_ref()));
    let beams = effect.beams.iter().filter_map(|b| builtin(b.texture.as_ref()));
    for kind in sprites.chain(ribbons).chain(beams) {
        builtin_data(kind);
    }
}

/// Generate built-in `kind` (deterministic for a given platform's `powf`).
pub(crate) fn generate(kind: BuiltinTexture) -> Generated {
    let size = BUILTIN_SIZE;
    let layers = super::layers::builtin_layers(kind);
    let mut mip0 = vec![0u8; (size * size * 4 * layers) as usize];
    for l in 0..layers {
        for py in 0..size {
            for px in 0..size {
                let x = (px as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let y = (py as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let (c, a) = texel(kind, l, layers, x, y);
                let o = (((l * size + py) * size + px) * 4) as usize;
                mip0[o..o + 4].copy_from_slice(&[
                    srgb(c[0]),
                    srgb(c[1]),
                    srgb(c[2]),
                    (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                ]);
            }
        }
    }
    let mut mips = vec![mip0];
    let mut s = size;
    while s > 1 {
        let prev = mips.last().expect("mip 0");
        let n = s / 2;
        let mut next = vec![0u8; (n * n * 4 * layers) as usize];
        for l in 0..layers {
            for y in 0..n {
                for x in 0..n {
                    // Premultiplied, linear box filter (no dark fringes at the edges).
                    let mut acc = [0.0f32; 4];
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let o = (((l * s + 2 * y + dy) * s + 2 * x + dx) * 4) as usize;
                        let a = prev[o + 3] as f32 / 255.0;
                        for c in 0..3 {
                            acc[c] += linear(prev[o + c]) * a;
                        }
                        acc[3] += a;
                    }
                    let a = acc[3] / 4.0;
                    let o = (((l * n + y) * n + x) * 4) as usize;
                    for c in 0..3 {
                        next[o + c] = srgb(if acc[3] > 0.0 { acc[c] / acc[3] } else { 0.0 });
                    }
                    next[o + 3] = (a * 255.0 + 0.5) as u8;
                }
            }
        }
        mips.push(next);
        s = n;
    }
    Generated { size, layers, mips }
}

/// A built-in texture on the GPU.
pub(crate) struct GpuBuiltin {
    pub kind: BuiltinTexture,
    pub view: wgpu::TextureView,
    pub layered: bool,
}

/// Create built-in `kind` on the device (written through the queue once).
pub(crate) fn create_builtin(device: &wgpu::Device, queue: &wgpu::Queue, kind: BuiltinTexture) -> GpuBuiltin {
    let g = builtin_data(kind);
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d particles built-in texture"),
        size: wgpu::Extent3d {
            width: g.size,
            height: g.size,
            depth_or_array_layers: g.layers,
        },
        mip_level_count: g.mips.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, data) in g.mips.iter().enumerate() {
        let s = (g.size >> level).max(1);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(s * 4),
                rows_per_image: Some(s),
            },
            wgpu::Extent3d {
                width: s,
                height: s,
                depth_or_array_layers: g.layers,
            },
        );
    }
    let layered = g.layers > 1;
    let view = texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(if layered {
            wgpu::TextureViewDimension::D2Array
        } else {
            wgpu::TextureViewDimension::D2
        }),
        ..Default::default()
    });
    GpuBuiltin { kind, view, layered }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-ins are deterministic, fully mipped, and look like what they are: a soft dot
    /// opaque in the middle and clear at the rim, a fireball that burns out (less light and
    /// coverage in its last frame than its first).
    #[test]
    fn builtins_generate_deterministically() {
        for kind in [
            BuiltinTexture::SoftDot,
            BuiltinTexture::Sparkle,
            BuiltinTexture::Fireball16,
            BuiltinTexture::Smoke16,
        ] {
            let a = generate(kind);
            let b = generate(kind);
            assert_eq!(a.mips, b.mips, "{kind:?}");
            assert_eq!(a.mips.len(), 7, "64² down to 1²");
            assert_eq!(a.mips[6].len(), 4 * a.layers as usize);
        }
        let dot = generate(BuiltinTexture::SoftDot);
        let px = |g: &Generated, l: u32, x: u32, y: u32| {
            let o = (((l * g.size + y) * g.size + x) * 4) as usize;
            [g.mips[0][o], g.mips[0][o + 1], g.mips[0][o + 2], g.mips[0][o + 3]]
        };
        assert!(px(&dot, 0, 32, 32)[3] > 240 && px(&dot, 0, 0, 0)[3] == 0);
        let t = std::time::Instant::now();
        let fire = generate(BuiltinTexture::Fireball16);
        eprintln!("Fireball16 generated in {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(fire.layers, 16);
        let sum = |l: u32| -> (u32, u32) {
            let mut s = (0, 0);
            for y in 0..64 {
                for x in 0..64 {
                    let p = px(&fire, l, x, y);
                    s.0 += p[0] as u32 * p[3] as u32 / 255;
                    s.1 += p[3] as u32;
                }
            }
            s
        };
        let (first, last) = (sum(0), sum(15));
        assert!(first.0 > last.0 * 2, "burns out: {first:?} → {last:?}");
    }
}
