//! Headless rendering: capture, clear color, determinism, lighting, textures.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;

#[test]
fn cube_covers_center_and_corners_stay_clear() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    app.world
        .spawn((Transform::default(), Mesh3d(mesh), Material::srgb(0.9, 0.3, 0.2)));
    spawn_camera(&mut app, 3.0);
    spawn_sun(&mut app);
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!((img.width, img.height), (64, 64));
    assert_eq!(img.pixels.len(), 64 * 64 * 4);
    let center = img.get(32, 32);
    assert_ne!(center, CLEAR, "cube must cover the center");
    assert!(center[0] > center[2], "lit red-ish surface expected, got {center:?}");
    for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63)] {
        assert_eq!(img.get(x, y), CLEAR, "corner ({x},{y}) must show the clear color");
    }
}

#[test]
fn output_is_deterministic() {
    let Some(mut app) = headless_app(96, 64) else { return };
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::sphere(0.8, 24, 12));
    for i in 0..5 {
        app.world.spawn((
            Transform::from_xyz(i as f32 - 2.0, 0.0, 0.0),
            Mesh3d(mesh),
            Material::srgb(0.2 * i as f32, 0.8, 0.4).with_metallic_roughness(0.5, 0.3),
        ));
    }
    app.world.spawn((
        Transform::from_xyz(0.0, 2.0, 2.0),
        PointLight {
            intensity: 8.0,
            ..Default::default()
        },
    ));
    spawn_camera(&mut app, 5.0);
    spawn_sun(&mut app);
    let a = render_to_image(&mut app).expect("capture");
    let b = render_to_image(&mut app).expect("capture");
    assert_eq!(a, b, "same scene must produce identical pixels");
}

#[test]
fn capture_task_resolves() {
    let Some(mut app) = headless_app(32, 32) else { return };
    spawn_camera(&mut app, 3.0);
    app.step_ticks(1);
    let task = app.resources.expect_mut::<Renderer>().capture();
    let img = task.block().expect("task").expect("capture");
    assert!(img.pixels.chunks(4).all(|p| p == CLEAR));
}

/// Regression: `capture()` waited for the GPU inside a blocking `device.poll` on a worker
/// thread. wgpu holds its device-wide snatch lock for the whole wait, so everything on the
/// main thread that takes it exclusively — `queue.present`, buffer/texture destruction —
/// stalled until the GPU had finished all outstanding work. A capture in flight must not
/// hold the device.
#[test]
fn capture_in_flight_does_not_stall_the_main_thread() {
    use std::time::{Duration, Instant};
    let Some(mut app) = headless_app(512, 512) else { return };
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::sphere(0.5, 32, 16));
    let h = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::<InstanceCompact>::new(BufferUsage::Dynamic));
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::default(),
        instances: h,
    },));
    spawn_camera(&mut app, 4.0);
    let wait = |app: &App| {
        let _ = app
            .resources
            .expect::<Renderer>()
            .device()
            .poll(wgpu::PollType::wait_indefinitely());
    };
    // Grow the (overlapping, vertex-heavy) instance count until one frame keeps the GPU busy
    // long enough to tell a stall from noise.
    let mut gpu = Duration::ZERO;
    let mut n = 0usize;
    while gpu < Duration::from_millis(40) && n < 2_000_000 {
        let add = (n * 3).max(8192);
        let buf = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .get_mut::<InstanceCompact>(h)
            .unwrap();
        for i in n..n + add {
            let a = i as f32 * 0.618;
            buf.push(InstanceCompact::new(
                Vec3::new(a.sin(), a.cos(), 0.0) * 0.5,
                Quat::IDENTITY,
                1.0,
            ));
        }
        n += add;
        app.step_ticks(1);
        wait(&app);
        let t = Instant::now();
        app.run_stage(Stage::Render);
        wait(&app);
        gpu = t.elapsed();
    }
    if gpu < Duration::from_millis(40) {
        eprintln!("skipping: GPU too fast to observe a stall ({gpu:?} per frame)");
        return;
    }

    app.run_stage(Stage::Render);
    let task = app.resources.expect_mut::<Renderer>().capture();
    std::thread::sleep(Duration::from_millis(2)); // the capture is being waited for now
    let device = app.resources.expect::<Renderer>().device().clone();
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 256,
        usage: wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let t = Instant::now();
    buffer.destroy(); // takes the snatch lock exclusively, like `queue.present`
    let stalled = t.elapsed();
    let img = task.block().expect("task").expect("capture");
    assert_eq!((img.width, img.height), (512, 512));
    assert!(
        stalled < gpu / 4,
        "destroying a buffer took {stalled:?} while a capture was in flight (GPU frame {gpu:?})"
    );
}

#[test]
fn msaa_and_resize() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    app.world
        .spawn((Transform::default(), Mesh3d(mesh), Material::unlit(Vec4::ONE)));
    spawn_camera(&mut app, 3.0);
    app.resources.expect_mut::<Renderer>().resize(48, 40);
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!((img.width, img.height), (48, 40));
    assert_eq!(img.get(24, 20), [255, 255, 255, 255], "unlit white is exact");
}

