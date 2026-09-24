//! A lit, rotating grid of cubes: 2 500 ECS entities batched into a handful of instanced draws,
//! plus 50 000 explicit instances in a dynamic [`InstanceBuffer`] orbiting the grid.
//!
//! ```text
//! cargo run -p space3d-platform --example spinning_cubes --release
//! cargo run -p space3d-platform --example spinning_cubes -- --frames 120   # auto-exit
//! cargo run -p space3d-platform --example spinning_cubes -- --capture shot.png --frames 90
//! ```
//! Controls: `Esc` quits, `C` toggles CPU / best GPU draw path. The window title shows FPS and
//! render stats.

use space3d_core::prelude::*;
use space3d_platform::*;
use space3d_render::*;

/// Per-entity spin.
struct Spin(Vec3);

/// Handle of the orbiting instance buffer.
struct Orbit(InstanceBufferHandle);

/// Pending window capture (`--capture path`).
struct PendingCapture {
    path: String,
    task: Option<space3d_tasks::Task<Result<ImageData, RenderError>>>,
}

const GRID: i32 = 50;
const ORBIT: usize = 50_000;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut max_frames: Option<u64> = std::env::var("SPACE3D_EXIT_AFTER_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut capture = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frames" => max_frames = args.next().and_then(|v| v.parse().ok()),
            "--capture" => capture = args.next(),
            _ => {}
        }
    }

    let mut app = App::new();
    app.add_plugin(PlatformPlugin {
        window: WindowPlugin::titled("Space3d — spinning cubes"),
    });
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::Update, spin);
    app.add_system(Stage::Update, orbit);
    app.add_system(Stage::Update, controls);
    if let Some(path) = capture {
        app.insert_resource(PendingCapture { path, task: None });
        app.add_system(Stage::Last, capture_window);
    }
    app.add_system(Stage::Last, move |ctx| {
        let frame = ctx.time().frame;
        if frame.is_multiple_of(30) {
            let fps = 1.0 / ctx.time().delta_secs().max(1e-6);
            let s = ctx.resources.expect::<RenderStats>().clone();
            ctx.resources.expect_mut::<WindowState>().title = format!(
                "Space3d — {fps:.0} fps | {:?} | {} draws, {} instances, {} KiB uploaded",
                s.draw_path,
                s.draw_calls,
                s.instances,
                s.bytes_uploaded / 1024
            );
        }
        if max_frames.is_some_and(|m| frame >= m) {
            let s = ctx.resources.expect::<RenderStats>().clone();
            println!("exiting after {frame} frames: {s:?}");
            ctx.send(AppExit);
        }
    });
    app.run();
}

fn setup(ctx: &mut SystemCtx) {
    let (cube, small) = {
        let assets = ctx.resources.expect_mut::<RenderAssets>();
        (
            assets.add_mesh(MeshData::cube(0.6)),
            assets.add_mesh(MeshData::cube(0.08)),
        )
    };
    let palette = [
        Material::srgb(0.95, 0.35, 0.3).with_metallic_roughness(0.0, 0.35),
        Material::srgb(0.3, 0.75, 0.95).with_metallic_roughness(0.0, 0.5),
        Material::srgb(0.95, 0.8, 0.3).with_metallic_roughness(1.0, 0.3),
        Material::srgb(0.5, 0.9, 0.5).with_metallic_roughness(0.0, 0.8),
    ];
    for x in 0..GRID {
        for z in 0..GRID {
            let p = Vec3::new(x as f32 - GRID as f32 / 2.0, 0.0, z as f32 - GRID as f32 / 2.0);
            let m = palette[((x + z) % palette.len() as i32) as usize].clone();
            let axis = Vec3::new((x % 3) as f32 - 1.0, 1.0, (z % 3) as f32 - 1.0).normalize();
            ctx.world.spawn((
                Transform::from_xyz(p.x, (x as f32 * 0.3).sin() * 0.5, p.z),
                Mesh3d(cube),
                m,
                Spin(axis * (0.5 + ((x * 7 + z * 3) % 10) as f32 * 0.15)),
            ));
        }
    }

    let buffer = InstanceBuffer::from_vec(
        vec![InstancePacked::new(Vec3::ZERO, Quat::IDENTITY, 1.0, Vec4::ONE, 0); ORBIT],
        BufferUsage::Dynamic,
    );
    let handle = ctx.resources.expect_mut::<InstanceBuffers>().insert(buffer);
    ctx.world.spawn((InstancedMesh {
        mesh: small,
        material: Material::default().with_emissive(Vec3::splat(0.05)),
        instances: handle,
    },));
    ctx.resources.insert(Orbit(handle));

    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.3), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.96, 0.9),
            intensity: 2.2,
        },
    ));
    ctx.world.spawn((
        Transform::from_xyz(0.0, 4.0, 0.0),
        PointLight {
            color: Vec3::new(0.4, 0.6, 1.0),
            intensity: 40.0,
            range: 20.0,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.12,
    });
    ctx.world.spawn((
        Transform::from_xyz(0.0, 22.0, 38.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.01, 0.012, 0.02, 1.0)),
    ));
}

