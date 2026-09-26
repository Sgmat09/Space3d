//! Frame modes (graphics spec §1.2–§1.3, milestone F4): Direct by default, HDR scene textures
//! with the composite (exposure, tonemapping, dither, alpha), the Ldr fallback, render scale,
//! post groups over several cameras, HDR and multisampled render targets.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;
use space3d_test::{CompareOptions, compare_images};

const W: u32 = 64;
const H: u32 = 32;

/// An unlit, camera-facing quad of `size` world units (linear `color`, alpha 1) at `pos`.
fn quad(app: &mut App, color: Vec3, pos: Vec3, size: f32) -> Entity {
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(size));
    app.world.spawn((
        Transform::from_xyz(pos.x, pos.y, pos.z).with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
        Mesh3d(mesh),
        Material::unlit(color.extend(1.0)),
    ))
}

/// Orthographic camera at z = 5 looking down -Z (2 world units high).
fn camera(app: &mut App, clear: Vec4) -> Entity {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default()
            .with_clear_color(clear)
            .with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 100.0,
            }),
    ))
}

/// A lit scene (cube on a plane, sun, ambient) and its perspective camera.
fn lit_scene(app: &mut App) -> Entity {
    let (cube, plane) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(1.2)), a.add_mesh(MeshData::plane(8.0)))
    };
    app.world.spawn((
        Transform::from_xyz(0.0, 0.1, 0.0).with_rotation(Quat::from_rotation_y(0.5)),
        Mesh3d(cube),
        Material::srgb(0.8, 0.3, 0.1).with_metallic_roughness(0.0, 0.5),
    ));
    app.world.spawn((
        Transform::from_xyz(0.0, -0.5, 0.0),
        Mesh3d(plane),
        Material::srgb(0.35, 0.37, 0.4),
    ));
    spawn_sun(app);
    app.insert_resource(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.08,
    });
    app.world.spawn((
        Transform::from_xyz(2.4, 1.9, 3.2).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.01, 0.012, 0.02, 1.0)),
    ))
}

