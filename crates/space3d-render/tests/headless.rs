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
