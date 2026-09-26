//! Every post effect on a top-down space scene (the SpaceCorps view), toggled at runtime: bloom,
//! auto exposure, FXAA, vignette, chromatic aberration, film grain, color grading with a 3D LUT,
//! screen-space distortion (a warp shockwave and engine heat haze) and dither, under any
//! tonemapper.
//!
//! ```text
//! cargo run -p post_processing
//! SPACE3D_EXIT_AFTER_FRAMES=3 cargo run -p post_processing   # smoke test (background mode)
//! SPACE3D_HEADLESS=1 cargo run -p post_processing            # no window: writes post_processing.png
//! ```
//! Keys: `1` bloom, `2` auto exposure, `3` FXAA, `4` vignette, `5` chromatic aberration, `6` film
//! grain, `7` color grading, `8` distortion, `9` dither, `T` next tonemapper, `G` GPU timings
//! (printed once per second with the active effects), `Esc` quit. Headless, the example renders
//! 30 frames, writes `post_processing.png` (or `$SPACE3D_SCREENSHOT`) and exits.

use space3d::prelude::*;
use space3d::render::{
    AutoExposure, ColorGrading, ColorLut, DistortionMaterial, DistortionMode, FilmGrain, FrameSettings, FxaaQuality,
    GpuTimings, RenderStats, Renderer, TextureHandle, Vignette, linear_to_srgb,
};
use space3d::tasks::Task;

/// Which effects are on (keys 1–9, T).
struct Effects {
    on: [bool; 9],
    tonemapper: usize,
    lut: Option<TextureHandle>,
    timings: bool,
}

const NAMES: [&str; 9] = [
    "bloom",
    "auto exposure",
    "FXAA",
    "vignette",
    "chromatic aberration",
    "film grain",
    "color grading",
    "distortion",
    "dither",
];

const TONEMAPPERS: [Tonemapper; 5] = [
    Tonemapper::AgX,
    Tonemapper::AcesFitted,
    Tonemapper::KhronosPbrNeutral,
    Tonemapper::Reinhard,
    Tonemapper::None,
];

impl Effects {
    /// The camera's `PostProcess` for the current toggles.
    fn post(&self) -> PostProcess {
        let [bloom, ae, fxaa, vignette, ca, grain, grading, distortion, dither] = self.on;
        let mut p = PostProcess::default().with_tonemapper(TONEMAPPERS[self.tonemapper]);
        p.bloom.enabled = bloom;
        if ae {
            // A darker key than middle grey suits space: the lit hulls, not the void, set it.
            p = p.with_auto_exposure(AutoExposure::on()).with_exposure(-1.5);
        }
        if fxaa {
            p = p.with_fxaa(FxaaQuality::Medium);
        }
        p.vignette = Vignette {
            enabled: vignette,
            intensity: 0.35,
            ..Vignette::default()
        };
        p = p.with_chromatic_aberration(if ca { 0.25 } else { 0.0 });
        p.film_grain = FilmGrain {
            enabled: grain,
            intensity: 0.05,
            ..FilmGrain::default()
        };
        if grading {
            p.color_grading = ColorGrading {
                temperature: 0.15,
                saturation: 1.15,
                contrast: 1.1,
                lut: self.lut.map(|t| ColorLut {
                    contribution: 0.7,
                    ..ColorLut::new(t, 17)
                }),
                ..ColorGrading::default()
            };
        }
        p.distortion.enabled = distortion;
        p.distortion.chromatic = 0.4;
        p.dither = dither;
        p
    }
}

/// The post camera.
struct MainCamera(Entity);

/// A ship circling the view's center.
struct Orbit {
    radius: f32,
    speed: f32,
    phase: f32,
    height: f32,
}

/// The warp shockwave: a ring expanding from `center` every `period` seconds.
struct Shockwave {
    center: Vec3,
    period: f32,
}

/// Headless run: the capture in flight, and the frame to request it at.
struct Screenshot {
    at_frame: u64,
    pending: Option<Task<Result<ImageData, space3d::render::RenderError>>>,
}

fn build(app: &mut App) {
    let headless = DefaultPlugins::headless_from_env((1280, 720)).is_some();
    app.add_plugin(DefaultPlugins::new().title("Space3d — post processing"));
    if headless {
        app.insert_resource(Screenshot {
            at_frame: 30,
            pending: None,
        });
    }
    add_game(app);
}

