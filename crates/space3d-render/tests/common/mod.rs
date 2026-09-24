//! Shared helpers for GPU integration tests. Every test calls [`headless_app`] first and
//! returns early (printing a message) when no GPU adapter is available.
#![allow(dead_code)]

use space3d_core::prelude::*;
use space3d_render::*;

/// App with `RenderPlugin` and a headless renderer, or `None` (test should skip).
pub fn headless_app(width: u32, height: u32) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
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

/// Camera at `z` looking down -Z with a pure-blue clear color.
pub fn spawn_camera(app: &mut App, z: f32) -> Entity {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, z),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
    ))
}

pub fn spawn_sun(app: &mut App) {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).looking_at(Vec3::new(-0.3, -1.0, -0.6), Vec3::Y),
        DirectionalLight { color: Vec3::ONE, intensity: 2.0 },
    ));
}

pub const CLEAR: [u8; 4] = [0, 0, 255, 255];

pub fn set_path(app: &mut App, path: DrawPath) {
    app.resources.expect_mut::<RenderSettings>().draw_path = Some(path);
}

pub fn stats(app: &App) -> RenderStats {
    app.resources.expect::<RenderStats>().clone()
}

pub fn best_path(app: &App) -> DrawPath {
    app.resources.expect::<Renderer>().capabilities().best_draw_path
}

/// Fraction of pixels whose max channel difference exceeds `tol`.
pub fn diff_fraction(a: &ImageData, b: &ImageData, tol: u8) -> f64 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let bad = a
        .pixels
        .chunks(4)
        .zip(b.pixels.chunks(4))
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > tol))
        .count();
    bad as f64 / (a.width * a.height) as f64
}
