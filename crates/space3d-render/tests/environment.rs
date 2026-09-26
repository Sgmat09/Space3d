//! Cubemaps, the skybox and image-based lighting (graphics spec §5.2 PM3/PM4): face
//! orientation, the equirect and three.js conversions, HDR sky radiance, the white furnace, the
//! LDR bake fallback and the steady state.
//!
//! Budgeted bakes (`ibl_sliced_equals_immediate`, `environment_state_transitions`) are unit
//! tests of `lighting` (headless renderers bake everything at once otherwise).

mod common;
use common::*;
use space3d_core::HdrImageData;
use space3d_core::prelude::*;
use space3d_render::*;

const W: u32 = 32;
const H: u32 = 32;

/// Face colors in wgpu order (+X, −X, +Y, −Y, +Z, −Z), sRGB bytes.
const FACE_COLORS: [[u8; 4]; 6] = [
    [230, 40, 40, 255],
    [40, 220, 220, 255],
    [40, 220, 40, 255],
    [220, 40, 220, 255],
    [40, 60, 230, 255],
    [230, 220, 40, 255],
];

/// Directions to look for each face, with an up vector.
const LOOK: [(Vec3, Vec3); 6] = [
    (Vec3::X, Vec3::Y),
    (Vec3::NEG_X, Vec3::Y),
    (Vec3::Y, Vec3::Z),
    (Vec3::NEG_Y, Vec3::Z),
    (Vec3::Z, Vec3::Y),
    (Vec3::NEG_Z, Vec3::Y),
];

fn solid_faces(size: u32) -> [ImageData; 6] {
    std::array::from_fn(|i| ImageData::filled(size, size, FACE_COLORS[i]))
}

fn add_cubemap(app: &mut App, source: CubemapSource) -> CubemapHandle {
    app.resources.expect_mut::<RenderAssets>().add_cubemap(source)
}

/// A camera at the origin looking along `dir` (90° vertical field of view).
fn sky_camera(app: &mut App, dir: Vec3, up: Vec3, sky: Skybox) -> Entity {
    app.world.spawn((
        Transform::IDENTITY.looking_at(dir, up),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Perspective {
                fov_y: 90f32.to_radians(),
                near: 0.1,
                far: 100.0,
            }),
        sky,
    ))
}

fn look(app: &mut App, camera: Entity, dir: Vec3, up: Vec3) {
    *app.world.get::<&mut Transform>(camera).unwrap() = Transform::IDENTITY.looking_at(dir, up);
}

fn close(a: [u8; 4], b: [u8; 4], tolerance: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tolerance)
}

fn gpu_errors(app: &App) -> u64 {
    app.resources.expect::<Renderer>().gpu_error_count()
}

