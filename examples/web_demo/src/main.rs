//! A browser-first demo: one `build` function, [`space3d::main!`] generates the
//! `#[wasm_bindgen(start)]` entry point (and `fn main` for desktop).
//!
//! ```text
//! # web (from this directory): builds wasm, runs wasm-bindgen, serves dist/ with COOP/COEP
//! cargo run -p space3d-cli -- serve
//! # or by hand
//! cargo build -p web_demo --release --target wasm32-unknown-unknown
//! wasm-bindgen --target web --no-typescript --out-dir dist ../../target/wasm32-unknown-unknown/release/web_demo.wasm
//! cp web/index.html dist/
//! # desktop
//! cargo run -p web_demo
//! ```
//! Controls: drag (mouse or one finger) orbits · wheel / pinch zooms · click / tap cycles
//! the palette · `1`–`3` switch the wave pattern.
//!
//! The scene is 4 096 cubes in one explicit [`InstanceBuffer`] rewritten every frame —
//! a single draw call on WebGPU and WebGL2 alike.

use space3d::prelude::*;

const SIDE: usize = 64;

/// Camera orbit + pattern state.
struct Demo {
    buffer: InstanceBufferHandle,
    yaw: f32,
    pitch: f32,
    distance: f32,
    palette: usize,
    pattern: u32,
    /// Distance between the two touches last frame (pinch zoom).
    pinch: Option<f32>,
}

const PALETTES: [[Vec3; 2]; 3] = [
    [Vec3::new(0.1, 0.5, 1.0), Vec3::new(1.0, 0.4, 0.2)],
    [Vec3::new(0.2, 0.9, 0.5), Vec3::new(0.9, 0.9, 0.2)],
    [Vec3::new(0.9, 0.2, 0.6), Vec3::new(0.3, 0.2, 1.0)],
];

fn build(app: &mut App) {
    app.add_plugin(
        DefaultPlugins::new()
            .window(WindowPlugin {
                title: "Space3d — web demo".into(),
                // Render into the page's <canvas id="space3d"> (see web/index.html); on desktop
                // this is ignored.
                canvas_id: Some("space3d".into()),
                ..WindowPlugin::default()
            })
            .without_physics()
            .without_audio(),
    );
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::Update, controls);
    app.add_system(Stage::Update, animate);
    app.add_system(Stage::Update, camera);
}

fn setup(ctx: &mut SystemCtx) {
    let cube = ctx.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    let buffer = ctx
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(
            vec![InstancePacked::new(Vec3::ZERO, Quat::IDENTITY, 1.0, Vec4::ONE, 0); SIDE * SIDE],
            BufferUsage::Dynamic,
        ));
    ctx.world.spawn((InstancedMesh {
        mesh: cube,
        material: Material::default().with_metallic_roughness(0.1, 0.4),
        instances: buffer,
    },));
    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.2), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.2,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.25,
    });
    ctx.world.spawn((
        Transform::default(),
        Camera3d::default().with_clear_color(Vec4::new(0.02, 0.02, 0.04, 1.0)),
    ));
    ctx.resources.insert(Demo {
        buffer,
        yaw: 0.7,
        pitch: 0.55,
        distance: 70.0,
        palette: 0,
        pattern: 1,
        pinch: None,
    });
}

fn controls(ctx: &mut SystemCtx) {
    let input = ctx.resources.expect::<Input>();
    let mut drag = if input.mouse_pressed(MouseButton::Left) {
        input.mouse_delta()
    } else {
        Vec2::ZERO
    };
    let touches = input.touches().to_vec();
    if touches.len() == 1 {
        drag = touches[0].delta;
    }
    let pinch_now = (touches.len() == 2).then(|| touches[0].position.distance(touches[1].position));
    let tapped = input.mouse_just_released(MouseButton::Left) || !input.just_ended_touches().is_empty();
    let pattern = [KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3]
        .iter()
        .position(|k| input.just_pressed(*k));
    let wheel = input.wheel_delta().y;
    let travelled = input.just_ended_touches().first().map(|t| t.position.distance(t.start));

    let demo = ctx.resources.expect_mut::<Demo>();
    demo.yaw -= drag.x * 0.005;
    demo.pitch = (demo.pitch + drag.y * 0.005).clamp(0.05, 1.5);
    demo.distance = (demo.distance - wheel * 3.0).clamp(15.0, 160.0);
    if let (Some(now), Some(before)) = (pinch_now, demo.pinch) {
        demo.distance = (demo.distance * before / now.max(1.0)).clamp(15.0, 160.0);
    }
    demo.pinch = pinch_now;
    // A tap (not the end of a drag) cycles the palette.
    if tapped && drag.length() < 1.0 && travelled.is_none_or(|d| d < 10.0) {
        demo.palette = (demo.palette + 1) % PALETTES.len();
    }
    if let Some(p) = pattern {
        demo.pattern = p as u32 + 1;
    }
}

/// Height of cell (x, z) at time t for a pattern.
fn wave(pattern: u32, x: f32, z: f32, t: f32) -> f32 {
    match pattern {
        1 => ((x * x + z * z).sqrt() * 0.35 - t * 2.5).sin() * 2.0,
        2 => (x * 0.3 + t).sin() * (z * 0.25 + t * 1.3).cos() * 2.5,
        _ => ((x * 0.2 + t).sin() + (z * 0.2 - t * 0.7).sin() + ((x + z) * 0.15 + t * 1.7).sin()) * 1.2,
    }
}

fn animate(ctx: &mut SystemCtx) {
    let t = ctx.time().elapsed_secs() as f32;
    let (buffer, pattern, [low, high]) = {
        let d = ctx.resources.expect::<Demo>();
        (d.buffer, d.pattern, PALETTES[d.palette])
    };
    let Some(instances) = ctx
        .resources
        .expect_mut::<InstanceBuffers>()
        .get_mut::<InstancePacked>(buffer)
    else {
        return;
    };
    let half = SIDE as f32 * 0.5;
    for (i, instance) in instances.as_mut_slice().iter_mut().enumerate() {
        let (x, z) = ((i % SIDE) as f32 - half, (i / SIDE) as f32 - half);
        let y = wave(pattern, x, z, t);
        let k = (y * 0.25 + 0.5).clamp(0.0, 1.0);
        *instance = InstancePacked::new(
            Vec3::new(x * 1.1, y, z * 1.1),
            Quat::from_rotation_y(y * 0.3),
            0.9,
            low.lerp(high, k).extend(1.0),
            0,
        );
    }
}

fn camera(ctx: &mut SystemCtx) {
    let (yaw, pitch, distance) = {
        let d = ctx.resources.expect::<Demo>();
        (d.yaw, d.pitch, d.distance)
    };
    let eye = Vec3::new(yaw.sin() * pitch.cos(), pitch.sin(), yaw.cos() * pitch.cos()) * distance;
    for (transform, _) in ctx.world.query_mut::<(&mut Transform, &Camera3d)>() {
        *transform = Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(Vec3::ZERO, Vec3::Y);
    }
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waves_stay_in_range() {
        for pattern in 1..=3 {
            for i in 0..100 {
                let y = wave(pattern, i as f32 - 50.0, (i * 7 % 64) as f32 - 32.0, i as f32 * 0.1);
                assert!(y.abs() <= 4.0, "pattern {pattern}: {y}");
            }
        }
    }
}
