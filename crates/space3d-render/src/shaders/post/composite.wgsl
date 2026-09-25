// Space3d composite (graphics spec §1.9): the one pass that turns a post group's scene color S
// into the display target D (or, with FXAA, into the LDR intermediate L that the FXAA pass reads).
// The renderer prepends OUTPUT_WGSL, the hash functions and FULLSCREEN_WGSL. Every effect is a
// uniform bit (`PostParams::flags.x`); the output kind is the pipeline override `S3D_POST_OUT`.
// Only explicit-LOD sampling and loads: no derivatives, so no uniformity constraints
// (WebGL2-safe).
//
// Order (normative): S at the rect [+ chromatic aberration taps] → un-premultiply (transparent
// groups) → exposure → bloom → vignette → [white balance, saturation, contrast, HDR hook] →
// tonemap → [lift/gamma/gain, LUT, LDR hook] → in the last pass: grain → dither → encode →
// re-premultiply. Bracketed steps belong to later post milestones; this file keeps their slots.

// 0: D has an sRGB format (hardware encodes), 1: 8-bit D without one (encode here),
// 2: float D (linear, no dither).
override S3D_POST_OUT: u32 = 0u;

const S3D_POST_BLOOM: u32 = 1u;
const S3D_POST_AUTO_EXPOSURE: u32 = 2u;
const S3D_POST_VIGNETTE: u32 = 4u;
const S3D_POST_CA: u32 = 8u;
const S3D_POST_GRAIN: u32 = 16u;
const S3D_POST_DITHER: u32 = 32u;
const S3D_POST_GRADE: u32 = 64u;
const S3D_POST_LUT: u32 = 128u;
const S3D_POST_DISTORTION: u32 = 256u;
const S3D_POST_TRANSPARENT: u32 = 512u;
const S3D_POST_SCENE_ALPHA: u32 = 1024u;
const S3D_POST_EXACT: u32 = 2048u;

// Matches `GpuPost` (post/chain.rs): one 256-byte slot per post group.
struct PostParams {
    out_rect: vec4<f32>,        // the group's rect in D pixels (x, y, w, h), unclipped
    src_rect: vec4<f32>,        // its rect in S pixels, unclipped
    src_texel: vec4<f32>,       // 1/S.w, 1/S.h, rect aspect (w/h), D height
    exposure: vec4<f32>,        // linear multiplier, -, bloom intensity, bloom mode (0 add, 1 mix)
    vignette: vec4<f32>,        // intensity, smoothness, roundness, center.x
    vignette_color: vec4<f32>,  // linear multiplier at full vignette (rgb), center.y
    lens: vec4<f32>,            // chromatic aberration scale (S px⁻²), -, distortion scale, distortion chromatic
    grain: vec4<f32>,           // intensity, cell size (D px), luminance response, -
    grade_matrix: mat3x3<f32>,
    lift_contrast: vec4<f32>,
    gamma_lut: vec4<f32>,
    gain_lut: vec4<f32>,
    flags: vec4<u32>,           // x effect bits, y tonemapper id, z 1 = last pass (0: FXAA follows), w -
    frame: vec4<u32>,           // pre-hashed seeds (s3d_pcg(seed)): x grain, y dither, z dither (second)
}

@group(0) @binding(0) var<uniform> post: PostParams;
@group(0) @binding(1) var post_sampler: sampler;
@group(1) @binding(0) var scene_tex: texture_2d<f32>;
@group(1) @binding(1) var bloom_tex: texture_2d<f32>;
@group(1) @binding(2) var exposure_tex: texture_2d<f32>;
@group(1) @binding(3) var lut_tex: texture_2d<f32>;
@group(1) @binding(4) var distortion_tex: texture_2d<f32>;

fn post_has(bit: u32) -> bool {
    return (post.flags.x & bit) != 0u;
}

