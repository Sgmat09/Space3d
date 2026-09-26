//! Every graphics feature of Space3d in the Battle reference scene (graphics spec §4.4), the
//! SpaceCorps top-down view: 300 ships of three procedural hulls with base color, packed ORM,
//! normal and emissive maps, asteroids, a nebula skybox that is also the image-based lighting,
//! a shadow-casting sun, 100k particles (engines, afterburner ribbons, held lasers and repair
//! beams, bolts, missiles, explosions with light flashes, warps' shockwaves), bloom, AgX punchy
//! and a vignette, two render-target ship previews in the corner — all under a
//! [`GraphicsQuality`] preset, which picks itself for the device
//! (`GraphicsQuality::recommended`: Low on WebGL2) and can be cycled at runtime.
//!
//! ```text
//! cargo run -p graphics_showcase --release
//! SPACE3D_QUALITY=ultra cargo run -p graphics_showcase --release   # force a tier (low|medium|high|ultra)
//! SPACE3D_EXIT_AFTER_FRAMES=3 cargo run -p graphics_showcase       # smoke test (background mode)
//! SPACE3D_HEADLESS=1 cargo run -p graphics_showcase --release      # no window: writes graphics_showcase.png
//! # web (WebGL2 → Low, WebGPU → Medium/High): from this directory
//! cargo run -p space3d-cli -- serve
//! ```
//!
//! Keys: `Q` next quality tier, `1` bloom, `2` shadows, `3` image-based lighting, `4`
//! distortion, `5` post-processing, `C` follow a ship / battle view, arrows or `WASD` pan,
//! mouse wheel or `+`/`-` zoom, `G` print GPU timings, `Esc` quit. An egui window shows the
//! tier, frame times and counters.
//!
//! The content is generated off the frame (`spawn_compute`, AGENTS rule 2) once the tier is
//! known (larger maps and sky for higher tiers); the battle simulates from the first frame and
//! gets its hulls, rocks and sky when the task is done. After that the tier's pipelines are
//! built ahead of use (`GraphicsQuality::prewarm_request`). Headless, the example renders two
//! seconds of battle, writes `graphics_showcase.png` (or `$SPACE3D_SCREENSHOT`) and exits.

use space3d::particles::prelude::*;
use space3d::prelude::*;
use space3d::render::{
    DirectionalShadow, EnvironmentLight, FrameSettings, GpuTimings, GraphicsQuality, RenderStats, Renderer, ShadowFit,
};
use space3d::stories::showcase::{self, BattleHandles, BattleScene, ContentOptions, ShowcaseAssets, ShowcaseContent};
use space3d::tasks::Task;

/// The scene's state and what the keys toggle.
struct Showcase {
    scene: BattleScene,
    handles: BattleHandles,
    /// Chosen once a renderer exists (recommended for the device, or `SPACE3D_QUALITY`).
    quality: Option<GraphicsQuality>,
    content: Option<Task<ShowcaseContent>>,
    assets: Option<ShowcaseAssets>,
    /// Frames rendered since the content arrived (prewarm after the first).
    dressed_frames: u64,
    bloom: bool,
    shadows: bool,
    environment: bool,
    distortion: bool,
    follow: bool,
    pan: Vec2,
    zoom: f32,
    timings: bool,
    report_at: f64,
}

/// Headless run: the capture in flight, and the frame (after the content) to request it at.
struct Screenshot {
    after_frames: u64,
    pending: Option<Task<Result<ImageData, space3d::render::RenderError>>>,
}

fn build(app: &mut App) {
    let headless = DefaultPlugins::headless_from_env((1280, 720)).is_some();
    app.add_plugin(
        DefaultPlugins::new()
            .title("Space3d — graphics showcase")
            .without_physics()
            .without_net(),
    );
    if headless {
        app.insert_resource(Screenshot {
            after_frames: 120,
            pending: None,
        });
    }
    add_game(app);
}

