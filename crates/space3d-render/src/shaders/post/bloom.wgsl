// Space3d bloom (graphics spec §5.2 PO1, design-post §5.3; Jimenez, "Next-Generation Post
// Processing in Call of Duty: Advanced Warfare", 2014). The renderer prepends OUTPUT_WGSL and
// FULLSCREEN_WGSL. Three entry points share one layout, each drawn as a full-screen triangle
// over one level of the post group's bloom chain:
//
// * `fs_prefilter`: S (the post group's rect) → level 0. 13 taps in 5 boxes; each box is exposed
//   (bloom works in exposed units: the composite adds it after exposure), soft-thresholded and
//   weighted by its Karis factor 1 / (1 + luma) (normalized: a hot pixel cannot dominate its
//   neighbourhood, so single-pixel fireflies do not flicker into large blobs).
// * `fs_down`: level i → level i + 1, the same 13 taps with the plain weights.
// * `fs_up`: level i + 1 → level i, a 3×3 tent, blended with src × scatter + dst × (1 − scatter)
//   (the pipeline's blend constant), so level 0 ends up holding Σ (1 − s)·s^k · level_k.
//
// Only explicit-LOD sampling: no derivatives, no uniformity constraints (WebGL2-safe). Each pass
// samples one level view while it renders into another (per-level views; GL binds them with
// TEXTURE_BASE_LEVEL/MAX_LEVEL, like `generate_mips`).

// Matches `GpuBloomPass` (post/bloom.rs): one aligned uniform slot per pass.
struct BloomPass {
    // Source rect in pixels of the source texture: the post group's rect in S for the
    // prefilter, the whole source level otherwise.
    src_rect: vec4<f32>,
    // 1 / source width, 1 / source height, tap step in source pixels, exposure multiplier.
    src_texel: vec4<f32>,
    // Soft threshold: t, t − k, 2k, 0.25 / k (k = t × soft_knee; 0 for a hard cut).
    threshold: vec4<f32>,
    // x: multiply by 2^E (auto exposure), y..w: 0.
    flags: vec4<u32>,
}

@group(0) @binding(0) var<uniform> bloom: BloomPass;
@group(0) @binding(1) var bloom_sampler: sampler;
@group(1) @binding(0) var bloom_src: texture_2d<f32>;
@group(1) @binding(1) var bloom_exposure: texture_2d<f32>;

// Largest value the chain stores (below the half-float and 11/10-bit float maxima): keeps an
// infinite scene value from turning the whole chain into infinities or NaNs.
const S3D_BLOOM_MAX: f32 = 65000.0;

// Bilinear tap of the scene at source pixel position `p` (texel centers at +0.5), clamped to the
// post group's rect (split screens never bleed into each other) and to 0..S3D_BLOOM_MAX
// (negative, infinite or NaN scene values never enter the chain).
fn bloom_tap_scene(p: vec2<f32>) -> vec3<f32> {
    let lo = bloom.src_rect.xy + 0.5;
    let hi = max(lo, bloom.src_rect.xy + bloom.src_rect.zw - 0.5);
    let q = clamp(p, lo, hi);
    let c = textureSampleLevel(bloom_src, bloom_sampler, q * bloom.src_texel.xy, 0.0).rgb;
    return clamp(c, vec3<f32>(0.0), vec3<f32>(S3D_BLOOM_MAX));
}

// Bilinear tap of a chain level (the whole level is the source rect: the sampler's clamp to edge
// equals clamping to its texel centers; the values are already sanitized).
fn bloom_tap(p: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(bloom_src, bloom_sampler, p * bloom.src_texel.xy, 0.0).rgb;
}

// The 13 taps around the destination pixel at `uv` (0..1 over the destination level), `step`
// source pixels apart:
//   a . b . c
//   . j . k .
//   d . e . f
//   . l . m .
//   g . h . i
struct Taps13 {
    a: vec3<f32>, b: vec3<f32>, c: vec3<f32>,
    d: vec3<f32>, e: vec3<f32>, f: vec3<f32>,
    g: vec3<f32>, h: vec3<f32>, i: vec3<f32>,
    j: vec3<f32>, k: vec3<f32>, l: vec3<f32>, m: vec3<f32>,
}

// One tap of the scene (`scene`: rect-clamped and sanitized) or of a chain level.
fn bloom_tap_of(p: vec2<f32>, scene: bool) -> vec3<f32> {
    if (scene) {
        return bloom_tap_scene(p);
    }
    return bloom_tap(p);
}