#[test]
fn textured_mesh_and_render_target() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let (quad, tex, rt) = {
        let assets = app.resources.expect_mut::<RenderAssets>();
        let quad = assets.add_mesh(MeshData::cube(2.0));
        let tex = assets.add_image(ImageData::checker(16, 8, [255, 0, 0, 255], [0, 255, 0, 255]));
        let rt = assets.add_render_target(32, 32);
        (quad, tex, rt)
    };
    // Unlit checker cube seen by the main camera.
    app.world.spawn((
        Transform::default(),
        Mesh3d(quad),
        Material::unlit(Vec4::ONE).with_texture(tex),
    ));
    spawn_camera(&mut app, 2.5);
    // Second camera renders into a texture first (order -1); must not disturb the primary.
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d {
            order: -1,
            target: RenderTarget::Texture(rt),
            ..Default::default()
        },
    ));
    let img = render_to_image(&mut app).expect("capture");
    let mut reds = 0;
    let mut greens = 0;
    for p in img.pixels.chunks(4) {
        if p[0] > 200 && p[1] < 60 {
            reds += 1;
        }
        if p[1] > 200 && p[0] < 60 {
            greens += 1;
        }
    }
    assert!(
        reds > 100 && greens > 100,
        "checker texture visible (r={reds}, g={greens})"
    );
    assert_eq!(stats(&app).views, 2);
}

#[test]
fn many_meshes_share_the_arena() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let meshes: Vec<_> = (0..200)
        .map(|i| {
            app.resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::sphere(0.5, 4 + i % 13, 3 + i % 7))
        })
        .collect();
    for (i, m) in meshes.iter().enumerate() {
        let x = (i % 20) as f32 * 0.5 - 5.0;
        let y = (i / 20) as f32 * 0.5 - 2.5;
        app.world
            .spawn((Transform::from_xyz(x, y, 0.0), Mesh3d(*m), Material::default()));
    }
    spawn_camera(&mut app, 8.0);
    set_path(&mut app, DrawPath::CpuDirect);
    render_to_image(&mut app).expect("capture");
    let s = stats(&app);
    assert_eq!(s.meshes_uploaded, 200);
    assert_eq!(s.batches, 200);
    let (v, i) = app.resources.expect::<RenderAssets>().arena_usage();
    assert!(v > 0 && i > 0);
    // Removing frees arena space for reuse.
    let assets = app.resources.expect_mut::<RenderAssets>();
    for m in &meshes[..100] {
        assert!(assets.remove_mesh(*m));
    }
    render_to_image(&mut app).expect("capture");
    let (v2, _) = app.resources.expect::<RenderAssets>().arena_usage();
    assert!(v2 < v);
}

/// A "monitor" cube whose material samples render target `rt`, a primary camera looking at it
/// and a camera rendering into `rt` (red clear). `rt_sees_monitor` places the RT camera so the
/// monitor is (or is not) inside its frustum.
fn monitor_scene(path: DrawPath, rt_sees_monitor: bool) -> Option<App> {
    let mut app = headless_app(64, 64)?;
    let (cube, rt) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(1.0)), a.add_render_target(32, 32))
    };
    app.world.spawn((
        Transform::default(),
        Mesh3d(cube),
        Material::unlit(Vec4::ONE).with_texture(rt),
    ));
    spawn_camera(&mut app, 2.5);
    let x = if rt_sees_monitor { 0.0 } else { 1000.0 };
    app.world.spawn((
        Transform::from_xyz(x, 0.0, 5.0),
        Camera3d {
            order: -1,
            target: RenderTarget::Texture(rt),
            clear_color: Some(Vec4::new(1.0, 0.0, 0.0, 1.0)),
            ..Default::default()
        },
    ));
    set_path(&mut app, path);
    Some(app)
}

fn count_red(img: &ImageData) -> usize {
    img.pixels
        .chunks(4)
        .filter(|p| p[0] > 200 && p[1] < 40 && p[2] < 40)
        .count()
}

/// Regression: a camera rendering into a texture used to bind that same texture as a sampled
/// resource inside its own pass (conflicting usages → wgpu validation panic). This happened on
/// every path when the camera saw the monitor, and on GPU-culling paths even when it did not
/// (indirect draws are recorded for every source).
#[test]
fn render_target_sampled_by_a_material_in_its_own_view_does_not_panic() {
    let Some(probe) = headless_app(8, 8) else { return };
    let best = best_path(&probe);
    drop(probe);
    let mut paths = vec![DrawPath::CpuDirect];
    if best != DrawPath::CpuDirect {
        paths.push(best);
    }
    for path in paths {
        for sees in [false, true] {
            let mut app = monitor_scene(path, sees).expect("adapter");
            for frame in 0..3 {
                let img = render_to_image(&mut app).expect("capture");
                let s = stats(&app);
                assert_eq!((s.views, s.draw_path), (2, path));
                let red = count_red(&img);
                // The monitor shows the RT camera's red clear color (fully red when the camera
                // looks away; a red frame around the recursive image when it sees itself).
                assert!(
                    red > 300,
                    "{path:?} sees={sees} frame {frame}: monitor must show the render target ({red} red px)"
                );
                if !sees {
                    assert_eq!(
                        img.get(32, 32),
                        [255, 0, 0, 255],
                        "{path:?}: monitor shows the clear color"
                    );
                }
            }
        }
    }
}