fn render(app: &mut App) -> ImageData {
    let img = render_to_image(app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    img
}

fn max_diff(a: &ImageData, b: &ImageData) -> u8 {
    a.pixels
        .iter()
        .zip(&b.pixels)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

fn srgb8(linear: f32) -> u8 {
    (linear_to_srgb(linear.clamp(0.0, 1.0)) * 255.0).round() as u8
}

fn frame_settings(app: &mut App) -> &mut FrameSettings {
    app.resources.expect_mut::<FrameSettings>()
}

fn no_dither() -> PostProcess {
    PostProcess {
        dither: false,
        ..PostProcess::default()
    }
}

/// No `PostProcess` (or a disabled one, or post switched off globally) is Direct mode: no
/// scene texture, no extra pass, the very same pixels (the engine goldens hold exactly:
/// `space3d-stories` `engine_goldens_are_exact`).
#[test]
fn defaults_are_direct_and_exact() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = lit_scene(&mut app);
    let direct = render(&mut app);
    let s = stats(&app);
    assert_eq!(
        (
            s.intermediate_groups,
            s.post_groups,
            s.render_passes,
            s.textures_created,
            s.target_bytes
        ),
        (0, 0, 0, 0, 0)
    );
    app.world
        .insert_one(
            cam,
            PostProcess {
                enabled: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(render(&mut app), direct, "disabled PostProcess");
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    frame_settings(&mut app).post = false;
    assert_eq!(render(&mut app), direct, "FrameSettings::post = false");
    assert_eq!(stats(&app).intermediate_groups, 0);
    frame_settings(&mut app).post = true;
    render(&mut app);
    assert_eq!((stats(&app).intermediate_groups, stats(&app).post_groups), (1, 1));
}

/// `PostProcess::passthrough()` (tonemapper None, no effects, no dither) through the HDR path
/// reproduces Direct mode within 1/255.
#[test]
fn passthrough_postprocess_matches_direct() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = lit_scene(&mut app);
    let direct = render(&mut app);
    app.world.insert_one(cam, PostProcess::passthrough()).unwrap();
    let hdr = render(&mut app);
    let s = stats(&app);
    assert_eq!((s.intermediate_groups, s.post_groups, s.post_passes), (1, 1, 1));
    assert!(max_diff(&direct, &hdr) <= 1, "max diff {}", max_diff(&direct, &hdr));
}

/// Unlit quads 0.05…64 through AgX and ACES: the displayed value never decreases with the
/// radiance, rises strictly below 1.0 and stays below white there.
#[test]
fn agx_and_aces_are_monotonic_below_one() {
    const N: usize = 16;
    let Some(mut app) = headless_app(8 * N as u32, 8) else {
        return;
    };
    let values: Vec<f32> = (0..N)
        .map(|i| 0.05 * (64.0f32 / 0.05).powf(i as f32 / (N - 1) as f32))
        .collect();
    // A 16:1 strip of quads, 0.125 units (8 pixels) apart, under an orthographic camera 0.125
    // units high.
    for (i, v) in values.iter().enumerate() {
        quad(
            &mut app,
            Vec3::splat(*v),
            Vec3::new(-1.0 + 0.0625 + i as f32 * 0.125, 0.0, 0.0),
            0.12,
        );
    }
    let cam = app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Orthographic {
                height: 0.125,
                near: 0.1,
                far: 100.0,
            }),
    ));
    for tonemapper in [Tonemapper::AgX, Tonemapper::AcesFitted] {
        app.world
            .insert_one(
                cam,
                PostProcess {
                    tonemapper,
                    ..no_dither()
                },
            )
            .unwrap();
        let img = render(&mut app);
        let px: Vec<u8> = (0..N as u32).map(|i| img.get(i * 8 + 4, 4)[0]).collect();
        for i in 1..N {
            assert!(px[i] >= px[i - 1], "{tonemapper:?} not monotonic: {px:?}");
        }
        let below_one = values_below_one();
        for i in 1..below_one {
            assert!(px[i] > px[i - 1], "{tonemapper:?} flat below 1.0: {px:?}");
            assert!(px[i] < 255, "{tonemapper:?} white below 1.0: {px:?}");
        }
    }
}

/// How many of the ramp values are below 1.0 (0.05 · 1280^(i/15)).
fn values_below_one() -> usize {
    (0..16)
        .filter(|i| 0.05 * (64.0f32 / 0.05).powf(*i as f32 / 15.0) < 1.0)
        .count()
}

/// One stop of exposure doubles the radiance: radiance x at +1 stop displays like 2x at 0.
#[test]
fn exposure_stops_are_linear() {
    let Some(mut app) = headless_app(W, H) else { return };
    let q = quad(&mut app, Vec3::splat(0.1), Vec3::ZERO, 4.0);
    let cam = camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    for tonemapper in [Tonemapper::None, Tonemapper::AgX, Tonemapper::Reinhard] {
        for (x, stops) in [(0.1f32, 1.0f32), (0.3, 2.0), (2.0, -1.0)] {
            let post = PostProcess {
                tonemapper,
                ..no_dither()
            };
            app.world
                .insert_one(q, Material::unlit(Vec3::splat(x).extend(1.0)))
                .unwrap();
            app.world.insert_one(cam, post.with_exposure(stops)).unwrap();
            let exposed = render(&mut app).get(W / 2, H / 2);
            app.world
                .insert_one(q, Material::unlit(Vec3::splat(x * stops.exp2()).extend(1.0)))
                .unwrap();
            app.world.insert_one(cam, post).unwrap();
            let scaled = render(&mut app).get(W / 2, H / 2);
            assert!(
                exposed.iter().zip(scaled).all(|(a, b)| a.abs_diff(b) <= 1),
                "{tonemapper:?}: {x} at {stops:+} stops = {exposed:?}, {} = {scaled:?}",
                x * stops.exp2()
            );
            if tonemapper == Tonemapper::None {
                assert!(exposed[0].abs_diff(srgb8(x * stops.exp2())) <= 1, "{exposed:?}");
            }
        }
    }
}