/// The scene and its systems, without the engine plugins (the tests add them to a headless
/// app).
fn add_game(app: &mut App) {
    // Until the renderer exists: the web's tier, or High (replaced by `choose_quality`).
    let initial = if cfg!(target_arch = "wasm32") {
        GraphicsQuality::Low
    } else {
        GraphicsQuality::High
    };
    app.add_plugin(ParticlesPlugin::with_settings(ParticleSettings::for_quality(initial)));
    let scene = BattleScene {
        quality: initial,
        ..BattleScene::default()
    };
    let handles = showcase::spawn_battle(app, &scene);
    app.insert_resource(Showcase {
        scene,
        handles,
        quality: None,
        content: None,
        assets: None,
        dressed_frames: 0,
        bloom: true,
        shadows: true,
        environment: true,
        distortion: true,
        follow: false,
        pan: Vec2::ZERO,
        zoom: 1.0,
        timings: false,
        report_at: 0.0,
    });
    app.add_system(Stage::FixedUpdate, showcase::turn_previews);
    app.add_system(Stage::PreUpdate, choose_quality);
    app.add_system(Stage::Update, load_content);
    app.add_system(Stage::Update, controls);
    app.add_system(Stage::PostUpdate, camera);
    app.add_system(Stage::Last, report);
    app.add_system(Stage::Last, screenshot);
    app.add_ui_system(hud);
}

/// `SPACE3D_QUALITY=low|medium|high|ultra`.
fn quality_from_env() -> Option<GraphicsQuality> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let v = std::env::var("SPACE3D_QUALITY").ok()?;
        GraphicsQuality::ALL
            .into_iter()
            .find(|q| q.name().eq_ignore_ascii_case(v.trim()))
    }
    #[cfg(target_arch = "wasm32")]
    None
}

/// Apply `q` everywhere: the preset (settings, MSAA, cameras, the sun) and the particles'
/// settings for the device.
fn set_quality(ctx: &mut SystemCtx, q: GraphicsQuality) {
    q.apply(ctx.world, ctx.resources);
    let particles = match ctx.resources.get::<Renderer>() {
        Some(r) => ParticleSettings::for_quality_on(q, r.graphics_caps()),
        None => ParticleSettings::for_quality(q),
    };
    *ctx.resources.expect_mut::<ParticleSettings>() = particles;
    let s = ctx.resources.expect_mut::<Showcase>();
    s.quality = Some(q);
    s.scene.quality = q;
    // Prewarm the tier's pipelines on the next frame.
    s.dressed_frames = s.dressed_frames.min(1);
}

/// Once the renderer exists: pick the tier and start generating content of its size.
fn choose_quality(ctx: &mut SystemCtx) {
    if ctx.resources.expect::<Showcase>().quality.is_some() {
        return;
    }
    let Some(renderer) = ctx.resources.get::<Renderer>() else {
        return;
    };
    let q = quality_from_env().unwrap_or_else(|| GraphicsQuality::recommended_for(renderer));
    println!(
        "graphics_showcase: {} on {} ({:?}); keys: Q tier, 1 bloom, 2 shadows, 3 IBL, 4 distortion, 5 post, C follow, G timings",
        q,
        renderer.capabilities().adapter.name,
        renderer.capabilities().adapter.backend
    );
    set_quality(ctx, q);
    // Bigger maps for higher tiers; the sky at most 1024² (a 2048² HDR sky is 200 MB).
    let sizes = ContentOptions::for_quality(q);
    let options = ContentOptions {
        sky_face_size: sizes.sky_face_size.min(1024),
        ..sizes
    };
    ctx.resources.expect_mut::<Showcase>().content = Some(space3d::tasks::spawn_compute(move || {
        ShowcaseContent::generate(options)
    }));
}