fn spin(ctx: &mut SystemCtx) {
    let dt = ctx.time().delta_secs();
    for (t, s) in ctx.world.query_mut::<(&mut Transform, &Spin)>() {
        t.rotation = (Quat::from_scaled_axis(s.0 * dt) * t.rotation).normalize();
    }
}

fn orbit(ctx: &mut SystemCtx) {
    let t = ctx.time().elapsed_secs() as f32;
    let Some(&Orbit(h)) = ctx.resources.get::<Orbit>() else {
        return;
    };
    let Some(buf) = ctx
        .resources
        .expect_mut::<InstanceBuffers>()
        .get_mut::<InstancePacked>(h)
    else {
        return;
    };
    for (i, inst) in buf.as_mut_slice().iter_mut().enumerate() {
        let f = i as f32 / ORBIT as f32;
        let a = f * std::f32::consts::TAU * 7.0 + t * (0.2 + f * 0.3);
        let r = 30.0 + 6.0 * (f * 40.0 + t).sin();
        let p = Vec3::new(a.cos() * r, 3.0 + 4.0 * (f * 90.0 + t * 0.7).sin(), a.sin() * r);
        let hue = f * std::f32::consts::TAU;
        let color = Vec4::new(
            0.6 + 0.4 * hue.cos(),
            0.6 + 0.4 * (hue + 2.1).cos(),
            0.6 + 0.4 * (hue + 4.2).cos(),
            1.0,
        );
        *inst = InstancePacked::new(p, Quat::from_rotation_y(a * 3.0), 1.0, color, 0);
    }
}

fn controls(ctx: &mut SystemCtx) {
    let input = ctx.resources.expect::<Input>();
    let (quit, toggle) = (input.just_pressed(KeyCode::Escape), input.just_pressed(KeyCode::KeyC));
    if quit {
        ctx.send(AppExit);
    }
    if toggle {
        let s = ctx.resources.expect_mut::<RenderSettings>();
        s.draw_path = match s.draw_path {
            Some(DrawPath::CpuDirect) => None,
            _ => Some(DrawPath::CpuDirect),
        };
    }
}

/// Capture the window 60 frames in (re-rendered offscreen), save it without blocking the frame.
fn capture_window(ctx: &mut SystemCtx) {
    let frame = ctx.time().frame;
    if frame == 60 && ctx.resources.contains::<Renderer>() {
        let task = ctx.resources.expect_mut::<Renderer>().capture();
        ctx.resources.expect_mut::<PendingCapture>().task = Some(task);
    }
    let pending = ctx.resources.expect_mut::<PendingCapture>();
    if let Some(task) = &mut pending.task
        && let Some(result) = task.poll()
    {
        pending.task = None;
        match result {
            Ok(img) => {
                let ok = image::RgbaImage::from_raw(img.width, img.height, img.pixels).map(|i| i.save(&pending.path));
                println!("captured {}x{} -> {} ({ok:?})", img.width, img.height, pending.path);
            }
            Err(e) => eprintln!("capture failed: {e}"),
        }
    }
}
