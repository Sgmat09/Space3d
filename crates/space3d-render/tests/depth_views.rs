//! Depth views (graphics spec §1.12, milestone F7): depth-only views — shadow maps — requested
//! through the lighting contract, injected here with `space3d_render::testing` (the lighting's
//! own shadow fitting is the materials area's). A depth view records the opaque casters,
//! culled against its own (light) frustum on every draw path — on GPU paths as one more frustum
//! of the camera views' culling dispatches — and draws them in a depth-only pass before the
//! camera views. A probe node samples the depth map in the camera pass, so each test reads the
//! map back through an ordinary capture (on every backend, GL included).

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::testing;
use space3d_render::*;

/// Capture and depth map size (one capture pixel per depth texel).
const S: u32 = 64;

/// Orthographic light 10 units above the origin looking down (screen right = +X, up = −Z),
/// covering x, z in ±4 and heights 10 (near) to −10 (far): depth = (10 − y) / 20.
fn light_matrix() -> Mat4 {
    let view = glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 10.0, 0.0), Vec3::ZERO, Vec3::NEG_Z);
    glam::camera::rh::proj::directx::orthographic(-4.0, 4.0, -4.0, 4.0, 0.0, 20.0) * view
}

/// Expected depth of a caster top at height `y`.
fn depth_at_height(y: f32) -> f32 {
    (10.0 - y) / 20.0
}

/// Map texel (column, row) a world point projects to.
fn texel(p: Vec3) -> (u32, u32) {
    let ndc = light_matrix().project_point3(p);
    let x = ((ndc.x + 1.0) * 0.5 * S as f32) as u32;
    let y = ((1.0 - ndc.y) * 0.5 * S as f32) as u32;
    (x.min(S - 1), y.min(S - 1))
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Depth the probe wrote at the texel of world point `p`.
fn depth(img: &ImageData, p: Vec3) -> f32 {
    let (x, y) = texel(p);
    srgb_to_linear(img.get(x, y)[0] as f32 / 255.0)
}

/// Draws a depth map over the whole target in the camera's Opaque phase (after the built-in
/// batches): `rgb = depth` sampled at the pixel.
struct DepthMapProbe {
    map: wgpu::TextureView,
    pipelines: Vec<(PassTarget, wgpu::RenderPipeline)>,
    bind_group: Option<wgpu::BindGroup>,
    layout: Option<wgpu::BindGroupLayout>,
}

static PROBE_SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Opaque, 100)];

const PROBE_WGSL: &str = "
@group(0) @binding(0) var map: texture_2d<f32>;
@group(0) @binding(1) var map_sampler: sampler;
@fragment
fn fs_probe(in: FullscreenOut) -> @location(0) vec4<f32> {
    let d = textureSampleLevel(map, map_sampler, in.uv, 0.0).x;
    return vec4<f32>(d, d, d, 1.0);
}
";

impl DepthMapProbe {
    fn pipeline(&mut self, device: &wgpu::Device, target: PassTarget) -> &wgpu::RenderPipeline {
        if let Some(i) = self.pipelines.iter().position(|(t, _)| *t == target) {
            return &self.pipelines[i].1;
        }
        let layout = self.layout.get_or_insert_with(|| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("depth map probe"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                        count: None,
                    },
                ],
            })
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("depth map probe"),
            source: wgpu::ShaderSource::Wgsl(format!("{FULLSCREEN_WGSL}\n{PROBE_WGSL}").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("depth map probe"),
            bind_group_layouts: &[Some(layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("depth map probe"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_fullscreen"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: target.depth_stencil(wgpu::CompareFunction::Always, false),
            multisample: target.multisample(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_probe"),
                compilation_options: Default::default(),
                targets: &[Some(target.color_target(None))],
            }),
            multiview_mask: None,
            cache: None,
        });
        if self.bind_group.is_none() {
            let sampler = device.create_sampler(&Default::default());
            self.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("depth map probe"),
                layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&self.map),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            }));
        }
        self.pipelines.push((target, pipeline));
        &self.pipelines.last().expect("pushed").1
    }
}

impl RenderNode for DepthMapProbe {
    fn slots(&self) -> &[PhaseSlot] {
        &PROBE_SLOTS
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let pipeline = self.pipeline(ctx.device, ctx.target).clone();
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, self.bind_group.as_ref(), &[]);
        pass.draw(0..3, 0..1);
    }
}

/// A cube of side 1 whose top is at height `top`.
fn cube_at(x: f32, top: f32, z: f32) -> Transform {
    Transform::from_xyz(x, top - 0.5, z)
}