/// When the RT camera sees its own monitor, the monitor inside the render target shows the
/// target's previous content (one frame of delay, like a video feedback loop).
#[test]
fn render_target_feedback_shows_the_previous_frame() {
    let Some(mut app) = monitor_scene(DrawPath::CpuDirect, true) else {
        return;
    };
    let frames: Vec<_> = (0..4).map(|_| render_to_image(&mut app).expect("capture")).collect();
    // Frame 1 samples the zero-initialised target, later frames the previous frame's red
    // border, so the image keeps changing for a few frames and then converges.
    assert_ne!(frames[0], frames[1], "second frame sees the first frame's content");
    assert!(count_red(&frames[3]) > count_red(&frames[0]));
}

/// Regression: `resize` replaced the offscreen target immediately, so a capture before the next
/// frame returned a blank image of the new size instead of the last rendered frame.
#[test]
fn capture_after_resize_returns_the_last_frame() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    app.world
        .spawn((Transform::default(), Mesh3d(mesh), Material::unlit(Vec4::ONE)));
    spawn_camera(&mut app, 3.0);
    let before = render_to_image(&mut app).expect("capture");
    assert_eq!(before.get(16, 16), [255, 255, 255, 255]);
    let r = app.resources.expect_mut::<Renderer>();
    r.resize(48, 40);
    assert_eq!(r.size(), (48, 40));
    let again = r.capture_blocking().expect("capture");
    assert_eq!(again, before, "capture reads back the last rendered frame");
    r.set_msaa_samples(4);
    let again = r.capture_blocking().expect("capture");
    assert_eq!(again, before, "changing MSAA does not discard the last frame either");
    let after = render_to_image(&mut app).expect("capture");
    assert_eq!((after.width, after.height), (48, 40));
    assert_eq!(after.get(24, 20), [255, 255, 255, 255]);
    assert_eq!(after.get(0, 0), CLEAR);
}

/// Regression: `update_texture`/`update_image` on a render target returned true and changed the
/// reported size while the GPU attachment kept its old size. Render targets are resized with
/// `resize_render_target` instead.
#[test]
fn render_targets_are_resized_not_updated() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let (cube, rt) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(1.0)), a.add_render_target(32, 32))
    };
    // The primary camera looks at a monitor showing the render target; the RT camera looks at
    // a white cube in front of a red clear color.
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0),
        Mesh3d(cube),
        Material::unlit(Vec4::ONE).with_texture(rt),
    ));
    app.world.spawn((
        Transform::from_xyz(100.0, 0.0, 0.0),
        Mesh3d(cube),
        Material::unlit(Vec4::ONE),
    ));
    app.world.spawn((
        Transform::from_xyz(100.0, 0.0, 4.0),
        Camera3d {
            order: -1,
            target: RenderTarget::Texture(rt),
            clear_color: Some(Vec4::new(1.0, 0.0, 0.0, 1.0)),
            ..Default::default()
        },
    ));
    spawn_camera(&mut app, 1.5);
    let before = render_to_image(&mut app).expect("capture");
    let assets = app.resources.expect_mut::<RenderAssets>();
    assert!(
        !assets.update_image(rt, ImageData::new(64, 32)),
        "render targets cannot be overwritten"
    );
    assert_eq!(assets.texture_size(rt), Some((32, 32)));
    assert!(assets.resize_render_target(rt, 64, 32));
    assert_eq!(assets.texture_size(rt), Some((64, 32)));
    let after = render_to_image(&mut app).expect("capture");
    assert_eq!(
        stats(&app).textures_uploaded,
        1,
        "the target is recreated at the new size"
    );
    // The white cube stays centred in the (now wider) target: the monitor's centre is white
    // and its left/right edges show the red clear color.
    for img in [&before, &after] {
        assert_eq!(img.get(32, 32), [255, 255, 255, 255]);
        assert_eq!(img.get(4, 32), [255, 0, 0, 255]);
        assert_eq!(img.get(59, 32), [255, 0, 0, 255]);
    }
    assert_ne!(before, after, "aspect ratio of the target changed");
}

