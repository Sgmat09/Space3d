// Space3d cubemap conversion and image-based-lighting bake (graphics spec §5.2 PM3/PM4,
// design-materials §5.5). The functions below compute one texel of a cube face (or of a 2D
// mip) at `uv` ((0, 0) at the top left) from the source declared below; two sets of entry
// points run them:
// * fragment passes (every backend, WebGL2 included): `FULLSCREEN_WGSL`, this file and
//   `ibl_bake_fragment.wgsl`: one face (`bake.face`) of one mip per pass, drawn into a
//   single-layer 2D view;
// * compute dispatches (backends with compute shaders and storage textures): this file and
//   `ibl_bake_compute.wgsl` (per storage format): all six faces of one mip per dispatch
//   (z = face), many dispatches in one compute pass.
// Cube-source entry points use the layout {0 params, 1 cube, 2 sampler} (+ 4 the output of the
// compute ones), 2D-source ones {0 params, 2 sampler, 3 texture_2d} (+ 4).
//
// Face directions (st = uv·2 − 1, t down): +X (1, −t, −s), −X (−1, −t, s), +Y (s, 1, t),
// −Y (s, −1, −t), +Z (s, −t, 1), −Z (−s, −t, −1) — the cube-map face table of every API.
//
// Sample sets are fixed Hammersley points, so bakes are deterministic. The Rust mirrors of
// `s3d_radical_inverse` and the GGX sampling are in `lighting/environment.rs` (BRDF LUT).

struct BakeParams {
    face: u32,
    samples: u32,
    // Bit 0: compute outputs encode sRGB themselves (an sRGB texture written through its linear
    // `Rgba8Unorm` storage view).
    flags: u32,
    pad: u32,
    // GGX roughness (perceptual) of a prefilter pass.
    roughness: f32,
    // Face edge of the source's mip 0 in texels (cube sources).
    src_size: f32,
    // Last mip of the source.
    src_max_lod: f32,
    // Explicit source LOD of copies (`fs_cube_copy`, `fs_copy_2d`, `fs_equirect`, and the
    // roughness-0 prefilter mip).
    lod: f32,
}

@group(0) @binding(0) var<uniform> bake: BakeParams;
@group(0) @binding(1) var s3d_bake_cube: texture_cube<f32>;
@group(0) @binding(2) var s3d_bake_sampler: sampler;
@group(0) @binding(3) var s3d_bake_2d: texture_2d<f32>;

const S3D_BAKE_PI: f32 = 3.141592653589793;

fn s3d_face_dir(face: u32, uv: vec2<f32>) -> vec3<f32> {
    let st = uv * 2.0 - 1.0;
    var d: vec3<f32>;
    switch face {
        case 0u: { d = vec3<f32>(1.0, -st.y, -st.x); }
        case 1u: { d = vec3<f32>(-1.0, -st.y, st.x); }
        case 2u: { d = vec3<f32>(st.x, 1.0, st.y); }
        case 3u: { d = vec3<f32>(st.x, -1.0, -st.y); }
        case 4u: { d = vec3<f32>(st.x, -st.y, 1.0); }
        default: { d = vec3<f32>(-st.x, -st.y, -1.0); }
    }
    return normalize(d);
}

// Van der Corput radical inverse in base 2 (bit reversal without `reverseBits`, which GLSL ES
// 3.00 lacks).
fn s3d_radical_inverse(i: u32) -> f32 {
    var b = (i << 16u) | (i >> 16u);
    b = ((b & 0x55555555u) << 1u) | ((b & 0xAAAAAAAAu) >> 1u);
    b = ((b & 0x33333333u) << 2u) | ((b & 0xCCCCCCCCu) >> 2u);
    b = ((b & 0x0F0F0F0Fu) << 4u) | ((b & 0xF0F0F0F0u) >> 4u);
    b = ((b & 0x00FF00FFu) << 8u) | ((b & 0xFF00FF00u) >> 8u);
    return f32(b) * 2.3283064365386963e-10;
}

// An orthonormal frame around `n` (columns: tangent, bitangent, n).
fn s3d_basis(n: vec3<f32>) -> mat3x3<f32> {
    let up = select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 1.0), abs(n.z) < 0.999);
    let t = normalize(cross(up, n));
    return mat3x3<f32>(t, cross(n, t), n);
}

// Solid angle of one texel of the source's mip 0.
fn s3d_texel_solid_angle() -> f32 {
    return 4.0 * S3D_BAKE_PI / (6.0 * bake.src_size * bake.src_size);
}

