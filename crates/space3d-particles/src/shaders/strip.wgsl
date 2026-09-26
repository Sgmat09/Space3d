// Space3d particles: strips — ribbons (trails) and beams (lasers) — after `particles.wgsl`
// (design-particles §8.5).
//
// A strip is a run of points (`render::strips::StripPoint`, 40 bytes); one instance draws the
// segment between two consecutive points as a 4-vertex triangle strip, reading point `k` from
// vertex buffer 0 and point `k + 1` from vertex buffer 1 (the same buffer, one point further).
// Both segments that meet at a point compute its side vector from the same data, so a strip is
// watertight at any bend. A segment whose second point starts a new strip is collapsed (strips
// of a layer share one draw). A point older than its life (`t > 1`) is clipped: the segment
// ends where the age reaches 1, so a trail shortens smoothly between its points. Points flagged
// `CAP` extend the strip by half a width into a round end (beam ends, a ribbon's head; a
// zero-length pair of capped points is a round glow).

struct S3dpStripIn {
    @builtin(vertex_index) vi: u32,
    // xyz: world position, w: u (along the strip).
    @location(0) a_pos_u: vec4<f32>,
    // xyz: tangent, w: t (the LUT coordinate: age / life for ribbons, along the beam for beams).
    @location(1) a_tan_t: vec4<f32>,
    // Linear tint (× fades), alpha.
    @location(2) a_tint: vec4<f32>,
    // x: flags (low half) and width multiplier (high half, f16), y: random bits.
    @location(3) a_misc: vec2<u32>,
    @location(4) b_pos_u: vec4<f32>,
    @location(5) b_tan_t: vec4<f32>,
    @location(6) b_tint: vec4<f32>,
    @location(7) b_misc: vec2<u32>,
}

struct S3dpStripOut {
    @builtin(position) clip: vec4<f32>,
    // x: u along, y: v across (−1..1), z: position along the segment in half-widths (negative
    // or past w on the round ends), w: the segment's length in half-widths.
    @location(0) uvl: vec4<f32>,
    // Linear color (× intensity × tint), opacity.
    @location(1) color: vec4<f32>,
    // x coverage, y fade, z half-width in pixels, w white-core brightness.
    @location(2) a: vec4<f32>,
    @location(3) view_depth: f32,
    @location(4) @interpolate(flat) bits: u32,
}

const S3DP_STRIP_START: u32 = 1u;
const S3DP_STRIP_CAP: u32 = 2u;
// `t` runs along the strip (beams), not over a life: nothing is clipped.
const S3DP_STRIP_ALONG: u32 = 4u;

// A vertex outside the clip volume (a collapsed segment: its triangles have no area).
fn s3dp_strip_none() -> S3dpStripOut {
    var out: S3dpStripOut;
    out.clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    out.uvl = vec4<f32>(0.0);
    out.color = vec4<f32>(0.0);
    out.a = vec4<f32>(0.0);
    out.view_depth = 0.0;
    out.bits = 0u;
    return out;
}

@vertex
fn vs_strip(in: S3dpStripIn) -> S3dpStripOut {
    let a_flags = in.a_misc.x & 0xffffu;
    let b_flags = in.b_misc.x & 0xffffu;
    if ((b_flags & S3DP_STRIP_START) != 0u) {
        return s3dp_strip_none();
    }
    var pa = in.a_pos_u.xyz;
    var pb = in.b_pos_u.xyz;
    var ua = in.a_pos_u.w;
    var ub = in.b_pos_u.w;
    var ta = in.a_tan_t.w;
    var tb = in.b_tan_t.w;
    let aged = ((a_flags | b_flags) & S3DP_STRIP_ALONG) == 0u;
    if (aged && ta >= 1.0 && tb >= 1.0) {
        return s3dp_strip_none();
    }
    // The dead end ends where its age reaches the life.
    if (aged && ta > 1.0) {
        let s = (ta - 1.0) / (ta - tb);
        pa = mix(pa, pb, s);
        ua = mix(ua, ub, s);
        ta = 1.0;
    }
    if (aged && tb > 1.0) {
        let s = (tb - 1.0) / (tb - ta);
        pb = mix(pb, pa, s);
        ub = mix(ub, ua, s);
        tb = 1.0;
    }
    let end_b = in.vi >= 2u;
    let side = select(-1.0, 1.0, (in.vi & 1u) == 1u);
    let p = select(pa, pb, end_b);
    let t = select(ta, tb, end_b);
    let seg = pb - pa;
    let fallback = s3dp_normalize_or(seg, vec3<f32>(1.0, 0.0, 0.0));
    let tangent = s3dp_normalize_or(select(in.a_tan_t.xyz, in.b_tan_t.xyz, end_b), fallback);
    let tint = select(in.a_tint, in.b_tint, end_b);
    let misc = select(in.a_misc, in.b_misc, end_b);
    let flags = select(a_flags, b_flags, end_b);

    let lut_u = (clamp(t, 0.0, 1.0) * 127.0 + 0.5) / 128.0;
    let col = textureSampleLevel(s3dp_lut, s3dp_lut_sampler, vec2<f32>(lut_u, s3dp_draw.lut.x), 0.0);
    let curves = textureSampleLevel(s3dp_lut, s3dp_lut_sampler, vec2<f32>(lut_u, s3dp_draw.lut.y), 0.0);
    var half = 0.5 * s3dp_draw.lut.w * max(curves.x, 0.0) * s3dp_half(misc.x >> 16u);

    // Side vector: across the tangent, facing the camera (or in the layer's plane).
    let right = vec3<f32>(view.view[0].x, view.view[1].x, view.view[2].x);
    let back = vec3<f32>(view.view[0].z, view.view[1].z, view.view[2].z);
    var across: vec3<f32>;
    if (s3dp_draw.mode.y == 1u) {
        across = cross(tangent, s3dp_draw.stretch.xyz);
    } else {
        let ortho = view.proj[3][3] > 0.5;
        let eye = select(s3dp_normalize_or(view.camera_pos.xyz - p, back), back, ortho);
        across = cross(tangent, eye);
    }
    across = s3dp_normalize_or(across, right);

    // At least `S3DP_MIN_PX` pixels of half-width, dimmed by the width gained (far beams
    // neither vanish nor stair-step).
    let w = max((view.view_proj * vec4<f32>(p, 1.0)).w, 1e-4);
    let ppu = 0.5 * view.viewport.w * view.proj[1][1] / w;
    var px = half * ppu;
    var dim = 1.0;
    if (px < S3DP_MIN_PX) {
        dim = px / S3DP_MIN_PX;
        half = S3DP_MIN_PX / max(ppu, 1e-6);
        px = S3DP_MIN_PX;
    }
    // Round ends: a capped first point moves back by half a width, a capped last one forward.
    let cap = select(0.0, 1.0, (flags & S3DP_STRIP_CAP) != 0u);
    let forward = select(-1.0, 1.0, end_b);
    let world = p + across * (side * half) + tangent * (forward * cap * half);
    var out: S3dpStripOut;
    out.clip = view.view_proj * vec4<f32>(world, 1.0);
    let len = length(seg) / max(half, 1e-6);
    out.uvl = vec4<f32>(select(ua, ub, end_b), side, select(-cap, len + cap, end_b), len);
    out.view_depth = -(view.view * vec4<f32>(world, 1.0)).z;

    let bright = curves.y * dim;
    let opacity = col.a * bright * tint.a;
    let coverage = select(0.0, opacity, s3dp_draw.mode.x != 0u);
    out.color = vec4<f32>(col.rgb * tint.rgb * s3dp_draw.lut.z, opacity);
    // The white core fades with the color's alpha (a trail's cooling tail).
    out.a = vec4<f32>(coverage, 1.0, px, s3dp_draw.lut.z * bright * tint.a * col.a);
    out.bits = misc.y;
    return out;
}

