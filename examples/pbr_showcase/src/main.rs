//! The materials and lighting of Space3d in one top-down space scene (the SpaceCorps camera
//! angle): the test ship `ship.glb` (baked base color, normal, ORM and emissive maps, lit
//! "straight from glTF"), a procedural nebula as skybox and image-based lighting, one sun whose
//! `DirectionalShadow` falls on rocks and a rocky ground, and every blend mode: an alpha-blended
//! shield, additive beacons, a premultiplied haze and an alpha-masked solar grate that casts a
//! cut-out shadow.
//!
//! ```text
//! cargo run -p pbr_showcase
//! SPACE3D_EXIT_AFTER_FRAMES=120 cargo run -p pbr_showcase   # smoke test: quit after 120 frames
//! SPACE3D_HEADLESS=1 SPACE3D_EXIT_AFTER_FRAMES=60 cargo run -p pbr_showcase   # no window (CI)
//! ```
//! Controls: `Esc` quits, `Space` pauses, `S` toggles the shadow, `E` the environment light.
//!
//! Content is generated off the frame (AGENTS rule 2): the glTF decode, its MikkTSpace tangents,
//! the sky's faces and the rock meshes run on the compute pool, and a system spawns them when the
//! task is done. On the web the environment bake is spread over frames
//! (`IblSettings::bake_passes_per_frame`) and the shadow map is 1024².

use space3d::assets::render::{ModelUploadOptions, PreparedModel, prepare_model, spawn_model};
use space3d::prelude::*;
use space3d::render::{
    BlendMode, CubemapSource, IblSettings, LightingSettings, RenderAssets, SamplerSettings, ShadingModel, ShadowFit,
    TextureSource, f32_to_f16,
};

/// The test ship (baked maps, no `TANGENT`: the upload computes MikkTSpace tangents).
const SHIP_GLB: &[u8] = include_bytes!("../../../crates/space3d-stories/assets/ship.glb");

/// Everything generated off the frame.
struct Content {
    ship: PreparedModel,
    sky: CubemapSource,
    rocks: [MeshData; 3],
    ground: MeshData,
}

/// The content task, until it is done.
#[derive(Default)]
struct Loading(Option<Task<Result<Content, String>>>);

/// Scene state: the sun, the camera, the ships and what the keys toggle.
#[derive(Default)]
struct Showcase {
    sun: Option<Entity>,
    camera: Option<Entity>,
    ships: Vec<(Entity, Vec3, f32)>,
    paused: bool,
    shadow: bool,
    environment: bool,
    sky: Option<CubemapHandle>,
}

fn build(app: &mut App) {
    app.add_plugin(DefaultPlugins::new().title("Space3d — PBR showcase"));
    add_game(app);
}

/// The scene and its systems, without the engine plugins (the tests add them to a headless app).
fn add_game(app: &mut App) {
    app.init_resource::<Loading>();
    app.init_resource::<Showcase>();
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::Update, spawn_content);
    app.add_system(Stage::Update, controls);
    app.add_system(Stage::Update, animate);
}

fn setup(ctx: &mut SystemCtx) {
    let web = cfg!(target_arch = "wasm32");
    ctx.resources.insert(LightingSettings {
        shading: ShadingModel::Standard,
        ibl: IblSettings {
            // WebGL2 links and draws the bake passes slowly: a few per frame.
            bake_passes_per_frame: web.then_some(6),
            ..IblSettings::default()
        },
        ..LightingSettings::default()
    });
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.55, 0.65, 1.0),
        intensity: 0.02,
    });
    let sun = ctx.world.spawn((
        Name("sun".into()),
        Transform::IDENTITY.looking_at(Vec3::new(-0.55, -0.75, -0.4), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.95, 0.88),
            intensity: 2.8,
        },
        sun_shadow(web),
    ));
    let camera = ctx.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(0.0, 26.0, 12.0).looking_at(Vec3::new(0.0, 0.0, 0.5), Vec3::Y),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Perspective {
                fov_y: 45f32.to_radians(),
                near: 0.5,
                far: 400.0,
            }),
        PostProcess::default(),
    ));
    let s = ctx.resources.expect_mut::<Showcase>();
    s.sun = Some(sun);
    s.camera = Some(camera);
    s.shadow = true;
    s.environment = true;
    ctx.resources.expect_mut::<Loading>().0 = Some(spawn_compute(generate));
}