/// Regression: render targets and the primary target were created at any requested size. Past
/// the device's `max_texture_dimension_2d` texture creation failed validation, which
/// invalidated the frame's whole command buffer — every view was lost, every frame (a panic
/// in debug builds). Oversized render targets are now not created (logged; their cameras are
/// skipped and materials sample white) and the primary target is clamped.
#[test]
fn targets_past_the_texture_size_limit_do_not_lose_frames() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let max = app
        .resources
        .expect::<Renderer>()
        .device()
        .limits()
        .max_texture_dimension_2d;
    let (cube, rt) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(1.0)), a.add_render_target(32, 32))
    };
    // A monitor showing the render target, whose camera sees a white cube on red.
    app.world.spawn((
        Transform::default(),
        Mesh3d(cube),
        Material::unlit(Vec4::ONE).with_texture(rt),
    ));
    app.world.spawn((
        Transform::from_xyz(100.0, 0.0, 0.0),
        Mesh3d(cube),
        Material::unlit(Vec4::ONE),
    ));
    app.world.spawn((
        Transform::from_xyz(100.0, 0.0, 4.0),
        Camera3d {
            order: -1,
            target: RenderTarget::Texture(rt),
            clear_color: Some(Vec4::new(1.0, 0.0, 0.0, 1.0)),
            ..Default::default()
        },
    ));
    spawn_camera(&mut app, 1.5);
    let before = render_to_image(&mut app).expect("capture");
    assert_eq!(before.get(4, 32), [255, 0, 0, 255], "the monitor shows the target");

    for (w, h) in [(max + 1, 4), (4, max + 1)] {
        assert!(
            app.resources
                .expect_mut::<RenderAssets>()
                .resize_render_target(rt, w, h)
        );
        let img = render_to_image(&mut app).expect("capture");
        assert_eq!(stats(&app).views, 1, "{w}x{h}: the target's camera is skipped");
        assert_eq!(
            img.get(4, 32),
            [255, 255, 255, 255],
            "{w}x{h}: the monitor samples white"
        );
        assert_eq!(img.get(0, 0), CLEAR, "{w}x{h}: the primary view is drawn");
    }
    // Back within the limit: the target is created again.
    app.resources
        .expect_mut::<RenderAssets>()
        .resize_render_target(rt, 32, 32);
    assert_eq!(render_to_image(&mut app).expect("capture"), before);

    // A new oversized target does not affect the other views.
    let big = app.resources.expect_mut::<RenderAssets>().add_render_target(max + 1, 8);
    app.world.spawn((
        Transform::from_xyz(-100.0, 0.0, 4.0),
        Camera3d {
            target: RenderTarget::Texture(big),
            ..Default::default()
        },
    ));
    assert_eq!(render_to_image(&mut app).expect("capture"), before);

    // The primary target is clamped per axis.
    let r = app.resources.expect_mut::<Renderer>();
    r.resize(max + 5, 2);
    assert_eq!(r.size(), (max, 2));
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!((img.width, img.height), (max, 2));
    app.resources.expect_mut::<Renderer>().resize(64, 64);
    assert_eq!(render_to_image(&mut app).expect("capture"), before);
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}

/// Regression: formats the material binding cannot sample (unfilterable 32-bit float without
/// FLOAT32_FILTERABLE, integer, depth) were reported as supported and panicked at bind-group
/// creation. They must be rejected and render with the white fallback.
#[test]
fn unsamplable_texture_formats_fall_back_to_white() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let r = app.resources.expect::<Renderer>();
    let f32_filterable = r.device().features().contains(wgpu::Features::FLOAT32_FILTERABLE);
    assert!(r.supports_texture_format(wgpu::TextureFormat::Rgba8UnormSrgb));
    assert!(r.supports_texture_format(wgpu::TextureFormat::Rgba16Float));
    assert!(!r.supports_texture_format(wgpu::TextureFormat::R32Uint));
    assert!(!r.supports_texture_format(wgpu::TextureFormat::Depth32Float));
    assert_eq!(
        r.supports_texture_format(wgpu::TextureFormat::Rgba32Float),
        f32_filterable
    );
    // Compressed formats stay supported whenever the device has their feature.
    let features = r.device().features();
    for (feature, format) in [
        (
            wgpu::Features::TEXTURE_COMPRESSION_BC,
            wgpu::TextureFormat::Bc7RgbaUnormSrgb,
        ),
        (
            wgpu::Features::TEXTURE_COMPRESSION_ETC2,
            wgpu::TextureFormat::Etc2Rgba8UnormSrgb,
        ),
        (
            wgpu::Features::TEXTURE_COMPRESSION_ASTC,
            wgpu::TextureFormat::Astc {
                block: wgpu::AstcBlock::B4x4,
                channel: wgpu::AstcChannel::UnormSrgb,
            },
        ),
    ] {
        assert_eq!(
            r.supports_texture_format(format),
            features.contains(feature),
            "{format:?}"
        );
    }
    let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    for (format, texel) in [
        (wgpu::TextureFormat::R32Uint, 4),
        (wgpu::TextureFormat::Rgba32Uint, 16),
        (wgpu::TextureFormat::Rgba32Float, 16),
    ] {
        let tex = app.resources.expect_mut::<RenderAssets>().add_texture(TextureSource {
            width: 4,
            height: 4,
            layers: 1,
            format,
            mip_level_count: 1,
            data: vec![0; 16 * texel],
            generate_mipmaps: true,
            sampler: Default::default(),
        });
        let e = app.world.spawn((
            Transform::default(),
            Mesh3d(cube),
            Material::unlit(Vec4::ONE).with_texture(tex),
        ));
        let cam = spawn_camera(&mut app, 3.0);
        let img = render_to_image(&mut app).expect("capture");
        let expected = if format == wgpu::TextureFormat::Rgba32Float && f32_filterable {
            [0, 0, 0, 0]
        } else {
            [255, 255, 255, 255]
        };
        assert_eq!(img.get(16, 16), expected, "{format:?}");
        app.world.despawn(e).unwrap();
        app.world.despawn(cam).unwrap();
    }
}

/// Scenes that stress what downlevel backends do differently (see `tests/gl.rs`, which runs
/// them on GL): arena meshes at `base_vertex > 0`, single-layer textures and render targets
/// sampled as `texture_2d`, mips of texture arrays generated through an array view (layer
/// picked per draw), and `Unorm8x4Bgra` instance colors (emulated with a swizzle). Here they
/// must render correctly on the default backend.
#[test]
fn portability_scenes_render_on_the_default_backend() {
    for (name, scene) in scenes::ALL {
        let Some(mut app) = headless_app(96, 64) else { return };
        let colors = scene(&mut app);
        let img = render_to_image(&mut app).expect("capture");
        for c in colors {
            let n = count_rgb(&img, *c);
            assert!(n >= 16, "{name}: color {c:?} covers {n} px");
        }
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    }
}