/// Each face shows up where the camera looks along its axis (wgpu / OpenGL face order), from a
/// faces source and from an equirect panorama of the same six colors; a closer opaque mesh
/// hides the sky (it is drawn at the far plane after the opaque phase).
#[test]
fn skybox_orientation_six_colors() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cube = add_cubemap(&mut app, CubemapSource::from_faces(solid_faces(16)).unwrap());
    let camera = sky_camera(&mut app, Vec3::X, Vec3::Y, Skybox::new(cube));
    for (i, &(dir, up)) in LOOK.iter().enumerate() {
        look(&mut app, camera, dir, up);
        let img = render_to_image(&mut app).unwrap();
        let px = img.get(W / 2, H / 2);
        assert!(close(px, FACE_COLORS[i], 2), "face {i}: {px:?} vs {:?}", FACE_COLORS[i]);
    }
    // Uploaded; no environment requested: the whole bake is left (2 specular mips and the
    // irradiance: a dispatch each with compute, a pass per face without).
    let state = app.resources.expect::<Renderer>().environment_state(cube);
    assert!(
        matches!(state, EnvironmentState::Baking { passes_left: 3 | 18 }),
        "{state:?}"
    );

    // An equirect panorama colored by the dominant axis of each pixel's direction (longitude 0
    // = −Z, +X to its right, top row = +Y).
    let (pw, ph) = (128u32, 64u32);
    let mut pano = ImageData::new(pw, ph);
    for y in 0..ph {
        for x in 0..pw {
            let lon = ((x as f32 + 0.5) / pw as f32 - 0.5) * std::f32::consts::TAU;
            let lat = (y as f32 + 0.5) / ph as f32 * std::f32::consts::PI;
            let d = Vec3::new(lat.sin() * lon.sin(), lat.cos(), -lat.sin() * lon.cos());
            let a = d.abs();
            let face = if a.x >= a.y && a.x >= a.z {
                if d.x > 0.0 { 0 } else { 1 }
            } else if a.y >= a.z {
                if d.y > 0.0 { 2 } else { 3 }
            } else if d.z > 0.0 {
                4
            } else {
                5
            };
            pano.set(x, y, FACE_COLORS[face]);
        }
    }
    let equirect = add_cubemap(&mut app, CubemapSource::from_equirect_image(pano).with_face_size(16));
    app.world.get::<&mut Skybox>(camera).unwrap().cubemap = equirect;
    for (i, &(dir, up)) in LOOK.iter().enumerate() {
        look(&mut app, camera, dir, up);
        let img = render_to_image(&mut app).unwrap();
        let px = img.get(W / 2, H / 2);
        assert!(
            close(px, FACE_COLORS[i], 2),
            "equirect face {i}: {px:?} vs {:?}",
            FACE_COLORS[i]
        );
    }

    // Opaque geometry in front hides the sky.
    look(&mut app, camera, Vec3::NEG_Z, Vec3::Y);
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, -3.0),
        Mesh3d(mesh),
        Material::unlit(Vec4::new(0.0, 0.0, 0.0, 1.0)),
    ));
    let img = render_to_image(&mut app).unwrap();
    assert_eq!(img.get(W / 2, H / 2), [0, 0, 0, 255], "cube in front");
    // (The cube covers the central fifth of the view; above it the sky is still −Z.)
    assert!(
        close(img.get(W / 2, 5), FACE_COLORS[5], 2),
        "sky around it: {:?}",
        img.get(W / 2, 5)
    );
    assert_eq!(gpu_errors(&app), 0);
}

/// `Skybox::rotation` turns the sky: a quarter turn about +Y (right-handed) brings the +X face
/// to where −Z was (looking down −Z shows +X).
#[test]
fn skybox_rotation() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cube = add_cubemap(&mut app, CubemapSource::from_faces(solid_faces(8)).unwrap());
    let rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    sky_camera(
        &mut app,
        Vec3::NEG_Z,
        Vec3::Y,
        Skybox::new(cube).with_rotation(rotation),
    );
    let img = render_to_image(&mut app).unwrap();
    // The sky direction seen at −Z is rotation⁻¹ · (−Z) = +X.
    assert!(
        close(img.get(W / 2, H / 2), FACE_COLORS[0], 2),
        "{:?}",
        img.get(W / 2, H / 2)
    );
}

/// Faces authored for three.js (`CubeTextureLoader`, a mirrored convention) look in this
/// engine as they do in three.js: looking toward +X shows three.js' `nx` image, and the right
/// half of the `pz` image is on the right when looking toward +Z.
#[test]
fn threejs_face_conversion() {
    let Some(mut app) = headless_app(W, H) else { return };
    let mut faces = solid_faces(16);
    // pz: left half red, right half green.
    for y in 0..16 {
        for x in 0..16 {
            faces[4].set(x, y, if x < 8 { [230, 40, 40, 255] } else { [40, 220, 40, 255] });
        }
    }
    let cube = add_cubemap(&mut app, CubemapSource::from_faces_threejs(faces).unwrap());
    let camera = sky_camera(&mut app, Vec3::X, Vec3::Y, Skybox::new(cube));
    let img = render_to_image(&mut app).unwrap();
    assert!(close(img.get(W / 2, H / 2), FACE_COLORS[1], 2), "+X shows nx");
    look(&mut app, camera, Vec3::NEG_X, Vec3::Y);
    let img = render_to_image(&mut app).unwrap();
    assert!(close(img.get(W / 2, H / 2), FACE_COLORS[0], 2), "−X shows px");
    look(&mut app, camera, Vec3::Z, Vec3::Y);
    let img = render_to_image(&mut app).unwrap();
    assert!(
        close(img.get(W / 4, H / 2), [230, 40, 40, 255], 2),
        "left: {:?}",
        img.get(W / 4, H / 2)
    );
    assert!(close(img.get(3 * W / 4, H / 2), [40, 220, 40, 255], 2), "right");
    assert_eq!(gpu_errors(&app), 0);
}