/// Without float targets (forced) post cameras use the Ldr fallback: an LDR scene texture,
/// exposure and tonemapping per fragment and for the clear color on the CPU. It looks like
/// the HDR path.
#[test]
fn ldr_fallback_matches_hdr_perceptually() {
    let Some(mut app) = headless_app(96, 64) else { return };
    let cam = lit_scene(&mut app);
    // Bloom is off in the Ldr fallback by design: compare the rest.
    app.world
        .insert_one(cam, PostProcess::default().without_bloom().with_exposure(1.0))
        .unwrap();
    let hdr = render(&mut app);
    let s = stats(&app);
    let hdr_bytes = s.target_bytes;
    assert_eq!(s.intermediate_groups, 1);
    frame_settings(&mut app).downlevel.no_float_targets = true;
    let ldr = render(&mut app);
    assert!(stats(&app).target_bytes < hdr_bytes, "the LDR scene texture is smaller");
    let cmp = compare_images(&hdr, &ldr, &CompareOptions::perceptual());
    assert!(cmp.passed, "{}", cmp.summary());
    // `HdrMode::Ldr` picks the same path on any device.
    frame_settings(&mut app).downlevel.no_float_targets = false;
    app.world
        .insert_one(cam, CameraFrame::default().with_hdr(HdrMode::Ldr))
        .unwrap();
    assert_eq!(render(&mut app), ldr);
}

/// A render target cleared with alpha 0 keeps alpha 0 through post: the composite
/// un-premultiplies before tonemapping and re-premultiplies after, so dither (and later bloom
/// and grain) is weighted by alpha — no haze where nothing was drawn.
#[test]
fn rt_transparent_clear_keeps_alpha() {
    let Some(mut app) = headless_app(8, 8) else { return };
    let rt = app.resources.expect_mut::<RenderAssets>().add_render_target(32, 32);
    quad(&mut app, Vec3::new(0.5, 0.25, 1.0), Vec3::ZERO, 1.0);
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default()
            .with_clear_color(Vec4::ZERO)
            .with_target(RenderTarget::Texture(rt))
            .with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 100.0,
            }),
        PostProcess::default(),
    ));
    camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    render(&mut app);
    render(&mut app);
    assert_eq!(stats(&app).post_groups, 1);
    let texture = {
        let assets = app.resources.expect::<RenderAssets>();
        assets.render_target_view(rt).unwrap().texture().clone()
    };
    let px = read_texture(app.resources.expect::<Renderer>(), &texture);
    let at = |x: usize, y: usize| px[y * 32 + x];
    for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31), (3, 16)] {
        assert_eq!(at(x, y), [0.0; 4], "transparent at ({x}, {y})");
    }
    let center = at(16, 16);
    assert_eq!(center[3], 1.0);
    let agx = Tonemapper::AgX.apply(Vec3::new(0.5, 0.25, 1.0));
    for c in 0..3 {
        assert!((center[c] - agx[c]).abs() < 0.02, "{center:?} vs AgX {agx}");
    }
}

