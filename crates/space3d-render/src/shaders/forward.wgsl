// Space3d forward shader: instanced meshes, directional + point lights, metallic/roughness
// Blinn-Phong with Schlick Fresnel, linear-space lighting, sRGB output.
//
// The INSTANCE marker below is replaced by the renderer with an `InstanceInput` struct and
// accessor functions generated from the instance layout (see `pipeline/instance.rs`); the CONSTS
// marker with per-pipeline constants; the TEXTURE marker with the material texture binding
// (`texture_2d` for single-layer textures, `texture_2d_array` for arrays) and `sample_base`;
// the VARYINGS marker with extra `VsOut` fields and the HOOKS marker with `vertex_hook` /
// `fragment_hook` — identity functions, or a material's `CustomShader`.
// Single-layer textures are never bound as arrays: wgpu's GL backend binds them with the
// TEXTURE_2D target, which a `sampler2DArray` reads as black on WebGL2/GLES.

//#CONSTS

const MAX_DIR_LIGHTS: u32 = 4u;
const MAX_POINT_LIGHTS: u32 = 16u;

struct DirLight {
    direction: vec4<f32>,   // xyz: direction the light travels (normalized)
    color: vec4<f32>,       // rgb: color * intensity
}

struct PointLight {
    position_range: vec4<f32>,
    color: vec4<f32>,
}

struct View {
    view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,
    ambient: vec4<f32>,
    counts: vec4<u32>,      // x: dir lights, y: point lights
    dir_lights: array<DirLight, 4>,
    point_lights: array<PointLight, 16>,
}

struct MaterialParams {
    base_color: vec4<f32>,
    emissive: vec4<f32>,    // w: unlit flag
    params: vec4<f32>,      // x: metallic, y: roughness, z: has texture
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

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) color: vec4<f32>,
    @location(4) @interpolate(flat) layer: i32,
//#VARYINGS
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

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

//#HOOKS

@vertex
fn vs_main(v: VertexInput, inst: InstanceInput) -> VsOut {
    let model = instance_model(inst);
    let world = model * vec4<f32>(v.position, 1.0);
    // Normal matrix = cofactor of the upper 3x3 (inverse-transpose up to scale), sign-corrected
    // for mirrored transforms. Handles non-uniform scale without a CPU-side inverse.
    let a = model[0].xyz;
    let b = model[1].xyz;
    let c = model[2].xyz;
    let cof = mat3x3<f32>(cross(b, c), cross(c, a), cross(a, b));
    let det = dot(a, cof[0]);
    let n = cof * v.normal * select(1.0, -1.0, det < 0.0);

    var out: VsOut;
    out.clip = view.view_proj * world;
    out.world_pos = world.xyz;
    out.normal = n;
    let rect = instance_uv_rect(inst);
    out.uv = v.uv * rect.zw + rect.xy;
    out.color = vec4<f32>(srgb_to_linear(v.color.rgb), v.color.a) * instance_color(inst);
    out.layer = instance_layer(inst);
    return vertex_hook(v, inst, out);
}

fn fresnel_schlick(f0: vec3<f32>, cos_theta: f32) -> vec3<f32> {
    return f0 + (vec3<f32>(1.0) - f0) * pow(1.0 - cos_theta, 5.0);
}

fn brdf(base: vec3<f32>, metallic: f32, roughness: f32, n: vec3<f32>, v: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    let ndl = max(dot(n, l), 0.0);
    if (ndl <= 0.0) {
        return vec3<f32>(0.0);
    }
    let h = normalize(l + v);
    let ndh = max(dot(n, h), 0.0);
    let vdh = max(dot(v, h), 0.0);
    let r = clamp(roughness, 0.02, 1.0);
    let shininess = exp2(12.0 * (1.0 - r) + 1.0);
    let f0 = mix(vec3<f32>(0.04), base, metallic);
    let f = fresnel_schlick(f0, vdh);
    let spec = f * pow(ndh, shininess) * (shininess + 8.0) / 8.0;
    let kd = (vec3<f32>(1.0) - f) * (1.0 - metallic);
    return (kd * base + spec) * ndl;
}

@fragment
fn fs_main(in: VsOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    // Sample unconditionally (uniform control flow); `params.z` blends it in.
    let tex = sample_base(in.uv, in.layer);
    let texel = mix(vec4<f32>(1.0), tex, material.params.z);
    let base = material.base_color * in.color * texel;

    var rgb: vec3<f32>;
    if (material.emissive.w > 0.5) {
        rgb = base.rgb + material.emissive.rgb;
    } else {
        var n = normalize(in.normal);
        if (!front) {
            n = -n;
        }
        let v = normalize(view.camera_pos.xyz - in.world_pos);
        let metallic = material.params.x;
        let roughness = material.params.y;
        var acc = view.ambient.rgb * base.rgb;
        for (var i = 0u; i < min(view.counts.x, MAX_DIR_LIGHTS); i++) {
            let light = view.dir_lights[i];
            acc += brdf(base.rgb, metallic, roughness, n, v, -light.direction.xyz) * light.color.rgb;
        }
        for (var i = 0u; i < min(view.counts.y, MAX_POINT_LIGHTS); i++) {
            let light = view.point_lights[i];
            let to_light = light.position_range.xyz - in.world_pos;
            let d = length(to_light);
            let range = light.position_range.w;
            let falloff = clamp(1.0 - pow(d / range, 4.0), 0.0, 1.0);
            let atten = falloff * falloff / (d * d + 1.0);
            acc += brdf(base.rgb, metallic, roughness, n, v, to_light / max(d, 1e-4)) * light.color.rgb * atten;
        }
        rgb = acc + material.emissive.rgb;
    }
    var color = fragment_hook(in, vec4<f32>(rgb, base.a));
    if (MANUAL_SRGB) {
        color = vec4<f32>(linear_to_srgb(clamp(color.rgb, vec3<f32>(0.0), vec3<f32>(1.0))), color.a);
    }
    return color;
}
