//! GPU timings (graphics spec §2.2.6, §4.4; milestone F8): with `FrameSettings::gpu_timing`
//! every pass writes timestamps into one query set, read back 1–3 frames late into the
//! `GpuTimings` resource — culling, depth views, scene passes, SceneRead, post (bloom,
//! composite, FXAA), overlays and render nodes' own passes. Skipped without `TIMESTAMP_QUERY`.

mod common;
use common::nodes::*;
use common::*;
use space3d_core::prelude::*;
use space3d_render::testing;
use space3d_render::*;

/// A node recording an (empty) compute pass in `prepare` with `PrepareContext::compute_timestamps`
/// — the writes borrow the frame's query set, not the context, so they go straight into
/// `ctx.encoder`.
struct TimedCompute;

impl RenderNode for TimedCompute {
    fn slots(&self) -> &[PhaseSlot] {
        &[]
    }
    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        let timestamp_writes = ctx.compute_timestamps("test.compute");
        let pass = ctx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("timed compute"),
            timestamp_writes,
        });
        drop(pass);
        ctx.count_compute_passes(1);
    }
    fn draw(&mut self, _: &mut wgpu::RenderPass<'static>, _: &DrawContext<'_>) {}
}

/// An AfterTonemap overlay drawing nothing (its pass is still timed).
struct Empty;

impl RenderOverlay for Empty {
    fn prepare(&mut self, _: &wgpu::Device, _: &wgpu::Queue, _: &mut wgpu::CommandEncoder, _: &OverlayTarget) {}
    fn render(&self, _: &mut wgpu::RenderPass<'static>, _: &OverlayTarget) {}
}

fn timed_app() -> Option<(App, Entity)> {
    let mut app = headless_app(320, 180)?;
    if !app.resources.expect::<Renderer>().graphics_caps().timestamps {
        eprintln!("skipping: no TIMESTAMP_QUERY");
        return None;
    }
    let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    for i in 0..8 {
        app.world.spawn((
            Transform::from_xyz(i as f32 - 3.5, 0.0, 0.0),
            Mesh3d(cube),
            Material::srgb(0.8, 0.4, 0.2).with_emissive(Vec3::splat(i as f32)),
        ));
    }
    spawn_sun(&mut app);
    let camera = spawn_camera(&mut app, 8.0);
    app.world
        .insert_one(camera, PostProcess::default().with_fxaa(FxaaQuality::Low))
        .unwrap();
    app.insert_resource(FrameSettings::default().with_gpu_timing(true));
    let light = glam::camera::rh::proj::directx::orthographic(-5.0, 5.0, -5.0, 5.0, 0.0, 20.0)
        * glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 10.0, 0.0), Vec3::ZERO, Vec3::NEG_Z);
    testing::add_depth_view(app.resources.expect_mut::<Renderer>(), light, 256, Default::default());
    let nodes = app.resources.expect_mut::<RenderNodes>();
    nodes.insert("timed-compute", TimedCompute);
    nodes.insert(
        "reader",
        Solid::new(
            "reader",
            vec![SolidSlot {
                slot: PhaseSlot::new(RenderPhase::Transparent, 10).reading(SceneReads::DEPTH),
                color: [0.2, 0.2, 0.2, 1.0],
                rect: [-0.1, -0.1, 0.1, 0.1],
            }],
            log(),
        ),
    );
    app.resources.expect_mut::<RenderOverlays>().insert("empty", 0, Empty);
    Some((app, camera))
}

#[test]
fn gpu_timings_report_scopes() {
    let Some((mut app, camera)) = timed_app() else { return };
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        // A frame's timestamps are resolved by a later frame once its GPU work completed, and
        // read back by the one after (`render_to_image` waits for each frame).
        for _ in 0..6 {
            render_to_image(&mut app).expect("capture");
        }
        let renderer = app.resources.expect::<Renderer>();
        assert_eq!(renderer.gpu_error_count(), 0);
        let t = app.resources.expect::<GpuTimings>().clone();
        assert_eq!(&t, renderer.gpu_timings(), "resource and renderer agree");
        assert!(t.supported);
        let frame = stats(&app).frame;
        let measured = t.frame.expect("timings arrived");
        assert!(
            measured < frame && measured + 5 >= frame,
            "{path:?}: {measured} of {frame}"
        );
        eprintln!("{path:?}: {:.3} ms in {:?}", t.total_ms, t.scopes);
        let labels: Vec<&str> = t.scopes.iter().map(|s| s.label).collect();
        let mut expected = vec![
            "shadow",
            "scene",
            "scene_read",
            "post.bloom",
            "post.composite",
            "post.fxaa",
            "overlays",
            "test.compute",
        ];
        if path.gpu_culling() {
            expected.push("cull");
        }
        for label in expected {
            assert!(labels.contains(&label), "{path:?}: no `{label}` scope in {labels:?}");
        }
        let sum: f32 = t.scopes.iter().map(|s| s.ms).sum();
        assert!((t.total_ms - sum).abs() < 1e-4);
        assert!(t.scopes.iter().all(|s| s.ms >= 0.0 && s.ms < 1000.0), "{:?}", t.scopes);
        assert!(t.scope_ms("scene") > 0.0, "{path:?}: {:?}", t.scopes);
        let scene = t.scopes.iter().find(|s| s.label == "scene").unwrap();
        assert_eq!(scene.camera, Some(camera));
        let bloom = t.scopes.iter().find(|s| s.label == "post.bloom").unwrap();
        assert_eq!(bloom.camera, Some(camera));
    }
}

/// Timing off: no scopes, and the timings of the last timed frame stay as they were.
#[test]
fn gpu_timings_off_record_nothing() {
    let Some((mut app, _)) = timed_app() else { return };
    app.insert_resource(FrameSettings::default());
    for _ in 0..3 {
        render_to_image(&mut app).expect("capture");
    }
    let t = app.resources.expect::<GpuTimings>();
    assert!(t.supported);
    assert_eq!(t.frame, None);
    assert!(t.scopes.is_empty());
}

/// Captures replay passes after the frame: they are not timed and do not disturb the ring.
#[test]
fn timed_frames_stay_allocation_free() {
    let Some((mut app, _)) = timed_app() else { return };
    for _ in 0..6 {
        render_to_image(&mut app).expect("capture");
    }
    for _ in 0..10 {
        app.run_stage(Stage::Render);
        let s = stats(&app);
        assert_eq!(s.cpu_reallocations, 0);
        assert_eq!(s.textures_created, 0);
    }
    let _ = app
        .resources
        .expect::<Renderer>()
        .device()
        .poll(wgpu::PollType::wait_indefinitely());
    app.run_stage(Stage::Render);
    assert!(app.resources.expect::<GpuTimings>().frame.is_some());
}
