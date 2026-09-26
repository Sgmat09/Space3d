//! A render node (graphics spec §2.2.4): a full-screen tint that reads scene depth, drawn in a
//! Transparent slot after the built-in meshes. Renders headless and saves a PNG.
//!
//! `cargo run -p space3d-render --example frame_nodes -- out.png [msaa] [webgl2]`
//!
//! * `msaa`: 4× MSAA — the multisampled depth is resolved (minimum of the samples) for the
//!   node's single-sample SceneRead pass;
//! * `webgl2`: `DownlevelOverride::webgl2()` — no read-only depth attachments, so the node
//!   samples depth without one (`DepthAccess::SampledOnly`).

use space3d_core::prelude::*;
use space3d_render::*;

/// Tints the scene by its distance: warm near the camera, cool far away.
struct DepthTint {
    /// The renderer the pipelines below were built for, and its shared layouts.
    renderer: u64,
    layouts: Option<(wgpu::BindGroupLayout, wgpu::BindGroupLayout)>,
    pipelines: Vec<(PassTarget, wgpu::RenderPipeline)>,
}

/// One Transparent slot at order 100 that reads scene depth: it draws in the view's SceneRead
/// pass, after the built-in blended meshes and every slot that does not read depth.
static SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Transparent, 100).reading(SceneReads::DEPTH)];

impl DepthTint {
    /// The pipeline for one pass signature: `VIEW_WGSL` (group 0), `scene_wgsl` (group 1, per
    /// depth access) and `OUTPUT_WGSL` (the encoding the pass needs) in front of our shader.
    fn pipeline(&mut self, device: &wgpu::Device, target: PassTarget) -> Option<&wgpu::RenderPipeline> {
        let (view, scene) = self.layouts.as_ref()?;
        if !self.pipelines.iter().any(|(t, _)| *t == target) {
            let src = format!(
                "{VIEW_WGSL}\n{}\n{OUTPUT_WGSL}\n{FULLSCREEN_WGSL}\n
                @fragment fn fs(in: FullscreenOut) -> @location(0) vec4<f32> {{
                    let px = vec2<i32>(in.pos.xy);
                    // Positive view distance of the scene under this pixel (far plane: sky).
                    let t = smoothstep(3.0, 14.0, s3d_view_depth_at(px));
                    let tint = mix(vec3<f32>(1.0, 0.55, 0.2), vec3<f32>(0.2, 0.45, 1.0), t);
                    let alpha = select(0.0, 0.35, s3d_depth_valid() && s3d_scene_depth(px) < 1.0);
                    return s3d_output(vec4<f32>(tint, alpha), {}u, view.exposure.x, view.output.y);
                }}",
                scene_wgsl(&target),
                target.output.id()
            );
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("depth tint"),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("depth tint"),
                bind_group_layouts: &[Some(view), Some(scene)],
                immediate_size: 0,
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("depth tint"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                // Outside the Opaque phase: never write depth (read-only where attached).
                depth_stencil: target.depth_stencil(wgpu::CompareFunction::Always, false),
                multisample: target.multisample(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(target.color_target(Some(wgpu::BlendState::ALPHA_BLENDING)))],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.pipelines.push((target, pipeline));
        }
        self.pipelines.iter().find(|(t, _)| *t == target).map(|(_, p)| p)
    }
}

impl RenderNode for DepthTint {
    fn slots(&self) -> &[PhaseSlot] {
        &SLOTS
    }

    /// Once per frame, before any view: keep the renderer's layouts (a new renderer — a new
    /// device — invalidates the pipelines).
    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        if ctx.frame.renderer != self.renderer {
            self.renderer = ctx.frame.renderer;
            self.layouts = Some((ctx.layouts.view.clone(), ctx.layouts.scene.clone()));
            self.pipelines.clear();
        }
    }

    /// Before the first draw with a new pass signature (and from `Renderer::prewarm`): build
    /// the pipelines here, not in `draw` (WebGL2 links programs at creation).
    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        for &(_, target) in targets {
            self.pipeline(device, target);
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let Some(pipeline) = self.pipeline(ctx.device, ctx.target) else {
            return;
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "frame_nodes.png".into());
    let flags: Vec<String> = args.collect();

    let mut app = App::new();
    app.add_plugin(RenderPlugin::headless(640, 360));
    if !app.resources.contains::<Renderer>() {
        eprintln!("no GPU adapter available");
        return;
    }
    if flags.iter().any(|f| f == "msaa") {
        app.resources.expect_mut::<Renderer>().set_msaa_samples(4);
    }
    if flags.iter().any(|f| f == "webgl2") {
        app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride::webgl2()));
    }
    let (cube, plane) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(1.0)), a.add_mesh(MeshData::plane(60.0)))
    };
    app.world.spawn((
        Transform::from_xyz(0.0, -0.5, 0.0),
        Mesh3d(plane),
        Material::srgb(0.7, 0.7, 0.72).with_metallic_roughness(0.0, 0.9),
    ));
    for i in 0..12 {
        let z = -(i as f32) * 2.0;
        for x in [-2.0f32, 2.0] {
            app.world.spawn((
                Transform::from_xyz(x, 0.0, z).with_rotation(Quat::from_rotation_y(i as f32 * 0.3)),
                Mesh3d(cube),
                Material::srgb(0.85, 0.85, 0.85).with_metallic_roughness(0.0, 0.5),
            ));
        }
    }
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).looking_at(Vec3::new(-0.4, -1.0, -0.5), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.5,
        },
    ));
    app.insert_resource(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.15,
    });
    app.world.spawn((
        Transform::from_xyz(0.0, 1.5, 4.0).looking_at(Vec3::new(0.0, 0.0, -6.0), Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.05, 0.07, 0.12, 1.0)),
    ));
    app.resources.expect_mut::<RenderNodes>().insert(
        "example/depth-tint",
        DepthTint {
            renderer: 0,
            layouts: None,
            pipelines: Vec::new(),
        },
    );

    let img = render_to_image(&mut app).expect("capture");
    let stats = app.resources.expect::<RenderStats>();
    println!(
        "node draws {}, SceneRead passes {}, depth resolves {}",
        stats.node_draws, stats.scene_read_passes, stats.depth_resolves
    );
    image::RgbaImage::from_raw(img.width, img.height, img.pixels)
        .expect("size")
        .save(&out)
        .expect("write png");
    println!("wrote {out}");
}
