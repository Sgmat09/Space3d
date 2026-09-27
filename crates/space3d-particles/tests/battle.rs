//! LOD and budget calibration with the Battle reference scene's particle load (graphics spec
//! §4.4, §5.2 PA4; `testing::battle`: 300 ships with three engine exhausts each, 64 ribbons, 16
//! beams, 30 explosions a second, hits, shield ripples, debris, warps and EMPs, seen by the
//! game's battle camera): `ParticleSettings::for_quality` holds it in every tier's budget —
//! 100k+ particles on the GPU backend at High, the CPU backend's 16 384 (8 192 on the web) at
//! Low — without refusing or shrinking a layer, with every engine in view burning and every
//! explosion playing. The GPU cost of the whole scene's particles is measured by an ignored
//! perf test.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::testing::{Battle, BattleLog, battle};
use space3d_particles::{ParticleBackend, ParticleRuntime};
use space3d_render::*;

/// The game's battle camera: 432 units from the origin at a 70° pitch (default 60° fov).
fn game_camera(app: &mut App) -> Entity {
    let pitch = 70f32.to_radians();
    app.world.spawn((
        Transform::from_xyz(0.0, 432.0 * pitch.sin(), 432.0 * pitch.cos()).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default(),
    ))
}

/// A tier's outcome after 5 s of battle.
#[derive(Debug, Default)]
struct Outcome {
    alive: u32,
    reserved: u32,
    /// The largest `pool_reserved` of any tick.
    peak_reserved: u32,
    capacity: u32,
    effects: u32,
    refused: u32,
    shrunk: u32,
    /// Engines in view at the end (the frame's aspect ratio, 60° fov), and the most of those in
    /// view without a ring at any tick checked (every 10th).
    engines_in_view: u32,
    engines_in_view_empty: u32,
    /// Explosions spawned, and those without a ring on their first tick.
    explosions: u32,
    explosions_empty: u32,
}

fn run(settings: ParticleSettings, size: (u32, u32)) -> Option<Outcome> {
    run_for(settings, size, 1, 300)
}

/// [`run`] with the battle's `seed`, for `ticks`.
fn run_for(settings: ParticleSettings, size: (u32, u32), seed: u32, ticks: u32) -> Option<Outcome> {
    let mut app = app_with(size.0, size.1, settings)?;
    game_camera(&mut app);
    battle(
        &mut app,
        Battle {
            seed,
            ..Battle::default()
        },
    );
    let mut o = Outcome::default();
    let mut checked = 0;
    for tick in 1..=ticks {
        app.step_ticks(1);
        if tick % 10 == 0 {
            let (_, empty) = engines_in_view(&app, size);
            o.engines_in_view_empty = o.engines_in_view_empty.max(empty);
        }
        let s = stats(&app);
        o.refused += s.refused;
        o.shrunk += s.shrunk;
        o.peak_reserved = o.peak_reserved.max(s.pool_reserved);
        // Explosions spawned this tick have their rings now.
        let log = app.resources.expect::<BattleLog>();
        let rt = app.resources.expect::<ParticleRuntime>();
        for &(e, _) in &log.explosions[checked..] {
            o.explosions += 1;
            if rt.state(e).is_none_or(|s| s.reserved == 0) {
                o.explosions_empty += 1;
            }
        }
        checked = log.explosions.len();
    }
    let s = settled_stats(&mut app);
    (o.alive, o.reserved, o.capacity, o.effects) = (s.alive, s.pool_reserved, s.pool_capacity, s.effects);
    let (in_view, empty) = engines_in_view(&app, size);
    o.engines_in_view = in_view;
    o.engines_in_view_empty = o.engines_in_view_empty.max(empty);
    assert_eq!(gpu_errors(&app), 0);
    Some(o)
}

