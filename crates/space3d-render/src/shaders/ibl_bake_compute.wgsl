// Compute entry points of `ibl_bake.wgsl`: all six faces of one mip per dispatch (8 × 8 texels
// per workgroup, z = face) into a write-only storage view of that mip. `S3D_BAKE_FORMAT` is
// replaced by the storage format (`rgba16float`, or `rgba8unorm` for sRGB textures written
// through their linear view, `bake.flags` bit 0: encoded here).

@group(0) @binding(4) var s3d_bake_out: texture_storage_2d_array<S3D_BAKE_FORMAT, write>;

fn s3d_bake_encode_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// The texel of invocation `id` (`None` outside the mip): its uv.
fn s3d_bake_uv(id: vec3<u32>) -> vec2<f32> {
    let size = textureDimensions(s3d_bake_out);
    return (vec2<f32>(id.xy) + 0.5) / vec2<f32>(size);
}

fn s3d_bake_inside(id: vec3<u32>) -> bool {
    let size = textureDimensions(s3d_bake_out);
    return id.x < size.x && id.y < size.y;
}

fn s3d_bake_store(id: vec3<u32>, c: vec3<f32>) {
    var o = c;
    if ((bake.flags & 1u) != 0u) {
        o = s3d_bake_encode_srgb(clamp(o, vec3<f32>(0.0), vec3<f32>(1.0)));
    }
    textureStore(s3d_bake_out, vec2<i32>(id.xy), i32(id.z), vec4<f32>(o, 1.0));
}

@compute @workgroup_size(8, 8, 1)
fn cs_cube_copy(@builtin(global_invocation_id) id: vec3<u32>) {
    if (s3d_bake_inside(id)) {
        s3d_bake_store(id, s3d_bake_cube_copy(id.z, s3d_bake_uv(id)));
    }
}

@compute @workgroup_size(8, 8, 1)
fn cs_equirect(@builtin(global_invocation_id) id: vec3<u32>) {
    if (s3d_bake_inside(id)) {
        s3d_bake_store(id, s3d_bake_equirect(id.z, s3d_bake_uv(id)));
    }
}

@compute @workgroup_size(8, 8, 1)
fn cs_prefilter(@builtin(global_invocation_id) id: vec3<u32>) {
    if (s3d_bake_inside(id)) {
        s3d_bake_store(id, s3d_bake_prefilter(id.z, s3d_bake_uv(id)));
    }
}

@compute @workgroup_size(8, 8, 1)
fn cs_irradiance(@builtin(global_invocation_id) id: vec3<u32>) {
    if (s3d_bake_inside(id)) {
        s3d_bake_store(id, s3d_bake_irradiance(id.z, s3d_bake_uv(id)));
    }
}
