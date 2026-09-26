//! Material maps on wgpu's GL backend (WebGL2 / GLES 3.0 class: 9 of the 16 texture units, no
//! `BASE_VERTEX`, texture targets fixed by the layer count) render like the default backend.
//!
//! Skips without a GL adapter; on macOS put ANGLE on the dyld path and build with the `angle`
//! feature (see `tests/gl.rs`):
//!
//! ```text
//! DYLD_FALLBACK_LIBRARY_PATH=<dir with libEGL.dylib> \
//!     cargo test -p space3d-render --features angle --test gl_materials -- --test-threads=1
//! ```

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;

fn app(backends: Option<wgpu::Backends>) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    let r = pollster::block_on(Renderer::new_headless_with(RendererConfig {
        width: 96,
        height: 48,
        backends,
        ..Default::default()
    }))
    .ok()?;
    if backends.is_some() {
        let c = r.capabilities();
        eprintln!(
            "GL adapter: {} ({}), draw path {:?}, base_vertex {}",
            c.adapter.name, c.adapter.driver_info, c.best_draw_path, c.base_vertex
        );
    }
    app.insert_resource(r);
    Some(app)
}

/// Every map bound, both texture kinds as arrays (layer 1 of each), and a normal-mapped quad
/// with vertex tangents and a second UV set placed past the start of the mesh arena.
fn scene(app: &mut App) {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default().with_clear_color(Vec4::new(0.02, 0.02, 0.05, 1.0)),
    ));
    app.world.spawn((
        Transform::IDENTITY.looking_at(Vec3::new(-0.3, -0.4, -1.0), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 1.5,
        },
    ));
    let e = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
    let pair = |a: [u8; 4], b: [u8; 4]| [ImageData::filled(8, 8, a), ImageData::filled(8, 8, b)];
    let a = app.resources.expect_mut::<RenderAssets>();
    let filler = a.add_mesh(MeshData::cube(1.0));
    let base = a.add_texture(TextureSource::from_layers(&pair([255; 4], [220, 180, 140, 255])).unwrap());
    let emissive = a.add_texture(TextureSource::from_layers(&pair([0; 4], [60, 30, 0, 255])).unwrap());
    let normal = a.add_texture(
        TextureSource::from_layers_linear(&pair([128, 128, 255, 255], [e(0.4), e(-0.2), e(0.89), 255])).unwrap(),
    );
    let orm = a.add_texture(TextureSource::from_layers_linear(&pair([255; 4], [180, 120, 200, 255])).unwrap());
    let sphere = a.add_mesh(MeshData::sphere(1.0, 32, 16));
    // The cube's faces as `MeshData::cube` builds them: (normal, +u, −v); tangent = +u.
    let faces = [
        (Vec3::X, Vec3::NEG_Z, Vec3::Y),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::X, Vec3::NEG_Z),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::NEG_X, Vec3::Y),
    ];
    let tangents = faces
        .iter()
        .flat_map(|&(n, u, v)| {
            let w = n.cross(u).dot(v).signum();
            [[u.x, u.y, u.z, w]; 4]
        })
        .collect();
    let quad = a.add_mesh_with(
        MeshData::cube(1.0),
        MeshExtras::new()
            .with_tangents(tangents)
            .with_uvs1(vec![[0.2, 0.8]; 24]),
    );
    let layers = vec![
        InstancePacked::new(Vec3::new(-1.3, 0.0, 0.0), Quat::IDENTITY, 1.0, Vec4::ONE, 1),
        InstancePacked::new(Vec3::new(3.0, 0.0, -50.0), Quat::IDENTITY, 1.0, Vec4::ONE, 0),
    ];
    let instances = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(layers, BufferUsage::Static));
    let material = Material::color(Vec4::ONE)
        .with_texture(base)
        .with_normal_map(normal, 1.0)
        .with_orm_map(orm)
        .with_emissive_map(emissive, Vec3::splat(2.0))
        .with_metallic_roughness(1.0, 1.0);
    app.world.spawn((InstancedMesh {
        mesh: sphere,
        material,
        instances,
    },));
    let flat = |a: &mut RenderAssets, rgba| a.add_image_linear(ImageData::filled(4, 4, rgba));
    let a = app.resources.expect_mut::<RenderAssets>();
    let quad_normal = flat(a, [e(-0.3), e(0.3), e(0.9), 255]);
    let quad_emissive = a.add_image(ImageData::checker(4, 2, [255, 0, 0, 255], [0, 0, 255, 255]));
    app.world.spawn((
        Transform::from_xyz(1.4, 0.0, 0.0).with_rotation(Quat::from_rotation_y(0.6) * Quat::from_rotation_x(0.3)),
        Mesh3d(quad),
        Material::color(Vec4::new(0.5, 0.6, 0.7, 1.0))
            .with_normal_map(quad_normal, 1.0)
            .with_emissive_map(quad_emissive, Vec3::splat(0.4))
            .with_uv1_maps(MapMask::EMISSIVE),
    ));
    app.world
        .spawn((Transform::from_xyz(0.0, 50.0, 0.0), Mesh3d(filler), Material::default()));
}