fn hdr_faces(values: [[f32; 3]; 6], size: u32) -> CubemapSource {
    let mut data = Vec::new();
    for v in values {
        for _ in 0..size * size {
            for c in [v[0], v[1], v[2], 1.0] {
                data.extend_from_slice(&f32_to_f16(c).to_le_bytes());
            }
        }
    }
    CubemapSource::Faces(TextureSource {
        width: size,
        height: size,
        layers: 6,
        format: wgpu::TextureFormat::Rgba16Float,
        mip_level_count: 1,
        data,
        generate_mipmaps: true,
        sampler: SamplerSettings::default(),
    })
}

fn capture_hdr(app: &mut App) -> HdrImageData {
    render_to_image(app).expect("capture");
    app.resources
        .expect_mut::<Renderer>()
        .capture_scene_hdr_blocking()
        .expect("hdr scene")
}

/// The sky writes linear radiance × intensity into the HDR scene (unclamped: it feeds bloom),
/// from HDR faces and from LDR ones.
#[test]
fn skybox_intensity_hdr() {
    let Some(mut app) = headless_app(W, H) else { return };
    let hdr = add_cubemap(&mut app, hdr_faces([[6.0, 3.0, 0.5]; 6], 8));
    let camera = sky_camera(&mut app, Vec3::NEG_Z, Vec3::Y, Skybox::new(hdr).with_intensity(2.0));
    app.world
        .insert_one(camera, PostProcess::default().without_bloom())
        .unwrap();
    let scene = capture_hdr(&mut app);
    let p = scene.get(W / 2, H / 2);
    for (got, want) in p.iter().zip([12.0, 6.0, 1.0]) {
        assert!((got - want).abs() <= want * 0.01, "{p:?}");
    }
    let ldr = add_cubemap(&mut app, CubemapSource::from_faces(solid_faces(8)).unwrap());
    *app.world.get::<&mut Skybox>(camera).unwrap() = Skybox::new(ldr).with_intensity(4.0);
    let scene = capture_hdr(&mut app);
    let p = scene.get(W / 2, H / 2);
    let want = FACE_COLORS[5].map(|c| {
        let c = c as f32 / 255.0;
        4.0 * if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    });
    for c in 0..3 {
        assert!((p[c] - want[c]).abs() <= want[c] * 0.01 + 0.004, "{p:?} vs {want:?}");
    }
    assert_eq!(gpu_errors(&app), 0);
}

/// Spheres of the furnace: (base color, metallic, roughness).
const FURNACE: [(f32, f32, f32); 4] = [(1.0, 0.0, 0.05), (1.0, 0.0, 1.0), (1.0, 1.0, 0.05), (1.0, 1.0, 1.0)];