/// The sun's shadow: fitted to the camera over the slab where the scene lives (`top_down`).
fn sun_shadow(web: bool) -> DirectionalShadow {
    DirectionalShadow::new()
        .with_map_size(if web { 1024 } else { 2048 })
        .with_fit(ShadowFit::top_down(0.0, 8.0, 200.0))
}

/// The CPU part of the content (compute pool).
fn generate() -> Result<Content, String> {
    let model = space3d::assets::loaders::load_gltf(SHIP_GLB, &space3d::assets::LoadContext::new("ship.glb"), true)
        .map_err(|e| e.to_string())?;
    Ok(Content {
        ship: prepare_model(&model, &ModelUploadOptions::default()),
        sky: nebula_faces(128),
        rocks: [0, 1, 2].map(rock),
        ground: ground(120.0, 96),
    })
}

/// Register the content and spawn the scene once the task is done.
fn spawn_content(ctx: &mut SystemCtx) {
    let loading = ctx.resources.expect_mut::<Loading>();
    let Some(result) = loading.0.as_mut().and_then(|t| t.poll()) else {
        return;
    };
    loading.0 = None;
    let content = match result {
        Ok(c) => c,
        Err(e) => {
            space3d::tracing::error!("pbr_showcase: content failed: {e}");
            return;
        }
    };
    let assets = ctx.resources.expect_mut::<RenderAssets>();
    let ship = content.ship.upload(assets);
    let sky = assets.add_cubemap(content.sky);
    let rocks = content.rocks.map(|m| assets.add_mesh(m));
    let ground = assets.add_mesh(content.ground);
    let sphere = assets.add_mesh(MeshData::sphere(1.0, 32, 16));
    let quad = assets.add_mesh(MeshData::plane(1.0));
    let grate = assets.add_image(ImageData::checker(64, 8, [210, 215, 225, 255], [0, 0, 0, 0]));
    let puff = assets.add_image_linear(soft_disc(64));

    ctx.world.spawn((
        Name("ground".into()),
        Transform::from_xyz(0.0, -4.0, 0.0),
        Mesh3d(ground),
        Material::color(Vec4::ONE).with_metallic_roughness(0.0, 0.95),
    ));
    for i in 0..40u32 {
        let r = |c: u32| unit(hash(i, c, 7));
        let size = 0.35 + 1.3 * r(3).powi(2);
        ctx.world.spawn((
            Transform::from_xyz(r(1) * 44.0 - 22.0, -1.5 + r(4) * 4.0, r(2) * 30.0 - 15.0)
                .with_rotation(Quat::from_euler(EulerRot::YXZ, r(5) * 6.3, r(6) * 6.3, 0.0))
                .with_scale(Vec3::splat(size)),
            Mesh3d(rocks[i as usize % 3]),
            Material::srgb(0.32 + 0.1 * r(7), 0.3 + 0.08 * r(7), 0.28).with_metallic_roughness(0.0, 0.85),
        ));
    }
    let mut ships = Vec::new();
    for (i, (at, yaw, scale)) in [
        (Vec3::new(-3.5, 5.0, 1.0), 200.0f32, 2.2f32),
        (Vec3::new(6.0, 3.5, -4.0), 235.0, 1.6),
        (Vec3::new(8.5, 6.5, 5.5), 170.0, 1.8),
    ]
    .into_iter()
    .enumerate()
    {
        let root = spawn_model(
            ctx.world,
            &ship,
            Transform::from_xyz(at.x, at.y, at.z)
                .with_rotation(Quat::from_rotation_y(yaw.to_radians()))
                .with_scale(Vec3::splat(scale)),
        );
        ctx.world.insert_one(root, Name(format!("ship {i}"))).unwrap();
        ships.push((root, at, yaw.to_radians()));
    }
    // Blend modes: a shield around the lead ship (alpha blended, lit; transparent materials
    // receive shadows but cast none), beacons (additive, unlit), haze (premultiplied: adds its
    // color and dims what is behind) and a solar grate (alpha mask: a cut-out shadow).
    let lead = ships[0].1;
    ctx.world.spawn((
        Name("shield".into()),
        Transform::from_xyz(lead.x, lead.y, lead.z).with_scale(Vec3::splat(2.6)),
        Mesh3d(sphere),
        Material::color(Vec4::new(0.3, 0.65, 1.0, 0.18))
            .with_metallic_roughness(0.0, 0.1)
            .with_blend(BlendMode::AlphaBlend),
    ));
    for (i, c) in [
        Vec3::new(1.0, 0.35, 0.1),
        Vec3::new(0.2, 0.9, 0.4),
        Vec3::new(0.3, 0.5, 1.0),
    ]
    .into_iter()
    .enumerate()
    {
        ctx.world.spawn((
            Name(format!("beacon {i}")),
            Transform::from_xyz(-12.0 + 9.0 * i as f32, 1.0, 8.0).with_scale(Vec3::splat(0.35)),
            Mesh3d(sphere),
            Material::additive((c * 3.0).extend(1.0)),
        ));
    }
    for (i, (x, z, size)) in [(-10.0f32, -6.0f32, 9.0f32), (-6.5, -3.5, 6.0), (-13.0, -2.0, 5.0)]
        .into_iter()
        .enumerate()
    {
        ctx.world.spawn((
            Name(format!("haze {i}")),
            Transform::from_xyz(x, 0.5 + i as f32 * 0.1, z).with_scale(Vec3::new(size, 1.0, size * 0.8)),
            Mesh3d(quad),
            Material::premultiplied(Vec4::new(0.08, 0.03, 0.12, 0.35)).with_texture(puff),
        ));
    }
    ctx.world.spawn((
        Name("solar grate".into()),
        Transform::from_xyz(13.0, 2.0, -9.0).with_scale(Vec3::new(5.0, 1.0, 3.0)),
        Mesh3d(quad),
        Material {
            double_sided: true,
            ..Material::srgb(0.75, 0.78, 0.85)
                .with_texture(grate)
                .with_metallic_roughness(0.9, 0.35)
                .with_alpha_mask(0.5)
        },
    ));
    let s = ctx.resources.expect_mut::<Showcase>();
    s.ships = ships;
    s.sky = Some(sky);
    if let Some(camera) = s.camera {
        let environment = s.environment;
        let light = EnvironmentLight::new(sky).with_intensity(0.8, 1.0);
        ctx.world.insert(camera, (Skybox::new(sky), light)).unwrap();
        if !environment {
            ctx.world.remove_one::<EnvironmentLight>(camera).ok();
        }
    }
}

