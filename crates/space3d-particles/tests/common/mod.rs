//! Shared helpers of the particle GPU tests. Every test calls [`app`] first and returns early
//! (printing a message) when no GPU adapter is available.
#![allow(dead_code)]

use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{FloatRange, Gradient, ParticleBlend, SpriteRender, SpriteShape};
use space3d_render::*;

/// App with `RenderPlugin`, `ParticlesPlugin` (with `settings`) and a headless renderer of
/// `width`×`height`, or `None` (the test should skip).
pub fn app_with(width: u32, height: u32, settings: ParticleSettings) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    app.add_plugin(ParticlesPlugin::with_settings(settings));
    match pollster::block_on(Renderer::new_headless(width, height)) {
        Ok(r) => {
            app.insert_resource(r);
            Some(app)
        }
        Err(e) => {
            eprintln!("skipping GPU test: no adapter available ({e})");
            None
        }
    }
}

/// [`app_with`] default settings.
pub fn app(width: u32, height: u32) -> Option<App> {
    app_with(width, height, ParticleSettings::default())
}

/// A camera at `eye` looking at `target` with a clear color.
pub fn camera(app: &mut App, eye: Vec3, target: Vec3, clear: Vec4) -> Entity {
    app.world.spawn((
        Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(target, Vec3::Y),
        Camera3d::default().with_clear_color(clear),
    ))
}

/// An orthographic camera looking down −Z from z = 10, `height` world units tall.
pub fn ortho_camera(app: &mut App, height: f32, clear: Vec4) -> Entity {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 10.0),
        Camera3d::default()
            .with_clear_color(clear)
            .with_projection(Projection::Orthographic {
                height,
                near: 0.1,
                far: 100.0,
            }),
    ))
}

/// Render the app (without advancing time) and read the image back.
pub fn render(app: &mut App) -> ImageData {
    render_to_image(app).expect("capture")
}

/// Write `img` to `$SPACE3D_PARTICLES_DUMP/<name>.png` when that variable is set (debugging).
pub fn dump(img: &ImageData, name: &str) {
    if let Ok(dir) = std::env::var("SPACE3D_PARTICLES_DUMP") {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        space3d_test::save_png(&path, img).expect("save png");
        eprintln!("wrote {}", path.display());
    }
}

/// The pixel at normalized coordinates `(x, y)` (0..1, top left origin).
pub fn at(img: &ImageData, x: f32, y: f32) -> [u8; 4] {
    let px = ((x * img.width as f32) as u32).min(img.width - 1);
    let py = ((y * img.height as f32) as u32).min(img.height - 1);
    img.get(px, py)
}

/// Fraction of pixels that differ from `rgb` by more than `tol` in some channel.
pub fn fraction_not(img: &ImageData, rgb: [u8; 3], tol: u8) -> f64 {
    let n = img
        .pixels
        .chunks(4)
        .filter(|p| p.iter().zip(rgb).any(|(a, b)| a.abs_diff(b) > tol))
        .count();
    n as f64 / (img.width * img.height) as f64
}

/// The renderer's validation error count (0 in every test).
pub fn gpu_errors(app: &App) -> u64 {
    app.resources.expect::<Renderer>().gpu_error_count()
}

/// Particle stats of the last frame.
pub fn stats(app: &App) -> ParticleStats {
    app.resources.expect::<ParticleStats>().clone()
}

/// Stats after two more frames without ticks: the GPU backend's alive count (read back 1–3
/// frames late) describes the current particles.
pub fn settled_stats(app: &mut App) -> ParticleStats {
    render(app);
    render(app);
    stats(app)
}

/// A layer of one long-lived, motionless particle drawn as a hard-edged disc of `size`.
pub fn dot(color: Vec4, blend: ParticleBlend, size: f32) -> ParticleLayer {
    ParticleLayer {
        lod: false,
        size: FloatRange::constant(size),
        color_over_life: Gradient::constant(color),
        render: SpriteRender {
            shape: SpriteShape::Glow {
                core: 0.0,
                hardness: 1.0,
            },
            blend,
            ..SpriteRender::default()
        },
        ..ParticleLayer::burst(1, 100.0)
    }
}

