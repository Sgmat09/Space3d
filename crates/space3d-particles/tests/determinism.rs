//! Determinism (graphics spec §4.3, design-particles §10): identical runs render identical
//! images; rendering every tick, rendering once after `n` ticks (render on demand) and replaying
//! only the catch-up window after a long gap all give the same image.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_render::*;
use space3d_test::TestApp;

const W: u32 = 256;
const H: u32 = 144;

/// Explosions, hit sparks and a ship with an engine exhaust (world and local layers, rate per
/// distance, inherited velocity) flying a curve, driven by the tick number only.
fn scene(app: &mut App) {
    app.world.spawn((
        Transform::from_xyz(0.0, 140.0, 120.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.01, 0.01, 0.02, 1.0)),
    ));
    let ship = app.world.spawn((Transform::IDENTITY,));
    app.spawn_effect(
        builtin::ENGINE_EXHAUST,
        EffectSpawn::from_transform(Transform::from_xyz(0.0, 0.0, 3.2))
            .follow(ship)
            .keep(),
    );
    app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
        let tick = ctx.time().tick;
        let a = tick as f32 * 0.03;
        if let Ok(mut t) = ctx.world.get::<&mut Transform>(ship) {
            t.translation = Vec3::new(a.cos() * 50.0, 0.0, a.sin() * 30.0);
            t.rotation = Quat::from_rotation_y(-a);
        }
        if tick.is_multiple_of(40) {
            ctx.spawn_effect(
                builtin::EXPLOSION,
                EffectSpawn::at(Vec3::new(-30.0 + (tick % 120) as f32 * 0.5, 0.0, 0.0)).scale(0.6),
            );
        }
        if tick.is_multiple_of(9) {
            ctx.spawn_effect(
                builtin::HIT_SPARK,
                EffectSpawn::at(Vec3::new(20.0, 0.0, -10.0)).facing(Vec3::new(a.cos(), 0.0, a.sin())),
            );
        }
    });
}

/// Every tick rendered (the renderer in the frame loop).
fn every_tick(ticks: u64) -> Option<ImageData> {
    let mut app = app(W, H)?;
    scene(&mut app);
    app.step_ticks(ticks);
    let img = render(&mut app);
    assert_eq!(gpu_errors(&app), 0);
    Some(img)
}

/// Rendered once, after all the ticks (the storybook's render-on-demand mode).
fn on_demand(ticks: u64) -> Option<(ImageData, ParticleStats)> {
    let mut base = App::new();
    base.add_plugin(ParticlesPlugin::default());
    let mut app = TestApp::builder().size(W, H).render_on_demand().build_with(base);
    app.backend()?;
    scene(&mut app);
    app.step_ticks(ticks);
    let img = app.screenshot()?;
    let stats = app.resources.expect::<ParticleStats>().clone();
    Some((img, stats))
}

#[test]
fn runs_are_identical() {
    let Some(a) = every_tick(120) else { return };
    let b = every_tick(120).unwrap();
    dump(&a, "determinism_120");
    assert_eq!(a, b);
}

/// 90 ticks: shorter than every life window, so the on-demand frame replays all 90 ticks.
#[test]
fn render_every_tick_equals_render_on_demand() {
    let Some(a) = every_tick(90) else { return };
    let Some((b, stats)) = on_demand(90) else { return };
    assert_eq!(stats.ticks_simulated, 90, "{stats:?}");
    assert_eq!(stats.catchup_resets, 0);
    assert_eq!(a, b);
}

/// 600 ticks: the unconsumed log was trimmed to the catch-up window, the on-demand frame resets
/// the pool and replays only that window, and the image is still identical.
#[test]
fn catch_up_reset_equals_every_tick() {
    let Some(a) = every_tick(600) else { return };
    let Some((b, stats)) = on_demand(600) else { return };
    dump(&b, "determinism_600");
    assert!(stats.ticks_simulated < 600, "{stats:?}");
    assert_eq!(stats.catchup_resets, 1, "{stats:?}");
    assert!(stats.alive > 100, "{stats:?}");
    assert_eq!(a, b);
}