/// Compact HDR (`Rg11b10Ufloat`, 4 bytes per pixel, no alpha) only for an opaque-cleared
/// primary target: render targets and transparent clears keep `Rgba16Float`.
#[test]
fn compact_hdr_only_on_opaque_primary() {
    let Some(mut app) = headless_app(W, H) else { return };
    let compact = app.resources.expect::<Renderer>().graphics_caps().compact_hdr_format;
    quad(&mut app, Vec3::splat(2.0), Vec3::ZERO, 1.0);
    let cam = camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    // The scene texture alone (no bloom chain).
    app.world
        .insert_one(cam, PostProcess::default().without_bloom())
        .unwrap();
    let bytes = |app: &mut App| {
        render(app);
        stats(app).target_bytes
    };
    let (px, depth) = ((W * H) as u64, (W * H * 4) as u64);
    assert_eq!(bytes(&mut app), px * 8 + depth, "Auto: Rgba16Float");
    app.world
        .insert_one(cam, CameraFrame::default().with_hdr(HdrMode::Compact))
        .unwrap();
    let compact_bytes = if compact.is_some() { px * 4 } else { px * 8 };
    assert_eq!(
        bytes(&mut app),
        compact_bytes + depth,
        "Compact on an opaque primary target"
    );
    let hdr = app
        .resources
        .expect_mut::<Renderer>()
        .capture_scene_hdr_blocking()
        .unwrap();
    assert!(
        (hdr.get(W / 2, H / 2)[0] - 2.0).abs() < 0.02,
        "{:?}",
        hdr.get(W / 2, H / 2)
    );
    // A transparent clear needs alpha.
    app.world.get::<&mut Camera3d>(cam).unwrap().clear_color = Some(Vec4::new(0.0, 0.0, 0.0, 0.5));
    assert_eq!(bytes(&mut app), px * 8 + depth, "transparent clear");
    // A render target is never compact.
    let rt = app.resources.expect_mut::<RenderAssets>().add_render_target(W, H);
    let mut c = *app.world.get::<&Camera3d>(cam).unwrap();
    c.clear_color = Some(Vec4::new(0.0, 0.0, 0.0, 1.0));
    c.target = RenderTarget::Texture(rt);
    app.world.insert_one(cam, c).unwrap();
    render(&mut app);
    assert_eq!(bytes(&mut app), px * 8 + depth, "render target");
}

/// Two post cameras side by side are two post groups: each composites only its half, with
/// its own settings, and its bilinear taps (render scale 0.5) never reach into the other half.
#[test]
fn split_screen_post_groups_do_not_bleed() {
    let Some(mut app) = headless_app(W, H) else { return };
    quad(&mut app, Vec3::ONE, Vec3::new(-50.0, 0.0, 0.0), 20.0);
    for (i, x) in [-50.0f32, 50.0].into_iter().enumerate() {
        app.world.spawn((
            Transform::from_xyz(x, 0.0, 5.0),
            Camera3d {
                viewport: Some([0.5 * i as f32, 0.0, 0.5, 1.0]),
                clear_color: Some(Vec4::new(0.0, 0.0, 0.0, 1.0)),
                order: i as i32,
                projection: Projection::Orthographic {
                    height: 2.0,
                    near: 0.1,
                    far: 100.0,
                },
                ..Default::default()
            },
            PostProcess {
                tonemapper: Tonemapper::None,
                ..no_dither()
            }
            .with_exposure(-(i as f32)),
        ));
    }
    frame_settings(&mut app).render_scale = 0.5;
    let img = render(&mut app);
    assert_eq!(stats(&app).post_groups, 2);
    for y in 0..H {
        for x in 0..W {
            let p = img.get(x, y);
            if x < W / 2 {
                assert_eq!(p, [255, 255, 255, 255], "left half at ({x}, {y})");
            } else {
                assert_eq!(p, [0, 0, 0, 255], "right half at ({x}, {y}): bled");
            }
        }
    }
}

