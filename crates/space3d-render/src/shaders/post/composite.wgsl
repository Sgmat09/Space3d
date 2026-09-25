// Space3d composite (graphics spec §1.9): the one pass that turns a post group's scene color S
// into the display target D. The renderer prepends OUTPUT_WGSL, the hash functions and
// FULLSCREEN_WGSL. Every effect is a uniform bit (`PostParams::flags.x`); the output kind is
// the pipeline override `S3D_POST_OUT`. Only explicit-LOD sampling and loads: no derivatives, so
// no uniformity constraints (WebGL2-safe).
//
// Order (normative): S at the rect → un-premultiply (transparent groups) → exposure → [bloom]
// → [vignette, white balance, saturation, contrast, HDR hook] → tonemap → [lift/gamma/gain,
// LUT, LDR hook] → [grain] → dither → encode → re-premultiply. Bracketed steps belong to the
// post area; this file keeps their slots in order.

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
    exposure: vec4<f32>,        // linear multiplier, -, bloom intensity, bloom mode
    vignette: vec4<f32>,
    vignette_color: vec4<f32>,
    lens: vec4<f32>,
    grain: vec4<f32>,
    grade_matrix: mat3x3<f32>,
    lift_contrast: vec4<f32>,
    gamma_lut: vec4<f32>,
    gain_lut: vec4<f32>,
    flags: vec4<u32>,           // x effect bits, y tonemapper id, z -, w -
    frame: vec4<u32>,           // x grain seed, y dither seed
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

// The scene color at D pixel center `px`: a texel load when S and D rects have the same size
// (render scale 1: exact), else bilinear with every tap inside the rect (split screens never
// bleed into each other).
fn post_scene(px: vec2<f32>) -> vec4<f32> {
    if (post_has(S3D_POST_EXACT)) {
        let p = vec2<i32>(floor(px - post.out_rect.xy + post.src_rect.xy));
        return textureLoad(scene_tex, p, 0);
    }
    let size = 1.0 / post.src_texel.xy;
    let uv = (px - post.out_rect.xy) / post.out_rect.zw;
    let lo = max(post.src_rect.xy, vec2<f32>(0.0)) + 0.5;
    let hi = min(post.src_rect.xy + post.src_rect.zw, size) - 0.5;
    let s = clamp(post.src_rect.xy + uv * post.src_rect.zw, lo, max(lo, hi));
    return textureSampleLevel(scene_tex, post_sampler, s * post.src_texel.xy, 0.0);
}

// Last-pass finish: dither (triangular, ±1 LSB in the encoded space) and encoding for D.
fn post_finish(rgb_in: vec3<f32>, px: vec2<u32>) -> vec3<f32> {
    var rgb = rgb_in;
    var n = 0.0;
    if (post_has(S3D_POST_DITHER)) {
        let seed = post.frame.y;
        n = (s3d_hash01(px, seed) - s3d_hash01(px, seed ^ 0x68E31DA4u)) / 255.0;
    }
    if (S3D_POST_OUT == 0u) {
        if (post_has(S3D_POST_DITHER)) {
            let e = s3d_linear_to_srgb(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)));
            rgb = s3d_srgb_to_linear(clamp(e + vec3<f32>(n), vec3<f32>(0.0), vec3<f32>(1.0)));
        }
    } else if (S3D_POST_OUT == 1u) {
        let e = s3d_linear_to_srgb(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)));
        rgb = clamp(e + vec3<f32>(n), vec3<f32>(0.0), vec3<f32>(1.0));
    }
    return rgb;
}

@fragment
fn fs_composite(in: FullscreenOut) -> @location(0) vec4<f32> {
    let c = post_scene(in.pos.xy);
    let a = select(1.0, c.a, post_has(S3D_POST_SCENE_ALPHA));
    var rgb = c.rgb;
    let transparent = post_has(S3D_POST_TRANSPARENT);
    if (transparent && a > 1.0 / 255.0) {
        rgb = rgb / a;
    }
    rgb = rgb * post.exposure.x;
    rgb = s3d_tonemap(post.flags.y, rgb);
    rgb = post_finish(rgb, vec2<u32>(in.pos.xy));
    if (transparent) {
        rgb = rgb * a;
    }
    return vec4<f32>(rgb, a);
}
