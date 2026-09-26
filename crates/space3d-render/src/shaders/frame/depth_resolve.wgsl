// Space3d depth resolve (graphics spec §1.6): the minimum of a multisampled depth texture's
// samples into a single-sample depth texture (a depth-only pass: compare Always, write on).
// Read-only-depth backends with multisampled groups only; the renderer prepends
// FULLSCREEN_WGSL. The minimum is conservative: scene-depth readers behind partially covered
// silhouette pixels are hidden (slight edge aliasing instead of halos).

@group(0) @binding(0) var depth_ms: texture_depth_multisampled_2d;

override S3D_SAMPLES: u32 = 4u;

@fragment
fn fs_resolve(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {
    let px = vec2<i32>(p.xy);
    var d = 1.0;
    for (var s = 0u; s < S3D_SAMPLES; s++) {
        d = min(d, textureLoad(depth_ms, px, i32(s)));
    }
    return d;
}
