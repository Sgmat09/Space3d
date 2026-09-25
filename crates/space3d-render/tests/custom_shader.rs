//! Custom shaders: WGSL hooks per material that read custom per-instance attributes.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::wgpu;
use space3d_render::*;

/// Position + scale + a free RGBA value for the custom shader (`Custom(0)`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Tinted {
    translation: [f32; 3],
    scale: f32,
    tint: [f32; 4],
    /// Second custom attribute (`Custom(3)`), a packed unorm8 RGBA.
    glow: [u8; 4],
}

impl InstanceData for Tinted {
    const LAYOUT: &'static InstanceLayout = &InstanceLayout {
        name: "Tinted",
        stride: 36,
        attributes: &[
            InstanceAttribute::new(InstanceSemantic::Translation, wgpu::VertexFormat::Float32x3, 0),
            InstanceAttribute::new(InstanceSemantic::UniformScale, wgpu::VertexFormat::Float32, 12),
            InstanceAttribute::new(InstanceSemantic::Custom(0), wgpu::VertexFormat::Float32x4, 16),
            InstanceAttribute::new(InstanceSemantic::Custom(3), wgpu::VertexFormat::Unorm8x4, 32),
        ],
    };
}

/// Adds `Custom(3)` to the vertex color (which multiplies the material color) and multiplies
/// the lit color by `Custom(0)`, passed to the fragment stage through the `custom` varying.
const TINT: &str = r"
fn vertex_hook(v: VertexInput, inst: InstanceInput, out: VsOut) -> VsOut {
    var o = out;
    o.custom = instance_custom0(inst);
    o.color = vec4<f32>(o.color.rgb + instance_custom3(inst).rgb, o.color.a);
    return o;
}

fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(color.rgb * in.custom.rgb, color.a);
}
";

fn tinted(x: f32, tint: [f32; 4], glow: [u8; 4]) -> Tinted {
    Tinted {
        translation: [x, 0.0, 0.0],
        scale: 1.0,
        tint,
        glow,
    }
}

fn scene(app: &mut App, material: Material) -> Entity {
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    let buf = InstanceBuffer::from_vec(
        vec![
            tinted(-0.8, [1.0, 0.0, 0.0, 1.0], [0, 0, 0, 0]),
            // Green: material 0.5 × (vertex color 1 + glow 1) = 1; tint (0, 1, 0): pure green.
            tinted(0.8, [0.0, 1.0, 0.0, 1.0], [0, 255, 0, 0]),
        ],
        BufferUsage::Static,
    );
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    spawn_camera(app, 4.0);
    app.world.spawn((InstancedMesh {
        mesh,
        material,
        instances,
    },))
}

/// A custom attribute changes pixels: the built-in shader ignores `Custom(n)`, the custom
/// shader reads it (on every draw path; GPU culling copies the whole instance, custom data
/// included).
#[test]
fn custom_attributes_drive_a_custom_shader() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let white = Material::unlit(Vec4::new(1.0, 0.5, 1.0, 1.0));
    let shader = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_shader(CustomShader::new("tint", TINT));
    let e = scene(&mut app, white.clone());
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        app.world.get::<&mut InstancedMesh>(e).unwrap().material = white.clone();
        let plain = render_to_image(&mut app).expect("capture");
        assert_eq!(
            plain.get(20, 32),
            plain.get(44, 32),
            "{path:?}: built-in shader ignores custom data"
        );

        app.world.get::<&mut InstancedMesh>(e).unwrap().material = white.clone().with_shader(shader);
        let img = render_to_image(&mut app).expect("capture");
        assert_eq!(img.get(20, 32), [255, 0, 0, 255], "{path:?}: tint (1, 0, 0)");
        assert_eq!(
            img.get(44, 32),
            [0, 255, 0, 255],
            "{path:?}: 0.5 × (1 + glow) × tint (0, 1, 0)"
        );
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.shader_error(shader), None);
        assert_eq!(r.gpu_error_count(), 0);
    }
}

