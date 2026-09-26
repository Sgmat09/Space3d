// Space3d particles: distortion sprites (`SpriteTarget::Distortion`, graphics spec §1.8, §2.5),
// after `sprite.wgsl` and the render crate's `DISTORTION_WGSL`.
//
// Drawn in the Distortion slot into the post group's distortion buffer (`Rgba8Unorm`, split
// signs, blend One/One, half resolution by default, no depth attachment). Each fragment writes
// `distortion_encode(offset, weight)`: an offset in view heights (the composite shows, at the
// pixel, the scene that far away) weighted by the sprite's shape, its opacity over life and its
// near fade, faded against scene depth (the soft distance, or a hard test) and zeroed by the
// foreground guard (a haze behind a ship never drags the ship's edge).
//
// The offset's direction comes from the shape:
// * `Ring`: toward the ring's center (the image under the band is pushed outward: a
//   shockwave);
// * `Vortex`: around the center (a swirl; the twist's sign turns it);
// * a texture: its RG channels (0.5 = none, 0 and 1 = −/+ along the image's U and V, like
//   `DistortionMaterial`'s normal mode), its alpha weighting;
// * the others (`Glow`, `Streak`, `Smoke`): heat haze, a smooth random field seeded per particle
//   that drifts across it as it ages.
// Directions laid out on the quad (ring, vortex, texture) are turned onto the screen through the
// inverse of the quad's screen-space Jacobian, keeping their length: they follow the sprite's
// rotation and plane facing, and a sprite seen edge-on writes nothing.

struct S3dpDistOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) a: vec4<f32>,
    @location(3) @interpolate(flat) b: vec4<f32>,
    @location(4) view_depth: f32,
    @location(5) @interpolate(flat) bits: u32,
    @location(6) @interpolate(flat) frame: vec3<f32>,
    // Clip-space x, y, w (interpolated with perspective: exact at the fragment), for the scene
    // pixel under a fragment of the (possibly half-size) distortion buffer.
    @location(7) clip_xyw: vec3<f32>,
    // Age / life.
    @location(8) @interpolate(flat) t: f32,
}

@vertex
fn vs_distortion(in: S3dpIn) -> S3dpDistOut {
    let s = s3dp_sprite(in);
    var out: S3dpDistOut;
    out.clip = s.clip;
    out.uv = s.uv;
    out.color = s.color;
    out.a = s.a;
    out.b = s.b;
    out.view_depth = s.view_depth;
    out.bits = s.bits;
    out.frame = s.frame;
    out.clip_xyw = s.clip.xyw;
    out.t = clamp(in.vel_age.w, 0.0, 1.0);
    return out;
}

// Screen offset (view heights, +y up) of a direction `d` laid out on the quad, whose
// coordinates change by `gx` / `gy` per pixel right / down: the pixel vector the inverse
// Jacobian maps `d` to, at `d`'s length. Zero where the quad is seen edge-on.
fn s3dp_quad_to_screen(d: vec2<f32>, gx: vec2<f32>, gy: vec2<f32>) -> vec2<f32> {
    let det = gx.x * gy.y - gy.x * gx.y;
    if (abs(det) < 1e-12) {
        return vec2<f32>(0.0);
    }
    let s = vec2<f32>(gy.y * d.x - gy.x * d.y, gx.x * d.y - gx.y * d.x) / det;
    let len = length(s);
    if (len < 1e-12) {
        return vec2<f32>(0.0);
    }
    return vec2<f32>(s.x, -s.y) * (length(d) / len);
}

@fragment
fn fs_distortion(in: S3dpDistOut) -> @location(0) vec4<f32> {
    // Derivatives first (uniform control flow).
    let gx = dpdx(in.uv);
    let gy = dpdy(in.uv);
    let kind = s3dp_draw.mode.z;
    var weight = 0.0;
    var offset = vec2<f32>(0.0);
    if (S3DP_TEXTURED && kind == 5u) {
        let tuv = vec2<f32>(in.uv.x, -in.uv.y) * 0.5 + 0.5;
        let cells = vec2<f32>(max(s3dp_draw.book.y, 1.0), max(s3dp_draw.book.z, 1.0));
        let tgx = vec2<f32>(gx.x, -gx.y) * 0.5 / cells;
        let tgy = vec2<f32>(gy.x, -gy.y) * 0.5 / cells;
        let c0 = s3dp_cell(tuv, in.frame.x);
        var tex = s3dp_tex(c0.xy, c0.z, tgx, tgy);
        if (in.frame.z > 0.0) {
            let c1 = s3dp_cell(tuv, in.frame.y);
            tex = mix(tex, s3dp_tex(c1.xy, c1.z, tgx, tgy), in.frame.z);
        }
        weight = tex.a;
        // Texture space (U right, V down the image) to the quad's (y up).
        let d = tex.rg * 2.0 - 1.0;
        offset = s3dp_quad_to_screen(vec2<f32>(d.x, -d.y), gx, gy);
    } else {
        weight = s3dp_shape(in.uv, in.a.z, in.b, in.bits, 0.0).z;
        let r = length(in.uv);
        switch kind {
            case 2u: {
                offset = s3dp_quad_to_screen(-in.uv / max(r, 1e-4), gx, gy);
            }
            case 4u: {
                let turn = select(1.0, -1.0, s3dp_draw.shape.y < 0.0);
                offset = s3dp_quad_to_screen(vec2<f32>(-in.uv.y, in.uv.x) * (turn / max(r, 1e-4)), gx, gy);
            }
            default: {
                // Heat haze: two noise fields (about two cells across the sprite) seeded by the
                // particle, drifting along the quad as it ages.
                let seed = vec2<f32>(f32(in.bits & 0xffffu), f32(in.bits >> 16u)) * (40.0 / 65536.0);
                let q = in.uv * 1.1 + seed + vec2<f32>(0.0, -2.5 * in.t);
                let n = vec2<f32>(s3dp_noise(q), s3dp_noise(q.yx * 1.3 + vec2<f32>(17.3, 5.1))) * 2.0 - 1.0;
                let h = n * 2.2;
                offset = h / max(1.0, length(h));
            }
        }
    }
    // The scene pixel under this fragment (the buffer may be smaller than the scene).
    let ndc = in.clip_xyw.xy / in.clip_xyw.z;
    let suv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    let px = vec2<i32>(floor(view.viewport.xy + suv * view.viewport.zw));
    // Shape × opacity over life × near fade, faded against scene depth.
    var w = weight * in.color.a * in.a.y;
    let soft = s3dp_draw.stretch.w;
    if (soft > 0.0) {
        w = w * s3d_soft_fade(px, in.view_depth, soft);
    } else if (s3d_depth_valid() && s3d_view_depth_at(px) < in.view_depth) {
        w = 0.0;
    }
    let o = offset * s3dp_draw.fade.w;
    w = w * s3d_distortion_guard(px, o, in.view_depth);
    return distortion_encode(o, max(w, 0.0));
}