fn controls(ctx: &mut SystemCtx) {
    let input = ctx.resources.expect::<Input>();
    let (quit, pause, shadow, env) = (
        input.just_pressed(KeyCode::Escape),
        input.just_pressed(KeyCode::Space),
        input.just_pressed(KeyCode::KeyS),
        input.just_pressed(KeyCode::KeyE),
    );
    if quit {
        ctx.send(AppExit);
    }
    let s = ctx.resources.expect_mut::<Showcase>();
    if pause {
        s.paused = !s.paused;
    }
    if shadow {
        s.shadow = !s.shadow;
        if let Some(sun) = s.sun {
            if s.shadow {
                ctx.world.insert_one(sun, sun_shadow(cfg!(target_arch = "wasm32"))).ok();
            } else {
                ctx.world.remove_one::<DirectionalShadow>(sun).ok();
            }
        }
    }
    if env {
        s.environment = !s.environment;
        if let (Some(camera), Some(sky)) = (s.camera, s.sky) {
            if s.environment {
                ctx.world
                    .insert_one(camera, EnvironmentLight::new(sky).with_intensity(0.8, 1.0))
                    .ok();
            } else {
                ctx.world.remove_one::<EnvironmentLight>(camera).ok();
            }
        }
    }
}

/// The ships drift and bank slowly; their shadows follow on the rocks and the ground.
fn animate(ctx: &mut SystemCtx) {
    let t = ctx.time().elapsed_secs() as f32;
    let s = ctx.resources.expect::<Showcase>();
    if s.paused {
        return;
    }
    for (i, &(ship, at, yaw)) in s.ships.iter().enumerate() {
        let phase = t * 0.4 + i as f32 * 2.1;
        if let Ok(mut transform) = ctx.world.get::<&mut Transform>(ship) {
            transform.translation = at + Vec3::new(phase.sin() * 1.5, (phase * 1.3).sin() * 0.3, phase.cos() * 0.8);
            transform.rotation =
                Quat::from_rotation_y(yaw + phase.cos() * 0.15) * Quat::from_rotation_z(phase.sin() * 0.12);
        }
    }
}