/// A later camera without post on a post camera's rect (gizmos, 3D HUD) is a display view:
/// drawn after the composite, LDR, not tonemapped.
#[test]
fn gizmo_camera_is_display_view() {
    let Some(mut app) = headless_app(W, H) else { return };
    quad(&mut app, Vec3::splat(0.5), Vec3::ZERO, 1.0);
    let cam = camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    app.world
        .insert_one(cam, PostProcess::default().without_bloom().with_exposure(2.0))
        .unwrap();
    let tonemapped = render(&mut app).get(W / 2, H / 2);
    let agx = Tonemapper::AgX.apply(Vec3::splat(2.0));
    assert!(tonemapped[0].abs_diff(srgb8(agx.x)) <= 2, "{tonemapped:?}");
    let gizmo = camera(&mut app, Vec4::ZERO);
    {
        let mut c = app.world.get::<&mut Camera3d>(gizmo).unwrap();
        c.clear_color = None;
        c.order = 10;
    }
    let img = render(&mut app);
    assert_eq!(img.get(W / 2, H / 2), [188, 188, 188, 255], "unlit 0.5, not tonemapped");
    assert_eq!(img.get(1, 1), [0, 0, 0, 255], "outside the quad: the composite");
    assert_eq!((stats(&app).views, stats(&app).post_groups), (2, 1));
}

/// A picture-in-picture post camera over a full-screen post camera: the full group composites
/// first, the PiP group overwrites its rect with its own settings.
#[test]
fn pip_post_camera_overwrites_rect() {
    let Some(mut app) = headless_app(W, H) else { return };
    quad(&mut app, Vec3::splat(0.25), Vec3::ZERO, 8.0);
    let post = PostProcess {
        tonemapper: Tonemapper::None,
        ..no_dither()
    };
    let full = camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    app.world.insert_one(full, post).unwrap();
    let pip = camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    {
        let mut c = app.world.get::<&mut Camera3d>(pip).unwrap();
        c.order = 1;
        c.viewport = Some([0.5, 0.5, 0.5, 0.5]);
    }
    app.world.insert_one(pip, post.with_exposure(1.0)).unwrap();
    let img = render(&mut app);
    assert_eq!(stats(&app).post_groups, 2);
    assert_eq!(img.get(8, 8)[0], srgb8(0.25), "full camera");
    assert_eq!(img.get(W - 8, H - 4)[0], srgb8(0.5), "PiP camera (+1 stop)");
}

/// Render scale 0.5 without post: the Ldr passthrough group renders into a half-size scene
/// texture and the composite upscales it to the full target.
#[test]
fn render_scale_half_upscales() {
    let Some(mut app) = headless_app(W, H) else { return };
    quad(&mut app, Vec3::new(0.8, 0.4, 0.1), Vec3::ZERO, 1.0);
    camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let direct = render(&mut app);
    frame_settings(&mut app).render_scale = 0.5;
    let scaled = render(&mut app);
    let s = stats(&app);
    assert_eq!((s.intermediate_groups, s.post_groups, s.post_passes), (1, 0, 1));
    assert_eq!((scaled.width, scaled.height), (W, H));
    let scene = app
        .resources
        .expect_mut::<Renderer>()
        .capture_scene_hdr_blocking()
        .unwrap();
    assert_eq!((scene.width, scene.height), (W / 2, H / 2));
    // Inside the quad the color is exact; only its edges differ (upscaled).
    assert_eq!(scaled.get(W / 2, H / 2), direct.get(W / 2, H / 2));
    assert!(diff_fraction(&direct, &scaled, 2) < 0.2);
}

/// `RenderTargetDesc::hdr()` targets store linear radiance unclamped (Direct mode into
/// `GraphicsCaps::hdr_format`); post on their cameras is ignored.
#[test]
fn hdr_render_target_is_linear_float() {
    let Some(mut app) = headless_app(8, 8) else { return };
    let rt = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_render_target_with(16, 16, RenderTargetDesc::hdr());
    quad(&mut app, Vec3::new(4.0, 0.5, 0.0), Vec3::ZERO, 8.0);
    let cam = camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    app.world.insert_one(cam, CameraFrame::default()).unwrap();
    app.world.get::<&mut Camera3d>(cam).unwrap().target = RenderTarget::Texture(rt);
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
    render(&mut app);
    render(&mut app);
    let hdr_format = app.resources.expect::<Renderer>().graphics_caps().hdr_format;
    let view = app.resources.expect::<RenderAssets>().render_target_view(rt).unwrap();
    let texture = view.texture().clone();
    let px = read_texture(app.resources.expect::<Renderer>(), &texture);
    match hdr_format {
        Some(f) => {
            assert_eq!(texture.format(), f);
            assert_eq!(px[8 * 16 + 8], [4.0, 0.5, 0.0, 1.0], "linear, unclamped");
            assert_eq!(stats(&app).post_groups, 0, "post ignored on HDR targets");
        }
        None => assert_eq!(texture.format(), wgpu::TextureFormat::Rgba8UnormSrgb),
    }
}

