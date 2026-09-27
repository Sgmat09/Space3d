//! Lighting unit tests: the pieces the public API cannot reach — budgeted bakes on a headless
//! renderer, environment states frame by frame, and the baked cubes' texels.

use super::*;
use crate::assets::{SamplerSettings, TextureSource};
use crate::components::{Material, Mesh3d, Projection};
use crate::renderer::Renderer;
use space3d_core::{App, ImageData, MeshData, Transform};

fn headless_app(w: u32, h: u32) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(crate::RenderPlugin::default());
    match pollster::block_on(Renderer::new_headless(w, h)) {
        Ok(r) => {
            app.insert_resource(r);
            Some(app)
        }
        Err(e) => {
            eprintln!("skipping GPU test: no adapter available ({e})");
            None
        }
    }
}

/// `Rgba16Float` faces from a function of (face, x, y) in texels.
fn hdr_faces(size: u32, f: impl Fn(u32, u32, u32) -> f32) -> api::CubemapSource {
    let mut data = Vec::new();
    for face in 0..6 {
        for y in 0..size {
            for x in 0..size {
                let v = f(face, x, y);
                for c in [v, v, v, 1.0] {
                    data.extend_from_slice(&crate::instance::f32_to_f16(c).to_le_bytes());
                }
            }
        }
    }
    api::CubemapSource::Faces(TextureSource {
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

/// Linear red of texel (x, y) of `face`, mip `mip` of an `Rgba16Float` cube (blocking readback).
fn read_texel(r: &Renderer, texture: &wgpu::Texture, face: u32, mip: u32, x: u32, y: u32) -> f32 {
    let (device, queue) = (r.device(), r.queue());
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 256,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: mip,
            origin: wgpu::Origin3d { x, y, z: face },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(1),
            },
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let data = buffer.slice(..).get_mapped_range().expect("mapped");
    crate::instance::f16_to_f32(u16::from_le_bytes([data[0], data[1]]))
}

/// An environment camera over a white sphere.
fn scene(app: &mut App, cubemap: CubemapHandle) -> Entity {
    let sphere = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::sphere(1.0, 32, 16));
    app.world.spawn((
        Transform::IDENTITY,
        Mesh3d(sphere),
        Material::default().with_metallic_roughness(1.0, 0.3),
    ));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 4.0),
        crate::Camera3d::default().with_projection(Projection::default()),
        api::EnvironmentLight::new(cubemap),
    ))
}

/// The bake against closed forms: the irradiance of a sky that is 1 above the horizon and 0
/// below is 1 facing up, 0 facing down and ½ on the horizon; the roughness-0 specular mip is the
/// sky itself. (Reads `Rgba16Float` texels back, which ANGLE's GLES 3.0 `readPixels` does not
/// support: skipped on GL, where `tests/gl_materials.rs` compares the lit result with Metal's.)
#[test]
fn ibl_bake_matches_analytic() {
    let Some(mut app) = headless_app(8, 8) else { return };
    if app.resources.expect::<Renderer>().capabilities().adapter.backend == wgpu::Backend::Gl {
        eprintln!("skipping: no float readback on GL");
        return;
    }
    const N: u32 = 64;
    // +Y face bright, −Y dark; side faces bright in their upper half (rows go down).
    let sky = hdr_faces(N, |face, _, y| match face {
        2 => 1.0,
        3 => 0.0,
        _ if y < N / 2 => 1.0,
        _ => 0.0,
    });
    let cube = app.resources.expect_mut::<RenderAssets>().add_cubemap(sky);
    scene(&mut app, cube);
    crate::render_to_image(&mut app).unwrap();
    let r = app.resources.expect::<Renderer>();
    assert_eq!(r.lighting.environment_count(), 1);
    let (specular, irradiance) = r.lighting.environment_textures(0);
    let c = 16; // center texel of the 32² irradiance faces
    let up = read_texel(r, irradiance, 2, 0, c, c);
    let down = read_texel(r, irradiance, 3, 0, c, c);
    let horizon = read_texel(r, irradiance, 0, 0, c, c);
    assert!((up - 1.0).abs() < 0.02, "facing up: {up}");
    assert!(down.abs() < 0.02, "facing down: {down}");
    assert!((horizon - 0.5).abs() < 0.03, "on the horizon: {horizon}");
    // Specular mip 0 (64² here: the source's size) is a copy of the sky.
    assert_eq!(read_texel(r, specular, 2, 0, 5, 40), 1.0);
    assert_eq!(read_texel(r, specular, 3, 0, 5, 40), 0.0);
    assert_eq!(read_texel(r, specular, 4, 0, 30, 10), 1.0);
    assert_eq!(read_texel(r, specular, 4, 0, 30, 50), 0.0);
    // Rougher mips blur across the horizon: the texel just above it on +Z loses light.
    let mips = specular.mip_level_count();
    let rough = read_texel(r, specular, 4, mips - 1, 4, 3);
    assert!(rough < 0.98 && rough > 0.5, "roughest mip above the horizon: {rough}");
    assert_eq!(r.gpu_error_count(), 0);
}