/// Engine exhausts in view of the camera (a frame of `size`, 60° fov), and those without a
/// ring.
fn engines_in_view(app: &App, size: (u32, u32)) -> (u32, u32) {
    let (cam_pos, cam_rot) = {
        let mut q = app.world.query::<(&Transform, &Camera3d)>();
        let (t, _) = q.iter().next().expect("camera");
        (t.translation, t.rotation)
    };
    let tan = 30f32.to_radians().tan();
    let aspect = size.0 as f32 / size.1 as f32;
    let rt = app.resources.expect::<ParticleRuntime>();
    let (mut in_view, mut empty) = (0, 0);
    for (e, em, g) in app.world.query::<(Entity, &ParticleEmitter, &GlobalTransform)>().iter() {
        if em.effect != builtin::ENGINE_EXHAUST {
            continue;
        }
        let v = cam_rot.inverse() * (g.0.w_axis.truncate() - cam_pos);
        let depth = -v.z;
        if depth > 0.0 && v.y.abs() <= depth * tan && v.x.abs() <= depth * tan * aspect {
            in_view += 1;
            if rt.state(e).is_none_or(|s| s.reserved == 0) {
                empty += 1;
            }
        }
    }
    (in_view, empty)
}

/// Every tier holds the battle: no refused or shrunk layers, every engine in view burning,
/// every explosion playing, within the budget; High keeps 100k+ particles alive, and each tier
/// below it a fraction.
#[test]
fn battle_fits_every_quality_tier() {
    let web_low = ParticleSettings::for_quality_web(GraphicsQuality::Low);
    let mobile = ParticleSettings {
        max_particles: 65_536,
        quality: 0.75,
        offscreen_scale: 0.125,
        ..ParticleSettings::for_quality(GraphicsQuality::Medium)
    };
    let tiers = [
        ("High", ParticleSettings::for_quality(GraphicsQuality::High), 100_000),
        ("Medium", ParticleSettings::for_quality(GraphicsQuality::Medium), 50_000),
        ("Medium (mobile)", mobile, 18_000),
        ("Low", ParticleSettings::for_quality(GraphicsQuality::Low), 3_500),
        ("Low (web)", web_low, 1_600),
    ];
    // The GPU tiers need compute: without it (WebGL2 / GLES 3.0, e.g. ANGLE) their settings
    // fall back to the CPU backend and its budget, which the High and Medium loads do not fit
    // (quality detection picks Low on such devices).
    let Some(probe) = app_with(64, 64, ParticleSettings::default()) else {
        return;
    };
    let compute = probe.resources.expect::<Renderer>().graphics_caps().compute;
    drop(probe);
    for (tier, settings, min_alive) in tiers {
        let gpu = settings.backend != ParticleBackend::Cpu;
        if gpu && !compute {
            eprintln!("{tier}: skipped (no compute: the GPU backend is unavailable)");
            continue;
        }
        let budget = if gpu {
            settings.max_particles
        } else {
            settings.max_particles_cpu
        };
        let Some(o) = run(settings, (640, 360)) else { return };
        eprintln!("{tier}: {o:?}");
        assert_eq!((o.refused, o.shrunk), (0, 0), "{tier}: {o:?}");
        assert!(o.capacity <= budget && o.reserved <= budget, "{tier}: {o:?}");
        assert!(o.alive >= min_alive && o.alive <= budget, "{tier}: {o:?}");
        assert!(o.engines_in_view > 80, "{tier}: {o:?}");
        assert_eq!(o.engines_in_view_empty, 0, "{tier}: every engine in view burns");
        assert!(o.explosions > 150, "{tier}: {o:?}");
        assert_eq!(o.explosions_empty, 0, "{tier}: every explosion plays");
    }
}

