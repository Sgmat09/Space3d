// Space3d depth-only shader (depth views: shadow maps; graphics spec §3.4, milestone F7).
//
// Composed like `forward.wgsl`: the VIEW marker becomes the shared `View` struct (group 0
// binding 0 = the depth view's own `GpuView` slot, whose `view_proj` is the light matrix), the
// INSTANCE marker the instance layout's `InstanceInput` and accessors, the TEXTURE marker the
// material's base texture binding and `sample_base` (single-layer or array).
//
// `vs_depth` (PassKind::Depth) has no fragment stage. `vs_mask` + `fs_mask` (PassKind::DepthMask)
// discard alpha-masked texels; they read the material (group 1) like the forward pass. Custom
// shaders are not applied here: vertex hooks do not displace shadows (graphics spec §6.2).

//#VIEW

struct MaterialParams {
    base_color: vec4<f32>,
    emissive: vec4<f32>,
    params: vec4<f32>,      // x: metallic, y: roughness, z: has texture
    maps: vec4<f32>,        // x: normal scale, y: occlusion strength, z: alpha cutoff
}

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var<uniform> material: MaterialParams;
@group(1) @binding(2) var base_sampler: sampler;
//#TEXTURE

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) color: vec4<f32>,
}

//#INSTANCE

fn quat_to_mat3(q_in: vec4<f32>) -> mat3x3<f32> {
    let q = normalize(q_in);
    let x2 = q.x + q.x;
    let y2 = q.y + q.y;
    let z2 = q.z + q.z;
    let xx = q.x * x2;
    let xy = q.x * y2;
    let xz = q.x * z2;
    let yy = q.y * y2;
    let yz = q.y * z2;
    let zz = q.z * z2;
    let wx = q.w * x2;
    let wy = q.w * y2;
    let wz = q.w * z2;
    return mat3x3<f32>(
        vec3<f32>(1.0 - (yy + zz), xy + wz, xz - wy),
        vec3<f32>(xy - wz, 1.0 - (xx + zz), yz + wx),
        vec3<f32>(xz + wy, yz - wx, 1.0 - (xx + yy)),
    );
}

fn trs(t: vec3<f32>, r: mat3x3<f32>, s: vec3<f32>) -> mat4x4<f32> {
    return mat4x4<f32>(
        vec4<f32>(r[0] * s.x, 0.0),
        vec4<f32>(r[1] * s.y, 0.0),
        vec4<f32>(r[2] * s.z, 0.0),
        vec4<f32>(t, 1.0),
    );
}

@vertex
fn vs_depth(v: VertexInput, inst: InstanceInput) -> @builtin(position) vec4<f32> {
    return view.view_proj * (instance_model(inst) * vec4<f32>(v.position, 1.0));
}

struct MaskOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) alpha: f32,
    @location(2) @interpolate(flat) layer: i32,
}

@vertex
fn vs_mask(v: VertexInput, inst: InstanceInput) -> MaskOut {
    var out: MaskOut;
    out.clip = view.view_proj * (instance_model(inst) * vec4<f32>(v.position, 1.0));
    let rect = instance_uv_rect(inst);
    out.uv = v.uv * rect.zw + rect.xy;
    out.alpha = v.color.a * instance_color(inst).a;
    out.layer = instance_layer(inst);
    return out;
}

@fragment
fn fs_mask(in: MaskOut) {
    // Sampled unconditionally (uniform control flow); `params.z` blends it in.
    let tex = sample_base(in.uv, in.layer).a;
    let alpha = material.base_color.a * in.alpha * mix(1.0, tex, material.params.z);
    // The material's cutoff (`Material::alpha_cutoff`).
    if (alpha < material.maps.z) {
        discard;
    }
}