/// `Unorm8x4Bgra` instance colors render exactly like the same colors in `Unorm8x4`.
#[test]
fn bgra_instance_colors_match_rgba() {
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    struct Rgba {
        position: [f32; 3],
        rgba: [u8; 4],
    }
    impl InstanceData for Rgba {
        const LAYOUT: &'static InstanceLayout = &InstanceLayout {
            name: "Rgba",
            stride: 16,
            attributes: &[
                InstanceAttribute::new(InstanceSemantic::Translation, wgpu::VertexFormat::Float32x3, 0),
                InstanceAttribute::new(InstanceSemantic::Color, wgpu::VertexFormat::Unorm8x4, 12),
            ],
        };
    }
    let colors = [[200u8, 30, 90, 255], [10, 220, 140, 128], [255, 255, 0, 255]];
    let mut images = Vec::new();
    for bgra in [false, true] {
        let Some(mut app) = headless_app(64, 32) else { return };
        spawn_camera(&mut app, 4.0);
        let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.8));
        let position = |i: usize| [i as f32 - 1.0, 0.0, 0.0];
        let instances = if bgra {
            let data = (0..3)
                .map(|i| {
                    let [r, g, b, a] = colors[i];
                    scenes::BgraInstance {
                        position: position(i),
                        bgra: [b, g, r, a],
                    }
                })
                .collect();
            let buffer = InstanceBuffer::from_vec(data, BufferUsage::Static);
            app.resources.expect_mut::<InstanceBuffers>().insert(buffer)
        } else {
            let data = (0..3)
                .map(|i| Rgba {
                    position: position(i),
                    rgba: colors[i],
                })
                .collect();
            let buffer = InstanceBuffer::from_vec(data, BufferUsage::Static);
            app.resources.expect_mut::<InstanceBuffers>().insert(buffer)
        };
        app.world.spawn((InstancedMesh {
            mesh: cube,
            material: Material::unlit(Vec4::ONE),
            instances,
        },));
        images.push(render_to_image(&mut app).expect("capture"));
    }
    assert_eq!(diff_fraction(&images[0], &images[1], 0), 0.0);
    assert!(
        count_rgb(&images[1], [255, 255, 0]) > 16,
        "the yellow instance is visible"
    );
}

/// Regression: debug builds panicked on every uncaptured wgpu error, including the "buffer is
/// invalid" errors that every use of a lost device produces, so a device loss (driver reset,
/// GPU removed, `destroy`) crashed the next capture or frame. After the device-lost callback
/// those errors are only logged: captures fail and frames are dropped, with no panic or hang.
#[test]
fn a_lost_device_degrades_instead_of_panicking() {
    for frames_before_capture in [3, 0] {
        let Some(mut app) = headless_app(64, 64) else { return };
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
        app.world
            .spawn((Transform::default(), Mesh3d(mesh), Material::unlit(Vec4::ONE)));
        spawn_camera(&mut app, 3.0);
        render_to_image(&mut app).expect("capture");
        let pending = app.resources.expect_mut::<Renderer>().capture();
        app.resources.expect::<Renderer>().device().destroy();
        for _ in 0..frames_before_capture {
            app.run_stage(Stage::Render);
        }
        let r = app.resources.expect_mut::<Renderer>();
        assert!(
            r.capture_blocking().is_err(),
            "{frames_before_capture}: capture of a lost device"
        );
        let _ = pending.block();
        let _ = r.capture().block();
        assert!(render_to_image(&mut app).is_none());
        app.run_stage(Stage::Render);
        // Frames that upload after the loss (moving camera + entities, new mesh and texture
        // every frame) must degrade too, not only static frames.
        for i in 0..4 {
            for t in app.world.query_mut::<&mut Transform>() {
                t.translation.x += 0.25;
            }
            let assets = app.resources.expect_mut::<RenderAssets>();
            let mesh = assets.add_mesh(MeshData::sphere(0.5 + i as f32 * 0.1, 16, 8));
            let tex = assets.add_image(ImageData::checker(64, 8, [255; 4], [0, 0, 0, 255]));
            app.world.spawn((
                Transform::default(),
                Mesh3d(mesh),
                Material::unlit(Vec4::ONE).with_texture(tex),
            ));
            app.step_ticks(1);
        }
        assert!(render_to_image(&mut app).is_none());
    }
}

// ---- viewport cameras ------------------------------------------------------------------------