/// When the content task is done: dress the battle (hulls, rocks, sky, previews); a frame
/// later, prewarm the tier's pipelines.
fn load_content(ctx: &mut SystemCtx) {
    let s = ctx.resources.expect_mut::<Showcase>();
    if let Some(content) = s.content.as_mut().and_then(Task::poll) {
        s.content = None;
        let scene = s.scene;
        let mut handles = std::mem::take(&mut s.handles);
        let assets = content.upload(ctx.resources.expect_mut::<RenderAssets>());
        showcase::dress_battle(ctx.world, ctx.resources, &assets, &scene, &mut handles);
        let s = ctx.resources.expect_mut::<Showcase>();
        s.handles = handles;
        s.assets = Some(assets);
        s.dressed_frames = 0;
        return;
    }
    let Some(assets) = s.assets.as_ref() else {
        return;
    };
    s.dressed_frames += 1;
    if s.dressed_frames == 2 {
        let mut materials = assets.hull_materials.to_vec();
        materials.push(assets.asteroid_material.clone());
        let request = s.quality.unwrap_or_default().prewarm_request(&materials);
        if let Some(renderer) = ctx.resources.get_mut::<Renderer>() {
            renderer.prewarm(&request);
        }
    }
}

fn controls(ctx: &mut SystemCtx) {
    let Some(input) = ctx.resources.get::<Input>() else {
        return;
    };
    let pressed = |k: KeyCode| input.just_pressed(k);
    let (quit, tier, bloom, shadows, env, distortion, post, follow, timings) = (
        pressed(KeyCode::Escape),
        pressed(KeyCode::KeyQ),
        pressed(KeyCode::Digit1),
        pressed(KeyCode::Digit2),
        pressed(KeyCode::Digit3),
        pressed(KeyCode::Digit4),
        pressed(KeyCode::Digit5),
        pressed(KeyCode::KeyC),
        pressed(KeyCode::KeyG),
    );
    let dt = ctx.time().delta_secs();
    let mut pan = Vec2::ZERO;
    for (keys, dir) in [
        ([KeyCode::KeyA, KeyCode::ArrowLeft], Vec2::NEG_X),
        ([KeyCode::KeyD, KeyCode::ArrowRight], Vec2::X),
        ([KeyCode::KeyW, KeyCode::ArrowUp], Vec2::NEG_Y),
        ([KeyCode::KeyS, KeyCode::ArrowDown], Vec2::Y),
    ] {
        if keys.iter().any(|&k| input.pressed(k)) {
            pan += dir;
        }
    }
    let mut zoom = -input.wheel_delta().y * 0.1;
    if input.pressed(KeyCode::Equal) {
        zoom -= dt;
    }
    if input.pressed(KeyCode::Minus) {
        zoom += dt;
    }
    if quit {
        ctx.send(AppExit);
    }
    if tier && let Some(q) = ctx.resources.expect::<Showcase>().quality {
        let next = GraphicsQuality::ALL[(GraphicsQuality::ALL.iter().position(|&t| t == q).unwrap_or(0) + 1) % 4];
        println!("quality: {next}");
        set_quality(ctx, next);
    }
    let s = ctx.resources.expect_mut::<Showcase>();
    s.zoom = (s.zoom * (1.0 + zoom)).clamp(0.25, 3.0);
    s.pan += pan * 500.0 * s.zoom * dt;
    s.bloom ^= bloom;
    s.shadows ^= shadows;
    s.environment ^= env;
    s.distortion ^= distortion;
    s.follow ^= follow;
    s.timings ^= timings;
    let (camera, sun, quality) = (s.handles.camera, s.handles.sun, s.quality.unwrap_or_default());
    let (b, sh, e, d, sky, t) = (
        s.bloom,
        s.shadows,
        s.environment,
        s.distortion,
        s.assets.as_ref().map(|a| a.sky),
        s.timings,
    );
    if let Some(camera) = camera {
        if let Ok(mut p) = ctx.world.get::<&mut PostProcess>(camera) {
            p.bloom.enabled = b;
            p.distortion.enabled = d;
        }
        if env && let Some(sky) = sky {
            if e {
                let _ = ctx
                    .world
                    .insert_one(camera, EnvironmentLight::new(sky).with_intensity(0.7, 1.0));
            } else {
                let _ = ctx.world.remove_one::<EnvironmentLight>(camera);
            }
        }
    }
    if shadows && let Some(sun) = sun {
        if sh {
            let shadow = DirectionalShadow::new().with_fit(ShadowFit::top_down(-30.0, 90.0, 900.0));
            let _ = ctx.world.insert_one(sun, quality.shadow(shadow));
        } else {
            let _ = ctx.world.remove_one::<DirectionalShadow>(sun);
        }
    }
    let frame = ctx.resources.expect_mut::<FrameSettings>();
    if post {
        frame.post = !frame.post;
    }
    frame.gpu_timing = t;
    for (name, on, toggled) in [
        ("bloom", b, bloom),
        ("shadows", sh, shadows),
        ("image-based lighting", e, env),
        ("distortion", d, distortion),
    ] {
        if toggled {
            println!("{name}: {}", if on { "on" } else { "off" });
        }
    }
}