/// The scene and its systems, without the engine plugins (the tests add them to a headless app).
fn add_game(app: &mut App) {
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::Update, controls);
    app.add_system(Stage::Update, animate);
    app.add_system(Stage::Update, report);
    app.add_system(Stage::Last, screenshot);
}

fn setup(ctx: &mut SystemCtx) {
    let assets = ctx.resources.expect_mut::<RenderAssets>();
    let cube = assets.add_mesh(MeshData::cube(1.0));
    let sphere = assets.add_mesh(MeshData::sphere(0.5, 32, 16));
    let planet = assets.add_mesh(MeshData::sphere(1.0, 64, 32));
    let plane = assets.add_mesh(MeshData::plane(1.0));
    let nebula = assets.add_image(nebula_image(256));
    let lut = ColorLut::from_cube(&teal_orange_cube(17))
        .ok()
        .map(|strip| assets.add_texture(strip));

    // Backdrop: a nebula and stars far below the ships.
    ctx.world.spawn((
        Transform::from_xyz(0.0, -8.0, 0.0).with_scale(Vec3::new(64.0, 1.0, 36.0)),
        Mesh3d(plane),
        Material::unlit(Vec4::new(0.9, 0.9, 0.9, 1.0)).with_texture(nebula),
    ));
    let mut seed = 7u32;
    let mut rng = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32
    };
    for _ in 0..900 {
        let p = Vec3::new(rng() * 44.0 - 22.0, -6.0, rng() * 25.0 - 12.5);
        let v = 1.0 + 10.0 * rng() * rng() * rng();
        ctx.world.spawn((
            Transform::from_xyz(p.x, p.y, p.z).with_scale(Vec3::splat(0.035 + 0.04 * rng())),
            Mesh3d(cube),
            Material::unlit((Vec3::new(0.85, 0.9, 1.0) * v).extend(1.0)),
        ));
    }
    ctx.world.spawn((
        Transform::from_xyz(-6.5, -3.0, 2.5).with_scale(Vec3::splat(3.0)),
        Mesh3d(planet),
        Material::srgb(0.78, 0.5, 0.34).with_metallic_roughness(0.0, 0.75),
    ));

    // Ships circling the center, engine glows and heat haze behind their nozzles.
    for (i, (tint, engine)) in [
        (Vec3::new(0.5, 0.52, 0.56), Vec3::new(4.0, 10.0, 24.0)),
        (Vec3::new(0.55, 0.5, 0.45), Vec3::new(20.0, 8.0, 2.0)),
        (Vec3::new(0.45, 0.5, 0.5), Vec3::new(6.0, 18.0, 8.0)),
    ]
    .into_iter()
    .enumerate()
    {
        let orbit = Orbit {
            radius: 2.2 + 1.3 * i as f32,
            speed: 0.35 - 0.08 * i as f32,
            phase: i as f32 * 2.1,
            height: 0.3 * i as f32,
        };
        let ship = ctx.world.spawn((Transform::default(), orbit));
        spawn_fighter(ctx, ship, cube, sphere, plane, tint, engine);
    }
    // Laser bolts between the ships' orbits.
    for (a, b, color) in [
        (
            Vec3::new(-1.0, 0.2, -1.5),
            Vec3::new(0.8, 0.2, -2.2),
            Vec3::new(24.0, 2.0, 1.0),
        ),
        (
            Vec3::new(1.5, 0.3, 1.0),
            Vec3::new(2.6, 0.3, 2.4),
            Vec3::new(2.0, 20.0, 4.0),
        ),
    ] {
        let d = b - a;
        ctx.world.spawn((
            Transform::from_xyz((a.x + b.x) / 2.0, a.y, (a.z + b.z) / 2.0)
                .with_rotation(Quat::from_rotation_arc(Vec3::Z, d.normalize()))
                .with_scale(Vec3::new(0.05, 0.05, d.length())),
            Mesh3d(cube),
            Material::unlit(color.extend(1.0)),
        ));
    }
    // The warp shockwave (below the ships: scene depth keeps them sharp) and its flash.
    let center = Vec3::new(3.5, -1.0, -2.0);
    ctx.world.spawn((
        Transform::from_xyz(center.x, center.y, center.z),
        Mesh3d(plane),
        DistortionMaterial {
            strength: 0.05,
            mode: DistortionMode::Radial { inner: 0.3, outer: 0.5 },
            ..DistortionMaterial::default()
        },
        Shockwave { center, period: 4.0 },
    ));

    ctx.world.spawn((
        Transform::IDENTITY.looking_at(Vec3::new(0.7, -0.5, -0.45), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.95, 0.88),
            intensity: 2.0,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.04,
    });
    let effects = Effects {
        on: [true, true, true, true, true, false, true, true, true],
        tonemapper: 0,
        lut,
        timings: false,
    };
    let camera = ctx.world.spawn((
        Transform::from_xyz(0.0, 12.0, 0.0).looking_at(Vec3::ZERO, Vec3::NEG_Z),
        Camera3d::default().with_clear_color(Vec4::new(0.004, 0.005, 0.012, 1.0)),
        effects.post(),
    ));
    ctx.resources.insert(MainCamera(camera));
    ctx.resources.insert(effects);
}