/// Whether `app`'s renderer bakes with compute dispatches (with `no_compute` forced: never).
fn compute_path(app: &App, no_compute: bool) -> bool {
    let r = app.resources.expect::<Renderer>();
    !no_compute && r.graphics_caps().compute && r.device().limits().max_storage_textures_per_shader_stage > 0
}

fn force_fragment_path(app: &mut App) {
    app.insert_resource(crate::FrameSettings {
        downlevel: crate::DownlevelOverride {
            no_compute: true,
            ..Default::default()
        },
        ..Default::default()
    });
}

/// A budgeted bake (2 steps per frame) takes several frames, reports its progress, keeps the
/// camera unlit by the environment until done, and ends bit-identical to the one-frame bake —
/// on the compute path (dispatches) and on the fragment path (render passes, forced with
/// `no_compute`). Then the transitions of `environment_state`: a re-upload re-bakes while the
/// old environment stays bound; a removed cubemap is `Missing`.
#[test]
fn ibl_sliced_equals_immediate() {
    let sky = || hdr_faces(32, |face, x, y| 0.2 + 0.1 * face as f32 + 0.01 * ((x ^ y) & 7) as f32);
    for no_compute in [false, true] {
        let Some(mut immediate) = headless_app(32, 32) else {
            return;
        };
        if no_compute {
            force_fragment_path(&mut immediate);
        }
        let cube = immediate.resources.expect_mut::<RenderAssets>().add_cubemap(sky());
        scene(&mut immediate, cube);
        let reference = crate::render_to_image(&mut immediate).unwrap();

        let mut app = headless_app(32, 32).unwrap();
        if no_compute {
            force_fragment_path(&mut app);
        }
        let compute = compute_path(&app, no_compute);
        let budget = 2;
        app.insert_resource(LightingSettings {
            ibl: api::IblSettings {
                bake_passes_per_frame: Some(budget),
                ..Default::default()
            },
            ..LightingSettings::default()
        });
        app.resources.expect_mut::<Renderer>().lighting.budget_headless = true;
        let cube = app.resources.expect_mut::<RenderAssets>().add_cubemap(sky());
        let state = |app: &App| app.resources.expect::<Renderer>().environment_state(cube);
        assert_eq!(state(&app), EnvironmentState::Missing, "not seen by a frame yet");
        // No environment requested: uploaded, the whole bake still to do.
        crate::render_to_image(&mut app).unwrap();
        let total = EnvKey::new(&api::IblSettings::default(), true).passes(32, compute);
        assert_eq!(total, if compute { 3 + 1 } else { 6 * 3 + 6 }, "compute {compute}");
        assert_eq!(state(&app), EnvironmentState::Baking { passes_left: total });
        scene(&mut app, cube);
        let mut left = total;
        let mut unlit = None;
        while left > 0 {
            let img = crate::render_to_image(&mut app).unwrap();
            left = left.saturating_sub(budget);
            let s = app.resources.expect::<crate::RenderStats>();
            if left > 0 {
                assert_eq!(state(&app), EnvironmentState::Baking { passes_left: left });
                assert_eq!(s.ibl_bake_passes, budget);
                assert_eq!(s.compute_passes > 0, compute);
                assert_ne!(img, reference, "not lit by the environment before it is baked");
                if let Some(prev) = &unlit {
                    assert_eq!(&img, prev);
                }
                unlit = Some(img);
            } else {
                assert_eq!(state(&app), EnvironmentState::Ready);
                assert_eq!(
                    img, reference,
                    "the sliced bake equals the immediate one (compute {compute})"
                );
            }
        }
        // A re-upload: the new bake starts, the old environment stays bound meanwhile.
        app.resources.expect_mut::<RenderAssets>().update_cubemap(cube, sky());
        let img = crate::render_to_image(&mut app).unwrap();
        assert_eq!(
            state(&app),
            EnvironmentState::Baking {
                passes_left: total - budget
            }
        );
        assert_eq!(img, reference);
        assert_eq!(app.resources.expect::<Renderer>().lighting.environment_count(), 2);
        for _ in 0..total.div_ceil(budget) {
            crate::render_to_image(&mut app).unwrap();
        }
        assert_eq!(state(&app), EnvironmentState::Ready);
        // The superseded environment is dropped once the new one is baked.
        crate::render_to_image(&mut app).unwrap();
        assert_eq!(app.resources.expect::<Renderer>().lighting.environment_count(), 1);
        app.resources.expect_mut::<RenderAssets>().remove_cubemap(cube);
        crate::render_to_image(&mut app).unwrap();
        assert_eq!(state(&app), EnvironmentState::Missing);
        assert_eq!(app.resources.expect::<Renderer>().lighting.environment_count(), 0);
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    }
}

