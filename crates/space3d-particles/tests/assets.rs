//! `.pfx.ron` effects through the asset server (feature `assets`): an effect registered with
//! `ParticleEffects::add_asset` has a handle at once, its emitters wait until it has loaded, it
//! follows hot reloads (a changed structure restarts running emitters), and one that fails to
//! load plays nothing (its emitters finish instead of waiting forever).
#![cfg(feature = "assets")]

use space3d_assets::{AssetServer, AssetsPlugin, Handle, MemorySource};
use space3d_core::prelude::*;
use space3d_particles::ParticleRuntime;
use space3d_particles::prelude::*;
use std::time::{Duration, Instant};

const SPARKS: &str = r#"ParticleEffect(
    name: "test/sparks",
    looping: true,
    layers: [(spawn: (rate: 60.0), lifetime: 0.5, speed: (1.0, 2.0))],
)"#;

const SPARKS_V2: &str = r#"ParticleEffect(
    name: "test/sparks",
    looping: true,
    layers: [
        (spawn: (rate: 60.0), lifetime: 0.5, speed: (1.0, 2.0)),
        (spawn: (rate: 30.0), lifetime: 0.5),
    ],
)"#;

fn run_until(app: &mut App, mut done: impl FnMut(&mut App) -> bool) {
    let start = Instant::now();
    while !done(app) {
        app.step_ticks(1);
        assert!(start.elapsed() < Duration::from_secs(20), "timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn app() -> (App, MemorySource) {
    let mem = MemorySource::new();
    let mut app = App::new();
    app.add_plugin(AssetsPlugin {
        root: "does-not-exist".into(),
        hot_reload: false,
    });
    app.resources.expect_mut::<AssetServer>().add_source("mem", mem.clone());
    app.add_plugin(ParticlesPlugin::default());
    (app, mem)
}

#[test]
fn effect_assets_load_wait_and_hot_reload() {
    let (mut app, mem) = app();
    mem.insert("fx/sparks.pfx.ron", SPARKS);
    let asset: Handle<ParticleEffect> = app
        .resources
        .expect_mut::<AssetServer>()
        .load("mem://fx/sparks.pfx.ron");
    let fx = app.resources.expect_mut::<ParticleEffects>().add_asset(asset.clone());
    assert_eq!(
        app.resources.expect_mut::<ParticleEffects>().add_asset(asset),
        fx,
        "one effect per asset"
    );
    // Spawned before the asset has loaded: it waits (no instance, not finished).
    let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
    assert!(app.resources.expect::<ParticleEffects>().is_pending(fx));
    run_until(&mut app, |app| {
        app.resources.expect::<ParticleRuntime>().state(e).is_some()
    });
    let fx_data = app.resources.expect::<ParticleEffects>().get(fx).cloned().unwrap();
    assert_eq!((fx_data.name.as_str(), fx_data.layers.len()), ("test/sparks", 1));
    app.step_ticks(10);
    let state = app.resources.expect::<ParticleRuntime>().state(e).unwrap();
    assert!(state.emitting && state.reserved > 0 && app.world.contains(e));

    // Hot reload with another structure: the effect is replaced, the emitter restarts.
    mem.insert("fx/sparks.pfx.ron", SPARKS_V2);
    assert_eq!(
        app.resources
            .expect_mut::<AssetServer>()
            .reload("mem://fx/sparks.pfx.ron"),
        1
    );
    run_until(&mut app, |app| {
        app.resources
            .expect::<ParticleEffects>()
            .get(fx)
            .is_some_and(|e| e.layers.len() == 2)
    });
    app.step_ticks(2);
    let before = state.reserved;
    let state = app.resources.expect::<ParticleRuntime>().state(e).unwrap();
    assert!(
        state.emitting && state.reserved > before,
        "the restarted emitter has both layers"
    );
}

#[test]
fn failed_effect_assets_play_nothing() {
    let (mut app, _mem) = app();
    let asset: Handle<ParticleEffect> = app
        .resources
        .expect_mut::<AssetServer>()
        .load("mem://fx/missing.pfx.ron");
    let fx = app.resources.expect_mut::<ParticleEffects>().add_asset(asset);
    let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO));
    run_until(&mut app, |app| !app.world.contains(e));
    assert!(!app.resources.expect::<ParticleEffects>().is_pending(fx));
}