/// Position + a BGRA-ordered color for the custom shader (`Custom(1)`, `Unorm8x4Bgra`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BgraTagged {
    translation: [f32; 3],
    bgra: [u8; 4],
}

impl InstanceData for BgraTagged {
    const LAYOUT: &'static InstanceLayout = &InstanceLayout {
        name: "BgraTagged",
        stride: 16,
        attributes: &[
            InstanceAttribute::new(InstanceSemantic::Translation, wgpu::VertexFormat::Float32x3, 0),
            InstanceAttribute::new(InstanceSemantic::Custom(1), wgpu::VertexFormat::Unorm8x4Bgra, 12),
        ],
    };
}

/// Regression: `Unorm8x4Bgra` attributes are fetched as `Unorm8x4` (GL cannot fetch BGRA) and
/// swizzled back in the generated shader, but `instance_customN` skipped the swizzle, so a
/// custom shader saw red and blue swapped. Bytes `[0, 0, 255, 255]` in BGRA order are red.
#[test]
fn bgra_custom_attributes_reach_custom_shaders_as_rgba() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let (mesh, shader) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        let shader = a.add_shader(CustomShader::new(
            "bgra",
            r"
fn vertex_hook(v: VertexInput, inst: InstanceInput, out: VsOut) -> VsOut {
    var o = out;
    o.custom = instance_custom1(inst);
    return o;
}
fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {
    return in.custom;
}",
        ));
        (a.add_mesh(MeshData::cube(1.0)), shader)
    };
    let buf = InstanceBuffer::from_vec(
        vec![BgraTagged {
            translation: [0.0; 3],
            bgra: [0, 0, 255, 255],
        }],
        BufferUsage::Static,
    );
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(Vec4::ONE).with_shader(shader),
        instances,
    },));
    spawn_camera(&mut app, 3.0);
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        let img = render_to_image(&mut app).expect("capture");
        assert_eq!(img.get(16, 16), [255, 0, 0, 255], "{path:?}: BGRA red");
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.shader_error(shader), None);
        assert_eq!(r.gpu_error_count(), 0);
    }
}

/// The same custom shader works for ECS entities (their `InstanceTransform` layout has no
/// custom attributes: `instance_customN` returns zero).
#[test]
fn custom_shaders_apply_to_ecs_entities() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let (mesh, invert) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        let invert = a.add_shader(CustomShader::new(
            "invert",
            r"
fn vertex_hook(v: VertexInput, inst: InstanceInput, out: VsOut) -> VsOut {
    var o = out;
    o.custom = instance_custom0(inst) + instance_custom2(inst);
    return o;
}
fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(vec3<f32>(1.0) - color.rgb + in.custom.rgb, color.a);
}",
        ));
        (a.add_mesh(MeshData::cube(1.0)), invert)
    };
    app.world.spawn((
        Transform::default(),
        Mesh3d(mesh),
        Material::unlit(Vec4::new(1.0, 0.0, 0.0, 1.0)).with_shader(invert),
    ));
    spawn_camera(&mut app, 3.0);
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        let img = render_to_image(&mut app).expect("capture");
        assert_eq!(img.get(16, 16), [0, 255, 255, 255], "{path:?}: red inverted");
    }
}

/// A shader that does not compile is reported (once, through `shader_error` and `tracing`) and
/// its draws use the built-in shader — the frame is not lost and no wgpu error escapes. Fixing
/// the source with `update_shader` (hot reload) rebuilds the pipelines; removing the shader
/// reverts the material to the built-in shader.
#[test]
fn broken_custom_shaders_fall_back_and_hot_reload() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let shader = app.resources.expect_mut::<RenderAssets>().add_shader(CustomShader::new(
        "broken",
        "fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> { return undefined_thing; }",
    ));
    scene(&mut app, Material::unlit(Vec4::ONE).with_shader(shader));
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(20, 32), [255, 255, 255, 255], "built-in shader");
    let err = app
        .resources
        .expect::<Renderer>()
        .shader_error(shader)
        .map(str::to_owned);
    assert!(err.as_deref().is_some_and(|e| e.contains("undefined_thing")), "{err:?}");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);

    assert!(
        app.resources
            .expect_mut::<RenderAssets>()
            .update_shader(shader, CustomShader::new("fixed", TINT))
    );
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(20, 32), [255, 0, 0, 255], "reloaded shader");
    assert_eq!(app.resources.expect::<Renderer>().shader_error(shader), None);

    assert!(app.resources.expect_mut::<RenderAssets>().remove_shader(shader));
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(20, 32), [255, 255, 255, 255], "removed shader: built-in");
}

