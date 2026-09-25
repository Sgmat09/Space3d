// Space3d scene inputs of a pass (`scene_wgsl`), group 1. Requires `VIEW_WGSL` first.
// `S3D_SCENE_DEPTH` is prepended by the renderer: false binds a 1×1 far-depth dummy.

@group(1) @binding(0) var s3d_scene_depth_tex: texture_2d<f32>;   // Depth32Float as unfilterable float
@group(1) @binding(1) var s3d_exposure_tex: texture_2d<f32>;      // 1×1, EV in .r

// Whether real scene depth is bound.
fn s3d_depth_valid() -> bool {
    return S3D_SCENE_DEPTH;
}

// 0..1 across the view's viewport from a fragment position (pixels of the pass target).
fn s3d_scene_uv(frag_xy: vec2<f32>) -> vec2<f32> {
    return (frag_xy - view.viewport.xy) / view.viewport.zw;
}

// Device depth of the scene at pixel `px` (1.0 without depth).
fn s3d_scene_depth(px: vec2<i32>) -> f32 {
    return textureLoad(s3d_scene_depth_tex, px, 0).x;
}

// Positive view-space distance of the scene at pixel `px` (perspective and orthographic).
fn s3d_view_depth_at(px: vec2<i32>) -> f32 {
    let uv = s3d_scene_uv(vec2<f32>(px) + 0.5);
    let v = view.inv_proj * vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, s3d_scene_depth(px), 1.0);
    return -v.z / v.w;
}

// Positive view-space distance of a world position.
fn s3d_view_depth_of(world: vec3<f32>) -> f32 {
    return -(view.view * vec4<f32>(world, 1.0)).z;
}

// Soft fade of a fragment at view distance `frag_view_depth` against the scene: 0 on contact,
// 1 at `softness` in front of it (1 without depth or softness).
fn s3d_soft_fade(px: vec2<i32>, frag_view_depth: f32, softness: f32) -> f32 {
    if (!s3d_depth_valid() || softness <= 0.0) {
        return 1.0;
    }
    return clamp((s3d_view_depth_at(px) - frag_view_depth) / softness, 0.0, 1.0);
}

// Manual LessEqual depth test (passes without depth attached): true when hidden.
fn s3d_scene_occluded(px: vec2<i32>, frag_device_depth: f32) -> bool {
    return s3d_depth_valid() && frag_device_depth > s3d_scene_depth(px);
}

// The view's post exposure in stops (0 without auto exposure).
fn s3d_exposure_ev() -> f32 {
    return textureLoad(s3d_exposure_tex, vec2<i32>(0), 0).r;
}