const RED: [u8; 3] = [255, 0, 0];
const GREEN: [u8; 3] = [0, 255, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const YELLOW: [u8; 3] = [255, 255, 0];
const MAGENTA: [u8; 3] = [255, 0, 255];
const WHITE: [u8; 3] = [255, 255, 255];
const BLACK: [u8; 3] = [0, 0, 0];

/// Linear color of a 0/255-channel color (exact in sRGB).
fn linear(rgb: [u8; 3]) -> Vec4 {
    Vec4::new(rgb[0] as f32, rgb[1] as f32, rgb[2] as f32, 255.0) / 255.0
}

/// Pixels in columns `xs` × rows `ys` whose RGB is exactly `rgb`.
fn count_in(img: &ImageData, xs: std::ops::Range<u32>, ys: std::ops::Range<u32>, rgb: [u8; 3]) -> usize {
    ys.flat_map(|y| xs.clone().map(move |x| (x, y)))
        .filter(|&(x, y)| img.get(x, y)[..3] == rgb)
        .count()
}

/// Unlit ECS cube.
fn ecs_cube(app: &mut App, mesh: MeshHandle, at: Vec3, rgb: [u8; 3]) {
    app.world.spawn((
        Transform::from_xyz(at.x, at.y, at.z),
        Mesh3d(mesh),
        Material::unlit(linear(rgb)),
    ));
}

/// Unlit cube in an explicit instance buffer (GPU-culled on GPU paths).
fn instanced_cube(app: &mut App, mesh: MeshHandle, at: Vec3, rgb: [u8; 3]) {
    let data = vec![InstanceTransform::from_trs(at, Quat::IDENTITY, Vec3::ONE)];
    let instances = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, BufferUsage::Static));
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(linear(rgb)),
        instances,
    },));
}

/// Camera at `at` looking down -Z.
fn camera_at(app: &mut App, at: Vec3, order: i32, viewport: Option<[f32; 4]>, clear: Option<[u8; 3]>) -> Entity {
    app.world.spawn((
        Transform::from_xyz(at.x, at.y, at.z),
        Camera3d {
            order,
            viewport,
            clear_color: clear.map(linear),
            ..Default::default()
        },
    ))
}

/// Headless app with `samples`× MSAA.
fn msaa_app(width: u32, height: u32, samples: u32) -> Option<App> {
    let mut app = headless_app(width, height)?;
    app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
    Some(app)
}

/// Render on every draw path the adapter has: each must equal the `CpuDirect` image, which is
/// returned.
fn render_every_path(app: &mut App) -> ImageData {
    let best = best_path(app);
    set_path(app, DrawPath::CpuDirect);
    let cpu = render_to_image(app).expect("capture");
    for path in paths_up_to(best).into_iter().skip(1) {
        set_path(app, path);
        let img = render_to_image(app).expect("capture");
        let d = diff_fraction(&cpu, &img, 2);
        assert_eq!(d, 0.0, "{path:?}: {:.2}% of pixels differ from CpuDirect", d * 100.0);
    }
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    cpu
}

/// Regression: every camera with a clear color cleared the whole target, so the second camera
/// of a split screen wiped the first one's half. Each half now keeps its own clear color and
/// objects — with and without MSAA, on every draw path.
#[test]
fn split_screen_viewports_keep_each_others_pixels() {
    for samples in [1, 4] {
        let Some(mut app) = msaa_app(128, 64, samples) else {
            return;
        };
        let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(2.0));
        // Each player's camera looks at its own object, far from the other one's.
        ecs_cube(&mut app, cube, Vec3::new(-50.0, 0.0, 0.0), GREEN);
        instanced_cube(&mut app, cube, Vec3::new(50.0, 0.0, 0.0), MAGENTA);
        camera_at(
            &mut app,
            Vec3::new(-50.0, 0.0, 6.0),
            0,
            Some([0.0, 0.0, 0.5, 1.0]),
            Some(RED),
        );
        camera_at(
            &mut app,
            Vec3::new(50.0, 0.0, 6.0),
            1,
            Some([0.5, 0.0, 0.5, 1.0]),
            Some(BLUE),
        );
        let img = render_every_path(&mut app);
        for (half, xs, clear, object, others) in [
            ("left", 0..64, RED, GREEN, [BLUE, MAGENTA]),
            ("right", 64..128, BLUE, MAGENTA, [RED, GREEN]),
        ] {
            let n = |rgb| count_in(&img, xs.clone(), 0..64, rgb);
            assert!(n(clear) > 3000, "{samples}x: {half} clear color covers {} px", n(clear));
            assert!(n(object) > 300, "{samples}x: {half} object covers {} px", n(object));
            for o in others {
                assert_eq!(n(o), 0, "{samples}x: {half} half shows {o:?}");
            }
        }
    }
}