/// Custom shaders cannot declare resources of their own (the forward pipeline layout has none
/// for them). naga accepts such WGSL, so it is checked explicitly before any GPU work: on
/// browser WebGPU the pipeline's validation error only arrives later, and every frame drawing
/// with the invalid pipeline would be lost until then. It falls back like a broken shader.
#[test]
fn custom_shaders_declaring_their_own_bindings_fall_back() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let shader = app.resources.expect_mut::<RenderAssets>().add_shader(CustomShader::new(
        "own binding",
        "@group(2) @binding(0) var<uniform> extra: vec4<f32>;\n\
         fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> { return extra; }",
    ));
    scene(&mut app, Material::unlit(Vec4::ONE).with_shader(shader));
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(20, 32), [255, 255, 255, 255], "built-in shader");
    let r = app.resources.expect::<Renderer>();
    let err = r.shader_error(shader).unwrap_or_default();
    assert!(
        err.contains("`extra`") && err.contains("@group(2) @binding(0)"),
        "{err}"
    );
    assert_eq!(r.gpu_error_count(), 0);
}

/// Workgroup memory exists only in compute shaders: naga accepts `var<workgroup>` in the
/// vertex and fragment hooks, but backends disagree about it (GL rejects it, browsers only
/// report it asynchronously). It is rejected before any GPU work, like declared bindings, on
/// every backend — the draws use the built-in shader and `shader_error` says why.
#[test]
fn custom_shaders_using_workgroup_memory_fall_back() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let shader = app.resources.expect_mut::<RenderAssets>().add_shader(CustomShader::new(
        "workgroup",
        "var<workgroup> tint: vec4<f32>;\n\
         fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {\n\
         \x20   tint = vec4<f32>(1.0, 0.0, 0.0, 1.0);\n    return tint;\n}\n",
    ));
    scene(&mut app, Material::unlit(Vec4::ONE).with_shader(shader));
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(20, 32), [255, 255, 255, 255], "built-in shader");
    let r = app.resources.expect::<Renderer>();
    let err = r.shader_error(shader).unwrap_or_default();
    assert!(err.contains("`var<workgroup> tint`"), "{err}");
    assert_eq!(r.gpu_error_count(), 0);
}

/// Built-ins WebGL2 / GLES 3.0 (GLSL ES 3.00) lack are rejected on every backend, before any
/// GPU work: the shader would run on Metal, Vulkan, DX12 and WebGPU but not compile on WebGL2.
/// Those naga polyfills for GLSL ES 3.00 build on every backend — this test also runs on GL
/// (`WGPU_BACKEND=gl`, e.g. ANGLE on macOS: GLES 3.0), where the shader really compiles.
#[test]
fn custom_shaders_are_limited_to_what_webgl2_compiles() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let rejected = app.resources.expect_mut::<RenderAssets>().add_shader(CustomShader::new(
        "bits",
        "fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {\n\
         \x20   return vec4<f32>(f32(countOneBits(u32(color.r * 3.0))) * 0.5, 0.0, 0.0, 1.0);\n}\n",
    ));
    let portable = app.resources.expect_mut::<RenderAssets>().add_shader(CustomShader::new(
        "portable",
        "fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {\n\
         \x20   let f = frexp(4.0);\n\
         \x20   let lz = f32(countLeadingZeros(1u)) / 31.0;\n\
         \x20   let g = fma(0.5, 0.5, 0.25) + dpdxFine(in.uv.x) * 0.0 + unpack4x8unorm(0xff00u).y;\n\
         \x20   return vec4<f32>(f.fract * 2.0 * lz, g - 1.0, 0.0, 1.0);\n}\n",
    ));
    let a = scene(&mut app, Material::unlit(Vec4::ONE).with_shader(rejected));
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(20, 32), [255, 255, 255, 255], "built-in shader");
    let r = app.resources.expect::<Renderer>();
    let err = r.shader_error(rejected).unwrap_or_default();
    assert!(err.contains("`countOneBits`") && err.contains("WebGL2"), "{err}");
    app.world.get::<&mut InstancedMesh>(a).unwrap().material = Material::unlit(Vec4::ONE).with_shader(portable);
    let img = render_to_image(&mut app).expect("capture");
    let r = app.resources.expect::<Renderer>();
    assert_eq!(r.shader_error(portable), None, "{:?}", r.capabilities().adapter.backend);
    // frexp(4) = 0.5 · 2³, countLeadingZeros(1) = 31, fma = 0.5, unpack4x8unorm(0xff00).y = 1.
    assert_eq!(
        img.get(20, 32),
        [255, 188, 0, 255],
        "custom shader on {:?}",
        r.capabilities().adapter.backend
    );
    assert_eq!(r.gpu_error_count(), 0);
}