/// The shadow scene: an ECS caster at (−2, −2) (top 0.5), an explicit instance buffer of three
/// casters at (2, 2), (2, −2), (−2, 2) (tops 1.5), a blended (non-casting) cube at the origin
/// (top 3), a camera looking at the region around x = 12 (none of the casters is in its view)
/// and one injected depth view with the probe drawing it. Returns the renderer's probe map.
fn shadow_scene(app: &mut App) -> wgpu::TextureView {
    let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    app.world
        .spawn((cube_at(-2.0, 0.5, -2.0), Mesh3d(cube), Material::srgb(0.8, 0.3, 0.2)));
    // `InstanceCompact`: culled by translation and scale (the ECS path culls by matrix).
    let instances = [(2.0, 2.0), (2.0, -2.0), (-2.0, 2.0)]
        .map(|(x, z)| InstanceCompact::new(Vec3::new(x, 1.0, z), Quat::IDENTITY, 1.0));
    let h = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(instances.to_vec(), BufferUsage::Static));
    app.world.spawn((InstancedMesh {
        mesh: cube,
        material: Material::srgb(0.2, 0.6, 0.9),
        instances: h,
    },));
    app.world.spawn((
        cube_at(0.0, 3.0, 0.0),
        Mesh3d(cube),
        Material {
            alpha_blend: true,
            ..Material::srgb(0.9, 0.9, 0.2)
        },
    ));
    app.world.spawn((
        Transform::from_xyz(12.0, 0.0, 10.0),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
    ));
    let renderer = app.resources.expect_mut::<Renderer>();
    let (_, map) = testing::add_depth_view(renderer, light_matrix(), S, Default::default());
    app.resources.expect_mut::<RenderNodes>().insert(
        "depth-map-probe",
        DepthMapProbe {
            map: map.clone(),
            pipelines: Vec::new(),
            bind_group: None,
            layout: None,
        },
    );
    map
}

fn render(app: &mut App) -> ImageData {
    let img = render_to_image(app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    img
}

fn assert_depth(img: &ImageData, p: Vec3, expected: f32, what: &str) {
    let d = depth(img, p);
    assert!((d - expected).abs() < 0.01, "{what}: depth {d}, expected {expected}");
}

/// Every caster's depth lands in the map on every draw path — ECS and explicit instances,
/// casters the camera does not see included — blended materials do not cast, and the stats
/// report the depth view.
#[test]
fn depth_view_renders_caster_depth() {
    let Some(mut app) = headless_app(S, S) else { return };
    shadow_scene(&mut app);
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        let img = render(&mut app);
        let what = |s: &str| format!("{s} ({path:?})");
        assert_depth(
            &img,
            Vec3::new(-2.0, 0.0, -2.0),
            depth_at_height(0.5),
            &what("ECS caster"),
        );
        for (x, z) in [(2.0, 2.0), (2.0, -2.0), (-2.0, 2.0)] {
            assert_depth(
                &img,
                Vec3::new(x, 0.0, z),
                depth_at_height(1.5),
                &what("instanced caster"),
            );
        }
        assert_depth(&img, Vec3::ZERO, 1.0, &what("blended cube (casts nothing)"));
        assert_depth(&img, Vec3::new(0.0, 0.0, -3.5), 1.0, &what("empty texel"));
        let s = stats(&app);
        assert_eq!(s.depth_views, 1, "{path:?}");
        assert_eq!(s.shadow_casters, 4, "{path:?}: submitted opaque casters");
        assert!(s.shadow_draw_calls >= 2, "{path:?}: {}", s.shadow_draw_calls);
        // The camera sees none of the casters: its own counters ignore the depth view.
        assert_eq!(s.instances, 5, "{path:?}: camera instances");
        assert_eq!(s.views, 1);
    }
}

/// On GPU paths a depth view is one more frustum in the camera views' culling dispatches: the
/// dispatch count does not change.
#[test]
fn depth_views_add_no_dispatches_on_gpu_paths() {
    let Some(mut app) = headless_app(S, S) else { return };
    shadow_scene(&mut app);
    for path in paths_up_to(best_path(&app)).into_iter().filter(|p| p.gpu_culling()) {
        set_path(&mut app, path);
        render(&mut app);
        let with = stats(&app).compute_dispatches;
        testing::clear_depth_views(app.resources.expect_mut::<Renderer>());
        render(&mut app);
        let without = stats(&app).compute_dispatches;
        assert!(without > 0, "{path:?}");
        assert_eq!(with, without, "{path:?}: dispatches with and without the depth view");
        assert_eq!(stats(&app).depth_views, 0);
        testing::add_depth_view(
            app.resources.expect_mut::<Renderer>(),
            light_matrix(),
            S,
            Default::default(),
        );
    }
}