/// A picture-in-picture camera (small viewport, drawn after the main camera) clears and draws
/// only its rectangle; the main camera's image around it survives. Every view starts with a
/// cleared depth buffer and the viewport clear writes no depth: the PiP scene lies deeper
/// (in its own camera's space) than the main scene's objects under the rectangle, yet shows.
#[test]
fn picture_in_picture_keeps_the_main_view() {
    for samples in [1, 4] {
        let Some(mut app) = msaa_app(128, 96, samples) else {
            return;
        };
        let (big, small, huge) = {
            let a = app.resources.expect_mut::<RenderAssets>();
            (
                a.add_mesh(MeshData::cube(3.0)),
                a.add_mesh(MeshData::cube(1.0)),
                a.add_mesh(MeshData::cube(16.0)),
            )
        };
        // Main view: a red cube in the middle and a white one close to the camera, under the
        // PiP.
        ecs_cube(&mut app, big, Vec3::ZERO, RED);
        ecs_cube(&mut app, small, Vec3::new(0.87, 0.62, 4.0), WHITE);
        camera_at(&mut app, Vec3::new(0.0, 0.0, 6.0), 0, None, Some(BLUE));
        // PiP: a green cube 18 units away, top right (x 80..120, y 6..38).
        instanced_cube(&mut app, huge, Vec3::new(50.0, 0.0, -20.0), GREEN);
        camera_at(
            &mut app,
            Vec3::new(50.0, 0.0, 6.0),
            1,
            Some([0.625, 0.0625, 0.3125, 1.0 / 3.0]),
            Some(YELLOW),
        );
        let img = render_every_path(&mut app);
        let inside = |rgb| count_in(&img, 81..119, 7..37, rgb);
        assert!(inside(YELLOW) > 200, "{samples}x: PiP clear: {} px", inside(YELLOW));
        assert!(inside(GREEN) > 200, "{samples}x: PiP object: {} px", inside(GREEN));
        for c in [BLUE, RED, WHITE] {
            assert_eq!(inside(c), 0, "{samples}x: {c:?} inside the PiP");
        }
        let all = |rgb| count_in(&img, 0..128, 0..96, rgb);
        let pip = |rgb| count_in(&img, 80..120, 6..38, rgb);
        let outside = |rgb| all(rgb) - pip(rgb);
        assert!(outside(BLUE) > 4000, "{samples}x: main clear: {} px", outside(BLUE));
        assert!(outside(RED) > 1500, "{samples}x: main red cube: {} px", outside(RED));
        assert!(
            outside(WHITE) > 100,
            "{samples}x: main white cube: {} px",
            outside(WHITE)
        );
        for c in [YELLOW, GREEN] {
            assert_eq!(outside(c), 0, "{samples}x: {c:?} outside the PiP");
        }
    }
}

/// A viewport camera without a clear color draws over what earlier cameras left in its
/// rectangle — here the full-screen camera's image, after another viewport camera (with a
/// clear color) drew the other half.
#[test]
fn viewport_without_clear_color_draws_over_earlier_cameras() {
    for samples in [1, 4] {
        let Some(mut app) = msaa_app(128, 64, samples) else {
            return;
        };
        let (cube, small) = {
            let a = app.resources.expect_mut::<RenderAssets>();
            (a.add_mesh(MeshData::cube(2.0)), a.add_mesh(MeshData::cube(1.5)))
        };
        // Full screen: blue, with a red cube in the right half (x 75..89, y 11..25).
        ecs_cube(&mut app, small, Vec3::new(2.0, 1.5, 0.0), RED);
        camera_at(&mut app, Vec3::new(0.0, 0.0, 6.0), 0, None, Some(BLUE));
        // Left half: yellow, with a green cube.
        ecs_cube(&mut app, cube, Vec3::new(-50.0, 0.0, 0.0), GREEN);
        camera_at(
            &mut app,
            Vec3::new(-50.0, 0.0, 6.0),
            1,
            Some([0.0, 0.0, 0.5, 1.0]),
            Some(YELLOW),
        );
        // Right half, no clear color: a magenta cube below the red one (y 39..53).
        instanced_cube(&mut app, small, Vec3::new(50.0, -1.5, 0.0), MAGENTA);
        camera_at(&mut app, Vec3::new(50.0, 0.0, 6.0), 2, Some([0.5, 0.0, 0.5, 1.0]), None);
        let img = render_every_path(&mut app);
        let left = |rgb| count_in(&img, 0..64, 0..64, rgb);
        let right = |rgb| count_in(&img, 64..128, 0..64, rgb);
        assert!(
            left(YELLOW) > 3000 && left(GREEN) > 300,
            "{samples}x: {} {}",
            left(YELLOW),
            left(GREEN)
        );
        for c in [BLUE, RED, MAGENTA] {
            assert_eq!(left(c), 0, "{samples}x: {c:?} in the left half");
        }
        assert!(
            right(BLUE) > 3000,
            "{samples}x: earlier camera's clear color kept: {} px",
            right(BLUE)
        );
        assert!(
            right(RED) > 100,
            "{samples}x: earlier camera's object kept: {} px",
            right(RED)
        );
        assert!(
            right(MAGENTA) > 100,
            "{samples}x: overlay object: {} px",
            right(MAGENTA)
        );
        for c in [YELLOW, GREEN] {
            assert_eq!(right(c), 0, "{samples}x: {c:?} in the right half");
        }
    }
}

