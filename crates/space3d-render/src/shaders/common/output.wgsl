// Space3d output tail (`OUTPUT_WGSL`): color encodings and tonemappers shared by the forward
// shader, render nodes, particles, overlays and post. No bindings. Every name is prefixed
// `s3d_` / `S3D_` (user code shares the module). CPU mirrors: `Tonemapper::apply`.

const S3D_OUTPUT_LINEAR: u32 = 0u;
const S3D_OUTPUT_MANUAL_SRGB: u32 = 1u;
const S3D_OUTPUT_TONEMAPPED: u32 = 2u;

const S3D_TM_NONE: u32 = 0u;
const S3D_TM_REINHARD: u32 = 1u;
const S3D_TM_ACES: u32 = 2u;
const S3D_TM_AGX: u32 = 3u;
const S3D_TM_PBR_NEUTRAL: u32 = 4u;
const S3D_TM_AGX_PUNCHY: u32 = 5u;

fn s3d_luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn s3d_srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn s3d_linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// Stephen Hill's ACES fit. `v * M` with M built from the HLSL rows equals HLSL `mul(M, v)`.
fn s3d_aces_fitted(c: vec3<f32>) -> vec3<f32> {
    let i = mat3x3<f32>(
        vec3<f32>(0.59719, 0.35458, 0.04823),
        vec3<f32>(0.07600, 0.90834, 0.01566),
        vec3<f32>(0.02840, 0.13383, 0.83777),
    );
    let o = mat3x3<f32>(
        vec3<f32>(1.60475, -0.53108, -0.07367),
        vec3<f32>(-0.10208, 1.10813, -0.00605),
        vec3<f32>(-0.00327, -0.07276, 1.07602),
    );
    let v = c * i;
    let r = (v * (v + 0.0245786) - 0.000090537) / (v * (0.983729 * v + 0.4329510) + 0.238081);
    return clamp(r * o, vec3<f32>(0.0), vec3<f32>(1.0));
}

// AgX (Sobotka; minimal polynomial fit by B. Wrensch): the inset, log encoding and sigmoid.
fn s3d_agx_curve(c: vec3<f32>) -> vec3<f32> {
    let m = mat3x3<f32>(
        vec3<f32>(0.842479062253094, 0.0423282422610123, 0.0423756549057051),
        vec3<f32>(0.0784335999999992, 0.878468636469772, 0.0784336),
        vec3<f32>(0.0792237451477643, 0.0791661274605434, 0.879142973793104),
    );
    let lo = -12.47393;
    let hi = 4.026069;
    var v = clamp(log2(max(m * c, vec3<f32>(1e-10))), vec3<f32>(lo), vec3<f32>(hi));
    v = (v - lo) / (hi - lo);
    let v2 = v * v;
    let v4 = v2 * v2;
    return 15.5 * v4 * v2 - 40.14 * v4 * v + 31.96 * v4 - 6.868 * v2 * v + 0.4298 * v2 + 0.1191 * v - 0.00232;
}

// AgX outset and display EOTF (pow 2.2): display-linear.
fn s3d_agx_display(v: vec3<f32>) -> vec3<f32> {
    let mi = mat3x3<f32>(
        vec3<f32>(1.19687900512017, -0.0528968517574562, -0.0529716355144438),
        vec3<f32>(-0.0980208811401368, 1.15190312990417, -0.0980434501171241),
        vec3<f32>(-0.0990297440797205, -0.0989611768448433, 1.15107367264116),
    );
    return pow(max(mi * v, vec3<f32>(0.0)), vec3<f32>(2.2));
}

// AgX, no look; outputs display-linear.
fn s3d_agx(c: vec3<f32>) -> vec3<f32> {
    return s3d_agx_display(s3d_agx_curve(c));
}

// AgX with Sobotka's "punchy" look: an ASC CDL in the encoded (log) space, power 1.35 then
// saturation 1.4 around the Rec. 709 luma of the powered value (clamped: the saturation can
// push a channel past display white). Saturated emissives keep their hue on the way to white,
// mid grey 0.18 maps to 0.12 instead of 0.21.
fn s3d_agx_punchy(c: vec3<f32>) -> vec3<f32> {
    let v = pow(max(s3d_agx_curve(c), vec3<f32>(0.0)), vec3<f32>(1.35));
    let l = s3d_luma(v);
    return min(s3d_agx_display(vec3<f32>(l) + 1.4 * (v - vec3<f32>(l))), vec3<f32>(1.0));
}

// Khronos PBR Neutral (start compression 0.76, desaturation 0.15).
fn s3d_pbr_neutral(c_in: vec3<f32>) -> vec3<f32> {
    var c = c_in;
    let x = min(c.r, min(c.g, c.b));
    let offset = select(0.04, x - 6.25 * x * x, x < 0.08);
    c -= vec3<f32>(offset);
    let peak = max(c.r, max(c.g, c.b));
    if (peak < 0.76) {
        return c;
    }
    let d = 1.0 - 0.76;
    let new_peak = 1.0 - d * d / (peak + d - 0.76);
    c *= new_peak / peak;
    let g = 1.0 - 1.0 / (0.15 * (peak - new_peak) + 1.0);
    return mix(c, vec3<f32>(new_peak), g);
}

// HDR linear → display-linear 0..1 with operator `op` (`Tonemapper::id`).
fn s3d_tonemap(op: u32, c_in: vec3<f32>) -> vec3<f32> {
    let c = max(c_in, vec3<f32>(0.0));
    switch op {
        case 1u: {
            return clamp(c / (1.0 + s3d_luma(c)), vec3<f32>(0.0), vec3<f32>(1.0));
        }
        case 2u: {
            return s3d_aces_fitted(c);
        }
        case 3u: {
            return s3d_agx(c);
        }
        case 4u: {
            return s3d_pbr_neutral(c);
        }
        case 5u: {
            return s3d_agx_punchy(c);
        }
        default: {
            return clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
        }
    }
}

// Encode a linear color for the pass being drawn (`OutputEncoding::id`): `Linear` returns it
// unclamped, `ManualSrgb` clamps and encodes sRGB (byte-identical to the legacy MANUAL_SRGB
// tail), `Tonemapped` applies `exposure` (linear multiplier) and the operator.
fn s3d_output(c: vec4<f32>, encoding: u32, exposure: f32, op: u32) -> vec4<f32> {
    if (encoding == S3D_OUTPUT_MANUAL_SRGB) {
        return vec4<f32>(s3d_linear_to_srgb(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0))), c.a);
    }
    if (encoding == S3D_OUTPUT_TONEMAPPED) {
        return vec4<f32>(s3d_tonemap(op, c.rgb * exposure), c.a);
    }
    return c;
}