fn taps13(uv: vec2<f32>, scene: bool) -> Taps13 {
    let p = bloom.src_rect.xy + uv * bloom.src_rect.zw;
    let s = bloom.src_texel.z;
    var t: Taps13;
    t.a = bloom_tap_of(p + vec2<f32>(-2.0, -2.0) * s, scene);
    t.b = bloom_tap_of(p + vec2<f32>(0.0, -2.0) * s, scene);
    t.c = bloom_tap_of(p + vec2<f32>(2.0, -2.0) * s, scene);
    t.d = bloom_tap_of(p + vec2<f32>(-2.0, 0.0) * s, scene);
    t.e = bloom_tap_of(p, scene);
    t.f = bloom_tap_of(p + vec2<f32>(2.0, 0.0) * s, scene);
    t.g = bloom_tap_of(p + vec2<f32>(-2.0, 2.0) * s, scene);
    t.h = bloom_tap_of(p + vec2<f32>(0.0, 2.0) * s, scene);
    t.i = bloom_tap_of(p + vec2<f32>(2.0, 2.0) * s, scene);
    t.j = bloom_tap_of(p + vec2<f32>(-1.0, -1.0) * s, scene);
    t.k = bloom_tap_of(p + vec2<f32>(1.0, -1.0) * s, scene);
    t.l = bloom_tap_of(p + vec2<f32>(-1.0, 1.0) * s, scene);
    t.m = bloom_tap_of(p + vec2<f32>(1.0, 1.0) * s, scene);
    return t;
}

// Soft-knee threshold on the brightest channel (continuous at t − k and t + k).
fn bloom_threshold(c: vec3<f32>) -> vec3<f32> {
    let t = bloom.threshold;
    let br = max(c.r, max(c.g, c.b));
    var rq = clamp(br - t.y, 0.0, t.z);
    rq = t.w * rq * rq;
    return c * (max(rq, br - t.x) / max(br, 1e-4));
}

// The exposure multiplier of the prefilter (manual, or × 2^E with auto exposure).
fn bloom_exposure_multiplier() -> f32 {
    var e = bloom.src_texel.w;
    if (bloom.flags.x != 0u) {
        e = e * exp2(textureLoad(bloom_exposure, vec2<i32>(0), 0).r);
    }
    return e;
}

// Karis-weighted box: its weight × 1 / (1 + luma) (luma of the exposed box).
fn bloom_karis(box_exposed: vec3<f32>, weight: f32) -> f32 {
    return weight / (1.0 + s3d_luma(box_exposed));
}

@fragment
fn fs_prefilter(in: FullscreenOut) -> @location(0) vec4<f32> {
    let t = taps13(in.uv, true);
    let e = bloom_exposure_multiplier();
    let b0 = (t.a + t.b + t.d + t.e) * (0.25 * e);
    let b1 = (t.b + t.c + t.e + t.f) * (0.25 * e);
    let b2 = (t.d + t.e + t.g + t.h) * (0.25 * e);
    let b3 = (t.e + t.f + t.h + t.i) * (0.25 * e);
    let b4 = (t.j + t.k + t.l + t.m) * (0.25 * e);
    let w0 = bloom_karis(b0, 0.125);
    let w1 = bloom_karis(b1, 0.125);
    let w2 = bloom_karis(b2, 0.125);
    let w3 = bloom_karis(b3, 0.125);
    let w4 = bloom_karis(b4, 0.5);
    let sum = bloom_threshold(b0) * w0 + bloom_threshold(b1) * w1 + bloom_threshold(b2) * w2
        + bloom_threshold(b3) * w3 + bloom_threshold(b4) * w4;
    let c = sum / (w0 + w1 + w2 + w3 + w4);
    return vec4<f32>(min(c, vec3<f32>(S3D_BLOOM_MAX)), 1.0);
}

@fragment
fn fs_down(in: FullscreenOut) -> @location(0) vec4<f32> {
    let t = taps13(in.uv, false);
    let c = t.e * 0.125 + (t.a + t.c + t.g + t.i) * 0.03125 + (t.b + t.d + t.f + t.h) * 0.0625
        + (t.j + t.k + t.l + t.m) * 0.125;
    return vec4<f32>(c, 1.0);
}

@fragment
fn fs_up(in: FullscreenOut) -> @location(0) vec4<f32> {
    let p = bloom.src_rect.xy + in.uv * bloom.src_rect.zw;
    let s = bloom.src_texel.z;
    var c = bloom_tap(p) * 4.0;
    c += (bloom_tap(p + vec2<f32>(-s, 0.0)) + bloom_tap(p + vec2<f32>(s, 0.0))
        + bloom_tap(p + vec2<f32>(0.0, -s)) + bloom_tap(p + vec2<f32>(0.0, s))) * 2.0;
    c += bloom_tap(p + vec2<f32>(-s, -s)) + bloom_tap(p + vec2<f32>(s, -s))
        + bloom_tap(p + vec2<f32>(-s, s)) + bloom_tap(p + vec2<f32>(s, s));
    return vec4<f32>(c * (1.0 / 16.0), 1.0);
}
