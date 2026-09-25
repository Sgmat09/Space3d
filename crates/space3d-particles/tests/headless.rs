//! Particles render headless: `render_to_image` and `Renderer::capture` include them, a capture
//! twice without ticks in between is identical, and nothing touches a window.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::ParticleBackend;
use space3d_particles::prelude::*;

const SPACE: Vec4 = Vec4::new(0.004, 0.005, 0.012, 1.0);

#[test]
fn render_to_image_includes_particles() {
    let Some(mut app) = app(320, 180) else { return };
    camera(&mut app, Vec3::new(0.0, 160.0, 220.0), Vec3::ZERO, SPACE);
    let empty = render(&mut app);
    app.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(Vec3::ZERO).seed(7));
    app.step_ticks(12);
    let img = render(&mut app);
    dump(&img, "headless_explosion");
    let bg = empty.get(0, 0);
    let lit = fraction_not(&img, [bg[0], bg[1], bg[2]], 4);
    assert!(lit > 0.05, "{lit:.3} of the image lit by the explosion");
    let s = stats(&app);
    assert!(s.alive > 50, "{s:?}");
    assert_eq!(s.backend, Some(ParticleBackend::Cpu));
    // The app rendered every tick (the renderer is in the frame loop): nothing left to replay.
    assert!(s.draw_calls > 0 && s.ticks_simulated == 0, "{s:?}");
    // Rendering again without ticks replays nothing, uploads nothing and gives the same image.
    let again = render(&mut app);
    assert_eq!(img, again);
    assert_eq!(stats(&app).bytes_uploaded, 0);
    assert_eq!(stats(&app).ticks_simulated, 0);
    assert_eq!(gpu_errors(&app), 0);
}

/// `Renderer::capture` (the asynchronous capture) returns the frame with its particles, the
/// same pixels as `render_to_image`.
#[test]
fn capture_includes_particles() {
    let Some(mut app) = app(160, 90) else { return };
    camera(&mut app, Vec3::new(0.0, 40.0, 60.0), Vec3::ZERO, SPACE);
    app.spawn_effect(
        builtin::HIT_SPARK,
        EffectSpawn::at(Vec3::ZERO)
            .facing(Vec3::X)
            .tint(Vec4::new(1.0, 0.3, 0.2, 1.0))
            .seed(3),
    );
    app.step_ticks(4);
    let expected = render(&mut app);
    let mut renderer = app.resources.remove::<space3d_render::Renderer>().unwrap();
    space3d_core::transform::propagate_transforms(&mut app.world);
    renderer.render_frame(&app.world, &mut app.resources);
    let captured = renderer.capture().block().expect("task").expect("capture");
    app.resources.insert(renderer);
    assert_eq!(captured, expected);
    let bg = [2, 3, 12];
    assert!(fraction_not(&captured, bg, 6) > 0.01, "the hit spark is in the capture");
}

/// Stories render on demand: the renderer is out of the frame loop while the ticks run, and
/// the first screenshot replays them (exactly `n` ticks of simulation).
#[test]
fn render_on_demand_replays_the_ticks() {
    let mut base = App::new();
    base.add_plugin(ParticlesPlugin::default());
    let mut app = space3d_test::TestApp::builder()
        .size(160, 90)
        .render_on_demand()
        .build_with(base);
    if app.backend().is_none() {
        return;
    }
    camera(&mut app, Vec3::new(0.0, 160.0, 220.0), Vec3::ZERO, SPACE);
    app.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(Vec3::ZERO).seed(7));
    app.step_ticks(20);
    let img = app.screenshot().expect("screenshot");
    let s = stats(&app);
    assert_eq!(s.ticks_simulated, 20, "{s:?}");
    assert!(s.alive > 50 && s.draw_calls > 0, "{s:?}");
    assert!(fraction_not(&img, [2, 3, 12], 6) > 0.02);
}

/// With no particles the plugin changes nothing: same pixels, no node draw, no SceneRead pass,
/// and a static scene still uploads 0 bytes (graphics spec §2.7 "adding ParticlesPlugin … changes
/// no mode and no pixels of views where the node reports no work").
#[test]
fn idle_plugin_changes_nothing() {
    let scene = |app: &mut App| {
        let cube = app
            .resources
            .expect_mut::<space3d_render::RenderAssets>()
            .add_mesh(MeshData::cube(1.0));
        app.world.spawn((
            Transform::IDENTITY.with_rotation(Quat::from_rotation_y(0.4)),
            space3d_render::Mesh3d(cube),
            space3d_render::Material::srgb(0.8, 0.3, 0.1),
        ));
        app.world.spawn((
            Transform::IDENTITY.looking_at(Vec3::new(-0.3, -1.0, -0.5), Vec3::Y),
            space3d_render::DirectionalLight {
                color: Vec3::ONE,
                intensity: 2.0,
            },
        ));
        camera(app, Vec3::new(2.0, 2.0, 3.0), Vec3::ZERO, SPACE);
    };
    let Some(mut with) = app(160, 90) else { return };
    let mut without = App::new();
    without.add_plugin(space3d_render::RenderPlugin::default());
    without.insert_resource(pollster::block_on(space3d_render::Renderer::new_headless(160, 90)).unwrap());
    scene(&mut with);
    scene(&mut without);
    let (a, b) = (render(&mut with), render(&mut without));
    assert_eq!(a, b, "same pixels");
    let (a2, b2) = (render(&mut with), render(&mut without));
    assert_eq!(a2, b2);
    let rs = |app: &App| app.resources.expect::<space3d_render::RenderStats>().clone();
    let (sw, so) = (rs(&with), rs(&without));
    assert_eq!((sw.node_draws, sw.scene_read_passes), (0, 0), "{sw:?}");
    assert_eq!(sw.bytes_uploaded, so.bytes_uploaded, "static: nothing more uploaded");
    assert_eq!(sw.bytes_uploaded, 0);
    assert_eq!(stats(&with).draw_calls, 0);
}
