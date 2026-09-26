// Space3d particles, GPU backend: shared declarations of the compute kernels (design-particles
// §6, §7). Prepended to `simulate.wgsl` and `compact.wgsl`.
//
// Every function mirrors its Rust counterpart in `cpu.rs` / `rng.rs` operation for operation
// (glam's formulas included), so the two backends keep identical alive sets and slots and
// positions within float rounding. Integer parts (RNG, ring slots, sub-tick times) are
// bit-identical.

// A pool slot (`cpu::Particle`, 48 bytes). `vel_life.w == 0`: dead.
struct S3dpParticle {
    pos_age: vec4<f32>,
    vel_life: vec4<f32>,
    // x: octahedral emitter X axis, y: seed, z: layer instance (entry), w: octahedral emitter
    // Y axis.
    misc: vec4<u32>,
}

// Tables (`gpu::tables`), `vec4<u32>` words: 24 per entry (layer instance) from 0, then the
// per-frame jobs and groups of the compaction.
@group(0) @binding(1) var<storage, read> tables: array<vec4<u32>>;

const S3DP_ENTRY_WORDS: u32 = 24u;
const S3DP_TAU: f32 = 6.2831855;
const S3DP_PI: f32 = 3.1415927;
const S3DP_GOLDEN: u32 = 0x9E3779B9u;

// Random streams (`rng::stream`).
const S3DP_LIFETIME: u32 = 0u;
const S3DP_SPEED: u32 = 1u;
const S3DP_SHAPE_A: u32 = 2u;
const S3DP_SHAPE_B: u32 = 3u;
const S3DP_SHAPE_C: u32 = 4u;
const S3DP_DIR_A: u32 = 5u;
const S3DP_DIR_B: u32 = 6u;
const S3DP_SIZE: u32 = 7u;
const S3DP_ROTATION: u32 = 8u;
const S3DP_ANGULAR: u32 = 9u;
const S3DP_COLOR: u32 = 10u;
const S3DP_COVERAGE: u32 = 11u;
const S3DP_DELAY: u32 = 12u;
const S3DP_MIRROR: u32 = 13u;
const S3DP_FLICKER: u32 = 14u;
const S3DP_BITS: u32 = 15u;
const S3DP_SPREAD_A: u32 = 16u;
const S3DP_SPREAD_B: u32 = 17u;

// Entry flags (word 1.x, `gpu::tables::flags`).
const S3DP_LOCAL: u32 = 1u;
const S3DP_EMITTER_PLANE: u32 = 2u;
const S3DP_MIRRORED: u32 = 4u;
const S3DP_PREMULTIPLIED: u32 = 8u;
const S3DP_TINTED: u32 = 16u;
const S3DP_FLICKERS: u32 = 32u;
const S3DP_PLANE: u32 = 64u;
const S3DP_AGE: u32 = 128u;

fn s3dp_word(e: u32, k: u32) -> vec4<u32> {
    return tables[e * S3DP_ENTRY_WORDS + k];
}

fn s3dp_wordf(e: u32, k: u32) -> vec4<f32> {
    return bitcast<vec4<f32>>(tables[e * S3DP_ENTRY_WORDS + k]);
}

// `rng::pcg` (PCG-RXS-M-XS).
fn s3dp_pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

// `rng::rnd`: uniform in [0, 1) with 24 bits.
fn s3dp_rnd(seed: u32, k: u32) -> f32 {
    return f32(s3dp_pcg(seed + k * S3DP_GOLDEN) >> 8u) * (1.0 / 16777216.0);
}

// `rng::range`.
fn s3dp_range(seed: u32, k: u32, lo: f32, hi: f32) -> f32 {
    return lo + (hi - lo) * s3dp_rnd(seed, k);
}

// `rng::range_exact`: one rounding on both backends.
fn s3dp_range_exact(seed: u32, k: u32, lo: f32, hi: f32) -> f32 {
    return fma(hi - lo, s3dp_rnd(seed, k), lo);
}

// `rng::particle_seed` of the 64-bit spawn index `(hi << 32) | lo`.
fn s3dp_particle_seed(entry_seed: u32, lo: u32, hi: u32) -> u32 {
    return s3dp_pcg(entry_seed ^ s3dp_pcg(lo ^ s3dp_pcg(hi ^ 0x68E31DA4u)));
}