/// A top-down fighter as children of `ship` (heading −Z): hull, swept wings, canopy, glowing
/// nozzles and a heat-haze strip behind each.
fn spawn_fighter(
    ctx: &mut SystemCtx,
    ship: Entity,
    cube: MeshHandle,
    sphere: MeshHandle,
    plane: MeshHandle,
    tint: Vec3,
    engine: Vec3,
) {
    let hull = Material::color(tint.extend(1.0)).with_metallic_roughness(0.75, 0.35);
    let wing = Material::color((tint * 0.5).extend(1.0)).with_metallic_roughness(0.6, 0.5);
    let parts: [(Vec3, f32, Vec3, &Material); 5] = [
        (Vec3::new(0.0, 0.0, 0.15), 0.0, Vec3::new(0.3, 0.2, 1.0), &hull),
        (
            Vec3::new(0.0, 0.0, -0.4),
            std::f32::consts::FRAC_PI_4,
            Vec3::new(0.3, 0.18, 0.3),
            &hull,
        ),
        (Vec3::new(-0.42, -0.03, 0.3), -0.45, Vec3::new(0.62, 0.05, 0.28), &wing),
        (Vec3::new(0.42, -0.03, 0.3), 0.45, Vec3::new(0.62, 0.05, 0.28), &wing),
        (Vec3::new(0.0, 0.02, 0.45), 0.0, Vec3::new(0.46, 0.16, 0.5), &hull),
    ];
    for (offset, yaw, size, material) in parts {
        ctx.world.spawn((
            Transform::from_xyz(offset.x, offset.y, offset.z)
                .with_rotation(Quat::from_rotation_y(yaw))
                .with_scale(size),
            Mesh3d(cube),
            material.clone(),
            Parent(ship),
        ));
    }
    ctx.world.spawn((
        Transform::from_xyz(0.0, 0.1, -0.05).with_scale(Vec3::new(0.14, 0.08, 0.3)),
        Mesh3d(sphere),
        Material::color(Vec4::new(0.05, 0.12, 0.2, 1.0)).with_emissive(Vec3::new(0.1, 0.35, 0.6)),
        Parent(ship),
    ));
    for x in [-0.2f32, 0.2] {
        ctx.world.spawn((
            Transform::from_xyz(x, 0.02, 0.72).with_scale(Vec3::new(0.13, 0.1, 0.08)),
            Mesh3d(sphere),
            Material::unlit(engine.extend(1.0)),
            Parent(ship),
        ));
        ctx.world.spawn((
            Transform::from_xyz(x, -0.35, 1.5).with_scale(Vec3::new(0.5, 1.0, 1.5)),
            Mesh3d(plane),
            DistortionMaterial {
                strength: 0.004,
                scroll: Vec2::new(0.0, -1.2),
                ..DistortionMaterial::default()
            },
            Parent(ship),
        ));
    }
}