/// `Renderer::environment_state` frame by frame: `Missing` before a frame has seen the cubemap,
/// `Baking { passes_left }` once uploaded (the whole bake while no camera uses it, then counting
/// down with `bake_passes_per_frame`), `Ready`, `Baking` again after a re-upload, `Missing` once
/// removed; a source the upload rejects is `Failed`.
#[test]
fn environment_state_transitions() {
    let Some(mut app) = headless_app(16, 16) else { return };
    let compute = compute_path(&app, false);
    app.insert_resource(LightingSettings {
        ibl: api::IblSettings {
            bake_passes_per_frame: Some(1),
            ..Default::default()
        },
        ..LightingSettings::default()
    });
    app.resources.expect_mut::<Renderer>().lighting.budget_headless = true;
    let sky = || hdr_faces(16, |face, _, _| 0.1 * face as f32);
    let cube = app.resources.expect_mut::<RenderAssets>().add_cubemap(sky());
    let state = |app: &App, h| app.resources.expect::<Renderer>().environment_state(h);
    assert_eq!(state(&app, cube), EnvironmentState::Missing);
    crate::render_to_image(&mut app).unwrap();
    let total = EnvKey::new(&api::IblSettings::default(), true).passes(16, compute);
    assert_eq!(state(&app, cube), EnvironmentState::Baking { passes_left: total });
    crate::render_to_image(&mut app).unwrap();
    assert_eq!(
        state(&app, cube),
        EnvironmentState::Baking { passes_left: total },
        "no camera uses it"
    );
    scene(&mut app, cube);
    for left in (0..total).rev() {
        crate::render_to_image(&mut app).unwrap();
        let want = if left == 0 {
            EnvironmentState::Ready
        } else {
            EnvironmentState::Baking { passes_left: left }
        };
        assert_eq!(state(&app, cube), want);
    }
    app.resources.expect_mut::<RenderAssets>().update_cubemap(cube, sky());
    crate::render_to_image(&mut app).unwrap();
    assert_eq!(state(&app, cube), EnvironmentState::Baking { passes_left: total - 1 });
    app.resources.expect_mut::<RenderAssets>().remove_cubemap(cube);
    crate::render_to_image(&mut app).unwrap();
    assert_eq!(state(&app, cube), EnvironmentState::Missing);
    // Five layers are not a cube.
    let mut five = TextureSource::from_layers(&vec![ImageData::filled(4, 4, [9, 9, 9, 255]); 5]).unwrap();
    five.generate_mipmaps = false;
    let bad = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_cubemap(api::CubemapSource::Faces(five));
    crate::render_to_image(&mut app).unwrap();
    assert_eq!(state(&app, bad), EnvironmentState::Failed);
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}

/// The compute bake (one compute pass) and the fragment bake (a render pass per face and mip,
/// the WebGL2 path) compute the same texels: same functions, same sample sets.
#[test]
fn compute_bake_matches_fragment_bake() {
    let render = |no_compute: bool| {
        let mut app = headless_app(48, 48)?;
        if no_compute {
            force_fragment_path(&mut app);
        }
        let sky = hdr_faces(64, |face, x, y| {
            0.1 + 0.15 * face as f32 + 0.3 * (((x / 8) ^ (y / 8)) & 1) as f32
        });
        let cube = app.resources.expect_mut::<RenderAssets>().add_cubemap(sky);
        let camera = scene(&mut app, cube);
        app.world.insert_one(camera, api::Skybox::new(cube)).unwrap();
        let img = crate::render_to_image(&mut app).unwrap();
        let s = app.resources.expect::<crate::RenderStats>().clone();
        Some((img, s, compute_path(&app, no_compute)))
    };
    let Some((compute, cs, is_compute)) = render(false) else {
        return;
    };
    if !is_compute {
        eprintln!("no compute path on this device");
        return;
    }
    let (fragment, fs, _) = render(true).unwrap();
    assert!(
        cs.compute_passes >= 1 && cs.ibl_bake_passes < fs.ibl_bake_passes,
        "{cs:?} vs {fs:?}"
    );
    let worst = compute
        .pixels
        .iter()
        .zip(&fragment.pixels)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    assert!(worst <= 1, "compute vs fragment bake: {worst}");
}