@fragment
fn fs_strip(in: S3dpStripOut) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.clip.xy);
    // Texture coordinates (v down) and their derivatives, before any non-uniform branch.
    let tuv = vec2<f32>(in.uvl.x, 0.5 - 0.5 * in.uvl.y);
    let gx = dpdx(tuv);
    let gy = dpdy(tuv);
    if (S3DP_MANUAL_DEPTH && s3d_scene_occluded(px, in.clip.z)) {
        discard;
    }
    // Distance from the center line in half-widths, rounded past the capped ends.
    let over = max(max(-in.uvl.z, in.uvl.z - in.uvl.w), 0.0);
    let d = length(vec2<f32>(in.uvl.y, over));
    if (d >= 1.0) {
        discard;
    }
    let p = s3dp_draw.shape;
    // The cross-section: a soft glow (hardness → a hard-edged band) with a white-hot core, the
    // gaussians widened by the pixel footprint like the sprites'.
    let halo = s3dp_widen(4.0, in.a.z);
    let white = s3dp_widen(28.0, in.a.z);
    let soft = exp(-d * d * halo.x) * halo.y * (1.0 - d * d);
    let hard = clamp((1.0 - d) * in.a.z + 0.5, 0.0, 1.0);
    var body = mix(soft, hard, p.y);
    var core = exp(-d * d * white.x) * white.y * p.x;
    var tex_rgb = vec3<f32>(1.0);
    if (s3dp_draw.mode.w != 0u) {
        let tex = s3dp_tex(tuv, 0.0, gx, gy);
        body = body * tex.a;
        core = core * tex.a;
        tex_rgb = tex.rgb;
    }
    if (p.z > 0.0) {
        // Scrolling breakup: value noise along the strip (2 and 4 cells per unit of u: whole
        // periods of the wrapped noise over the CPU's 128-unit rebase) and across it.
        let seed = f32(in.bits & 0xffu);
        let n = s3dp_noise_wrap(vec2<f32>(in.uvl.x * 2.0, in.uvl.y * 1.2 + seed)) * 0.65
            + s3dp_noise_wrap(vec2<f32>(in.uvl.x * 4.0 + 17.0, in.uvl.y * 2.6 + seed * 1.7)) * 0.35;
        let k = max(1.0 + p.z * (2.0 * n - 1.0) * 1.6, 0.0);
        body = body * k;
        core = core * k;
    }
    var fade = in.a.y;
    if (s3dp_view.flags.y != 0u) {
        fade = fade * s3d_soft_fade(px, in.view_depth, s3dp_draw.stretch.w);
    }
    var rgb = (in.color.rgb * tex_rgb * (body * in.color.a) + vec3<f32>(core * in.a.w)) * fade;
    let alpha = clamp(in.a.x * body * fade, 0.0, 1.0);
    if (s3dp_view.flags.x != 0u) {
        rgb = vec3<f32>(1.0) - exp(-max(rgb, vec3<f32>(0.0)));
    }
    return s3d_output(vec4<f32>(rgb, alpha), S3DP_OUTPUT, view.exposure.x, view.output.y);
}