// `f32::signum`: ±1 by the sign bit (−0 gives −1).
fn s3dp_sign(x: f32) -> f32 {
    return select(1.0, -1.0, (bitcast<u32>(x) & 0x80000000u) != 0u);
}

// glam `normalize_or`.
fn s3dp_normalize_or(v: vec3<f32>, fallback: vec3<f32>) -> vec3<f32> {
    let len = length(v);
    if (len > 1e-37 && len < 3.0e38) {
        return v * (1.0 / len);
    }
    return fallback;
}

// glam `Quat * Vec3`.
fn s3dp_rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let b = q.xyz;
    let w = q.w;
    return v * (w * w - dot(b, b)) + b * (dot(v, b) * 2.0) + cross(b, v) * (w * 2.0);
}

// glam `Quat::lerp` (shortest path, normalized).
fn s3dp_qlerp(a: vec4<f32>, b: vec4<f32>, s: f32) -> vec4<f32> {
    let end = b * s3dp_sign(dot(a, b));
    return normalize(a * (1.0 - s) + end * s);
}

// glam `Vec3::any_orthonormal_pair` (Pixar's branchless basis).
fn s3dp_orthonormal_pair(n: vec3<f32>) -> mat2x3<f32> {
    let sign = s3dp_sign(n.z);
    let a = -1.0 / (sign + n.z);
    let b = n.x * n.y * a;
    return mat2x3<f32>(
        vec3<f32>(1.0 + sign * n.x * n.x * a, sign * b, -sign * n.x),
        vec3<f32>(b, sign + n.y * n.y * a, -n.y),
    );
}

// `cpu::uniform_dir`.
fn s3dp_uniform_dir(u: f32, v: f32) -> vec3<f32> {
    let z = 1.0 - 2.0 * u;
    let r = sqrt(max(1.0 - z * z, 0.0));
    let a = S3DP_TAU * v;
    return vec3<f32>(r * cos(a), r * sin(a), z);
}

// `cpu::cone_dir`.
fn s3dp_cone_dir(axis: vec3<f32>, half: f32, u: f32, v: f32) -> vec3<f32> {
    let cos_t = 1.0 - u * (1.0 - cos(min(half, S3DP_PI)));
    let sin_t = sqrt(max(1.0 - cos_t * cos_t, 0.0));
    let a = S3DP_TAU * v;
    let basis = s3dp_orthonormal_pair(axis);
    return s3dp_normalize_or(axis * cos_t + (basis[0] * cos(a) + basis[1] * sin(a)) * sin_t, axis);
}

// `pack::oct_encode`.
fn s3dp_oct_encode(v: vec3<f32>) -> u32 {
    let n = s3dp_normalize_or(v, vec3<f32>(0.0, 1.0, 0.0));
    let l1 = abs(n.x) + abs(n.y) + abs(n.z);
    var p = n.xy / l1;
    if (n.z < 0.0) {
        let s = vec2<f32>(s3dp_sign(p.x), s3dp_sign(p.y));
        p = (vec2<f32>(1.0) - abs(p.yx)) * s;
    }
    let q = vec2<i32>(round(clamp(p, vec2<f32>(-1.0), vec2<f32>(1.0)) * 32767.0));
    return (bitcast<u32>(q.x) & 0xffffu) | (bitcast<u32>(q.y) << 16u);
}

// `pack::oct_decode`.
fn s3dp_oct_decode(w: u32) -> vec3<f32> {
    let p = vec2<f32>(f32(bitcast<i32>(w << 16u) >> 16u), f32(bitcast<i32>(w) >> 16u)) / 32767.0;
    var n = vec3<f32>(p.x, p.y, 1.0 - abs(p.x) - abs(p.y));
    if (n.z < 0.0) {
        let s = vec2<f32>(s3dp_sign(n.x), s3dp_sign(n.y));
        let q = (vec2<f32>(1.0) - abs(n.yx)) * s;
        n = vec3<f32>(q.x, q.y, n.z);
    }
    return s3dp_normalize_or(n, vec3<f32>(0.0, 1.0, 0.0));
}

// Linear 2D dispatch index: `(x + y · groups_x) · 64 + lane` (grids wider than 65535 groups
// wrap into y).
fn s3dp_thread(gid: vec3<u32>, groups: vec3<u32>) -> u32 {
    return gid.x + gid.y * groups.x * 64u;
}
