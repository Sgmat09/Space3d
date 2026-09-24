//! One million cubes in a single explicit [`InstanceBuffer`] (32-byte [`InstancePacked`]
//! instances), GPU-culled where the adapter supports it, with a travelling wave that rewrites
//! only a small band of instances per frame (dirty-range uploads).
//!
//! ```text
//! cargo run -p instancing_million --release
//! cargo run -p instancing_million --release -- --count 250000 --static
//! SPACE3D_EXIT_AFTER_FRAMES=300 cargo run -p instancing_million --release
//! ```
//! Flags: `--count N` (default 1 000 000), `--static` (no per-frame edits → 0 bytes uploaded),
//! `--band N` instances rewritten per frame (default 16 384).
//!
//! Controls: `Esc` quit · `Space` pause the wave · `C` toggle CPU culling / best GPU path ·
//! `F` toggle frustum culling · `F3` engine debug overlay · `F4` profiler. An egui stats window
//! (plus the window title and stdout every 2 s) shows FPS and [`RenderStats`]: draw path, draw
//! calls, instances, bytes uploaded this frame.

use space3d::prelude::*;

/// Command-line options.
#[derive(Clone, Copy, Debug)]
struct Options {
    count: usize,
    band: usize,
    animate: bool,
}

impl Options {
    fn from_args() -> Self {
        let mut o = Options {
            count: 1_000_000,
            band: 16_384,
            animate: true,
        };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--count" => o.count = args.next().and_then(|v| v.parse().ok()).unwrap_or(o.count),
                "--band" => o.band = args.next().and_then(|v| v.parse().ok()).unwrap_or(o.band),
                "--static" => o.animate = false,
                other => eprintln!("ignoring unknown argument {other:?}"),
            }
        }
        o.band = o.band.min(o.count);
        o
    }

    /// Instances per grid row (the field is square).
    fn side(&self) -> usize {
        (self.count as f64).sqrt().ceil() as usize
    }
}

const SPACING: f32 = 1.25;

/// The explicit instance buffer and the wave state.
struct Field {
    buffer: InstanceBufferHandle,
    options: Options,
    /// Next instance index the wave band starts at.
    cursor: usize,
    paused: bool,
}

/// Stats line refreshed every 30 frames.
#[derive(Default)]
struct StatsLine(String);

fn build(app: &mut App) {
    let options = Options::from_args();
    app.add_plugin(
        DefaultPlugins::new()
            .title(format!("Space3d — {} instances", options.count))
            .without_physics()
            .without_net()
            .with_debug_overlay(),
    );
    add_game(app, options);
}

/// The instance field and its systems, without the engine plugins (the tests add them to a
/// headless app).
fn add_game(app: &mut App, options: Options) {
    app.insert_resource(options);
    app.init_resource::<StatsLine>();
    app.add_ui_system(stats_window);
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::Update, controls);
    app.add_system(Stage::Update, wave);
    app.add_system(Stage::Update, orbit_camera);
    app.add_system(Stage::Last, report);
}

/// Instance `i` of the field at time `t`.
fn instance(i: usize, side: usize, t: f32) -> InstancePacked {
    let (x, z) = ((i % side) as f32, (i / side) as f32);
    let half = side as f32 * SPACING * 0.5;
    let p = Vec3::new(x * SPACING - half, 0.0, z * SPACING - half);
    let d = (p.x * p.x + p.z * p.z).sqrt();
    let y = (d * 0.05 - t * 2.0).sin() * 3.0 + (p.x * 0.02).sin() * (p.z * 0.03).cos() * 6.0;
    let hue = (d * 0.004 + t * 0.05).fract() * std::f32::consts::TAU;
    let color = Vec4::new(
        0.55 + 0.45 * hue.cos(),
        0.55 + 0.45 * (hue + 2.094).cos(),
        0.55 + 0.45 * (hue + 4.188).cos(),
        1.0,
    );
    let rotation = Quat::from_rotation_y(d * 0.01 + t) * Quat::from_rotation_x(y * 0.1);
    InstancePacked::new(Vec3::new(p.x, y, p.z), rotation, 0.55, color, 0)
}