/// A soft disc, premultiplied (color = coverage = a smooth falloff), for the haze.
fn soft_disc(size: u32) -> ImageData {
    let mut img = ImageData::new(size, size);
    for y in 0..size {
        for x in 0..size {
            let (u, v) = (
                (x as f32 + 0.5) / size as f32 * 2.0 - 1.0,
                (y as f32 + 0.5) / size as f32 * 2.0 - 1.0,
            );
            let f = (1.0 - (u * u + v * v)).clamp(0.0, 1.0).powi(2);
            let b = (f * 255.0).round() as u8;
            img.set(x, y, [b, b, b, b]);
        }
    }
    img
}

fn hash(x: u32, y: u32, seed: u32) -> u32 {
    let mut h = x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0x9e37_79b9);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^ (h >> 16)
}

fn unit(h: u32) -> f32 {
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Smooth 3D value noise in 0..1.
fn noise(p: Vec3, seed: u32) -> f32 {
    let i = p.floor();
    let f = p - i;
    let w = f * f * (Vec3::splat(3.0) - 2.0 * f);
    let corner = |dx: i32, dy: i32, dz: i32| {
        let (x, y, z) = (i.x as i32 + dx, i.y as i32 + dy, i.z as i32 + dz);
        unit(hash(x as u32 ^ (z as u32).rotate_left(16), y as u32, seed))
    };
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let x0 = lerp(
        lerp(corner(0, 0, 0), corner(1, 0, 0), w.x),
        lerp(corner(0, 1, 0), corner(1, 1, 0), w.x),
        w.y,
    );
    let x1 = lerp(
        lerp(corner(0, 0, 1), corner(1, 0, 1), w.x),
        lerp(corner(0, 1, 1), corner(1, 1, 1), w.x),
        w.y,
    );
    lerp(x0, x1, w.z)
}

fn fbm(p: Vec3, seed: u32) -> f32 {
    (0..4)
        .map(|o| noise(p * 2f32.powi(o), seed + o as u32) * 0.5f32.powi(o + 1))
        .sum::<f32>()
        / 0.9375
}

/// A rock: a sphere displaced by noise, smooth normals.
fn rock(seed: u32) -> MeshData {
    let mut m = MeshData::sphere(1.0, 28, 18);
    for p in &mut m.positions {
        let d = Vec3::from_array(*p);
        let bumps = fbm(d * 1.6 + Vec3::splat(seed as f32 * 3.7), seed) - 0.5;
        *p = (d * (1.0 + 0.55 * bumps)).to_array();
    }
    space3d::assets::edit::mesh::recalculate_smooth_normals(&mut m);
    m
}

fn ground_height(x: f32, z: f32) -> f32 {
    (fbm(Vec3::new(x * 0.08, 0.0, z * 0.08), 5) - 0.5) * 2.4
}

/// A rocky ground: a displaced grid, darker in the hollows (vertex colors).
fn ground(size: f32, n: u32) -> MeshData {
    let mut m = MeshData {
        positions: Vec::new(),
        normals: Vec::new(),
        uvs: Vec::new(),
        colors: Vec::new(),
        indices: Vec::new(),
    };
    let step = size / n as f32;
    for j in 0..=n {
        for i in 0..=n {
            let (x, z) = (i as f32 * step - size * 0.5, j as f32 * step - size * 0.5);
            let y = ground_height(x, z);
            let e = step * 0.5;
            let normal = Vec3::new(
                ground_height(x - e, z) - ground_height(x + e, z),
                2.0 * e,
                ground_height(x, z - e) - ground_height(x, z + e),
            );
            m.positions.push([x, y, z]);
            m.normals.push(normal.normalize().to_array());
            m.uvs.push([i as f32 / n as f32, j as f32 / n as f32]);
            let t = ((y + 1.2) / 2.4).clamp(0.0, 1.0);
            m.colors.push(
                Vec3::new(0.05, 0.05, 0.055)
                    .lerp(Vec3::new(0.2, 0.18, 0.17), t * t)
                    .extend(1.0)
                    .to_array(),
            );
        }
    }
    for j in 0..n {
        for i in 0..n {
            let a = j * (n + 1) + i;
            m.indices
                .extend_from_slice(&[a, a + n + 1, a + 1, a + 1, a + n + 1, a + n + 2]);
        }
    }
    m
}

/// Linear HDR radiance of the procedural nebula toward `d`: a dark sky, magenta and teal
/// clouds, a warm glow and stars brighter than white (they bloom).
fn nebula(d: Vec3) -> Vec3 {
    let n = fbm(d * 2.5 + Vec3::splat(4.0), 11);
    let clouds = ((n - 0.45) * 3.0).clamp(0.0, 1.0);
    let tint = Vec3::new(0.5, 0.1, 0.45).lerp(Vec3::new(0.05, 0.35, 0.5), fbm(d * 1.3, 21));
    let glow = d.dot(Vec3::new(0.6, 0.3, -0.75).normalize()).max(0.0).powf(24.0);
    let cell = (d * 60.0).floor();
    let h = hash(
        cell.x as i32 as u32,
        (cell.y as i32 as u32) ^ (cell.z as i32 as u32).rotate_left(11),
        99,
    );
    let star = if h % 100 < 5 {
        let c = ((cell + Vec3::splat(0.5)) / 60.0).normalize();
        6.0 * (-((d - c).length() / 0.004).powi(2)).exp()
    } else {
        0.0
    };
    Vec3::new(0.002, 0.003, 0.008) + tint * clouds * 0.6 + Vec3::new(1.0, 0.6, 0.35) * glow * 1.5 + Vec3::splat(star)
}

/// Six `Rgba16Float` faces of [`nebula`] (cube-map face order +X, −X, +Y, −Y, +Z, −Z).
fn nebula_faces(size: u32) -> CubemapSource {
    let mut data = Vec::with_capacity((size * size * 48) as usize);
    for face in 0..6 {
        for y in 0..size {
            for x in 0..size {
                let s = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let t = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let d = match face {
                    0 => Vec3::new(1.0, -t, -s),
                    1 => Vec3::new(-1.0, -t, s),
                    2 => Vec3::new(s, 1.0, t),
                    3 => Vec3::new(s, -1.0, -t),
                    4 => Vec3::new(s, -t, 1.0),
                    _ => Vec3::new(-s, -t, -1.0),
                }
                .normalize();
                for c in nebula(d).extend(1.0).to_array() {
                    data.extend_from_slice(&f32_to_f16(c).to_le_bytes());
                }
            }
        }
    }
    CubemapSource::Faces(TextureSource {
        width: size,
        height: size,
        layers: 6,
        format: space3d::wgpu::TextureFormat::Rgba16Float,
        mip_level_count: 1,
        data,
        generate_mipmaps: true,
        sampler: SamplerSettings::default(),
    })
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    /// The content loads off the frame and renders headless with the shadow (one depth view)
    /// and a baked environment; `S` removes the shadow.
    #[test]
    fn showcase_renders_with_shadow_and_environment() {
        let mut app = App::new();
        app.add_plugin(DefaultPlugins::headless(320, 200));
        if app.resources.get::<Renderer>().is_none() {
            eprintln!("skipping: no GPU adapter");
            return;
        }
        add_game(&mut app);
        app.step_ticks(1);
        for _ in 0..10_000 {
            if app.resources.expect::<Loading>().0.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            app.step_ticks(1);
        }
        assert!(!app.resources.expect::<Showcase>().ships.is_empty(), "content spawned");
        let img = render_to_image(&mut app).expect("capture");
        if let Ok(dir) = std::env::var("SPACE3D_TEST_OUT") {
            // A binary PPM (no image dependency): the capture for inspection.
            let mut ppm = format!("P6 {} {} 255\n", img.width, img.height).into_bytes();
            ppm.extend(img.pixels.chunks(4).flat_map(|p| [p[0], p[1], p[2]]));
            std::fs::write(format!("{dir}/pbr_showcase.ppm"), ppm).unwrap();
        }
        let stats = app.resources.expect::<RenderStats>().clone();
        assert_eq!(stats.depth_views, 1, "{stats:?}");
        assert!(stats.shadow_casters > 40, "{stats:?}");
        let r = app.resources.expect::<Renderer>();
        let sky = app.resources.expect::<Showcase>().sky.unwrap();
        assert_eq!(r.environment_state(sky), space3d::render::EnvironmentState::Ready);
        assert_eq!(r.gpu_error_count(), 0);
        let lit = img.pixels.chunks(4).filter(|p| p[0] > 40).count();
        assert!(lit > 5000, "{lit} lit pixels");

        app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::KeyS);
        app.step_ticks(1);
        render_to_image(&mut app).expect("capture");
        assert_eq!(app.resources.expect::<RenderStats>().depth_views, 0);
    }
}