/// T3's budget (graphics spec §4.4: WebGL2 at 1440 × 810, the CPU backend's 8 192 on the web)
/// holds the Battle at Low for 20 s through the game's camera at T3's 16:9 (the on-screen test
/// depends on the aspect ratio only), whatever the battle's seed: no layer instance refused or
/// shrunk, every engine in view burning at every tick checked, every explosion playing (M2
/// final review: at 0.125, without detail layers left out and with the plumes as `Low` as the
/// rest of the engine, refusals began 5.2 s in and dropped engine plumes and afterburners). The
/// peak reservation stays below the engines' 75 % with a margin. Wider than 16:9 (21:9) more
/// engines are in view and with some seeds the budget binds (seeds 1 and 4 of these four:
/// 103 and 226 layer instances refused, 3 shrunk with seed 4; the plumes outrank the details
/// they refuse), yet every engine in view keeps burning at every tick checked and every
/// explosion plays, whatever the seed.
#[test]
fn battle_fits_the_web_budget_at_low() {
    let settings = ParticleSettings::for_quality_web(GraphicsQuality::Low);
    assert_eq!((settings.max_particles_cpu, settings.quality), (8_192, 0.1));
    let low_limit = settings.max_particles_cpu * 3 / 4;
    for seed in 1..=4 {
        let Some(o) = run_for(settings.clone(), (640, 360), seed, 1200) else {
            return;
        };
        eprintln!("16:9, seed {seed}: {o:?}");
        assert_eq!((o.refused, o.shrunk), (0, 0), "seed {seed}: {o:?}");
        // A tenth of the engines' share to spare at the busiest tick.
        assert!(o.peak_reserved <= low_limit * 9 / 10, "seed {seed}: {o:?}");
        assert!(o.engines_in_view > 80, "seed {seed}: {o:?}");
        assert_eq!(o.engines_in_view_empty, 0, "seed {seed}: every engine in view burns");
        assert!(o.explosions > 600, "seed {seed}: {o:?}");
        assert_eq!(o.explosions_empty, 0, "seed {seed}: every explosion plays");
    }
    let mut bound = 0;
    for seed in 1..=4 {
        let Some(o) = run_for(settings.clone(), (840, 360), seed, 1200) else {
            return;
        };
        eprintln!("21:9, seed {seed}: {o:?}");
        bound += u32::from(o.refused + o.shrunk > 0);
        assert!(o.engines_in_view > 100, "seed {seed}: {o:?}");
        assert_eq!(
            o.engines_in_view_empty, 0,
            "21:9, seed {seed}: the plumes outrank the details"
        );
        assert_eq!(o.explosions_empty, 0, "21:9, seed {seed}: every explosion plays");
    }
    // The case above is exercised: the budget binds at 21:9 with some of the seeds.
    assert!(bound > 0, "21:9: no seed reached the budget");
}

