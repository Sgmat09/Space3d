// Fragment entry points of `ibl_bake.wgsl` (one face of one mip per pass; after
// `FULLSCREEN_WGSL`).

@fragment
fn fs_cube_copy(in: FullscreenOut) -> @location(0) vec4<f32> {
    return vec4<f32>(s3d_bake_cube_copy(bake.face, in.uv), 1.0);
}

@fragment
fn fs_copy_2d(in: FullscreenOut) -> @location(0) vec4<f32> {
    return vec4<f32>(s3d_bake_copy_2d(in.uv), 1.0);
}

@fragment
fn fs_equirect(in: FullscreenOut) -> @location(0) vec4<f32> {
    return vec4<f32>(s3d_bake_equirect(bake.face, in.uv), 1.0);
}

@fragment
fn fs_prefilter(in: FullscreenOut) -> @location(0) vec4<f32> {
    return vec4<f32>(s3d_bake_prefilter(bake.face, in.uv), 1.0);
}

@fragment
fn fs_irradiance(in: FullscreenOut) -> @location(0) vec4<f32> {
    return vec4<f32>(s3d_bake_irradiance(bake.face, in.uv), 1.0);
}