/// A white-furnace scene: a uniform environment of radiance `radiance`, no other light, four
/// white spheres (dielectric and metal, smooth and rough) side by side, Standard shading.
fn furnace(app: &mut App, radiance: f32) -> (Entity, CubemapHandle) {
    let env = add_cubemap(app, hdr_faces([[radiance; 3]; 6], 32));
    app.insert_resource(LightingSettings {
        shading: ShadingModel::Standard,
        ..LightingSettings::default()
    });
    app.insert_resource(AmbientLight {
        color: Vec3::ZERO,
        intensity: 0.0,
    });
    let sphere = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::sphere(1.0, 48, 24));
    for (i, (base, metallic, roughness)) in FURNACE.into_iter().enumerate() {
        app.world.spawn((
            Transform::from_xyz(-3.3 + 2.2 * i as f32, 0.0, 0.0),
            Mesh3d(sphere),
            Material::color(Vec4::new(base, base, base, 1.0)).with_metallic_roughness(metallic, roughness),
        ));
    }
    let camera = app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 10.0),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Orthographic {
                height: 3.0,
                near: 0.1,
                far: 100.0,
            }),
        EnvironmentLight::new(env),
    ));
    (camera, env)
}

/// Energy conservation of the image-based lighting (graphics spec PM4 acceptance): in a
/// uniform environment of radiance 0.5, white dielectric and metal spheres, smooth and rough,
/// reflect 0.5 ± 3 % everywhere on their visible disc.
#[test]
fn ibl_white_furnace() {
    let Some(mut app) = headless_app(160, 48) else { return };
    let (camera, env) = furnace(&mut app, 0.5);
    app.world
        .insert_one(camera, PostProcess::default().without_bloom())
        .unwrap();
    let scene = capture_hdr(&mut app);
    assert_eq!(
        app.resources.expect::<Renderer>().environment_state(env),
        EnvironmentState::Ready,
        "headless renderers bake at once"
    );
    // The view is 3 units high: 16 px per unit, spheres of radius 1 at x = −3.3 … 3.3.
    let (w, h) = (scene.width as f32, scene.height as f32);
    let px_per_unit = h / 3.0;
    for (i, (_, metallic, roughness)) in FURNACE.into_iter().enumerate() {
        let cx = w / 2.0 + (-3.3 + 2.2 * i as f32) * px_per_unit;
        let (mut n, mut worst) = (0, 0.0f32);
        for y in 0..scene.height {
            for x in 0..scene.width {
                let (dx, dy) = (
                    (x as f32 + 0.5 - cx) / px_per_unit,
                    (y as f32 + 0.5 - h / 2.0) / px_per_unit,
                );
                // Inside the disc, away from the silhouette's partially covered pixels.
                if dx * dx + dy * dy > 0.85 * 0.85 {
                    continue;
                }
                n += 1;
                for c in &scene.get(x, y)[..3] {
                    worst = worst.max((c / 0.5 - 1.0).abs());
                }
            }
        }
        assert!(n > 100, "sphere {i} on screen");
        assert!(
            worst <= 0.03,
            "sphere {i} (metallic {metallic}, roughness {roughness}): off by {:.2} %",
            worst * 100.0
        );
    }
    assert_eq!(gpu_errors(&app), 0);
}