/// Culling uses the depth view's own frustum: casters outside the camera's view but inside the
/// light's are drawn into the map (above), casters outside the light's are culled — on the CPU
/// and on the GPU.
#[test]
fn depth_view_culls_offscreen_casters() {
    let Some(mut app) = headless_app(S, S) else { return };
    shadow_scene(&mut app);
    // Outside the light frustum (x, z in ±4), in the camera's view.
    let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    for x in [11.0, 12.0, 13.0] {
        app.world
            .spawn((cube_at(x, 0.5, 0.0), Mesh3d(cube), Material::srgb(0.8, 0.3, 0.2)));
    }
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        let img = render(&mut app);
        let s = stats(&app);
        assert_eq!(s.shadow_casters, 7, "{path:?}: casters submitted");
        let drawn = testing::depth_view_instances(app.resources.expect::<Renderer>(), 0);
        assert_eq!(drawn, 4, "{path:?}: casters drawn into the depth view");
        assert_depth(&img, Vec3::new(-2.0, 0.0, -2.0), depth_at_height(0.5), "ECS caster");
        // The camera draws the three cubes in its view (and culls the rest on the CPU path).
        if path == DrawPath::CpuDirect {
            assert_eq!(s.instances_drawn, 3, "camera instances drawn");
        }
    }
}

/// Alpha-masked casters draw with the `DepthMask` pipeline (material bound in group 1): texels
/// below the cutoff cast nothing.
#[test]
fn depth_mask_discards_masked_texels() {
    let Some(mut app) = headless_app(S, S) else { return };
    shadow_scene(&mut app);
    let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    // Opaque materials (not blended) with a low alpha: masked away by the DepthMask pipeline.
    app.world.spawn((
        cube_at(0.0, 2.0, -3.0),
        Mesh3d(cube),
        Material::color(Vec4::new(1.0, 1.0, 1.0, 0.2)),
    ));
    testing::force_depth_mask(app.resources.expect_mut::<Renderer>(), true);
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        let img = render(&mut app);
        assert_depth(&img, Vec3::new(-2.0, 0.0, -2.0), depth_at_height(0.5), "opaque caster");
        assert_depth(&img, Vec3::new(2.0, 0.0, 2.0), depth_at_height(1.5), "instanced caster");
        assert_depth(&img, Vec3::new(0.0, 0.0, -3.0), 1.0, "masked caster");
    }
    testing::force_depth_mask(app.resources.expect_mut::<Renderer>(), false);
    let img = render(&mut app);
    assert_depth(&img, Vec3::new(0.0, 0.0, -3.0), depth_at_height(2.0), "unmasked");
}

/// A still scene with a depth view uploads nothing after the first frames, and the renderer's
/// own work allocates nothing (no scratch growth, no new textures).
#[test]
fn static_depth_views_upload_nothing() {
    let Some(mut app) = headless_app(S, S) else { return };
    shadow_scene(&mut app);
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        for _ in 0..3 {
            render(&mut app);
        }
        let s = stats(&app);
        assert_eq!(s.bytes_uploaded, 0, "{path:?}");
        assert_eq!(s.cpu_reallocations, 0, "{path:?}");
        assert_eq!(s.textures_created, 0, "{path:?}");
        assert_eq!(s.depth_views, 1);
    }
}

/// Several depth views, each with its own frustum and target, in one frame.
#[test]
fn several_depth_views_render_their_own_frustums() {
    let Some(mut app) = headless_app(S, S) else { return };
    shadow_scene(&mut app);
    // A second light looking at the scene from below: it sees the casters' bottom faces.
    let below = {
        let view = glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, -10.0, 0.0), Vec3::ZERO, Vec3::Z);
        glam::camera::rh::proj::directx::orthographic(-4.0, 4.0, -4.0, 4.0, 0.0, 20.0) * view
    };
    let renderer = app.resources.expect_mut::<Renderer>();
    let (_, second) = testing::add_depth_view(renderer, below, S, Default::default());
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        render(&mut app);
        let s = stats(&app);
        assert_eq!(s.depth_views, 2, "{path:?}");
        assert_eq!(s.shadow_casters, 8, "{path:?}");
    }
    // Probe the second map instead: seen from below, the ECS cube's bottom (y = −0.5) is at
    // depth (10 − 0.5) / 20 and the instanced cubes' bottoms (y = 0.5) at (10 + 0.5) / 20.
    app.resources.expect_mut::<RenderNodes>().insert(
        "depth-map-probe",
        DepthMapProbe {
            map: second,
            pipelines: Vec::new(),
            bind_group: None,
            layout: None,
        },
    );
    let img = render(&mut app);
    // Screen up = +Z from below, right = +X mirrored: find the texels through the matrix.
    let at = |p: Vec3| {
        let ndc = below.project_point3(p);
        let x = (((ndc.x + 1.0) * 0.5 * S as f32) as u32).min(S - 1);
        let y = (((1.0 - ndc.y) * 0.5 * S as f32) as u32).min(S - 1);
        srgb_to_linear(img.get(x, y)[0] as f32 / 255.0)
    };
    let d = at(Vec3::new(-2.0, 0.0, -2.0));
    assert!((d - 9.5 / 20.0).abs() < 0.01, "ECS cube from below: {d}");
    let d = at(Vec3::new(2.0, 0.0, 2.0));
    assert!((d - 10.5 / 20.0).abs() < 0.01, "instanced cube from below: {d}");
}