/// `RenderTargetDesc::samples` multisamples the cameras rendering into a render target:
/// edges get intermediate coverage.
#[test]
fn rt_msaa_resolves() {
    let Some(mut app) = headless_app(8, 8) else { return };
    let mut edge_pixels = Vec::new();
    for samples in [1, 4] {
        let rt = app.resources.expect_mut::<RenderAssets>().add_render_target_with(
            32,
            32,
            RenderTargetDesc::default().with_samples(samples),
        );
        let q = quad(&mut app, Vec3::ONE, Vec3::ZERO, 1.0);
        {
            let mut t = app.world.get::<&mut Transform>(q).unwrap();
            t.rotation = Quat::from_rotation_z(0.4) * t.rotation;
        }
        let cam = camera(&mut app, Vec4::new(0.0, 0.0, 0.0, 1.0));
        app.world.get::<&mut Camera3d>(cam).unwrap().target = RenderTarget::Texture(rt);
        render(&mut app);
        render(&mut app);
        let texture = app
            .resources
            .expect::<RenderAssets>()
            .render_target_view(rt)
            .unwrap()
            .texture()
            .clone();
        let px = read_texture(app.resources.expect::<Renderer>(), &texture);
        edge_pixels.push(px.iter().filter(|p| p[0] > 0.02 && p[0] < 0.98).count());
        app.world.despawn(q).unwrap();
        app.world.despawn(cam).unwrap();
    }
    assert_eq!(edge_pixels[0], 0, "1 sample: hard edges");
    let max_msaa = app.resources.expect::<Renderer>().capabilities().max_msaa;
    if max_msaa > 1 {
        assert!(edge_pixels[1] > 20, "4 samples: antialiased edges ({})", edge_pixels[1]);
    }
}

/// Rendering the same state twice gives the same image, dither included (its seed is
/// `Time::frame`, which `render_to_image` does not advance).
#[test]
fn render_to_image_twice_identical() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = lit_scene(&mut app);
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    let a = render(&mut app);
    let b = render(&mut app);
    assert_eq!(a, b);
    // A new frame reseeds the dither: each image is within ±1 of the undithered one.
    app.update(std::time::Duration::from_millis(16));
    let c = render(&mut app);
    assert_ne!(a, c, "reseeded");
    assert!(max_diff(&a, &c) <= 2);
}

/// A resize reallocates the scene texture at the next frame's start, once: a capture in between
/// still returns the last frame.
#[test]
fn resize_is_lazy() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = lit_scene(&mut app);
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    let before = render(&mut app);
    render(&mut app);
    assert_eq!(stats(&app).textures_created, 0);
    app.resources.expect_mut::<Renderer>().resize(96, 48);
    let captured = app.resources.expect_mut::<Renderer>().capture_blocking().unwrap();
    assert_eq!(captured, before, "the last frame, at its size");
    let hdr = app
        .resources
        .expect_mut::<Renderer>()
        .capture_scene_hdr_blocking()
        .unwrap();
    assert_eq!((hdr.width, hdr.height), (W, H));
    let after = render(&mut app);
    assert_eq!((after.width, after.height), (96, 48));
    assert!(stats(&app).textures_created > 0, "reallocated once");
    render(&mut app);
    assert_eq!(stats(&app).textures_created, 0);
}

