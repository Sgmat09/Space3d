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