#[test]
fn gl_materials_match_default_backend() {
    let Some(mut gl) = app(Some(wgpu::Backends::GL)) else {
        eprintln!("skipping GL test: no GL adapter");
        return;
    };
    assert_eq!(
        gl.resources.expect::<Renderer>().capabilities().adapter.backend,
        wgpu::Backend::Gl
    );
    scene(&mut gl);
    let gl_img = render_to_image(&mut gl).expect("GL capture");
    assert_eq!(gl.resources.expect::<Renderer>().gpu_error_count(), 0);
    let Some(mut native) = app(None) else { return };
    if native.resources.expect::<Renderer>().capabilities().adapter.backend == wgpu::Backend::Gl {
        eprintln!("default backend is GL too: nothing to compare");
        return;
    }
    scene(&mut native);
    let native_img = render_to_image(&mut native).expect("capture");
    if let Ok(dir) = std::env::var("SPACE3D_TEST_OUT") {
        for (name, img) in [("gl", &gl_img), ("native", &native_img)] {
            image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone())
                .expect("size")
                .save(format!("{dir}/gl_materials_{name}.png"))
                .expect("write png");
        }
    }
    let diff = diff_fraction(&gl_img, &native_img, 4);
    assert!(
        diff < 0.01,
        "GL differs from the default backend in {:.2}% of pixels",
        diff * 100.0
    );
    // Both objects are drawn (and lit, not black).
    let lit = gl_img.pixels.chunks(4).filter(|p| p[0] > 40 || p[1] > 40).count();
    assert!(lit > 300, "{lit}");
}

/// Six face colors in wgpu order (+X, −X, +Y, −Y, +Z, −Z), sRGB.
const FACES: [[u8; 4]; 6] = [
    [230, 40, 40, 255],
    [40, 220, 220, 255],
    [40, 220, 40, 255],
    [220, 40, 220, 255],
    [40, 60, 230, 255],
    [230, 220, 40, 255],
];

/// A camera at the origin with a skybox of six solid faces (90° field of view).
fn sky_app(backends: Option<wgpu::Backends>) -> Option<(App, Entity, CubemapHandle)> {
    let mut app = app(backends)?;
    let faces = std::array::from_fn(|i| ImageData::filled(16, 16, FACES[i]));
    let cube = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_cubemap(CubemapSource::from_faces(faces).unwrap());
    let camera = app.world.spawn((
        Transform::IDENTITY,
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Perspective {
                fov_y: 90f32.to_radians(),
                near: 0.1,
                far: 100.0,
            }),
        Skybox::new(cube),
    ));
    Some((app, camera, cube))
}

/// On GL a square 6-layer texture is a `TEXTURE_CUBE_MAP`: the skybox shows each face in its
/// direction (uploads, cube mips rendered face by face, `samplerCube`), and an environment
/// bakes into cube faces (`framebufferTexture2D` with the face targets) and lights a mirror
/// sphere like the default backend does.
#[test]
fn cube_on_gl_is_texture_cube_map() {
    let Some((mut gl, camera, cube)) = sky_app(Some(wgpu::Backends::GL)) else {
        eprintln!("skipping GL test: no GL adapter");
        return;
    };
    let looks = [
        (Vec3::X, Vec3::Y),
        (Vec3::NEG_X, Vec3::Y),
        (Vec3::Y, Vec3::Z),
        (Vec3::NEG_Y, Vec3::Z),
        (Vec3::Z, Vec3::Y),
        (Vec3::NEG_Z, Vec3::Y),
    ];
    for (i, (dir, up)) in looks.into_iter().enumerate() {
        *gl.world.get::<&mut Transform>(camera).unwrap() = Transform::IDENTITY.looking_at(dir, up);
        let img = render_to_image(&mut gl).expect("GL capture");
        let (w, h) = (img.width, img.height);
        let px = img.get(w / 2, h / 2);
        assert!(
            px.iter().zip(FACES[i]).all(|(a, b)| a.abs_diff(b) <= 2),
            "face {i}: {px:?} vs {:?}",
            FACES[i]
        );
    }
    // Image-based lighting: a smooth metal sphere mirrors the faces, rough dielectric beside it.
    let scene = |app: &mut App, camera: Entity, cube: CubemapHandle| {
        let sphere = app
            .resources
            .expect_mut::<RenderAssets>()
            .add_mesh(MeshData::sphere(1.0, 32, 16));
        for (x, metallic, roughness) in [(-1.2, 1.0, 0.1), (1.2, 0.0, 0.8)] {
            app.world.spawn((
                Transform::from_xyz(x, 0.0, -4.0),
                Mesh3d(sphere),
                Material::srgb(0.9, 0.9, 0.9).with_metallic_roughness(metallic, roughness),
            ));
        }
        app.insert_resource(LightingSettings {
            shading: ShadingModel::Standard,
            ..LightingSettings::default()
        });
        *app.world.get::<&mut Transform>(camera).unwrap() = Transform::IDENTITY;
        app.world.insert_one(camera, EnvironmentLight::new(cube)).unwrap();
    };
    scene(&mut gl, camera, cube);
    let gl_img = render_to_image(&mut gl).expect("GL capture");
    let r = gl.resources.expect::<Renderer>();
    assert_eq!(r.environment_state(cube), EnvironmentState::Ready);
    assert_eq!(r.gpu_error_count(), 0);
    let Some((mut native, camera, cube)) = sky_app(None) else {
        return;
    };
    if native.resources.expect::<Renderer>().capabilities().adapter.backend == wgpu::Backend::Gl {
        return;
    }
    scene(&mut native, camera, cube);
    let native_img = render_to_image(&mut native).expect("capture");
    if let Ok(dir) = std::env::var("SPACE3D_TEST_OUT") {
        for (name, img) in [("gl", &gl_img), ("native", &native_img)] {
            image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone())
                .expect("size")
                .save(format!("{dir}/gl_environment_{name}.png"))
                .expect("write png");
        }
    }
    let diff = diff_fraction(&gl_img, &native_img, 6);
    assert!(
        diff < 0.02,
        "GL environment differs from the default backend in {:.2}% of pixels",
        diff * 100.0
    );
}