/// The LDR bake fallback (no float render targets: `Rgba8UnormSrgb` radiance and environment
/// cubes) lights like the HDR bake for an LDR environment.
#[test]
fn ibl_ldr_fallback_close_to_hdr() {
    let render = |no_float: bool| -> Option<ImageData> {
        let mut app = headless_app(128, 48)?;
        let faces = std::array::from_fn(|i| {
            let mut img = ImageData::filled(32, 32, FACE_COLORS[i]);
            for y in 0..32 {
                for x in 0..32 {
                    if (x / 8 + y / 8) % 2 == 0 {
                        img.set(x, y, [200, 200, 200, 255]);
                    }
                }
            }
            img
        });
        let env = add_cubemap(&mut app, CubemapSource::from_faces(faces).unwrap());
        app.insert_resource(FrameSettings {
            downlevel: DownlevelOverride {
                no_float_targets: no_float,
                ..Default::default()
            },
            ..FrameSettings::default()
        });
        app.insert_resource(LightingSettings {
            shading: ShadingModel::Standard,
            ..LightingSettings::default()
        });
        let sphere = app
            .resources
            .expect_mut::<RenderAssets>()
            .add_mesh(MeshData::sphere(1.0, 48, 24));
        for (i, (metallic, roughness)) in [(1.0, 0.1), (1.0, 0.5), (0.0, 0.3), (0.0, 1.0)].into_iter().enumerate() {
            app.world.spawn((
                Transform::from_xyz(-3.3 + 2.2 * i as f32, 0.0, 0.0),
                Mesh3d(sphere),
                Material::srgb(0.9, 0.8, 0.7).with_metallic_roughness(metallic, roughness),
            ));
        }
        app.world.spawn((
            Transform::from_xyz(0.0, 0.5, 10.0).looking_at(Vec3::ZERO, Vec3::Y),
            Camera3d::default().with_projection(Projection::Orthographic {
                height: 3.0,
                near: 0.1,
                far: 100.0,
            }),
            EnvironmentLight::new(env),
            Skybox::new(env),
        ));
        let img = render_to_image(&mut app)?;
        assert_eq!(gpu_errors(&app), 0);
        Some(img)
    };
    let Some(hdr) = render(false) else { return };
    let ldr = render(true).unwrap();
    let stats = space3d_test::compare_images(&hdr, &ldr, &space3d_test::CompareOptions::perceptual());
    assert!(stats.passed, "LDR bake vs HDR bake: {}", stats.summary());
    let worst = hdr
        .pixels
        .iter()
        .zip(&ldr.pixels)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    assert!(worst <= 8, "max channel difference {worst}");
}

/// `DownlevelOverride::webgl2()` (no float targets: the LDR bake; no compute: render-pass bake
/// steps; no read-only depth) renders the skybox and the image-based lighting of an LDR
/// environment like the default path.
#[test]
fn environment_webgl2_parity() {
    let render = |webgl2: bool| -> Option<ImageData> {
        let mut app = headless_app(96, 48)?;
        if webgl2 {
            app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride::webgl2()));
        }
        app.insert_resource(LightingSettings {
            shading: ShadingModel::Standard,
            ..LightingSettings::default()
        });
        let faces = std::array::from_fn(|i| {
            let mut img = ImageData::filled(32, 32, FACE_COLORS[i]);
            for y in (0..32).step_by(4) {
                for x in 0..32 {
                    img.set(x, y, [230, 230, 230, 255]);
                }
            }
            img
        });
        let env = add_cubemap(&mut app, CubemapSource::from_faces(faces).unwrap());
        let sphere = app
            .resources
            .expect_mut::<RenderAssets>()
            .add_mesh(MeshData::sphere(1.0, 48, 24));
        for (i, (metallic, roughness)) in [(1.0, 0.05), (1.0, 0.6), (0.0, 0.3)].into_iter().enumerate() {
            app.world.spawn((
                Transform::from_xyz(-2.2 + 2.2 * i as f32, 0.0, -4.0),
                Mesh3d(sphere),
                Material::srgb(0.9, 0.85, 0.8).with_metallic_roughness(metallic, roughness),
            ));
        }
        app.world.spawn((
            Transform::IDENTITY,
            Camera3d::default(),
            Skybox::new(env),
            EnvironmentLight::new(env),
        ));
        let img = render_to_image(&mut app)?;
        assert_eq!(gpu_errors(&app), 0);
        Some(img)
    };
    let Some(default) = render(false) else { return };
    let webgl2 = render(true).unwrap();
    let stats = space3d_test::compare_images(&default, &webgl2, &space3d_test::CompareOptions::perceptual());
    assert!(stats.passed, "webgl2() vs default: {}", stats.summary());
}

