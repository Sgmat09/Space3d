// Space3d particles: sprite billboards (graphics spec §2.5, design-particles §8.3), after
// `particles.wgsl`.
//
// One instanced triangle strip of 4 vertices per particle; the corners come from the vertex
// index and everything else from per-instance attributes and uniforms.

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
    // Flipbook frames: x this frame, y the next, z the blend toward it.
    @location(6) @interpolate(flat) frame: vec3<f32>,
}

// Flipbook frames `(frame, next, blend)` of a particle at `t` (age / life), `age` seconds old,
// with random bits `bits` (`S3dpDraw::book`, `book2`).
fn s3dp_frames(t: f32, age: f32, bits: u32) -> vec3<f32> {
    let frames = max(s3dp_draw.book.x, 1.0);
    let mode = u32(s3dp_draw.book.w);
    var raw = 0.0;
    var looping = true;
    switch mode {
        case 1u: {
            let cycles = s3dp_draw.book2.x;
            raw = t * frames * cycles;
            looping = cycles > 1.0;
        }
        case 2u: {
            raw = age * s3dp_draw.book2.x;
        }
        case 3u: {
            raw = floor(s3dp_rnd(bits, 1u) * frames);
        }
        case 4u: {
            raw = floor(s3dp_rnd(bits, 1u) * frames) + age * s3dp_draw.book2.x;
        }
        default: {}
    }
    var f0 = floor(raw);
    let blend = select(0.0, raw - f0, s3dp_draw.book2.y > 0.5 && mode != 3u);
    var f1 = f0 + 1.0;
    if (looping) {
        f0 = f0 - frames * floor(f0 / frames);
        f1 = f1 - frames * floor(f1 / frames);
    } else {
        f0 = min(f0, frames - 1.0);
        f1 = min(f1, frames - 1.0);
    }
    return vec3<f32>(f0, f1, blend);
}

// Texture coordinates and array layer `(u, v, layer)` of flipbook frame `frame` at quad
// coordinates `uv` (0..1, v down): frames fill the atlas cells of a layer, then the next layer.
fn s3dp_cell(uv: vec2<f32>, frame: f32) -> vec3<f32> {
    let cols = max(s3dp_draw.book.y, 1.0);
    let rows = max(s3dp_draw.book.z, 1.0);
    let cells = cols * rows;
    let layer = floor(frame / cells);
    let cell = frame - layer * cells;
    let row = floor(cell / cols);
    let col = cell - row * cols;
    return vec3<f32>((uv + vec2<f32>(col, row)) / vec2<f32>(cols, rows), layer);
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
    // Fill-rate guard: at most `max screen size` of the viewport height (the layer's, else the
    // settings').
    let max_screen = select(s3dp_view.params.x, s3dp_draw.fade.z, s3dp_draw.fade.z > 0.0);
    let size = min(abs(in.pos_size.w) * curves.x, max_screen * 2.0 * w / p11);
    var half = 0.5 * size;
    let px_per_unit = 0.5 * view.viewport.w * p11 / w;
    var px = half * px_per_unit;
    let facing = s3dp_draw.mode.y;
    // Sub-pixel sprites: keep a footprint of at least `S3DP_MIN_PX` pixels radius, dimmed by
    // the area it gained, so a small spark's total light stays the same wherever it sits on
    // the pixel grid (smaller footprints are point-sampled at pixel centers: a 1 px sprite on a
    // pixel corner vanished, and small sprites twinkled while the camera panned).
    var shrink = 1.0;
    if (px < S3DP_MIN_PX) {
        shrink = px / S3DP_MIN_PX;
        half = S3DP_MIN_PX / max(px_per_unit, 1e-6);
        px = S3DP_MIN_PX;
    }
    var dim = shrink * shrink;

    // Flipbooks at frames per second carry rotation and age in seconds as two halves.
    var rot = bitcast<f32>(in.rot_frame.x);
    var age_secs = 0.0;
    if (s3dp_draw.book2.w > 0.5) {
        rot = s3dp_half(in.rot_frame.x & 0xffffu);
        age_secs = s3dp_half(in.rot_frame.x >> 16u);
    }
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
        // A widened streak keeps its length (at least round): dimmed by the width it gained,
        // and by the length only when it was shorter than the minimum footprint.
        let kept = stretch * shrink;
        stretch = max(kept, 1.0);
        dim = shrink * kept / stretch;
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
    out.frame = s3dp_frames(age, age_secs, in.rot_frame.y);
    return out;
}