/// The first camera drawing into a target in a frame starts from a cleared target: with its
/// clear color where it covers the target, otherwise black — never the previous frame's
/// content, and never its clear color outside its viewport. With MSAA the whole target is
/// resolved, not only the viewport (wgpu's GL backend clips its resolve blit to the scissor
/// rect).
#[test]
fn the_first_view_of_a_frame_clears_its_target() {
    for samples in [1, 4] {
        let Some(mut app) = msaa_app(128, 64, samples) else {
            return;
        };
        let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(2.0));
        ecs_cube(&mut app, cube, Vec3::new(-50.0, 0.0, 0.0), GREEN);
        let full = camera_at(&mut app, Vec3::new(0.0, 0.0, 6.0), 0, None, Some(WHITE));
        let img = render_every_path(&mut app);
        assert_eq!(count_in(&img, 0..128, 0..64, WHITE), 128 * 64);

        app.world.get::<&mut Camera3d>(full).unwrap().active = false;
        let left = camera_at(
            &mut app,
            Vec3::new(-50.0, 0.0, 6.0),
            0,
            Some([0.0, 0.0, 0.5, 1.0]),
            Some(RED),
        );
        let img = render_every_path(&mut app);
        assert!(count_in(&img, 0..64, 0..64, RED) > 3000, "{samples}x");
        assert!(count_in(&img, 0..64, 0..64, GREEN) > 300, "{samples}x");
        assert_eq!(
            count_in(&img, 64..128, 0..64, BLACK),
            64 * 64,
            "{samples}x: outside the viewport"
        );

        app.world.get::<&mut Camera3d>(left).unwrap().clear_color = None;
        let img = render_every_path(&mut app);
        let n = |xs, rgb| count_in(&img, xs, 0..64, rgb);
        assert!(n(0..64, GREEN) > 300, "{samples}x");
        assert_eq!(n(0..128, RED) + n(0..128, WHITE), 0, "{samples}x: no stale content");
        assert_eq!(n(64..128, BLACK), 64 * 64, "{samples}x: no clear color: black");
        if samples == 1 {
            assert_eq!(n(0..64, BLACK) + n(0..64, GREEN), 64 * 64, "no clear color: black");
        }
    }
}

/// Viewports work the same in render targets: two cameras fill the halves of one target,
/// which a monitor in the main view shows.
#[test]
fn viewports_share_a_render_target() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let (cube, rt) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(1.0)), a.add_render_target(32, 32))
    };
    app.world.spawn((
        Transform::default(),
        Mesh3d(cube),
        Material::unlit(Vec4::ONE).with_texture(rt),
    ));
    for (x, order, clear) in [(0.0, -2, RED), (0.5, -1, GREEN)] {
        app.world.spawn((
            Transform::from_xyz(1000.0, 0.0, 0.0),
            Camera3d {
                order,
                target: RenderTarget::Texture(rt),
                viewport: Some([x, 0.0, 0.5, 1.0]),
                clear_color: Some(linear(clear)),
                ..Default::default()
            },
        ));
    }
    camera_at(&mut app, Vec3::new(0.0, 0.0, 2.5), 0, None, Some(BLUE));
    let img = render_every_path(&mut app);
    let n = |rgb| count_in(&img, 0..64, 0..64, rgb);
    assert!(n(RED) > 150 && n(GREEN) > 150, "red {} green {}", n(RED), n(GREEN));
}

/// The first-pass rule holds for render targets too: a render target whose only camera has a
/// viewport starts the frame black outside it — not with last frame's content (here a
/// full-target red camera's). A monitor filling the main view shows the target.
#[test]
fn the_first_view_of_a_frame_clears_a_render_target() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let (cube, rt) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(3.4)), a.add_render_target(32, 32))
    };
    app.world.spawn((
        Transform::default(),
        Mesh3d(cube),
        Material::unlit(Vec4::ONE).with_texture(rt),
    ));
    camera_at(&mut app, Vec3::new(0.0, 0.0, 3.0), 0, None, Some(BLUE));
    let rt_camera = |app: &mut App, order, viewport, clear| {
        app.world.spawn((
            Transform::from_xyz(100.0, 0.0, 5.0),
            Camera3d {
                order,
                target: RenderTarget::Texture(rt),
                viewport,
                clear_color: Some(linear(clear)),
                ..Default::default()
            },
        ))
    };
    let full = rt_camera(&mut app, -2, None, RED);
    let img = render_every_path(&mut app);
    assert!(count_in(&img, 0..64, 0..64, RED) > 2000, "the monitor shows the target");

    app.world.get::<&mut Camera3d>(full).unwrap().active = false;
    rt_camera(&mut app, -1, Some([0.0, 0.0, 0.5, 1.0]), GREEN);
    let img = render_every_path(&mut app);
    let n = |rgb| count_in(&img, 0..64, 0..64, rgb);
    assert_eq!(n(RED), 0, "last frame's content in the render target");
    assert!(
        n(GREEN) > 500 && n(BLACK) > 500,
        "green {} black {}",
        n(GREEN),
        n(BLACK)
    );
}

/// A viewport thinner than a pixel keeps the pixel holding its center; at the right or bottom
/// edge of the target that pixel is the last column or row (growing the far edge to one pixel
/// used to push it just outside the target, where nothing was drawn).
#[test]
fn viewports_thinner_than_a_pixel_keep_one_pixel_of_the_target() {
    for samples in [1, 4] {
        for (viewport, xs, ys) in [
            ([0.999, 0.0, 0.001, 1.0], 96..97, 0..61),
            ([0.0, 0.999, 1.0, 0.001], 0..97, 60..61),
            ([0.0, 0.0, 0.001, 1.0], 0..1, 0..61),
        ] {
            let Some(mut app) = msaa_app(97, 61, samples) else {
                return;
            };
            camera_at(&mut app, Vec3::new(0.0, 0.0, 5.0), 0, None, Some(BLUE));
            camera_at(&mut app, Vec3::new(0.0, 0.0, 5.0), 1, Some(viewport), Some(RED));
            let img = render_every_path(&mut app);
            let line = xs.len() * ys.len();
            assert_eq!(count_in(&img, xs, ys, RED), line, "{viewport:?} ×{samples}");
            assert_eq!(count_in(&img, 0..97, 0..61, RED), line, "{viewport:?} ×{samples}");
        }
    }
}