/// After warm-up no mode reallocates CPU scratch or creates textures, and a still scene
/// uploads nothing (when frames advance, the dither seeds of 64 frames go up at once).
#[test]
fn steady_state_in_every_mode() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = lit_scene(&mut app);
    let modes: [(&str, Option<PostProcess>, Option<CameraFrame>, f32); 4] = [
        ("hdr", Some(no_dither()), None, 1.0),
        (
            "ldr",
            Some(no_dither()),
            Some(CameraFrame::default().with_hdr(HdrMode::Ldr)),
            1.0,
        ),
        ("passthrough", None, None, 0.75),
        ("hdr + dither", Some(PostProcess::default()), None, 1.0),
    ];
    for (name, post, frame, scale) in modes {
        match post {
            Some(p) => app.world.insert_one(cam, p).unwrap(),
            None => {
                let _ = app.world.remove_one::<PostProcess>(cam);
            }
        }
        match frame {
            Some(f) => app.world.insert_one(cam, f).unwrap(),
            None => {
                let _ = app.world.remove_one::<CameraFrame>(cam);
            }
        }
        frame_settings(&mut app).render_scale = scale;
        for _ in 0..3 {
            app.update(std::time::Duration::from_millis(16));
        }
        for _ in 0..5 {
            app.update(std::time::Duration::from_millis(16));
            let s = stats(&app);
            assert_eq!(s.intermediate_groups, 1, "{name}");
            assert_eq!((s.cpu_reallocations, s.textures_created), (0, 0), "{name}: {s:?}");
            let dither = post.is_some_and(|p| p.dither);
            let limit = if dither { 16 } else { 0 };
            assert!(s.bytes_uploaded <= limit, "{name}: {} bytes uploaded", s.bytes_uploaded);
        }
    }
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}

/// `DownlevelOverride::webgl2()` on any device (no float targets, no compute, no read-only
/// depth) with the CPU draw path: the WebGL2-class path renders a post camera like the default
/// path (perceptually).
#[test]
fn webgl2_override_parity() {
    let Some(mut app) = headless_app(96, 64) else { return };
    let cam = lit_scene(&mut app);
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    let default = render(&mut app);
    frame_settings(&mut app).downlevel = DownlevelOverride::webgl2();
    set_path(&mut app, DrawPath::CpuDirect);
    let webgl2 = render(&mut app);
    assert_eq!(stats(&app).intermediate_groups, 1);
    let cmp = compare_images(&default, &webgl2, &CompareOptions::perceptual());
    assert!(cmp.passed, "{}", cmp.summary());
}