/// Every custom-shader fixture of this file renders in every frame mode — Direct, the HDR scene
/// texture (`Linear` output), the Ldr fallback (`Tonemapped` output) and render-scale
/// passthrough — with MSAA on and off, and never falls back to the built-in shader (graphics
/// spec §3.2; the unit test of the same name builds every pipeline permutation).
#[test]
fn custom_shaders_compile_against_every_permutation() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let fixtures = [
        TINT,
        "fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {
             if (MANUAL_SRGB) { return color; }
             return vec4<f32>(s3d_tonemap(view.output.y, color.rgb * view.exposure.x), color.a);
         }",
        "fn vertex_hook(v: VertexInput, inst: InstanceInput, out: VsOut) -> VsOut {
             var o = out;
             o.clip = view.proj * view.view * vec4<f32>(o.world_pos, 1.0);
             return o;
         }",
    ];
    let shaders: Vec<ShaderHandle> = fixtures
        .iter()
        .enumerate()
        .map(|(i, wgsl)| {
            app.resources
                .expect_mut::<RenderAssets>()
                .add_shader(CustomShader::new(format!("fixture {i}"), *wgsl))
        })
        .collect();
    let camera = {
        let mut q = app.world.query::<(Entity, &Camera3d)>();
        q.iter().next().map(|(e, _)| e)
    };
    let camera = camera.unwrap_or_else(|| spawn_camera(&mut app, 4.0));
    let mut entities = Vec::new();
    for &shader in &shaders {
        entities.push(scene(&mut app, Material::color(Vec4::splat(0.5)).with_shader(shader)));
    }
    type Setup = fn(&mut App, Entity);
    let modes: [(&str, Setup); 4] = [
        ("direct", |_, _| {}),
        ("hdr", |app, cam| {
            app.world.insert_one(cam, PostProcess::default()).unwrap()
        }),
        ("ldr", |app, cam| {
            app.world.insert_one(cam, PostProcess::default()).unwrap();
            app.resources.expect_mut::<FrameSettings>().downlevel.no_float_targets = true;
        }),
        ("passthrough", |app, _| {
            app.resources.expect_mut::<FrameSettings>().render_scale = 0.5
        }),
    ];
    for (name, setup) in modes {
        for samples in [1, 4] {
            let _ = app.world.remove_one::<PostProcess>(camera);
            *app.resources.expect_mut::<FrameSettings>() = FrameSettings::default();
            app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
            setup(&mut app, camera);
            render_to_image(&mut app).expect("capture");
            let r = app.resources.expect::<Renderer>();
            for &shader in &shaders {
                assert_eq!(r.shader_error(shader), None, "{name} ×{samples}");
            }
            assert_eq!(r.gpu_error_count(), 0, "{name} ×{samples}");
            assert_eq!(
                stats(&app).intermediate_groups,
                u32::from(name != "direct"),
                "{name} ×{samples}"
            );
        }
    }
    let _ = entities;
}
