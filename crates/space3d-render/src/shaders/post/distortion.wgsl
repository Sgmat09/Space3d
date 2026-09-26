// Space3d distortion writers (`DISTORTION_WGSL`). Requires `VIEW_WGSL` and `scene_wgsl` first.
// Writers draw into the post group's distortion buffer (`DISTORTION_FORMAT`, split signs:
// R = +x, G = +y, B = -x, A = -y) with blend One/One on color and alpha (`DISTORTION_BLEND`).
//
// An offset says where a pixel looks: the composite shows, at each pixel, the scene `offset`
// away from it (in view heights: 1.0 = the view's height; +x right, +y up), so the image under
// a writer appears moved by -offset. Overlapping writers add up; each axis saturates at
// ±DISTORTION_RANGE (a hard cap, never wraps).

// Largest |offset| per axis, in view heights.
const DISTORTION_RANGE: f32 = 0.0625;

// Encode an offset (view heights, +y up the screen) scaled by `weight` for additive blending.
fn distortion_encode(offset: vec2<f32>, weight: f32) -> vec4<f32> {
    let o = clamp(offset * weight / DISTORTION_RANGE, vec2<f32>(-1.0), vec2<f32>(1.0));
    return vec4<f32>(max(o, vec2<f32>(0.0)), max(-o, vec2<f32>(0.0)));
}

// The offset (view heights, +y up) a distortion buffer texel holds: the inverse of
// `distortion_encode` (after blending: the sum of every writer, per axis within the range).
fn s3d_distortion_decode(texel: vec4<f32>) -> vec2<f32> {
    return (texel.rg - texel.ba) * DISTORTION_RANGE;
}

// Fade of a writer fragment against scene depth: `frag` is its `@builtin(position)` in the
// distortion buffer (`db_size` pixels: `DrawContext::target_size`), `softness` the fade distance
// in world units. 0 where the scene is in front of the fragment, 1 at `softness` behind it, and 1
// without scene depth (GL with MSAA). `softness` 0 is a hard test: 1 unless the scene is closer.
fn distortion_depth_fade(frag: vec4<f32>, db_size: vec2<f32>, softness: f32) -> f32 {
    if (!s3d_depth_valid()) {
        return 1.0;
    }
    let px = vec2<i32>(view.viewport.xy + frag.xy / db_size * view.viewport.zw);
    let uv = frag.xy / db_size;
    let p = view.inv_proj * vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, frag.z, 1.0);
    let frag_depth = -p.z / p.w;
    if (softness <= 0.0) {
        return select(1.0, 0.0, s3d_view_depth_at(px) < frag_depth);
    }
    return s3d_soft_fade(px, frag_depth, softness);
}
