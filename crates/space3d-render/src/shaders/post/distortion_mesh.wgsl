// Space3d distortion meshes (`DistortionMaterial`, node "space3d/distortion-meshes"): each mesh
// writes an offset into the post group's distortion buffer. The node prepends VIEW_WGSL,
// scene_wgsl(target), DISTORTION_WGSL and the hash functions. Vertex slot 0 is the mesh arena
// (MESH_VERTEX_LAYOUT, bound at the mesh's first vertex), slot 1 one instance per mesh:
// its model matrix and material. No storage buffers (WebGL2).

@group(2) @binding(0) var s3d_dm_texture: texture_2d<f32>;
@group(2) @binding(1) var s3d_dm_sampler: sampler;

struct DmIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) color: vec4<f32>,
    @location(4) m0: vec4<f32>,
    @location(5) m1: vec4<f32>,
    @location(6) m2: vec4<f32>,
    @location(7) m3: vec4<f32>,
    // strength (view heights), mode (0 noise, 1 texture, 2 radial), inner, outer radius (UV)
    @location(8) params: vec4<f32>,
    // UV scroll offset (x, y), soft depth (world units), 1 = the texture is sRGB-encoded
    @location(9) extra: vec4<f32>,
}

struct DmOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    // Vertex color alpha: a painted weight.
    @location(1) weight: f32,
    // Clip x, y, w: the fragment's position in the view (perspective-correct, so x/w and y/w
    // are its NDC) without knowing the distortion buffer's size.
    @location(2) clip: vec3<f32>,
    // Positive view-space distance (linear in the world, so exact per fragment).
    @location(3) view_depth: f32,
    // The instance's `params` and `extra` (constant over the mesh).
    @location(4) params: vec4<f32>,
    @location(5) extra: vec4<f32>,
}

@vertex
fn vs_distortion(in: DmIn) -> DmOut {
    let model = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let world = model * vec4<f32>(in.position, 1.0);
    let clip = view.view_proj * world;
    var out: DmOut;
    out.pos = clip;
    out.uv = in.uv;
    out.weight = in.color.a;
    out.clip = clip.xyw;
    out.view_depth = s3d_view_depth_of(world.xyz);
    out.params = in.params;
    out.extra = in.extra;
    return out;
}

// Hash of an integer lattice point: 0..1.
fn s3d_dm_hash(p: vec2<i32>, seed: u32) -> f32 {
    let q = bitcast<vec2<u32>>(p);
    return f32(s3d_pcg(q.x ^ s3d_pcg(q.y ^ seed))) * (1.0 / 4294967295.0);
}

// Smooth value noise 0..1 over cells of 1, repeating every `period` cells (so a scrolled UV
// wraps seamlessly at 1 when `p = uv × period`).
fn s3d_dm_value(p: vec2<f32>, period: f32, seed: u32) -> f32 {
    // Wrapped with floats (GLSL ES leaves `%` of negative integers undefined).
    let q = p - period * floor(p / period);
    let fl = floor(q);
    let f = q - fl;
    let u = f * f * (3.0 - 2.0 * f);
    let next = fl + 1.0;
    let i0 = vec2<i32>(fl);
    let i1 = vec2<i32>(next - period * floor(next / period));
    let a = s3d_dm_hash(i0, seed);
    let b = s3d_dm_hash(vec2<i32>(i1.x, i0.y), seed);
    let c = s3d_dm_hash(vec2<i32>(i0.x, i1.y), seed);
    let d = s3d_dm_hash(i1, seed);
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

// Built-in heat-haze field: two octaves of value noise per axis, -1..1, repeating every UV unit.
fn s3d_dm_noise(uv: vec2<f32>) -> vec2<f32> {
    let p = uv * 6.0;
    let x = s3d_dm_value(p, 6.0, 0x51u) + 0.5 * s3d_dm_value(p * 2.0, 12.0, 0x93u);
    let y = s3d_dm_value(p, 6.0, 0x2Du) + 0.5 * s3d_dm_value(p * 2.0, 12.0, 0xC7u);
    return vec2<f32>(x, y) * (2.0 / 1.5) - 1.0;
}

@fragment
fn fs_distortion(in: DmOut) -> @location(0) vec4<f32> {
    // Derivatives and the implicit-LOD sample first, in uniform control flow.
    let duv_dx = dpdx(in.uv);
    let duv_dy = dpdy(in.uv);
    let scrolled = in.uv + in.extra.xy;
    var texel = textureSample(s3d_dm_texture, s3d_dm_sampler, scrolled).rg;

    let mode = in.params.y;
    var dir = vec2<f32>(0.0);
    var amount = 1.0;
    if (mode > 1.5) {
        // Radial: a smooth ring between the inner and outer UV radii, looking toward the center
        // (so the image under the ring is pushed outward).
        let d = in.uv - vec2<f32>(0.5);
        let r = length(d);
        let x = clamp((r - in.params.z) / max(in.params.w - in.params.z, 1e-4), 0.0, 1.0);
        let s = sin(3.14159265 * x);
        amount = s * s;
        dir = -d / max(r, 1e-6);
    } else {
        if (mode > 0.5) {
            if (in.extra.w > 0.5) {
                // An sRGB-format texture is decoded when sampled: back to its stored values.
                let t = clamp(texel, vec2<f32>(0.0), vec2<f32>(1.0));
                texel = select(1.055 * pow(t, vec2<f32>(1.0 / 2.4)) - 0.055, t * 12.92, t <= vec2<f32>(0.0031308));
            }
            dir = texel * 2.0 - 1.0;
        } else {
            dir = s3d_dm_noise(scrolled);
        }
        // Feathered toward the UV rectangle's edges: no hard seam around the mesh.
        let e = min(min(in.uv.x, 1.0 - in.uv.x), min(in.uv.y, 1.0 - in.uv.y));
        amount = smoothstep(0.0, 0.2, e);
    }
    // The UV-space vector on screen: through the inverse of the UV Jacobian, keeping its length
    // (a rotated or mirrored mesh rotates or mirrors its offsets; a mesh seen edge-on writes 0).
    let a = duv_dx.x;
    let b = duv_dy.x;
    let c = duv_dx.y;
    let dd = duv_dy.y;
    let det = a * dd - b * c;
    var screen = vec2<f32>(0.0);
    if (abs(det) > 1e-12) {
        screen = vec2<f32>(dd * dir.x - b * dir.y, -c * dir.x + a * dir.y) * (sign(det) / sqrt(abs(det)));
    }
    // Screen pixels are y down; offsets are +y up.
    let offset = vec2<f32>(screen.x, -screen.y) * in.params.x * amount * in.weight;

    // Occlusion by scene depth, at the scene pixel under this fragment.
    var fade = 1.0;
    if (s3d_depth_valid()) {
        let ndc = in.clip.xy / in.clip.z;
        let px = vec2<i32>(view.viewport.xy + (ndc * vec2<f32>(0.5, -0.5) + 0.5) * view.viewport.zw);
        let scene = s3d_view_depth_at(px);
        let soft = in.extra.z;
        if (soft > 0.0) {
            fade = clamp((scene - in.view_depth) / soft, 0.0, 1.0);
        } else {
            fade = select(1.0, 0.0, scene < in.view_depth);
        }
    }
    return distortion_encode(offset, fade);
}
