// Space3d particles: sprite billboards (graphics spec §2.5, design-particles §8.3).
//
// Composed per pass signature as: `S3DP_OUTPUT` / `S3DP_MANUAL_DEPTH` consts, `VIEW_WGSL`
// (group 0), `scene_wgsl(target)` (group 1), `OUTPUT_WGSL`, then this file (group 2).
//
// One instanced triangle strip of 4 vertices per particle; the corners come from the vertex
// index and everything else from per-instance attributes and uniforms: the vertex shader reads
// no storage buffers (WebGL2). Curves are sampled from the LUT in the vertex shader. Blending is
// premultiplied (`One, OneMinusSrcAlpha`): additive sprites write alpha 0, alpha-blended ones
// their opacity, premultiplied ones their coverage.

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
    // 2 plane), z: shape (0 glow, 1 streak, 2 ring, 3 smoke, 4 vortex), w: 0.
    mode: vec4<u32>,
    shape: vec4<f32>,
    // x: seconds of velocity, y: minimum stretch, z: maximum stretch, w: soft distance.
    stretch: vec4<f32>,
    // x, y: near fade (invisible at x, full at y; off when y <= x).
    fade: vec4<f32>,
}

@group(2) @binding(0) var s3dp_lut: texture_2d<f32>;
@group(2) @binding(1) var s3dp_lut_sampler: sampler;
@group(2) @binding(2) var<uniform> s3dp_view: S3dpView;
@group(2) @binding(3) var<uniform> s3dp_draw: S3dpDraw;

struct S3dpIn {
    @builtin(vertex_index) vi: u32,
    // World position, signed size (negative: mirrored).
    @location(0) pos_size: vec4<f32>,
    // World velocity (plane normal for plane facings), age / life.
    @location(1) vel_age: vec4<f32>,
    // Linear tint, alpha (coverage for premultiplied layers).
    @location(2) tint: vec4<f32>,
    // x: rotation (f32 bits), y: random bits.
    @location(3) rot_frame: vec2<u32>,
}

struct S3dpOut {
    @builtin(position) clip: vec4<f32>,
    // Quad-local position in [-1, 1]² (x across, y along).
    @location(0) uv: vec2<f32>,
    // Linear color (× intensity), opacity.
    @location(1) color: vec4<f32>,
    // x coverage, y fade, z stretch, w white-core brightness.
    @location(2) @interpolate(flat) a: vec4<f32>,
    // x radius in pixels.
    @location(3) @interpolate(flat) b: vec4<f32>,
    // Positive view distance of the fragment (soft fade).
    @location(4) view_depth: f32,
    @location(5) @interpolate(flat) bits: u32,
}

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

// In-plane basis of a plane-facing sprite (the Rust `cpu::plane_basis`).
fn s3dp_plane_basis(n: vec3<f32>) -> mat2x3<f32> {
    let helper = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(n.y) > 0.99);
    let bx = normalize(cross(helper, n));
    return mat2x3<f32>(bx, cross(n, bx));
}