// Shape of a sprite at quad position `uv`: (color weight, white core, coverage mask). `peak`
// is the sprite's brightest channel as the tonemapper sees it (color × opacity × exposure).
fn s3dp_shape(uv: vec2<f32>, stretch: f32, px: f32, bits: u32, peak: f32) -> vec3<f32> {
    let p = s3dp_draw.shape;
    switch s3dp_draw.mode.z {
        case 1u: {
            // Streak: a capsule along +y (the velocity) whose width and brightness taper toward
            // the tail by p.y (taper 1: a quarter of the head's width, dark), with a white-hot
            // head of p.x. Brightness falls with the square of the distance from the head, so
            // tonemapping keeps a hot head and a thin fading tail instead of an even rod.
            let along = uv.y * stretch;
            let t = clamp((along + stretch) / (2.0 * stretch), 0.0, 1.0);
            // The tail narrows to a quarter of the head's width, but never below a pixel of
            // half-width (a thinner tail is point-sampled and twinkles): the light it would
            // have had is spread over the wider tail instead.
            let taper = mix(1.0 - 0.75 * p.y, 1.0, t);
            let width = max(taper, min(1.0, 1.0 / max(px, 1e-3)));
            let dy = max(abs(along) - (stretch - 1.0), 0.0);
            let d = length(vec2<f32>(uv.x / width, dy));
            if (d >= 1.0) {
                return vec3<f32>(0.0);
            }
            let soft = s3dp_widen(4.0, px * width);
            let white = s3dp_widen(28.0, px * width);
            let body = exp(-d * d * soft.x) * soft.y * (1.0 - d * d) * (taper / width);
            let k = mix(1.0 - p.y, 1.0, t * t);
            let hot = mix(1.0, t * t * t, p.y);
            return vec3<f32>(body * k, exp(-d * d * white.x) * white.y * p.x * hot, body * k);
        }
        case 2u: {
            // Ring of thickness p.x (fraction of the radius), focused toward +x by p.y, filled
            // faintly by p.z. The band sits 1.5 thicknesses inside the rim (0.4 % of its peak
            // there) and the rim fades over 1.5 pixels: no hard, stair-stepped outer edge.
            let r = length(uv);
            if (r >= 1.0) {
                return vec3<f32>(0.0);
            }
            let th = max(p.x, 0.004);
            let x = (r - (1.0 - 1.5 * th)) / th;
            let aa = clamp(1.5 / max(px, 1e-3), 0.0, 1.0);
            let rim = 1.0 - smoothstep(1.0 - aa, 1.0, r);
            var ring = (exp(-x * x * 2.5) + p.z * r * r * (1.0 - smoothstep(0.85, 1.0, r))) * rim;
            if (p.y > 0.0) {
                let c = uv.x / max(r, 1e-4) * 0.5 + 0.5;
                ring = ring * pow(max(c, 0.0), p.y);
            }
            return vec3<f32>(ring, 0.0, ring);
        }
        case 3u: {
            // Smoke: a noisy puff (breakup p.x), its light gathered in the middle (p.y). Four
            // value-noise octaves (the two finest give billow edges detail at 720p and above)
            // and a coverage edge 0.5 wide (it was 0.8: blurry puffs).
            let seed = vec2<f32>(f32(bits & 0xffffu), f32(bits >> 16u)) * (40.0 / 65536.0);
            let d = length(uv);
            let n = s3dp_noise(uv * 2.3 + seed) * 0.5 + s3dp_noise(uv * 5.1 + seed.yx) * 0.25
                + s3dp_noise(uv * 11.0 + seed * 1.7) * 0.15 + s3dp_noise(uv * 23.0 + seed.yx * 2.3) * 0.1;
            let cov = 1.0 - smoothstep(0.35, 0.85, d + (n - 0.5) * p.x);
            if (cov <= 0.001) {
                return vec3<f32>(0.0);
            }
            // Billows inside the puff: its light (not its coverage) follows a billow field (two
            // more noise octaves), brighter where the field rises toward the upper left of the
            // sprite (a cheap bump light), so a fireball reads as a rolling volume instead of a
            // flat disc with a noisy rim (the coverage is 1 over the inner third of the radius).
            let q = uv + seed.yx * 1.3;
            let coarse = s3dp_noise(q * 3.7);
            let b = coarse * 0.65 + s3dp_noise(q * 8.3) * 0.35;
            let lit = coarse - s3dp_noise((q + vec2<f32>(0.09, 0.12)) * 3.7);
            let billow = clamp(mix(0.6, 1.2, b) + lit * 1.8, 0.4, 1.5);
            return vec3<f32>(pow(cov, 1.0 + 6.0 * p.y) * billow, 0.0, cov);
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
            // Glow: soft halo (p.y hardens it into a disc) and a white-hot core of p.x. The
            // halo's window (1 − d²) steepens as (1 − d²)^(1 + ½·log2 peak) for a sprite brighter
            // than 1: tonemapped, a bright gaussian saturates out to where the window cuts it
            // and reads as a flat disc with a hard rim (embers, a bolt's head at 10–15×); the
            // steeper window spreads its edge over ~8 stops instead. Unchanged up to 1.
            let d = length(uv);
            if (d >= 1.0) {
                return vec3<f32>(0.0);
            }
            var win = 1.0 - d * d;
            if (peak > 1.0) {
                win = win * pow(win, min(0.5 * log2(peak), 3.0));
            }
            let halo = s3dp_widen(4.0, px);
            let white = s3dp_widen(28.0, px);
            let soft = exp(-d * d * halo.x) * halo.y * win;
            let hard = clamp((1.0 - d) * px + 0.5, 0.0, 1.0);
            let body = mix(soft, hard, p.y);
            return vec3<f32>(body, exp(-d * d * white.x) * white.y * p.x, body);
        }
    }
}

