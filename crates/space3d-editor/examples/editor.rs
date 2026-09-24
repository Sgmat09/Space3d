//! Standalone editor on a small demo scene.
//!
//! ```sh
//! cargo run -p space3d-editor --example editor                 # window
//! cargo run -p space3d-editor --example editor -- --frames 120 # quit after 120 frames
//! cargo run -p space3d-editor --example editor -- --headless --frames 30
//! cargo run -p space3d-editor --example editor -- --frames 60 --screenshot shot.png
//! ```
//!
//! Try: click a cube (or pick it in the hierarchy), drag the gizmo arrows or the inspector
//! values, Ctrl/Cmd+Z to undo, Space to play the spin simulation, "." to step one tick,
//! edit the custom `Spin` component as JSON, File > Save / Open, Window > Texture editor /
//! Model editor, F3 debug overlay, F4 profiler, ` console.

use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_editor::{EditorConfig, MeshSource, run_editor};
use space3d_render::{Camera3d, DirectionalLight, Material, PointLight};
use space3d_scene::ComponentRegistry;

/// Game component, registered in the scene registry so the inspector can edit it as JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Spin {
    /// Radians per second around Y.
    speed: f32,
}

fn setup(app: &mut App) {
    app.resources.expect_mut::<ComponentRegistry>().register::<Spin>("Spin");
    let w = &mut app.world;
    w.spawn((
        Name("Camera".into()),
        Transform::from_xyz(0.0, 4.0, 10.0).looking_at(Vec3::new(0.0, 0.5, 0.0), Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.05, 0.06, 0.09, 1.0)),
    ));
    w.spawn((
        Name("Sun".into()),
        Transform::from_xyz(0.0, 6.0, 0.0).looking_at(Vec3::new(-0.4, -1.0, -0.5), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.95, 0.9),
            intensity: 2.0,
        },
    ));
    w.spawn((
        Name("Lamp".into()),
        Transform::from_xyz(2.5, 1.5, 2.0),
        PointLight {
            color: Vec3::new(1.0, 0.6, 0.3),
            intensity: 4.0,
            range: 8.0,
        },
    ));
    w.spawn((
        Name("Ground".into()),
        Transform::IDENTITY,
        MeshSource::Plane { size: 16.0 },
        Material::srgb(0.35, 0.38, 0.4),
    ));
    let pivot = w.spawn((
        Name("Carousel".into()),
        Transform::from_xyz(0.0, 0.75, 0.0),
        Spin { speed: 0.6 },
    ));
    for (i, color) in [(0.9, 0.3, 0.3), (0.3, 0.8, 0.4), (0.3, 0.5, 0.95)]
        .into_iter()
        .enumerate()
    {
        let angle = i as f32 * std::f32::consts::TAU / 3.0;
        w.spawn((
            Name(format!("Cube {}", i + 1)),
            Transform::from_xyz(angle.cos() * 2.5, 0.0, angle.sin() * 2.5),
            MeshSource::Cube { size: 1.0 },
            Material::srgb(color.0, color.1, color.2).with_metallic_roughness(0.1, 0.4),
            Spin { speed: 1.5 },
            Parent(pivot),
        ));
    }
    w.spawn((
        Name("Sphere".into()),
        Transform::from_xyz(0.0, 1.0, 0.0),
        MeshSource::Sphere {
            radius: 0.8,
            sectors: 32,
            stacks: 16,
        },
        Material::srgb(0.95, 0.85, 0.3).with_metallic_roughness(0.8, 0.25),
    ));
    app.add_system(Stage::FixedUpdate, |sys| {
        let dt = sys.time().fixed_delta_secs();
        for (t, s) in sys.world.query_mut::<(&mut Transform, &Spin)>() {
            t.rotation = Quat::from_rotation_y(s.speed * dt) * t.rotation;
        }
    });
}

/// `--screenshot` support: capture at frame `at`, write the PNG, then quit. Quits with an
/// error message if the capture fails or never finishes.
fn screenshot_system(app: &mut App, path: String, at: u64) {
    const GIVE_UP_AFTER_FRAMES: u64 = 600;
    let mut task: Option<space3d_tasks::Task<Result<ImageData, space3d_render::RenderError>>> = None;
    let mut requested = false;
    app.add_system(Stage::Last, move |sys| {
        let frame = sys.time().frame;
        if !requested && frame >= at {
            requested = true;
            task = sys.resources.get_mut::<space3d_render::Renderer>().map(|r| r.capture());
            if task.is_none() {
                eprintln!("screenshot failed: no renderer");
                sys.send(AppExit);
                return;
            }
        }
        if let Some(t) = &mut task {
            // Captures resolve on device polls (idle devices, WebGL).
            if let Some(r) = sys.resources.get::<space3d_render::Renderer>() {
                r.poll();
            }
            match t.poll() {
                Some(Ok(img)) => {
                    task = None;
                    let written = space3d_assets::edit::texture::encode_png(&img)
                        .map_err(|e| e.to_string())
                        .and_then(|png| std::fs::write(&path, png).map_err(|e| e.to_string()));
                    match written {
                        Ok(()) => println!("screenshot saved to {path} ({}x{})", img.width, img.height),
                        Err(e) => eprintln!("screenshot failed: {path}: {e}"),
                    }
                    sys.send(AppExit);
                }
                Some(Err(e)) => {
                    task = None;
                    eprintln!("screenshot failed: {e}");
                    sys.send(AppExit);
                }
                None if frame >= at + GIVE_UP_AFTER_FRAMES => {
                    eprintln!("screenshot failed: capture did not finish within {GIVE_UP_AFTER_FRAMES} frames");
                    sys.send(AppExit);
                }
                None => {}
            }
        }
    });
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let frames: Option<u64> = args
        .iter()
        .position(|a| a == "--frames")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok());
    // `--screenshot out.png`: capture frame `--frames` (default 120; window surface or headless
    // target), write it and quit once it is written.
    let screenshot = args
        .iter()
        .position(|a| a == "--screenshot")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let at = frames.unwrap_or(120).max(1);
    let exit_after = if screenshot.is_some() { None } else { frames };
    let mut config = EditorConfig::new("Space3d Editor").with_setup(move |app: &mut App| {
        setup(app);
        if let Some(path) = screenshot.clone() {
            screenshot_system(app, path, at);
        }
    });
    config.exit_after_frames = exit_after;
    if args.iter().any(|a| a == "--headless") {
        config.headless = Some((1280, 720));
    }
    run_editor(config);
}