/// The battle camera: the game's view (432 units away at a 70° pitch) panned and zoomed, or
/// following the first ship from 150 units.
fn camera(ctx: &mut SystemCtx) {
    let s = ctx.resources.expect::<Showcase>();
    let Some(camera) = s.handles.camera else {
        return;
    };
    let (follow, pan, zoom) = (s.follow, s.pan, s.zoom);
    let ship = ctx
        .resources
        .get::<space3d::particles::testing::BattleLog>()
        .and_then(|l| l.ships.first().copied());
    let (target, distance) = match ship.filter(|_| follow) {
        Some(ship) => (
            ctx.world
                .get::<&Transform>(ship)
                .map(|t| t.translation)
                .unwrap_or_default(),
            150.0 * zoom,
        ),
        None => (Vec3::new(pan.x, 0.0, pan.y), 432.0 * zoom),
    };
    let pitch = 70f32.to_radians();
    let eye = target + Vec3::new(0.0, pitch.sin(), pitch.cos()) * distance;
    if let Ok(mut t) = ctx.world.get::<&mut Transform>(camera) {
        *t = Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(target, Vec3::Y);
    }
}

/// With `G`: the GPU pass timings once a second.
fn report(ctx: &mut SystemCtx) {
    let now = ctx.time().elapsed_secs();
    let s = ctx.resources.expect_mut::<Showcase>();
    if !s.timings || now < s.report_at {
        return;
    }
    s.report_at = now + 1.0;
    let t = ctx.resources.expect::<GpuTimings>();
    if t.frame.is_none() {
        return;
    }
    let mut line = format!("GPU (sum of pass scopes) {:.3} ms:", t.total_ms);
    for scope in &t.scopes {
        line += &format!(" {} {:.3}", scope.label, scope.ms);
    }
    println!("{line}");
}

/// The egui HUD: tier, frame times, counters, keys.
fn hud(ui: &egui::Context, ctx: &mut SystemCtx) {
    let stats = ctx.resources.get::<RenderStats>().cloned().unwrap_or_default();
    let particles = ctx.resources.get::<ParticleStats>().cloned().unwrap_or_default();
    let gpu = ctx.resources.get::<GpuTimings>().map(|t| t.total_ms).unwrap_or(0.0);
    let fps = 1.0 / ctx.time().delta_secs().max(1e-6);
    let s = ctx.resources.expect::<Showcase>();
    egui::Window::new("Graphics")
        .default_pos([12.0, 12.0])
        .resizable(false)
        .show(ui, |ui| {
            egui::Grid::new("graphics_stats").num_columns(2).show(ui, |ui| {
                let mut row = |k: &str, v: String| {
                    ui.label(k);
                    ui.monospace(v);
                    ui.end_row();
                };
                row("quality", s.quality.map_or("…".into(), |q| q.to_string()));
                row("fps", format!("{fps:.0}"));
                if s.timings {
                    row("GPU scopes", format!("{gpu:.2} ms"));
                }
                row(
                    "content",
                    if s.assets.is_some() { "ready" } else { "generating…" }.into(),
                );
                row(
                    "particles",
                    format!("{} ({:?})", particles.alive, particles.backend.unwrap_or_default()),
                );
                row(
                    "draw calls",
                    format!("{} + {} particles", stats.draw_calls, particles.draw_calls),
                );
                row("render passes", format!("{}", stats.render_passes));
                row("targets", format!("{:.0} MB", stats.target_bytes as f64 / 1e6));
                let on = |b: bool| if b { "on" } else { "off" };
                row(
                    "bloom · shadows · IBL · distortion",
                    format!(
                        "{} · {} · {} · {}",
                        on(s.bloom),
                        on(s.shadows),
                        on(s.environment),
                        on(s.distortion)
                    ),
                );
            });
            ui.small("Q tier · 1 bloom · 2 shadows · 3 IBL · 4 distortion · 5 post · C follow · G timings");
        });
}

