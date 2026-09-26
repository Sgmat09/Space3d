// Space3d particles: declarations shared by the sprite and strip shaders (graphics spec §2.5,
// design-particles §8). Composed per pass signature as: `S3DP_OUTPUT` / `S3DP_MANUAL_DEPTH`
// consts, `VIEW_WGSL` (group 0), `scene_wgsl(target)` (group 1), `OUTPUT_WGSL`, the texture
// declarations of the variant (group 3: `s3dp_tex`, a stub without a texture), this file
// (group 2), then `sprite.wgsl` or `strip.wgsl`.
//
// Blending is premultiplied (`One, OneMinusSrcAlpha`): additive draws write alpha 0,
// alpha-blended ones their opacity, premultiplied ones their coverage. No stage reads storage
// buffers (WebGL2); curves are sampled from the LUT in the vertex shaders.

struct S3dpView {
    // x: 1 = soft-clip the color (`1 − e^−x`, `FrameMode::Direct` only), y: 1 = soft particles.
    flags: vec4<u32>,
    // x: largest sprite, as a fraction of the viewport height.
    params: vec4<f32>,
}

struct S3dpDraw {
    // x: color row (LUT v), y: curve row (LUT v), z: intensity, w: aspect.
    lut: vec4<f32>,
    // x: blend (0 additive, 1 alpha, 2 premultiplied), y: facing (0 camera, 1 velocity,
    // 2 plane), z: shape (0 glow, 1 streak, 2 ring, 3 smoke, 4 vortex, 5 texture), w: 0.
    mode: vec4<u32>,
    shape: vec4<f32>,
    // x: seconds of velocity, y: minimum stretch, z: maximum stretch, w: soft distance.
    stretch: vec4<f32>,
    // x, y: near fade (invisible at x, full at y; off when y <= x); z: the layer's largest
    // sprite as a fraction of the viewport height (0: the view's).
    fade: vec4<f32>,
    // Flipbook: x frames, y atlas columns, z atlas rows, w mode (0 none, 1 over life, 2 fps,
    // 3 random, 4 random start + fps).
    book: vec4<f32>,
    // Flipbook: x cycles or fps, y blend (0/1), z texture layers (built-ins), w 1 when the
    // instances carry rotation and age in seconds as two halves.
    book2: vec4<f32>,
}

// Strips (ribbons, beams) reuse the draw parameters: lut = (color row, width row, intensity,
// width), mode = (blend, facing 0 camera / 1 plane, 6, textured), shape = (core, hardness,
// noise, 0), stretch = (plane normal, soft distance).

@group(2) @binding(0) var s3dp_lut: texture_2d<f32>;
@group(2) @binding(1) var s3dp_lut_sampler: sampler;
@group(2) @binding(2) var<uniform> s3dp_view: S3dpView;
@group(2) @binding(3) var<uniform> s3dp_draw: S3dpDraw;

// `s3d_pcg` (bit-identical to the Rust `rng::pcg`).
fn s3dp_pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

// Uniform in [0, 1) with 24 bits (the Rust `rng::rnd`).
fn s3dp_rnd(seed: u32, k: u32) -> f32 {
    return f32(s3dp_pcg(seed + k * 0x9E3779B9u) >> 8u) * (1.0 / 16777216.0);
}

fn s3dp_hash(i: vec2<f32>) -> f32 {
    let c = bitcast<vec2<u32>>(vec2<i32>(i));
    return f32(s3dp_pcg(c.x ^ s3dp_pcg(c.y)) >> 8u) * (1.0 / 16777216.0);
}

fn s3dp_noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = p - i;
    let w = f * f * (3.0 - 2.0 * f);
    let a = s3dp_hash(i);
    let b = s3dp_hash(i + vec2<f32>(1.0, 0.0));
    let c = s3dp_hash(i + vec2<f32>(0.0, 1.0));
    let d = s3dp_hash(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, w.x), mix(c, d, w.x), w.y);
}

// Value noise that repeats every 256 lattice cells along x (strips scroll it along their
// length; the CPU keeps their `u` small by whole periods).
fn s3dp_noise_wrap(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = p - i;
    let w = f * f * (3.0 - 2.0 * f);
    let x0 = i.x - 256.0 * floor(i.x / 256.0);
    let x1 = select(x0 + 1.0, 0.0, x0 >= 255.0);
    let a = s3dp_hash(vec2<f32>(x0, i.y));
    let b = s3dp_hash(vec2<f32>(x1, i.y));
    let c = s3dp_hash(vec2<f32>(x0, i.y + 1.0));
    let d = s3dp_hash(vec2<f32>(x1, i.y + 1.0));
    return mix(mix(a, b, w.x), mix(c, d, w.x), w.y);
}

// The value of half-float bits `h` (low 16 bits), in integer arithmetic: `unpack2x16float`
// needs a capability WebGL2-class devices lack.
fn s3dp_half(h: u32) -> f32 {
    let sign = select(1.0, -1.0, (h & 0x8000u) != 0u);
    let e = (h >> 10u) & 0x1fu;
    let m = f32(h & 0x3ffu);
    if (e == 0u) {
        return sign * m * (1.0 / 16777216.0);
    }
    if (e == 31u) {
        return sign * 65504.0;
    }
    return sign * (1.0 + m / 1024.0) * exp2(f32(e) - 15.0);
}

// `normalize(v)`, or `fallback` for a (near) zero vector.
fn s3dp_normalize_or(v: vec3<f32>, fallback: vec3<f32>) -> vec3<f32> {
    let len2 = dot(v, v);
    if (len2 > 1e-20) {
        return v * inverseSqrt(len2);
    }
    return fallback;
}

// In-plane basis of a plane-facing sprite (the Rust `cpu::plane_basis`).
fn s3dp_plane_basis(n: vec3<f32>) -> mat2x3<f32> {
    let helper = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(n.y) > 0.99);
    let bx = normalize(cross(helper, n));
    return mat2x3<f32>(bx, cross(n, bx));
}

// Smallest sprite footprint, in pixels of radius (see `vs_sprite`).
const S3DP_MIN_PX: f32 = 1.5;
// Variance (px²) of the pixel footprint the gaussian profiles are widened by.
const S3DP_PX_VAR: f32 = 0.16;

// A gaussian `exp(−k·d²)` of a sprite `r` pixels in radius, widened by the pixel footprint
// (σ² + S3DP_PX_VAR, in pixels) and scaled to keep its light: `(k', k'/k)`. Unchanged for
// large sprites; a narrow white core on a 2 px spark becomes a wider, dimmer spot instead of a
// point sampled (or missed) by the pixel centers.
fn s3dp_widen(k: f32, r: f32) -> vec2<f32> {
    let r2 = r * r;
    let kk = k * r2 / (r2 + 2.0 * k * S3DP_PX_VAR);
    return vec2<f32>(kk, kk / k);
}