/// Register a one-layer effect.
pub fn effect(app: &mut App, layer: ParticleLayer) -> EffectHandle {
    app.resources
        .expect_mut::<ParticleEffects>()
        .add(ParticleEffect::new("test").layer(layer))
        .unwrap()
}

/// Default settings without the Direct-mode soft clip (exact pixel values).
pub fn settings() -> ParticleSettings {
    ParticleSettings {
        direct_soft_clip: false,
        ..ParticleSettings::default()
    }
}

/// Linear → sRGB byte.
pub fn srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

/// `got` is the sRGB encoding of `linear` within `tol`.
#[track_caller]
pub fn assert_rgb(got: [u8; 4], linear: Vec3, tol: u8, what: &str) {
    let want = [srgb(linear.x), srgb(linear.y), srgb(linear.z)];
    for c in 0..3 {
        assert!(
            got[c].abs_diff(want[c]) <= tol,
            "{what}: got {got:?}, want {want:?} (linear {linear})"
        );
    }
}

/// The three blend modes over a 0.2 grey background at x = −3, 0, 3 (ortho height 10), and
/// the linear colors each must produce.
pub fn blend_scene(app: &mut App) -> [(f32, Vec3); 3] {
    let bg = 0.2;
    let add = effect(app, dot(Vec4::new(0.5, 0.25, 0.0, 1.0), ParticleBlend::Additive, 2.0));
    let alpha = effect(app, dot(Vec4::new(0.8, 0.1, 0.1, 0.5), ParticleBlend::Alpha, 2.0));
    let premul = effect(
        app,
        ParticleLayer {
            coverage: FloatRange::constant(0.5),
            ..dot(Vec4::new(0.3, 0.6, 0.9, 1.0), ParticleBlend::Premultiplied, 2.0)
        },
    );
    for (fx, x) in [(add, -3.0), (alpha, 0.0), (premul, 3.0)] {
        app.spawn_effect(fx, EffectSpawn::at(Vec3::new(x, 0.0, 0.0)).keep());
    }
    let b = Vec3::splat(bg);
    [
        (-3.0, b + Vec3::new(0.5, 0.25, 0.0)),
        (0.0, Vec3::new(0.8, 0.1, 0.1) * 0.5 + b * 0.5),
        (3.0, Vec3::new(0.3, 0.6, 0.9) + b * 0.5),
    ]
}

/// An unlit 0.1 grey wall at z = 0 over the left half (seen by an ortho camera of height 10 at
/// 320×180), particles 0.25 in front of it (soft distance 1), over the open background, and
/// behind the wall. Returns the rendered image.
pub fn soft_scene(app: &mut App) -> ImageData {
    let quad = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(10.0));
    app.world.spawn((
        Transform::from_xyz(-5.0, 0.0, 0.0).with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
        Mesh3d(quad),
        Material {
            double_sided: true,
            ..Material::unlit(Vec4::new(0.1, 0.1, 0.1, 1.0))
        },
    ));
    let soft = effect(
        app,
        ParticleLayer {
            render: SpriteRender {
                soft_distance: 1.0,
                ..dot(Vec4::ONE, ParticleBlend::Additive, 2.0).render
            },
            ..dot(Vec4::ONE, ParticleBlend::Additive, 2.0)
        },
    );
    for p in [
        Vec3::new(-2.5, 0.0, 0.25),
        Vec3::new(2.5, 0.0, 0.25),
        Vec3::new(-2.5, 2.5, -0.5),
    ] {
        app.spawn_effect(soft, EffectSpawn::at(p).keep());
    }
    app.step_ticks(2);
    render(app)
}

/// The pixels of the particle in front of the wall, over the open background and behind the
/// wall in [`soft_scene`].
pub fn soft_probe(img: &ImageData) -> ([u8; 4], [u8; 4], [u8; 4]) {
    let px = |x: f32, y: f32| at(img, 0.5 + x * 18.0 / 320.0, 0.5 - y * 18.0 / 180.0);
    (px(-2.5, 0.0), px(2.5, 0.0), px(-2.5, 2.5))
}