// Filtered importance sampling (Křivánek & Colbert 2008): the source mip whose texels cover the
// solid angle `1 / (N · pdf)` of one sample, one level blurrier.
fn s3d_sample_lod(pdf: f32) -> f32 {
    let omega_s = 1.0 / (f32(bake.samples) * pdf + 1e-6);
    return clamp(0.5 * log2(omega_s / s3d_texel_solid_angle()) + 1.0, 0.0, bake.src_max_lod);
}

// A cube face from another cube (face conversion, radiance mips: the source view is the
// previous mip, sampled bilinearly at the destination texel's direction = a 2×2 average).
fn s3d_bake_cube_copy(face: u32, uv: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(s3d_bake_cube, s3d_bake_sampler, s3d_face_dir(face, uv), bake.lod).rgb;
}

// A mip of a 2D texture from the previous one (panorama mips).
fn s3d_bake_copy_2d(uv: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(s3d_bake_2d, s3d_bake_sampler, uv, bake.lod).rgb;
}

// A cube face from an equirectangular panorama: longitude 0 (u = 0.5) looks down −Z, +X is to
// its right, the top row is +Y.
fn s3d_bake_equirect(face: u32, uv: vec2<f32>) -> vec3<f32> {
    let d = s3d_face_dir(face, uv);
    let u = 0.5 + atan2(d.x, -d.z) / (2.0 * S3D_BAKE_PI);
    let v = acos(clamp(d.y, -1.0, 1.0)) / S3D_BAKE_PI;
    return textureSampleLevel(s3d_bake_2d, s3d_bake_sampler, vec2<f32>(u, v), bake.lod).rgb;
}

// One mip of the GGX-prefiltered specular cube (split-sum, V = N = R): `bake.samples` GGX
// importance samples of the source radiance, weighted by n·l. Roughness 0 is a copy at the
// source LOD matching the destination's resolution.
fn s3d_bake_prefilter(face: u32, uv: vec2<f32>) -> vec3<f32> {
    let n = s3d_face_dir(face, uv);
    if (bake.roughness <= 0.0) {
        return textureSampleLevel(s3d_bake_cube, s3d_bake_sampler, n, bake.lod).rgb;
    }
    let a = bake.roughness * bake.roughness;
    let a2 = a * a;
    let basis = s3d_basis(n);
    var sum = vec3<f32>(0.0);
    var weight = 0.0;
    for (var i = 0u; i < bake.samples; i++) {
        let xi = vec2<f32>(f32(i) / f32(bake.samples), s3d_radical_inverse(i));
        let phi = 2.0 * S3D_BAKE_PI * xi.x;
        let cos_t = sqrt((1.0 - xi.y) / (1.0 + (a2 - 1.0) * xi.y));
        let sin_t = sqrt(max(1.0 - cos_t * cos_t, 0.0));
        let h = basis * vec3<f32>(sin_t * cos(phi), sin_t * sin(phi), cos_t);
        let l = 2.0 * dot(n, h) * h - n;
        let ndl = dot(n, l);
        if (ndl > 0.0) {
            // pdf of l = D(h)·(n·h) / (4·(v·h)) with v = n: D / 4.
            let dd = cos_t * cos_t * (a2 - 1.0) + 1.0;
            let d = a2 / (S3D_BAKE_PI * dd * dd);
            let lod = s3d_sample_lod(d * 0.25);
            sum += textureSampleLevel(s3d_bake_cube, s3d_bake_sampler, l, lod).rgb * ndl;
            weight += ndl;
        }
    }
    return sum / max(weight, 1e-6);
}

// The irradiance cube, stored as the cosine-weighted mean radiance E/π (a white Lambert
// surface's outgoing radiance): `bake.samples` cosine-distributed samples.
fn s3d_bake_irradiance(face: u32, uv: vec2<f32>) -> vec3<f32> {
    let n = s3d_face_dir(face, uv);
    let basis = s3d_basis(n);
    var sum = vec3<f32>(0.0);
    for (var i = 0u; i < bake.samples; i++) {
        let xi = vec2<f32>(f32(i) / f32(bake.samples), s3d_radical_inverse(i));
        let phi = 2.0 * S3D_BAKE_PI * xi.x;
        let cos_t = sqrt(1.0 - xi.y);
        let sin_t = sqrt(xi.y);
        let l = basis * vec3<f32>(sin_t * cos(phi), sin_t * sin(phi), cos_t);
        let lod = s3d_sample_lod(cos_t / S3D_BAKE_PI);
        sum += textureSampleLevel(s3d_bake_cube, s3d_bake_sampler, l, lod).rgb;
    }
    return sum / f32(max(bake.samples, 1u));
}