// `s3d_hash01(p, seed)` with `hashed = s3d_pcg(seed)` computed on the CPU (bit-identical).
fn post_hash01(p: vec2<u32>, hashed: u32) -> f32 {
    return f32(s3d_pcg(p.x ^ s3d_pcg(p.y ^ hashed))) * (1.0 / 4294967295.0);
}

// The position in S (pixels) of D pixel center `px`: the same offset when S and D rects have the
// same size (render scale 1), else the rect mapped onto the rect.
fn post_scene_pos(px: vec2<f32>) -> vec2<f32> {
    if (post_has(S3D_POST_EXACT)) {
        return px - post.out_rect.xy + post.src_rect.xy;
    }
    let uv = (px - post.out_rect.xy) / post.out_rect.zw;
    return post.src_rect.xy + uv * post.src_rect.zw;
}

// The rect's part inside S (pixels): lo, hi.
fn post_scene_clip() -> vec4<f32> {
    let size = 1.0 / post.src_texel.xy;
    let lo = max(post.src_rect.xy, vec2<f32>(0.0));
    let hi = min(post.src_rect.xy + post.src_rect.zw, size);
    return vec4<f32>(lo, max(lo, hi));
}

// Bilinear scene color at S position `s`, every tap inside the rect (split screens never bleed
// into each other).
fn post_scene_bilinear(s_in: vec2<f32>) -> vec4<f32> {
    let clip = post_scene_clip();
    let s = clamp(s_in, clip.xy + 0.5, max(clip.xy + 0.5, clip.zw - 0.5));
    return textureSampleLevel(scene_tex, post_sampler, s * post.src_texel.xy, 0.0);
}

// The scene color at S position `s`: a texel load at render scale 1 (exact), else bilinear.
fn post_scene(s: vec2<f32>) -> vec4<f32> {
    if (post_has(S3D_POST_EXACT)) {
        return textureLoad(scene_tex, vec2<i32>(floor(s)), 0);
    }
    return post_scene_bilinear(s);
}

// Bloom level 0 at S position `s` (already exposed and thresholded). The chain covers the
// rect's part inside S.
fn post_bloom(s: vec2<f32>) -> vec3<f32> {
    let clip = post_scene_clip();
    let uv = (s - clip.xy) / max(clip.zw - clip.xy, vec2<f32>(1.0));
    return textureSampleLevel(bloom_tex, post_sampler, uv, 0.0).rgb;
}

// Chromatic aberration: red and blue sampled `off` apart around `s`, radially from the rect's
// center and growing with r² (`lens.x` puts intensity × 2 % of the rect width at the corners).
fn post_chromatic(s: vec2<f32>, c: vec4<f32>) -> vec4<f32> {
    let d = s - (post.src_rect.xy + post.src_rect.zw * 0.5);
    let off = d * (dot(d, d) * post.lens.x);
    let r = post_scene_bilinear(s - off).r;
    let b = post_scene_bilinear(s + off).b;
    return vec4<f32>(r, c.g, b, c.a);
}

// Vignette multiplier at `uv` (0..1 over the rect in D): 1 inside, `vignette_color` × intensity
// toward the corners (distance 1 at the corners of a centered vignette; `roundness` 1 = circular,
// 0 = the rect's shape; the falloff starts `smoothness` before the corners).
fn post_vignette(uv: vec2<f32>) -> vec3<f32> {
    let v = post.vignette;
    let k = vec2<f32>(mix(1.0, post.src_texel.z, v.z), 1.0);
    let d = (uv - vec2<f32>(v.w, post.vignette_color.w)) * 2.0 * k;
    let r = length(d) / length(k);
    let f = smoothstep(1.0 - v.y, 1.0, r) * v.x;
    return mix(vec3<f32>(1.0), post.vignette_color.rgb, f);
}