fn setup(ctx: &mut SystemCtx) {
    let options = *ctx.resources.expect::<Options>();
    let cube = ctx.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    // Build the million instances off the main thread, then register the buffer.
    ctx.resources.expect_mut::<AsyncQueue>().compute_then(
        move || {
            let side = options.side();
            (0..options.count).map(|i| instance(i, side, 0.0)).collect::<Vec<_>>()
        },
        move |ctx, instances: Vec<InstancePacked>| {
            let usage = if options.animate {
                BufferUsage::Dynamic
            } else {
                BufferUsage::Static
            };
            let buffer = ctx
                .resources
                .expect_mut::<InstanceBuffers>()
                .insert(InstanceBuffer::from_vec(instances, usage));
            ctx.world.spawn((InstancedMesh {
                mesh: cube,
                material: Material::default().with_metallic_roughness(0.2, 0.45),
                instances: buffer,
            },));
            ctx.resources.insert(Field {
                buffer,
                options,
                cursor: 0,
                paused: !options.animate,
            });
            println!(
                "{} instances ready ({} MiB)",
                options.count,
                options.count * 32 / (1024 * 1024)
            );
        },
    );

    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.3, -1.0, -0.4), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.95, 0.88),
            intensity: 2.0,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.2,
    });
    ctx.world.spawn((
        Transform::from_xyz(0.0, 120.0, 400.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.01, 0.01, 0.02, 1.0))
            .with_projection(Projection::Perspective {
                fov_y: 55f32.to_radians(),
                near: 0.5,
                far: 5000.0,
            }),
    ));
}

/// Rewrite one band of instances per frame: only those bytes are uploaded.
fn wave(ctx: &mut SystemCtx) {
    let t = ctx.time().elapsed_secs() as f32;
    let Some(field) = ctx.resources.get_mut::<Field>() else {
        return;
    };
    if field.paused || field.options.band == 0 {
        return;
    }
    let (count, band, side, start) = (
        field.options.count,
        field.options.band,
        field.options.side(),
        field.cursor,
    );
    let end = (start + band).min(count);
    field.cursor = if end == count { 0 } else { end };
    let handle = field.buffer;
    if let Some(buf) = ctx
        .resources
        .expect_mut::<InstanceBuffers>()
        .get_mut::<InstancePacked>(handle)
    {
        for (offset, inst) in buf.range_mut(start..end).iter_mut().enumerate() {
            *inst = instance(start + offset, side, t);
        }
    }
}

fn orbit_camera(ctx: &mut SystemCtx) {
    let t = ctx.time().elapsed_secs() as f32 * 0.05;
    for (transform, _) in ctx.world.query_mut::<(&mut Transform, &Camera3d)>() {
        let eye = Vec3::new(t.sin() * 420.0, 110.0 + (t * 3.0).sin() * 40.0, t.cos() * 420.0);
        *transform = Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(Vec3::ZERO, Vec3::Y);
    }
}

fn controls(ctx: &mut SystemCtx) {
    let input = ctx.resources.expect::<Input>();
    let (quit, pause, path, cull) = (
        input.just_pressed(KeyCode::Escape),
        input.just_pressed(KeyCode::Space),
        input.just_pressed(KeyCode::KeyC),
        input.just_pressed(KeyCode::KeyF),
    );
    if quit {
        ctx.send(AppExit);
    }
    if pause && let Some(field) = ctx.resources.get_mut::<Field>() {
        field.paused = !field.paused;
    }
    let settings = ctx.resources.expect_mut::<RenderSettings>();
    if path {
        settings.draw_path = match settings.draw_path {
            Some(DrawPath::CpuDirect) => None,
            _ => Some(DrawPath::CpuDirect),
        };
    }
    if cull {
        settings.frustum_culling = !settings.frustum_culling;
    }
}

fn report(ctx: &mut SystemCtx) {
    let time = ctx.time();
    let (frame, fps) = (time.frame, 1.0 / time.delta_secs().max(1e-6));
    if !frame.is_multiple_of(30) {
        return;
    }
    let s = ctx.resources.expect::<RenderStats>().clone();
    let line = format!(
        "{fps:.0} fps | {:?} | {} draws ({} indirect) | {} instances | {} tris | {} KiB uploaded",
        s.draw_path,
        s.draw_calls,
        s.indirect_draw_calls,
        s.instances,
        s.triangles,
        s.bytes_uploaded / 1024,
    );
    if frame.is_multiple_of(120) {
        println!("frame {frame}: {line}");
    }
    let count = ctx.resources.expect::<Options>().count;
    if let Some(window) = ctx.resources.get_mut::<WindowState>() {
        window.title = format!("Space3d — {count} instances — {line}");
    }
    ctx.resources.expect_mut::<StatsLine>().0 = line;
}