/// A still scene with a skybox and an environment reaches a steady state: nothing uploaded,
/// no texture created, no bake pass, identical frames.
#[test]
fn environment_steady_state() {
    let Some(mut app) = headless_app(W, H) else { return };
    let (camera, env) = furnace(&mut app, 0.5);
    app.world.insert_one(camera, Skybox::new(env)).unwrap();
    let first = render_to_image(&mut app).unwrap();
    let s = stats(&app);
    assert!(s.ibl_bake_passes > 0 && s.textures_created > 0, "{s:?}");
    for _ in 0..3 {
        let img = render_to_image(&mut app).unwrap();
        assert_eq!(img, first);
        let s = stats(&app);
        assert_eq!(
            (
                s.ibl_bake_passes,
                s.textures_created,
                s.bytes_uploaded,
                s.cpu_reallocations
            ),
            (0, 0, 0, 0),
            "{s:?}"
        );
    }
    // Removing the cubemap turns both off; the renderer reports it missing.
    app.resources.expect_mut::<RenderAssets>().remove_cubemap(env);
    let img = render_to_image(&mut app).unwrap();
    assert_ne!(img, first);
    assert_eq!(
        app.resources.expect::<Renderer>().environment_state(env),
        EnvironmentState::Missing
    );
    assert_eq!(gpu_errors(&app), 0);
}

/// Invalid sources fail (logged, not a GPU error) and the camera renders without them.
#[test]
fn invalid_cubemap_fails() {
    let Some(mut app) = headless_app(W, H) else { return };
    let mut five = TextureSource::from_layers(&solid_faces(8)[..5]).unwrap();
    five.generate_mipmaps = false;
    let bad = add_cubemap(&mut app, CubemapSource::Faces(five));
    sky_camera(&mut app, Vec3::NEG_Z, Vec3::Y, Skybox::new(bad));
    let img = render_to_image(&mut app).unwrap();
    assert_eq!(img.get(W / 2, H / 2), [0, 0, 0, 255], "clear color");
    assert_eq!(
        app.resources.expect::<Renderer>().environment_state(bad),
        EnvironmentState::Failed
    );
    assert_eq!(gpu_errors(&app), 0);
}

/// Perf gate "IBL bake" (graphics spec §4.4: one-time, ≤ 3 ms on T1; design-materials §9: a
/// 1024² LDR cubemap → 256² specular + 32² irradiance): the GPU time (`ibl.bake` scope) of the
/// cubemap's upload (its mip chain) plus its environment bake, 1080p headless, the GPU kept busy
/// by back-to-back frames; medians of 5 cubemaps, each uploaded in one frame and baked in the
/// next. The gate applies to the default path (compute dispatches where available); the
/// fragment path (WebGL2's, forced with `no_compute`) is printed for the record. Needs
/// `TIMESTAMP_QUERY`; run alone:
/// `cargo test -p space3d-render --release --test environment -- --ignored --nocapture ibl_bake_gate`.
#[test]
#[ignore = "perf gate: run alone with --ignored"]
fn ibl_bake_gate() {
    let median = |mut v: Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    let mut gated = None;
    for no_compute in [false, true] {
        let Some(mut app) = headless_app(1920, 1080) else {
            return;
        };
        if !app.resources.expect::<Renderer>().graphics_caps().timestamps {
            eprintln!("skipping: no TIMESTAMP_QUERY");
            return;
        }
        app.insert_resource(FrameSettings {
            gpu_timing: true,
            downlevel: DownlevelOverride {
                no_compute,
                ..Default::default()
            },
            ..FrameSettings::default()
        });
        let sphere = app
            .resources
            .expect_mut::<RenderAssets>()
            .add_mesh(MeshData::sphere(1.0, 64, 32));
        app.world
            .spawn((Transform::IDENTITY, Mesh3d(sphere), Material::default()));
        let camera = app
            .world
            .spawn((Transform::from_xyz(0.0, 0.0, 3.0), Camera3d::default()));
        let frame = |app: &mut App| {
            space3d_core::transform::propagate_transforms(&mut app.world);
            app.run_stage(Stage::Render);
            let _ = app
                .resources
                .expect::<Renderer>()
                .device()
                .poll(wgpu::PollType::wait_indefinitely());
            app.resources.expect::<RenderStats>().frame
        };
        // The `ibl.bake` time of frame `f` (timings arrive a few frames late).
        let timed = |app: &mut App, f: u64| {
            for _ in 0..10 {
                frame(app);
                let t = app.resources.expect::<GpuTimings>();
                if t.frame == Some(f) {
                    return t.scope_ms("ibl.bake");
                }
            }
            panic!("no timings for frame {f}");
        };
        for _ in 0..60 {
            frame(&mut app);
        }
        let (mut uploads, mut bakes) = (Vec::new(), Vec::new());
        for k in 0..5u8 {
            let faces = std::array::from_fn(|i| {
                let mut img = ImageData::new(1024, 1024);
                for y in 0..1024 {
                    for x in 0..1024 {
                        let v = ((x ^ y) as u8).wrapping_add(k.wrapping_mul(40).wrapping_add(i as u8 * 20));
                        img.set(x, y, [v, v / 2, 255 - v, 255]);
                    }
                }
                img
            });
            let cube = add_cubemap(&mut app, CubemapSource::from_faces(faces).unwrap());
            let f = frame(&mut app);
            uploads.push(timed(&mut app, f));
            app.world.insert_one(camera, EnvironmentLight::new(cube)).unwrap();
            let f = frame(&mut app);
            let steps = app.resources.expect::<RenderStats>().ibl_bake_passes;
            bakes.push(timed(&mut app, f));
            assert_eq!(
                app.resources.expect::<Renderer>().environment_state(cube),
                EnvironmentState::Ready
            );
            eprintln!(
                "{} cubemap {k}: upload {:.3} ms, bake {:.3} ms ({steps} steps)",
                if no_compute { "fragment" } else { "default" },
                uploads[k as usize],
                bakes[k as usize]
            );
        }
        let (upload, bake) = (median(uploads), median(bakes));
        eprintln!(
            "{}: upload {upload:.3} ms + bake {bake:.3} ms = {:.3} ms (medians)",
            if no_compute { "fragment path" } else { "default path" },
            upload + bake
        );
        if !no_compute {
            gated = Some(upload + bake);
        }
    }
    let total = gated.unwrap();
    let scale = std::env::var("SPACE3D_PERF_BUDGET_SCALE")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(1.0);
    assert!(
        total <= 3.0 * scale,
        "IBL upload + bake {total:.3} ms > {:.1} ms",
        3.0 * scale
    );
}