/// Where the Ldr fallback (`DownlevelOverride::webgl2()`) differs from the HDR path by design
/// (graphics spec §1.2, docs/graphics/frame.md "The Ldr fallback"), and that it differs nowhere
/// else:
///
/// * **MSAA:** each sample is tonemapped before the resolve (HDR resolves radiance, then
///   tonemaps), so silhouettes between bright and dark surfaces resolve differently. Every
///   pixel off by more than 8/255 is a silhouette pixel: one where the HDR image at 4× differs
///   from the HDR image at 1×.
/// * **Blending:** translucent surfaces blend tonemapped colors (HDR blends radiance). Every
///   pixel off by more than 8/255 is covered by the translucent quad.
/// * **Split screens and a gizmo display view** combine both, per post group.
///
/// Opaque single-sample scenes match within `CompareOptions::perceptual()`
/// ([`webgl2_override_parity`]); these stay within the looser bound documented in frame.md.
#[test]
fn webgl2_override_parity_msaa_and_blending() {
    const PW: u32 = 160;
    const PH: u32 = 96;
    let bound = CompareOptions::perceptual().max_diff_fraction(0.08);
    // (name, samples, translucent quad, split screen with a gizmo display view)
    let cases = [
        ("msaa4", 4, false, false),
        ("blended", 1, true, false),
        ("split + gizmo", 1, false, true),
        ("everything", 4, true, true),
    ];
    for (name, samples, blended, split) in cases {
        let Some(mut app) = headless_app(PW, PH) else { return };
        let cam = lit_scene(&mut app);
        // Bloom is off in the Ldr fallback by design: compare the rest.
        app.world
            .insert_one(cam, PostProcess::default().without_bloom())
            .unwrap();
        let translucent = blended.then(|| {
            let mesh = app
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::plane(1.4));
            let mut material = Material::srgb(0.95, 0.9, 0.2).with_emissive(Vec3::splat(1.5));
            material.base_color.w = 0.45;
            material.alpha_blend = true;
            app.world.spawn((
                Transform::from_xyz(0.4, 0.3, 1.0).with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
                Mesh3d(mesh),
                material,
            ))
        });
        if split {
            app.world.get::<&mut Camera3d>(cam).unwrap().viewport = Some([0.0, 0.0, 0.5, 1.0]);
            // A second post camera on the right half (darker, another clear color), and a
            // gizmo camera without post over the left half.
            app.world.spawn((
                Transform::from_xyz(-2.8, 1.6, 2.6).looking_at(Vec3::ZERO, Vec3::Y),
                Camera3d {
                    viewport: Some([0.5, 0.0, 0.5, 1.0]),
                    order: 1,
                    clear_color: Some(Vec4::new(0.3, 0.2, 0.1, 1.0)),
                    ..Default::default()
                },
                PostProcess::default().without_bloom().with_exposure(-0.5),
            ));
            let gizmo = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.3));
            app.world.spawn((
                Transform::from_xyz(-0.9, 0.9, 0.0),
                Mesh3d(gizmo),
                Material::unlit(Vec4::new(0.0, 1.0, 0.0, 1.0)),
            ));
            app.world.spawn((
                Transform::from_xyz(2.4, 1.9, 3.2).looking_at(Vec3::ZERO, Vec3::Y),
                Camera3d {
                    viewport: Some([0.0, 0.0, 0.5, 1.0]),
                    order: 2,
                    clear_color: None,
                    ..Default::default()
                },
            ));
        }
        let mut render_at = |samples: u32, downlevel: DownlevelOverride| {
            app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
            frame_settings(&mut app).downlevel = downlevel;
            let webgl2 = downlevel != DownlevelOverride::default();
            let path = if webgl2 { DrawPath::CpuDirect } else { best_path(&app) };
            set_path(&mut app, path);
            let img = render(&mut app);
            assert_eq!(
                (stats(&app).post_groups, stats(&app).intermediate_groups),
                (1 + u32::from(split), 1),
                "{name}"
            );
            img
        };
        let hdr = render_at(samples, DownlevelOverride::default());
        let webgl2 = render_at(samples, DownlevelOverride::webgl2());
        let hdr_1x = render_at(1, DownlevelOverride::default());
        let without_quad = translucent.map(|quad| {
            app.world.despawn(quad).unwrap();
            app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
            render(&mut app)
        });
        let cmp = compare_images(&hdr, &webgl2, &bound);
        eprintln!("{name}: {}", cmp.summary());
        assert!(cmp.passed, "{name}: {}", cmp.summary());
        // Every pixel off by more than 8/255 is a silhouette (MSAA) or under the quad.
        let mut unexplained = Vec::new();
        for y in 0..PH {
            for x in 0..PW {
                let (a, b) = (hdr.get(x, y), webgl2.get(x, y));
                if (0..3).all(|c| a[c].abs_diff(b[c]) <= 8) {
                    continue;
                }
                let silhouette = samples > 1 && hdr_1x.get(x, y) != a;
                let under_quad = without_quad.as_ref().is_some_and(|w| w.get(x, y) != a);
                if !(silhouette || under_quad) {
                    unexplained.push(((x, y), a, b));
                }
            }
        }
        assert!(
            unexplained.is_empty(),
            "{name}: {} pixels off outside silhouettes and the translucent quad: {:?}",
            unexplained.len(),
            &unexplained[..unexplained.len().min(8)]
        );
    }
}