/// egui window with the live numbers (the `ui` feature of `space3d`, on by default).
fn stats_window(ui: &egui::Context, ctx: &mut SystemCtx) {
    let stats = ctx.resources.expect::<RenderStats>().clone();
    let fps = 1.0 / ctx.time().delta_secs().max(1e-6);
    let (count, band, paused) = match ctx.resources.get::<Field>() {
        Some(f) => (f.options.count, f.options.band, f.paused),
        None => (ctx.resources.expect::<Options>().count, 0, true),
    };
    egui::Window::new("Instancing")
        .default_pos([12.0, 120.0])
        .resizable(false)
        .show(ui, |ui| {
            egui::Grid::new("stats").num_columns(2).show(ui, |ui| {
                let mut row = |k: &str, v: String| {
                    ui.label(k);
                    ui.monospace(v);
                    ui.end_row();
                };
                row("fps", format!("{fps:.0}"));
                row("instances", format!("{count}"));
                row("draw path", format!("{:?}", stats.draw_path));
                row(
                    "draw calls",
                    format!("{} ({} indirect)", stats.draw_calls, stats.indirect_draw_calls),
                );
                row("triangles", format!("{}", stats.triangles));
                row("uploaded", format!("{} KiB / frame", stats.bytes_uploaded / 1024));
                row(
                    "wave band",
                    if paused {
                        "paused".into()
                    } else {
                        format!("{band} instances")
                    },
                );
            });
            ui.small("Space: pause · C: CPU path · F: culling · F3: overlay");
        });
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    fn headless(options: Options) -> App {
        let mut app = App::new();
        app.add_plugin(DefaultPlugins::headless(64, 64).without_physics().without_net());
        add_game(&mut app, options);
        app
    }

    /// Step until the compute pool has built the instances (the frame never waits for it).
    fn wait_for_field(app: &mut App) {
        for _ in 0..600 {
            if app.resources.contains::<Field>() {
                return;
            }
            app.step_ticks(1);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("instances were never built");
    }

    fn snapshot(app: &App) -> Vec<InstancePacked> {
        let handle = app.resources.expect::<Field>().buffer;
        let buffers = app.resources.expect::<InstanceBuffers>();
        buffers.get::<InstancePacked>(handle).unwrap().as_slice().to_vec()
    }

    #[test]
    fn field_is_built_off_thread_and_the_wave_rewrites_one_band_per_frame() {
        let options = Options {
            count: 1000,
            band: 100,
            animate: true,
        };
        let mut app = headless(options);
        wait_for_field(&mut app);
        assert_eq!(options.side(), 32);
        let before = snapshot(&app);
        assert_eq!(before.len(), 1000);
        assert_eq!(app.world.query::<&InstancedMesh>().iter().count(), 1);

        let cursor = app.resources.expect::<Field>().cursor;
        app.step_ticks(1);
        let after = snapshot(&app);
        let changed: Vec<usize> = (0..1000).filter(|&i| before[i] != after[i]).collect();
        assert!(!changed.is_empty());
        assert!(
            changed.iter().all(|&i| (cursor..cursor + 100).contains(&i)),
            "only the band [{cursor}, {}) is rewritten: {changed:?}",
            cursor + 100
        );
        assert_eq!(app.resources.expect::<Field>().cursor, (cursor + 100) % 1000);

        // Space pauses the wave: nothing changes any more.
        app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::Space);
        app.step_ticks(1);
        let paused = snapshot(&app);
        app.step_ticks(5);
        assert_eq!(snapshot(&app), paused);
    }

    #[test]
    fn static_field_never_changes() {
        let mut app = headless(Options {
            count: 500,
            band: 64,
            animate: false,
        });
        wait_for_field(&mut app);
        let before = snapshot(&app);
        app.step_ticks(10);
        assert_eq!(snapshot(&app), before);
    }
}