@vertex
fn vs_sprite(in: S3dpIn) -> S3dpOut {
    var out: S3dpOut;
    let corner = vec2<f32>(f32(in.vi & 1u), f32(in.vi >> 1u)) * 2.0 - 1.0;
    let age = clamp(in.vel_age.w, 0.0, 1.0);
    let lut_u = (age * 127.0 + 0.5) / 128.0;
    let col = textureSampleLevel(s3dp_lut, s3dp_lut_sampler, vec2<f32>(lut_u, s3dp_draw.lut.x), 0.0);
    let curves = textureSampleLevel(s3dp_lut, s3dp_lut_sampler, vec2<f32>(lut_u, s3dp_draw.lut.y), 0.0);

    let center = in.pos_size.xyz;
    let clip_center = view.view_proj * vec4<f32>(center, 1.0);
    let w = max(clip_center.w, 1e-4);
    let p11 = view.proj[1][1];
    // Fill-rate guard: at most `max screen size` of the viewport height.
    let size = min(abs(in.pos_size.w) * curves.x, s3dp_view.params.x * 2.0 * w / p11);
    var half = 0.5 * size;
    let px_per_unit = 0.5 * view.viewport.w * p11 / w;
    var px = half * px_per_unit;
    let facing = s3dp_draw.mode.y;
    var dim = 1.0;
    if (px < 0.6) {
        // Sub-pixel: keep a 0.6 px footprint, dimmed by the lost area (length for streaks).
        let f = px / 0.6;
        dim = select(f * f, f, facing == 1u);
        half = 0.6 / max(px_per_unit, 1e-6);
        px = 0.6;
    }

    let rot = bitcast<f32>(in.rot_frame.x);
    let cs = cos(rot);
    let sn = sin(rot);
    let right = vec3<f32>(view.view[0].x, view.view[1].x, view.view[2].x);
    let up = vec3<f32>(view.view[0].y, view.view[1].y, view.view[2].y);
    let back = vec3<f32>(view.view[0].z, view.view[1].z, view.view[2].z);
    var ax = right * cs + up * sn;
    var ay = up * cs - right * sn;
    var stretch = 1.0;
    if (facing == 1u) {
        let v = in.vel_age.xyz;
        let vp = v - dot(v, back) * back;
        let len = length(vp);
        if (len > 1e-5) {
            ay = vp / len;
            ax = cross(ay, back);
            stretch = clamp(1.0 + len * s3dp_draw.stretch.x / max(size, 1e-5), s3dp_draw.stretch.y, s3dp_draw.stretch.z);
        }
    } else if (facing == 2u) {
        let n = in.vel_age.xyz;
        if (dot(n, n) > 1e-8) {
            let basis = s3dp_plane_basis(normalize(n));
            ax = basis[0] * cs + basis[1] * sn;
            ay = basis[1] * cs - basis[0] * sn;
        }
    }
    let aspect = s3dp_draw.lut.w;
    let world = center + ax * (corner.x * half * aspect) + ay * (corner.y * half * stretch);
    out.clip = view.view_proj * vec4<f32>(world, 1.0);
    out.uv = vec2<f32>(select(corner.x, -corner.x, in.pos_size.w < 0.0), corner.y);
    out.view_depth = -(view.view * vec4<f32>(world, 1.0)).z;

    let blend = s3dp_draw.mode.x;
    let bright = curves.y * dim;
    var opacity = col.a * bright;
    var coverage = 0.0;
    var white = s3dp_draw.lut.z * bright;
    if (blend == 2u) {
        coverage = curves.z * in.tint.a * dim;
    } else {
        opacity = opacity * in.tint.a;
        white = white * in.tint.a;
        if (blend == 1u) {
            coverage = opacity;
        }
    }
    out.color = vec4<f32>(col.rgb * in.tint.rgb * s3dp_draw.lut.z, opacity);
    var fade = 1.0;
    let near = s3dp_draw.fade;
    if (near.y > near.x) {
        fade = clamp((distance(center, view.camera_pos.xyz) - near.x) / (near.y - near.x), 0.0, 1.0);
    }
    out.a = vec4<f32>(coverage, fade, stretch, white);
    out.b = vec4<f32>(px, 0.0, 0.0, 0.0);
    out.bits = in.rot_frame.y;
    return out;
}

