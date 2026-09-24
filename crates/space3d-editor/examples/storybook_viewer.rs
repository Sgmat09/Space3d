//! Storybook viewer on a small demo storybook.
//!
//! ```sh
//! cargo run -p space3d-editor --example storybook_viewer
//! cargo run -p space3d-editor --example storybook_viewer -- --frames 120
//! cargo run -p space3d-editor --example storybook_viewer -- --headless --frames 60
//! ```

use space3d_core::prelude::*;
use space3d_editor::{StorybookConfig, run_storybook_with};
use space3d_render::{Camera3d, DirectionalLight, Material, Mesh3d, PointLight, RenderAssets};
use space3d_stories::{Story, Storybook};
use space3d_ui::{UiAppExt, egui};

fn camera(app: &mut App, eye: Vec3) {
    app.world.spawn((
        Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.08, 0.09, 0.12, 1.0)),
    ));
    app.world.spawn((
        Transform::IDENTITY.looking_at(Vec3::new(-0.4, -1.0, -0.6), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.0,
        },
    ));
}

#[derive(Clone, Copy)]
struct Spinning(f32);

fn spin_system(app: &mut App) {
    app.add_system(Stage::FixedUpdate, |sys| {
        let dt = sys.time().fixed_delta_secs();
        for (t, s) in sys.world.query_mut::<(&mut Transform, &Spinning)>() {
            t.rotation = Quat::from_rotation_y(s.0 * dt) * Quat::from_rotation_x(s.0 * 0.5 * dt) * t.rotation;
        }
    });
}

/// A few stories exercising every knob kind.
fn demo_storybook() -> Storybook {
    let mut book = Storybook::new();
    book.add(
        Story::new("Primitives/Cube", |app, k| {
            camera(app, Vec3::new(0.0, 1.5, 4.0));
            let mesh = app
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::cube(k.f32("size")));
            let [r, g, b, a] = k.color("color");
            app.world.spawn((
                Transform::IDENTITY,
                Mesh3d(mesh),
                Material::color(Vec4::new(r, g, b, a)).with_metallic_roughness(k.f32("metallic"), 0.4),
                Spinning(if k.bool("spin") { 1.0 } else { 0.0 }),
            ));
            spin_system(app);
        })
        .description("A lit, spinning cube.")
        .knob_f32("size", 1.2, 0.2..=2.5)
        .knob_color("color", [0.9, 0.35, 0.3, 1.0])
        .knob_f32("metallic", 0.1, 0.0..=1.0)
        .knob_bool("spin", true)
        .size(480, 360)
        .ticks(30),
    );
    book.add(
        Story::new("Primitives/Sphere", |app, k| {
            camera(app, Vec3::new(0.0, 1.0, 3.5));
            let n = k.i64("sectors") as u32;
            let mesh = app
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::sphere(1.0, n, (n / 2).max(2)));
            app.world
                .spawn((Transform::IDENTITY, Mesh3d(mesh), Material::srgb(0.3, 0.7, 0.9)));
        })
        .knob_i64("sectors", 16, 3..=64)
        .size(400, 400),
    );
    book.add(
        Story::new("Lights/Point lights", |app, k| {
            camera(app, Vec3::new(0.0, 3.0, 6.0));
            let plane = app
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::plane(8.0));
            app.world
                .spawn((Transform::IDENTITY, Mesh3d(plane), Material::srgb(0.7, 0.7, 0.7)));
            let colors = [
                Vec3::new(1.0, 0.3, 0.2),
                Vec3::new(0.2, 1.0, 0.4),
                Vec3::new(0.3, 0.4, 1.0),
            ];
            let color = colors[k.choice("color")];
            for i in 0..k.i64("count") {
                let a = i as f32 * 1.3;
                app.world.spawn((
                    Transform::from_xyz(a.cos() * 2.0, 0.6, a.sin() * 2.0),
                    PointLight {
                        color,
                        intensity: k.f32("intensity"),
                        range: 4.0,
                    },
                ));
            }
        })
        .knob_choice("color", &["red", "green", "blue"], 0)
        .knob_i64("count", 3, 0..=8)
        .knob_f32("intensity", 3.0, 0.0..=10.0)
        .tag("lighting"),
    );
    book.add(
        Story::new("UI/Button", |app, k| {
            camera(app, Vec3::new(0.0, 0.0, 3.0));
            let label = k.text("label").to_owned();
            let mut clicks = 0u32;
            app.add_ui_system(move |ctx: &egui::Context, _sys: &mut SystemCtx| {
                egui::Window::new("Story UI").default_pos([40.0, 40.0]).show(ctx, |ui| {
                    if ui.button(&label).clicked() {
                        clicks += 1;
                    }
                    ui.label(format!("clicked {clicks} times"));
                });
            });
        })
        .description("egui inside a story; click it in the viewport.")
        .knob_text("label", "Press me")
        .tag("ui")
        .size(360, 240),
    );
    book
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let frames = args
        .iter()
        .position(|a| a == "--frames")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok());
    let config = StorybookConfig {
        exit_after_frames: frames,
        headless: args.iter().any(|a| a == "--headless").then_some((1280, 720)),
        ..Default::default()
    };
    run_storybook_with(demo_storybook(), config);
}