/// An HDR sky (`Rgba16Float` faces) becomes a radiance cube of `GraphicsCaps::hdr_sky_format`:
/// `Rg11b10Ufloat` converted and mipmapped by one dispatch per mip where compute writes it
/// (the review of 102c034: a render pass per face and mip made the 1024² sky's upload 7 ms),
/// `Rgba16Float` (the faces written straight into mip 0) mipmapped by dispatches where compute
/// cannot write the compact format, a render pass per face and step without compute (the
/// compact cube on Metal with `no_compute`, `Rgba16Float` on GL).
#[test]
fn hdr_sky_upload_steps_and_format() {
    for no_compute in [false, true] {
        let Some(mut app) = headless_app(8, 8) else { return };
        if no_compute {
            force_fragment_path(&mut app);
        }
        let sky = hdr_faces(64, |face, x, _| 0.5 + face as f32 + x as f32 / 16.0);
        let cube = app.resources.expect_mut::<RenderAssets>().add_cubemap(sky);
        app.world
            .spawn((Transform::IDENTITY, crate::Camera3d::default(), api::Skybox::new(cube)));
        crate::render_to_image(&mut app).unwrap();
        let r = app.resources.expect::<Renderer>();
        let caps = r.graphics_caps().masked(crate::DownlevelOverride {
            no_compute,
            ..Default::default()
        });
        let format = caps.hdr_sky_format();
        let s = app.resources.expect::<crate::RenderStats>();
        // 64²: 6 mips below mip 0, and a conversion into mip 0 unless the cube keeps the faces'
        // `Rgba16Float` (GL, WebGPU: they are written into it directly).
        use wgpu::TextureFormat as F;
        let conversion = if format == Some(F::Rgba16Float) { 0 } else { 1 };
        let computed = compute_path(&app, no_compute) && (format != Some(F::Rg11b10Ufloat) || caps.compact_hdr_storage);
        let steps = if computed { conversion + 6 } else { 6 * (conversion + 6) };
        if format.is_some() {
            assert_eq!(s.ibl_bake_passes, steps, "no_compute {no_compute}: {caps:?}");
        }
        let texel = if format == Some(wgpu::TextureFormat::Rgba16Float) {
            8
        } else {
            4
        };
        let texels: u64 = (0..7).map(|m| (64u64 >> m).pow(2)).sum();
        assert_eq!(
            s.lighting_bytes,
            texels * 6 * texel,
            "no_compute {no_compute}: {format:?}"
        );
        if r.capabilities().adapter.backend == wgpu::Backend::Metal && r.capabilities().adapter.name.contains("Apple") {
            assert_eq!(format, Some(wgpu::TextureFormat::Rg11b10Ufloat));
            assert_eq!(caps.compact_hdr_storage, !no_compute);
        }
        assert_eq!(r.gpu_error_count(), 0);
    }
}

/// Skybox-only cubemaps never bake; an environment unused for `ENV_IDLE_FRAMES` is dropped.
#[test]
fn environments_are_lazy_and_evicted() {
    let Some(mut app) = headless_app(8, 8) else { return };
    let cube = app.resources.expect_mut::<RenderAssets>().add_cubemap(
        api::CubemapSource::from_faces(std::array::from_fn(|_| ImageData::filled(8, 8, [90, 90, 90, 255]))).unwrap(),
    );
    let camera = app
        .world
        .spawn((Transform::IDENTITY, crate::Camera3d::default(), api::Skybox::new(cube)));
    crate::render_to_image(&mut app).unwrap();
    let mip_steps = if compute_path(&app, false) { 3 } else { 6 * 3 };
    assert_eq!(
        app.resources.expect::<crate::RenderStats>().ibl_bake_passes,
        mip_steps,
        "the cube's mips only"
    );
    assert_eq!(app.resources.expect::<Renderer>().lighting.environment_count(), 0);
    app.world.insert_one(camera, api::EnvironmentLight::new(cube)).unwrap();
    crate::render_to_image(&mut app).unwrap();
    assert_eq!(app.resources.expect::<Renderer>().lighting.environment_count(), 1);
    app.world.remove_one::<api::EnvironmentLight>(camera).unwrap();
    for _ in 0..ENV_IDLE_FRAMES {
        crate::render_to_image(&mut app).unwrap();
    }
    assert_eq!(app.resources.expect::<Renderer>().lighting.environment_count(), 1);
    crate::render_to_image(&mut app).unwrap();
    assert_eq!(app.resources.expect::<Renderer>().lighting.environment_count(), 0);
}