/// The GPU cost of the battle's particles at 1080p through `PostProcess::default()` on the Dev
/// machine (a perf gate, ignored by default: run it alone on an idle Dev machine,
/// `--ignored --nocapture`; `SPACE3D_PERF_BUDGET_SCALE` relaxes it elsewhere):
/// * the passes the particles record while ticking — `particles.sim`, `particles.compact`,
///   `particles.sort` — from `GpuTimings`, medians of frames back to back (an Apple GPU's pass
///   scope includes its overlap with its neighbours: reported, the sum gated at 0.4 ms);
/// * what the draws and the distortion writers cost, as throughput: the same frozen frame
///   (no ticks) rendered back to back with everything, without distortion
///   (`PostProcess::distortion` off: no Distortion pass, no composite taps) and without
///   particles (`ParticleSettings::enabled` off), median wall time of render + wait. Gates: the
///   distortion writers ≤ 0.3 ms (spec §4.4: 0.15 on T1 for the reference scene's 2
///   shockwaves; the battle keeps about 14 alive), the particles' draws ≤ 1.2 ms (spec §4.4:
///   the whole Transparent phase, 1.2 ms on T1).
#[test]
#[ignore = "perf gate: run alone with --ignored"]
fn battle_gpu_cost() {
    let scale: f32 = std::env::var("SPACE3D_PERF_BUDGET_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let Some(mut app) = app_with(1920, 1080, ParticleSettings::for_quality(GraphicsQuality::High)) else {
        return;
    };
    if !app.resources.expect::<Renderer>().graphics_caps().timestamps {
        eprintln!("skipping: no timestamp queries");
        return;
    }
    app.resources.expect_mut::<FrameSettings>().gpu_timing = true;
    let cam = game_camera(&mut app);
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    battle(&mut app, Battle::default());
    app.step_ticks(240);
    let labels = ["particles.sim", "particles.compact", "particles.sort"];
    let mut samples: Vec<[f32; 3]> = Vec::new();
    for _ in 0..60 {
        app.step_ticks(1);
        let t = app.resources.expect::<GpuTimings>();
        if t.frame.is_some() {
            samples.push(labels.map(|l| t.scope_ms(l)));
        }
    }
    let s = settled_stats(&mut app);
    assert!(samples.len() >= 30, "{} timed frames", samples.len());
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let [sim, compact, sort] = [0, 1, 2].map(|k| median(samples.iter().map(|s| s[k] as f64).collect()));
    // Throughput of the frozen frame (render + wait, no ticks).
    app.resources.expect_mut::<FrameSettings>().gpu_timing = false;
    let frame = |app: &mut App| {
        let mut renderer = app.resources.remove::<Renderer>().unwrap();
        renderer.poll();
        let t = std::time::Instant::now();
        renderer.render_frame(&app.world, &mut app.resources);
        let _ = renderer
            .device()
            .poll(space3d_render::wgpu::PollType::wait_indefinitely());
        let ms = t.elapsed().as_secs_f64() * 1e3;
        app.resources.insert(renderer);
        ms
    };
    let throughput = |app: &mut App| {
        for _ in 0..10 {
            frame(app);
        }
        median((0..60).map(|_| frame(app)).collect())
    };
    let full = throughput(&mut app);
    let mut post = PostProcess::default();
    post.distortion.enabled = false;
    app.world.insert_one(cam, post).unwrap();
    let no_distortion = throughput(&mut app);
    app.resources.expect_mut::<ParticleSettings>().enabled = false;
    let no_particles = throughput(&mut app);
    let (distortion, draws) = (full - no_distortion, no_distortion - no_particles);
    // The per-view sort, as throughput of ticking frames: the smoke layers sorted per particle
    // (`ByDistance`) against per emitter.
    app.resources.expect_mut::<ParticleSettings>().enabled = true;
    let ticking = |app: &mut App| {
        let mut v: Vec<f64> = (0..50)
            .map(|_| {
                let t = std::time::Instant::now();
                app.step_ticks(1);
                let r = app.resources.expect::<Renderer>();
                let _ = r.device().poll(space3d_render::wgpu::PollType::wait_indefinitely());
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    ticking(&mut app);
    let sorted = ticking(&mut app);
    let dispatches = stats(&app).dispatches;
    let effects = app.resources.expect_mut::<ParticleEffects>();
    for h in [builtin::EXPLOSION, builtin::DEBRIS_SPARKS, builtin::MISSILE_TRAIL] {
        let mut fx = effects.get(h).unwrap().clone();
        for l in &mut fx.layers {
            if l.render.sort == space3d_particles::SortMode::ByDistance {
                l.render.sort = space3d_particles::SortMode::ByEmitter;
            }
        }
        effects.set(h, fx).unwrap();
    }
    ticking(&mut app);
    let unsorted = ticking(&mut app);
    eprintln!(
        "battle at 1080p: {} alive, {} draws, {dispatches} dispatches per tick; in-frame scopes: sim {sim:.3} + compact {compact:.3} + sort {sort:.3} ms; throughput: frame {full:.3} ms, distortion writers {distortion:.3} ms, particle draws {draws:.3} ms (frame without particles {no_particles:.3} ms), ticking frame {sorted:.3} ms sorted vs {unsorted:.3} ms per emitter",
        s.alive, s.draw_calls
    );
    assert!(s.alive >= 95_000, "{}", s.alive);
    assert!(
        sim + compact + sort <= 0.4 * scale as f64,
        "{:.3} ms",
        sim + compact + sort
    );
    assert!(distortion <= 0.3 * scale as f64, "distortion {distortion:.3} ms");
    assert!(draws <= 1.2 * scale as f64, "draws {draws:.3} ms");
}