// Film grain in the encoded (sRGB) space at D pixel `px`: value noise over cells of
// `grain.y` pixels (bilinear between hashed corners), mono, amplitude intensity, fading in the
// highlights with `luminance_response`.
fn post_grain(e: vec3<f32>, px: vec2<f32>) -> vec3<f32> {
    let g = max(px - post.out_rect.xy, vec2<f32>(0.0)) / post.grain.y;
    let i = vec2<u32>(floor(g));
    let f = fract(g);
    let u = f * f * (3.0 - 2.0 * f);
    let seed = post.frame.x;
    let a = post_hash01(i, seed);
    let b = post_hash01(i + vec2<u32>(1u, 0u), seed);
    let c = post_hash01(i + vec2<u32>(0u, 1u), seed);
    let d = post_hash01(i + vec2<u32>(1u, 1u), seed);
    let n = mix(mix(a, b, u.x), mix(c, d, u.x), u.y) * 2.0 - 1.0;
    let luma = dot(e, vec3<f32>(0.2126, 0.7152, 0.0722));
    let amp = post.grain.x * mix(1.0, 1.0 - clamp(luma, 0.0, 1.0), post.grain.z);
    return e + vec3<f32>(n * amp);
}

// Last-pass finish: grain and dither (triangular, ±1 LSB) in the encoded space, and the encoding
// for D. `rgb_in` is display-linear.
fn post_finish(rgb_in: vec3<f32>, px_f: vec2<f32>) -> vec3<f32> {
    let grain = post_has(S3D_POST_GRAIN);
    // Float D keeps linear values and gets no dither.
    let dither = post_has(S3D_POST_DITHER) && S3D_POST_OUT != 2u;
    if (!grain && !dither) {
        if (S3D_POST_OUT == 1u) {
            return s3d_linear_to_srgb(clamp(rgb_in, vec3<f32>(0.0), vec3<f32>(1.0)));
        }
        return rgb_in;
    }
    var e = s3d_linear_to_srgb(clamp(rgb_in, vec3<f32>(0.0), vec3<f32>(1.0)));
    if (grain) {
        e = post_grain(e, px_f);
    }
    if (dither) {
        let px = vec2<u32>(px_f);
        e = e + vec3<f32>((post_hash01(px, post.frame.y) - post_hash01(px, post.frame.z)) / 255.0);
    }
    e = clamp(e, vec3<f32>(0.0), vec3<f32>(1.0));
    if (S3D_POST_OUT == 1u) {
        return e;
    }
    return s3d_srgb_to_linear(e);
}

@fragment
fn fs_composite(in: FullscreenOut) -> @location(0) vec4<f32> {
    let s = post_scene_pos(in.pos.xy);
    var c = post_scene(s);
    if (post_has(S3D_POST_CA)) {
        c = post_chromatic(s, c);
    }
    let a = select(1.0, c.a, post_has(S3D_POST_SCENE_ALPHA));
    var rgb = c.rgb;
    let transparent = post_has(S3D_POST_TRANSPARENT);
    if (transparent && a > 1.0 / 255.0) {
        rgb = rgb / a;
    }
    rgb = rgb * post.exposure.x;
    // Bloom is in exposed units (the prefilter exposed it): added after exposure, never exposed
    // twice. Transparent groups weight it by alpha below (no haze where nothing was drawn).
    if (post_has(S3D_POST_BLOOM)) {
        let b = post_bloom(s);
        if (post.exposure.w > 0.5) {
            rgb = mix(rgb, b, post.exposure.z);
        } else {
            rgb = rgb + b * post.exposure.z;
        }
    }
    if (post_has(S3D_POST_VIGNETTE)) {
        rgb = rgb * post_vignette((in.pos.xy - post.out_rect.xy) / post.out_rect.zw);
    }
    rgb = s3d_tonemap(post.flags.y, rgb);
    if (post.flags.z != 0u) {
        rgb = post_finish(rgb, in.pos.xy);
    }
    if (transparent) {
        rgb = rgb * a;
    }
    return vec4<f32>(rgb, a);
}