#[test]
fn neutral_without_cubemaps() {
    let Ok(r) = pollster::block_on(crate::Renderer::new_headless(4, 4)) else {
        eprintln!("skipping GPU test: no adapter available");
        return;
    };
    let mut lighting = Lighting::new(r.device(), r.queue(), r.graphics_caps());
    let world = World::new();
    assert!(
        lighting
            .depth_views(&world, &[], &LightingSettings::default())
            .is_empty()
    );
    assert_eq!(lighting.view_resources(None).key, 0);
    assert_eq!(lighting.view_resources(Some(world.reserve_entity())).key, 0);
    let mut nodes = RenderNodes::default();
    Lighting::register_nodes(&mut nodes);
    assert!(nodes.contains("space3d/skybox"));
    assert_eq!(r.gpu_error_count(), 0);
}

#[test]
fn rotations_are_inverted() {
    let q = Quat::from_rotation_y(0.7);
    let m = rotation_columns(q);
    let m = Mat3::from_cols(
        glam::Vec4::from(m[0]).truncate(),
        glam::Vec4::from(m[1]).truncate(),
        glam::Vec4::from(m[2]).truncate(),
    );
    let v = glam::Vec3::new(0.3, -0.2, 0.9);
    assert!((m * (q * v) - v).length() < 1e-5);
    assert_eq!(
        rotation_columns(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0))[0],
        [1.0, 0.0, 0.0, 0.0]
    );
}

/// The shadow map follows the casting light: created at the requested (power-of-two) size,
/// bound to camera views only (depth views get the neutral map), kept while unused for
/// `ENV_IDLE_FRAMES` frames, then dropped; a light with strength 0 requests no depth view.
#[test]
fn shadow_map_follows_the_light() {
    let Some(mut app) = headless_app(8, 8) else { return };
    let camera = app.world.spawn((
        Transform::from_xyz(0.0, 10.0, 0.0).looking_at(glam::Vec3::ZERO, glam::Vec3::NEG_Z),
        crate::Camera3d::default(),
    ));
    let light = app.world.spawn((
        Transform::IDENTITY.looking_at(glam::Vec3::new(-0.3, -1.0, 0.2), glam::Vec3::Y),
        crate::DirectionalLight::default(),
        api::DirectionalShadow::new().with_map_size(300),
    ));
    fn lighting(app: &App) -> &Lighting {
        &app.resources.expect::<Renderer>().lighting
    }
    crate::render_to_image(&mut app).unwrap();
    assert_eq!(lighting(&app).shadow_map_size(), Some(512));
    let keys = |app: &App| {
        let l = lighting(app);
        (l.view_resources(Some(camera)).key, l.view_resources(None).key)
    };
    let (camera_key, depth_key) = keys(&app);
    assert_ne!(camera_key >> 32, 0, "cameras bind the map");
    assert_eq!(depth_key, 0, "depth views bind the neutral map");
    // Strength 0: no depth view, no shadow in the views, the map is kept.
    app.world.get::<&mut api::DirectionalShadow>(light).unwrap().strength = 0.0;
    crate::render_to_image(&mut app).unwrap();
    assert_eq!(app.resources.expect::<crate::RenderStats>().depth_views, 0);
    assert_eq!(lighting(&app).shadow_map_size(), None);
    assert!(lighting(&app).shadow_map.is_some());
    assert_eq!(keys(&app).0, 0);
    app.world.remove_one::<api::DirectionalShadow>(light).unwrap();
    for _ in 0..ENV_IDLE_FRAMES {
        crate::render_to_image(&mut app).unwrap();
    }
    assert!(lighting(&app).shadow_map.is_some());
    crate::render_to_image(&mut app).unwrap();
    assert!(lighting(&app).shadow_map.is_none(), "dropped after the idle frames");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}