@fragment
fn fs_sprite(in: S3dpOut) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.clip.xy);
    if (S3DP_MANUAL_DEPTH && s3d_scene_occluded(px, in.clip.z)) {
        discard;
    }
    // Texture coordinates (v down) and their derivatives, before any non-uniform branch.
    let tuv = vec2<f32>(in.uv.x, -in.uv.y) * 0.5 + 0.5;
    let cells = vec2<f32>(max(s3dp_draw.book.y, 1.0), max(s3dp_draw.book.z, 1.0));
    let gx = dpdx(tuv) / cells;
    let gy = dpdy(tuv) / cells;
    let peak = max(in.color.r, max(in.color.g, in.color.b)) * in.color.a * view.exposure.x;
    var s: vec3<f32>;
    var tex_rgb = vec3<f32>(1.0);
    if (s3dp_draw.mode.z == 5u) {
        // A texture (or a flipbook frame of it, blended toward the next): its color multiplies
        // the particle's, its alpha is the shape (straight alpha).
        let c0 = s3dp_cell(tuv, in.frame.x);
        var tex = s3dp_tex(c0.xy, c0.z, gx, gy);
        if (in.frame.z > 0.0) {
            let c1 = s3dp_cell(tuv, in.frame.y);
            tex = mix(tex, s3dp_tex(c1.xy, c1.z, gx, gy), in.frame.z);
        }
        s = vec3<f32>(tex.a, 0.0, tex.a);
        tex_rgb = tex.rgb;
    } else {
        s = s3dp_shape(in.uv, in.a.z, in.b.x, in.bits, peak);
    }
    var fade = in.a.y;
    if (s3dp_view.flags.y != 0u) {
        fade = fade * s3d_soft_fade(px, in.view_depth, s3dp_draw.stretch.w);
    }
    var rgb = (in.color.rgb * tex_rgb * (s.x * in.color.a) + vec3<f32>(s.y * in.a.w)) * fade;
    let alpha = clamp(in.a.x * s.z * fade, 0.0, 1.0);
    if (s3dp_view.flags.x != 0u) {
        rgb = vec3<f32>(1.0) - exp(-max(rgb, vec3<f32>(0.0)));
    }
    return s3d_output(vec4<f32>(rgb, alpha), S3DP_OUTPUT, view.exposure.x, view.output.y);
}