fn controls(ctx: &mut SystemCtx) {
    let input = ctx.resources.expect::<Input>();
    let keys = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    let toggled: Vec<usize> = (0..keys.len()).filter(|&i| input.just_pressed(keys[i])).collect();
    let (next_tonemapper, timings, quit) = (
        input.just_pressed(KeyCode::KeyT),
        input.just_pressed(KeyCode::KeyG),
        input.just_pressed(KeyCode::Escape),
    );
    if quit {
        ctx.send(AppExit);
    }
    if toggled.is_empty() && !next_tonemapper && !timings {
        return;
    }
    let effects = ctx.resources.expect_mut::<Effects>();
    for i in toggled {
        effects.on[i] = !effects.on[i];
        println!("{}: {}", NAMES[i], if effects.on[i] { "on" } else { "off" });
    }
    if next_tonemapper {
        effects.tonemapper = (effects.tonemapper + 1) % TONEMAPPERS.len();
        println!("tonemapper: {:?}", TONEMAPPERS[effects.tonemapper]);
    }
    if timings {
        effects.timings = !effects.timings;
    }
    let (post, timings) = (effects.post(), effects.timings);
    ctx.resources.expect_mut::<FrameSettings>().gpu_timing = timings;
    let camera = ctx.resources.expect::<MainCamera>().0;
    let _ = ctx.world.insert_one(camera, post);
}

fn animate(ctx: &mut SystemCtx) {
    let t = ctx.time().elapsed_secs() as f32;
    for (transform, orbit) in ctx.world.query_mut::<(&mut Transform, &Orbit)>() {
        let a = orbit.phase + orbit.speed * t;
        transform.translation = Vec3::new(a.cos() * orbit.radius, orbit.height, a.sin() * orbit.radius);
        // Heading along the orbit (counter-clockwise seen from above).
        let heading = Vec3::new(-a.sin(), 0.0, a.cos()) * -orbit.speed.signum();
        transform.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, heading);
    }
    for (transform, material, wave) in ctx
        .world
        .query_mut::<(&mut Transform, &mut DistortionMaterial, &Shockwave)>()
    {
        let progress = (t / wave.period).fract();
        transform.translation = wave.center;
        transform.scale = Vec3::splat(1.0 + 12.0 * progress);
        material.strength = 0.05 * (1.0 - progress);
    }
}

/// Once per second: the active effects, the post passes and (with `G`) the GPU timings.
fn report(ctx: &mut SystemCtx) {
    let time = ctx.time();
    let (now, before) = (time.elapsed_secs(), time.elapsed_secs() - time.delta_secs() as f64);
    if now.floor() == before.floor() {
        return;
    }
    let effects = ctx.resources.expect::<Effects>();
    let on: Vec<&str> = (0..NAMES.len()).filter(|&i| effects.on[i]).map(|i| NAMES[i]).collect();
    let stats = ctx.resources.expect::<RenderStats>();
    let mut line = format!(
        "{:?}, {} | {} post passes, {} distortion pass(es), {:.1} MB of targets",
        TONEMAPPERS[effects.tonemapper],
        on.join(", "),
        stats.post_passes,
        stats.distortion_passes,
        stats.target_bytes as f64 / 1e6
    );
    if effects.timings {
        let t = ctx.resources.expect::<GpuTimings>();
        for label in [
            "post.exposure",
            "post.bloom",
            "post.composite",
            "post.fxaa",
            "distortion",
        ] {
            line += &format!(" | {label} {:.3} ms", t.scope_ms(label));
        }
    }
    println!("{line}");
}

/// Headless: request a capture at `at_frame`, write it when it arrives, exit.
fn screenshot(ctx: &mut SystemCtx) {
    let frame = ctx.time().frame;
    let Some(shot) = ctx.resources.get_mut::<Screenshot>() else {
        return;
    };
    if shot.pending.is_none() && frame >= shot.at_frame {
        if let Some(renderer) = ctx.resources.get_mut::<Renderer>() {
            let task = renderer.capture();
            ctx.resources.expect_mut::<Screenshot>().pending = Some(task);
        }
        return;
    }
    let Some(result) = shot.pending.as_mut().and_then(|t| t.poll()) else {
        return;
    };
    shot.pending = None;
    let path = std::env::var("SPACE3D_SCREENSHOT").unwrap_or_else(|_| "post_processing.png".into());
    match result {
        Ok(image) => match space3d::test::save_png(&path, &image) {
            Ok(()) => println!("wrote {path} ({} × {})", image.width, image.height),
            Err(e) => eprintln!("could not write {path}: {e}"),
        },
        Err(e) => eprintln!("capture failed: {e}"),
    }
    ctx.send(AppExit);
}