// Shape of a sprite at quad position `uv`: (color weight, white core, coverage mask).
fn s3dp_shape(uv: vec2<f32>, stretch: f32, px: f32, bits: u32) -> vec3<f32> {
    let p = s3dp_draw.shape;
    switch s3dp_draw.mode.z {
        case 1u: {
            // Streak: a capsule along +y (the velocity), tapering toward its tail.
            let along = uv.y * stretch;
            let dy = max(abs(along) - (stretch - 1.0), 0.0);
            let d = length(vec2<f32>(uv.x, dy));
            if (d >= 1.0) {
                return vec3<f32>(0.0);
            }
            let body = exp(-d * d * 4.0) * (1.0 - d * d);
            let t = clamp((along + stretch) / (2.0 * stretch), 0.0, 1.0);
            let k = mix(1.0 - p.y, 1.0, t);
            return vec3<f32>(body * k, exp(-d * d * 28.0) * p.x * k, body * k);
        }
        case 2u: {
            // Ring of thickness p.x (fraction of the radius), focused toward +x by p.y, filled
            // faintly by p.z.
            let r = length(uv);
            if (r > 1.0) {
                return vec3<f32>(0.0);
            }
            let th = max(p.x, 0.004);
            let x = (r - (1.0 - th)) / th;
            var ring = exp(-x * x * 2.5) + p.z * r * r * (1.0 - smoothstep(0.85, 1.0, r));
            if (p.y > 0.0) {
                let c = uv.x / max(r, 1e-4) * 0.5 + 0.5;
                ring = ring * pow(max(c, 0.0), p.y);
            }
            return vec3<f32>(ring, 0.0, ring);
        }
        case 3u: {
            // Smoke: a noisy puff (breakup p.x), its light gathered in the middle (p.y).
            let seed = vec2<f32>(f32(bits & 0xffffu), f32(bits >> 16u)) * (40.0 / 65536.0);
            let d = length(uv);
            let n = s3dp_noise(uv * 2.3 + seed) * 0.65 + s3dp_noise(uv * 5.1 + seed.yx) * 0.35;
            let cov = 1.0 - smoothstep(0.2, 1.0, d + (n - 0.5) * p.x);
            if (cov <= 0.001) {
                return vec3<f32>(0.0);
            }
            return vec3<f32>(pow(cov, 1.0 + 6.0 * p.y), 0.0, cov);
        }
        case 4u: {
            // Vortex: p.x spiral arms with p.y turns from rim to center, and a soft core.
            let r = length(uv);
            if (r > 1.0) {
                return vec3<f32>(0.0);
            }
            let ang = atan2(uv.y, uv.x);
            let s = 0.5 + 0.5 * sin(p.x * ang + p.y * 6.2831853 * r);
            let arms = s * s * s * pow(max(1.0 - r, 0.0), 1.2) * smoothstep(0.0, 0.3, r);
            let v = (arms + exp(-r * r * 14.0) * 0.7) * (1.0 - smoothstep(0.75, 1.0, r));
            return vec3<f32>(v, 0.0, v);
        }
        default: {
            // Glow: soft halo (p.y hardens it into a disc) and a white-hot core of p.x.
            let d = length(uv);
            if (d >= 1.0) {
                return vec3<f32>(0.0);
            }
            let soft = exp(-d * d * 4.0) * (1.0 - d * d);
            let hard = clamp((1.0 - d) * px + 0.5, 0.0, 1.0);
            let body = mix(soft, hard, p.y);
            return vec3<f32>(body, exp(-d * d * 28.0) * p.x, body);
        }
    }
}

@fragment
fn fs_sprite(in: S3dpOut) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.clip.xy);
    if (S3DP_MANUAL_DEPTH && s3d_scene_occluded(px, in.clip.z)) {
        discard;
    }
    let s = s3dp_shape(in.uv, in.a.z, in.b.x, in.bits);
    var fade = in.a.y;
    if (s3dp_view.flags.y != 0u) {
        fade = fade * s3d_soft_fade(px, in.view_depth, s3dp_draw.stretch.w);
    }
    var rgb = (in.color.rgb * (s.x * in.color.a) + vec3<f32>(s.y * in.a.w)) * fade;
    let alpha = clamp(in.a.x * s.z * fade, 0.0, 1.0);
    if (s3dp_view.flags.x != 0u) {
        rgb = vec3<f32>(1.0) - exp(-max(rgb, vec3<f32>(0.0)));
    }
    return s3d_output(vec4<f32>(rgb, alpha), S3DP_OUTPUT, view.exposure.x, view.output.y);
}