/// Perf gate "forward full-screen lit pixels, all maps + IBL + PCF9, vs today's shader ≤ +0.20
/// ms" (graphics spec §4.4): the `scene` pass of a 1080p view filled by one plane, lit by a sun
/// and 4 point lights, with a legacy material (Classic, no maps, no environment, no shadow) vs
/// every map at 1024² (base color, normal, ORM, emissive), Standard shading, image-based
/// lighting and the sun's 2048² shadow with the 9-tap filter (its depth pass is timed apart, as
/// `shadow`). Frames alternate between the two; the gate is the median of 120 paired differences
/// of adjacent frames.
/// Needs `TIMESTAMP_QUERY`; run alone:
/// `cargo test -p space3d-render --release --test environment -- --ignored --nocapture forward_full_screen_gate`.
#[test]
#[ignore = "perf gate: run alone with --ignored"]
fn forward_full_screen_gate() {
    let Some(mut app) = headless_app(1920, 1080) else {
        return;
    };
    if !app.resources.expect::<Renderer>().graphics_caps().timestamps {
        eprintln!("skipping: no TIMESTAMP_QUERY");
        return;
    }
    app.insert_resource(FrameSettings::default().with_gpu_timing(true));
    let noise = |seed: u32| {
        let mut img = ImageData::new(1024, 1024);
        for y in 0..1024u32 {
            for x in 0..1024u32 {
                let h = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed).wrapping_mul(2_654_435_761);
                let v = (h >> 24) as u8;
                img.set(x, y, [v, v.wrapping_add(90), 255 - v, 255]);
            }
        }
        img
    };
    let a = app.resources.expect_mut::<RenderAssets>();
    let plane = a.add_mesh(MeshData::plane(1.0));
    let base = a.add_image(noise(1));
    let normal = a.add_image_linear(noise(2));
    let orm = a.add_image_linear(noise(3));
    let emissive = a.add_image(noise(4));
    let env = a.add_cubemap(CubemapSource::from_faces(solid_faces(256)).unwrap());
    let legacy = Material::srgb(0.7, 0.7, 0.7).with_metallic_roughness(0.5, 0.5);
    let mapped = legacy
        .clone()
        .with_texture(base)
        .with_normal_map(normal, 1.0)
        .with_orm_map(orm)
        .with_emissive_map(emissive, Vec3::splat(0.5));
    let quad = app.world.spawn((
        Transform::IDENTITY
            .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
            .with_scale(Vec3::new(4.0, 1.0, 2.0)),
        Mesh3d(plane),
        legacy.clone(),
    ));
    let camera = app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default().with_projection(Projection::Orthographic {
            height: 1.5,
            near: 0.1,
            far: 100.0,
        }),
    ));
    let sun = app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).looking_at(Vec3::new(-0.3, -1.0, -0.6), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.0,
        },
    ));
    for i in 0..4 {
        app.world.spawn((
            Transform::from_xyz(-1.5 + i as f32, 0.3, 1.0),
            PointLight {
                color: Vec3::ONE,
                intensity: 3.0,
                range: 6.0,
            },
        ));
    }
    let set = |app: &mut App, full: bool| {
        *app.world.get::<&mut Material>(quad).unwrap() = if full { mapped.clone() } else { legacy.clone() };
        app.resources.expect_mut::<LightingSettings>().shading = if full {
            ShadingModel::Standard
        } else {
            ShadingModel::Classic
        };
        if full {
            app.world.insert_one(camera, EnvironmentLight::new(env)).unwrap();
            app.world.insert_one(sun, DirectionalShadow::new()).unwrap();
        } else {
            let _ = app.world.remove_one::<EnvironmentLight>(camera);
            let _ = app.world.remove_one::<DirectionalShadow>(sun);
        }
    };
    let frame = |app: &mut App| {
        space3d_core::transform::propagate_transforms(&mut app.world);
        app.run_stage(Stage::Render);
        let _ = app
            .resources
            .expect::<Renderer>()
            .device()
            .poll(wgpu::PollType::wait_indefinitely());
        app.resources.expect::<RenderStats>().frame
    };
    // Warm up both variants (pipelines, the bake, the textures).
    for i in 0..40 {
        set(&mut app, i % 2 == 0);
        frame(&mut app);
    }
    // Frame index → variant; timings arrive a few frames late.
    let mut variant = std::collections::HashMap::new();
    let mut timed: Vec<(u64, bool, f32)> = Vec::new();
    let mut i = 0;
    while timed.len() < 240 {
        let full = i % 2 == 0;
        set(&mut app, full);
        variant.insert(frame(&mut app), full);
        let t = app.resources.expect::<GpuTimings>();
        if let Some(f) = t.frame
            && let Some(full) = variant.remove(&f)
        {
            timed.push((f, full, t.scope_ms("scene")));
        }
        i += 1;
        assert!(i < 2000, "timings do not arrive");
    }
    let median = |mut v: Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    // Paired differences of adjacent frames (the GPU's clocks drift slower than a frame).
    let mut diffs = Vec::new();
    for w in timed.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b.0 == a.0 + 1 && a.1 != b.1 {
            let (full, legacy) = if a.1 { (a.2, b.2) } else { (b.2, a.2) };
            diffs.push(full - legacy);
        }
    }
    let l = median(timed.iter().filter(|t| !t.1).map(|t| t.2).collect());
    let f = median(timed.iter().filter(|t| t.1).map(|t| t.2).collect());
    let d = median(diffs);
    eprintln!(
        "scene pass, 1080p full screen: legacy {l:.3} ms, all maps + Standard + IBL + PCF9 {f:.3} ms; paired difference {d:+.3} ms"
    );
    let scale = std::env::var("SPACE3D_PERF_BUDGET_SCALE")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(1.0);
    assert!(d <= 0.20 * scale, "{d:+.3} ms > +0.20 ms");
}
