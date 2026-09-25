//! Shared helpers of the particle GPU tests. Every test calls [`app`] first and returns early
//! (printing a message) when no GPU adapter is available.
#![allow(dead_code)]

use space3d_core::prelude::*;
use space3d_particles::prelude::*;
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