/// A soft, domain-warped nebula (sRGB), deterministic.
fn nebula_image(n: u32) -> ImageData {
    let hash = |x: i32, y: i32, o: u32| {
        let mut h =
            (x as u32).wrapping_mul(0x8DA6_B343) ^ (y as u32).wrapping_mul(0xD816_3841) ^ o.wrapping_mul(0xCB1A_B31F);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5BD1_E995);
        h ^= h >> 15;
        (h & 0x00FF_FFFF) as f32 / 16_777_215.0
    };
    let value = |x: f32, y: f32, period: i32, o: u32| {
        let (fx, fy) = (x.floor(), y.floor());
        let (tx, ty) = (x - fx, y - fy);
        let (ux, uy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
        let w = |i: i32| i.rem_euclid(period);
        let (x0, y0) = (fx as i32, fy as i32);
        let (a, b) = (hash(w(x0), w(y0), o), hash(w(x0 + 1), w(y0), o));
        let (c, d) = (hash(w(x0), w(y0 + 1), o), hash(w(x0 + 1), w(y0 + 1), o));
        let top = a + (b - a) * ux;
        top + (c + (d - c) * ux - top) * uy
    };
    let fbm = |x: f32, y: f32, o: u32| {
        let (mut sum, mut amp, mut f) = (0.0, 0.5, 4.0);
        for i in 0..5 {
            sum += amp * value(x * f, y * f, f as i32, o + i);
            amp *= 0.5;
            f *= 2.0;
        }
        sum
    };
    let (thin, dense) = (Vec3::new(0.05, 0.08, 0.32), Vec3::new(0.5, 0.12, 0.42));
    let mut img = ImageData::new(n, n);
    for y in 0..n {
        for x in 0..n {
            let (u, v) = (x as f32 / n as f32, y as f32 / n as f32);
            let warp = fbm(u, v, 38);
            let density = fbm(u + 0.35 * warp, v - 0.25 * warp, 21);
            let cloud = ((density - 0.42) / 0.38).clamp(0.0, 1.0).powf(1.6);
            let c = thin.lerp(dense, (density * 1.6 - 0.5).clamp(0.0, 1.0)) * cloud;
            let px = [c.x, c.y, c.z].map(|v| (linear_to_srgb(v.clamp(0.0, 1.0)) * 255.0).round() as u8);
            img.set(x, y, [px[0], px[1], px[2], 255]);
        }
    }
    img
}

/// A teal & orange `.cube` LUT of size `n` (sRGB-encoded in and out).
fn teal_orange_cube(n: u32) -> String {
    let mut s = format!("TITLE \"teal orange\"\nLUT_3D_SIZE {n}\n");
    let last = (n - 1) as f32;
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let c = Vec3::new(r as f32, g as f32, b as f32) / last;
                let luma = c.dot(Vec3::new(0.2126, 0.7152, 0.0722));
                let tint = Vec3::new(0.0, 0.5, 0.55).lerp(Vec3::new(1.0, 0.62, 0.3), luma);
                let o = c.lerp(tint * (luma * 1.6), 0.35).clamp(Vec3::ZERO, Vec3::ONE);
                s += &format!("{} {} {}\n", o.x, o.y, o.z);
            }
        }
    }
    s
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    fn headless() -> App {
        let mut app = App::new();
        app.add_plugin(DefaultPlugins::headless(160, 90));
        add_game(&mut app);
        app
    }

    /// Keys toggle the effects on the camera's `PostProcess`; the scene renders every effect
    /// without GPU errors, the Distortion pass included.
    #[test]
    fn keys_toggle_effects() {
        let mut app = headless();
        app.step_ticks(3);
        let camera = app.resources.expect::<MainCamera>().0;
        let post = *app.world.get::<&PostProcess>(camera).unwrap();
        assert!(post.bloom.enabled && post.auto_exposure.enabled && post.distortion.enabled);
        app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::Digit1);
        app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::KeyT);
        app.step_ticks(1);
        let post = *app.world.get::<&PostProcess>(camera).unwrap();
        assert!(!post.bloom.enabled);
        assert_eq!(post.tonemapper, Tonemapper::AcesFitted);
        if let Some(r) = app.resources.get::<Renderer>() {
            assert_eq!(r.gpu_error_count(), 0);
            let s = app.resources.expect::<RenderStats>();
            assert_eq!(s.post_groups, 1);
        }
        app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::Escape);
        app.step_ticks(1);
        assert!(app.should_exit());
    }
}