/// Headless: capture a while after the content arrived, write the PNG, exit.
fn screenshot(ctx: &mut SystemCtx) {
    let dressed = ctx.resources.expect::<Showcase>().dressed_frames;
    let Some(shot) = ctx.resources.get_mut::<Screenshot>() else {
        return;
    };
    if shot.pending.is_none() {
        if dressed >= shot.after_frames
            && let Some(renderer) = ctx.resources.get_mut::<Renderer>()
        {
            let task = renderer.capture();
            ctx.resources.expect_mut::<Screenshot>().pending = Some(task);
        }
        return;
    }
    let Some(result) = shot.pending.as_mut().and_then(Task::poll) else {
        return;
    };
    shot.pending = None;
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = std::env::var("SPACE3D_SCREENSHOT").unwrap_or_else(|_| "graphics_showcase.png".into());
        match result {
            Ok(image) => match space3d::test::save_png(&path, &image) {
                Ok(()) => println!("wrote {path} ({} × {})", image.width, image.height),
                Err(e) => eprintln!("could not write {path}: {e}"),
            },
            Err(e) => eprintln!("capture failed: {e}"),
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = result;
    ctx.send(AppExit);
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    /// Headless: the tier is chosen for the device, the content arrives off the frame and
    /// dresses the battle, the frame renders every feature without GPU errors, `Q` switches
    /// the tier.
    #[test]
    fn showcase_renders_the_battle_headless() {
        let mut app = App::new();
        app.add_plugin(DefaultPlugins::headless(480, 270));
        if app.resources.get::<Renderer>().is_none() {
            eprintln!("skipping: no GPU adapter");
            return;
        }
        add_game(&mut app);
        app.step_ticks(1);
        let q = app.resources.expect::<Showcase>().quality.expect("tier chosen");
        assert_eq!(q, GraphicsQuality::recommended_for(app.resources.expect::<Renderer>()));
        for _ in 0..20_000 {
            if app.resources.expect::<Showcase>().assets.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            app.step_ticks(1);
        }
        assert!(app.resources.expect::<Showcase>().assets.is_some(), "content arrived");
        app.step_ticks(60);
        let img = render_to_image(&mut app).expect("capture");
        let stats = app.resources.expect::<RenderStats>().clone();
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
        assert_eq!(stats.depth_views, 1, "{stats:?}");
        assert!(stats.post_groups >= 1, "{stats:?}");
        assert!(app.resources.expect::<ParticleStats>().alive > 1000);
        let lit = img.pixels.chunks(4).filter(|p| p[0].max(p[1]).max(p[2]) > 60).count();
        assert!(lit > 2000, "{lit} lit pixels");
        let next = GraphicsQuality::ALL[(GraphicsQuality::ALL.iter().position(|&t| t == q).unwrap() + 1) % 4];
        app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::KeyQ);
        app.step_ticks(2);
        assert_eq!(app.resources.expect::<Showcase>().quality, Some(next));
        render_to_image(&mut app).expect("capture");
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    }
}
