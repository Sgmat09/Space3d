//! Lighting stories (`Engine/Environment/*`, `Engine/Lights/Directional Shadow`): skybox, image-based lighting, shadows.
//!
//! Owner: P-MAT (graphics spec §3.6). The environments are generated in code: a nebula (an HDR
//! panorama of warped value noise over a dark sky, a bright core and stars up to 8× white) and a
//! photo studio (a sky gradient, a big softbox and a warm strip light). So are the asteroid
//! field's rocks (noise-displaced spheres) and the rocky ground under them.

use super::{ambient, camera, mesh, sun};
use crate::{KnobValues, Story};
use space3d_assets::render::{ModelUploadOptions, spawn_model, upload_model};
use space3d_core::HdrImageData;
use space3d_core::glam::EulerRot;
use space3d_core::prelude::*;
use space3d_render::{
    Aabb, CubemapHandle, CubemapSource, DirectionalLight, DirectionalShadow, EnvironmentLight, LightingSettings,
    Material, Mesh3d, PointLight, PostProcess, Projection, RenderAssets, SamplerSettings, ShadingModel, ShadowFilter,
    ShadowFit, Skybox, TextureSource, Tonemapper, f32_to_f16,
};
use std::sync::OnceLock;

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    vec![skybox(), ibl_spheres(), nebula_ship(), directional_shadow()]
}

/// The test ship: a hull with baked base color, normal, ORM and emissive maps.
const SHIP_GLB: &[u8] = include_bytes!("../../assets/ship.glb");

fn hash(x: i32, y: i32, z: i32, seed: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343)
        ^ (y as u32).wrapping_mul(0xd816_3841)
        ^ (z as u32).wrapping_mul(0xcb1a_b31f)
        ^ seed.wrapping_mul(0x9e37_79b9);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^ (h >> 16)
}

fn unit(h: u32) -> f32 {
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Trilinear value noise in 0..1 with smoothstep weights.
fn value_noise(p: Vec3, seed: u32) -> f32 {
    let i = p.floor();
    let f = p - i;
    let w = f * f * (Vec3::splat(3.0) - 2.0 * f);
    let (x, y, z) = (i.x as i32, i.y as i32, i.z as i32);
    let v = |dx: i32, dy: i32, dz: i32| unit(hash(x + dx, y + dy, z + dz, seed));
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let x00 = lerp(v(0, 0, 0), v(1, 0, 0), w.x);
    let x10 = lerp(v(0, 1, 0), v(1, 1, 0), w.x);
    let x01 = lerp(v(0, 0, 1), v(1, 0, 1), w.x);
    let x11 = lerp(v(0, 1, 1), v(1, 1, 1), w.x);
    lerp(lerp(x00, x10, w.y), lerp(x01, x11, w.y), w.z)
}

fn fbm(p: Vec3, seed: u32) -> f32 {
    let (mut sum, mut amp, mut freq) = (0.0, 0.5, 1.0);
    for octave in 0..4 {
        sum += amp * value_noise(p * freq, seed + octave);
        amp *= 0.5;
        freq *= 2.03;
    }
    sum / 0.9375
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Linear HDR radiance of the nebula toward the unit direction `d`.
fn nebula(d: Vec3) -> Vec3 {
    // Clouds concentrate in a tilted band (a galactic plane) and around a bright core.
    let band = Vec3::new(0.25, 1.0, 0.35).normalize();
    let core = Vec3::new(0.55, 0.2, -0.8).normalize();
    let in_band = (-(d.dot(band) / 0.45).powi(2)).exp();
    let q = d * 2.3;
    let warp = Vec3::new(
        fbm(q + Vec3::splat(1.7), 11),
        fbm(q + Vec3::splat(9.2), 12),
        fbm(q + Vec3::splat(4.4), 13),
    );
    let n = fbm(q + warp * 1.6, 7);
    let density = smoothstep(0.42, 0.8, n) * (0.35 + 0.65 * in_band);
    let mix_t = smoothstep(0.35, 0.65, fbm(q * 0.8 + Vec3::splat(3.1), 21));
    let magenta = Vec3::new(0.55, 0.1, 0.45);
    let teal = Vec3::new(0.06, 0.35, 0.55);
    let tint = magenta.lerp(teal, mix_t);
    let glow = d.dot(core).max(0.0).powf(28.0);
    let mut c =
        Vec3::new(0.003, 0.004, 0.01) + tint * density * 0.8 + Vec3::new(1.0, 0.55, 0.3) * glow * (0.5 + 2.0 * density);
    // Stars: at most one per cell of a 3D grid on the sphere, gaussian, up to 8× white.
    let g = 70.0;
    let cell = (d * g).floor();
    let h = hash(cell.x as i32, cell.y as i32, cell.z as i32, 99);
    if h % 100 < 6 {
        let r = |i: i32| unit(hash(h as i32, i, 0, 3));
        let star = ((cell + Vec3::new(r(1), r(2), r(3))) / g).normalize();
        let dist = (d - star).length();
        let brightness = 0.6 + 7.4 * r(4).powi(6);
        let color = Vec3::new(1.0, 0.85, 0.7).lerp(Vec3::new(0.7, 0.85, 1.0), r(5));
        c += color * brightness * (-(dist / 0.003).powi(2)).exp();
    }
    c
}

/// Linear HDR radiance of the photo studio toward the unit direction `d`.
fn studio(d: Vec3) -> Vec3 {
    let horizon = Vec3::new(0.55, 0.58, 0.62);
    let sky = if d.y > 0.0 {
        horizon.lerp(Vec3::new(0.75, 0.82, 0.95), d.y.powf(0.6))
    } else {
        horizon.lerp(Vec3::new(0.08, 0.075, 0.07), (-d.y * 4.0).min(1.0))
    };
    // A square softbox up front-left, and a warm strip light low on the right.
    let box_dir = Vec3::new(-0.45, 0.65, 0.6).normalize();
    let right = Vec3::Y.cross(box_dir).normalize();
    let top = box_dir.cross(right);
    let (u, v) = (d.dot(right), d.dot(top));
    let softbox = if d.dot(box_dir) > 0.0 && u.abs() < 0.28 && v.abs() < 0.28 {
        7.0
    } else {
        0.0
    };
    let strip_dir = Vec3::new(0.9, 0.05, -0.2).normalize();
    let strip = if d.dot(strip_dir) > 0.97 && d.y.abs() < 0.05 {
        Vec3::new(5.0, 2.6, 1.2)
    } else {
        Vec3::ZERO
    };
    sky + Vec3::splat(softbox) + strip
}

/// A panorama of `f` (longitude 0 = −Z, +X to its right, top row = +Y), `width` × `width / 2`.
fn panorama(width: u32, f: fn(Vec3) -> Vec3) -> HdrImageData {
    let height = width / 2;
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        let lat = (y as f32 + 0.5) / height as f32 * std::f32::consts::PI;
        for x in 0..width {
            let lon = ((x as f32 + 0.5) / width as f32 - 0.5) * std::f32::consts::TAU;
            let d = Vec3::new(lat.sin() * lon.sin(), lat.cos(), -lat.sin() * lon.cos());
            pixels.push(f(d).extend(1.0).to_array());
        }
    }
    HdrImageData::new(width, height, pixels)
}

/// Six `Rgba16Float` faces of `f` in wgpu order (the cube-map face table).
fn faces(size: u32, f: fn(Vec3) -> Vec3) -> CubemapSource {
    let mut data = Vec::with_capacity((size * size * 6 * 8) as usize);
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
                for c in f(d).extend(1.0).to_array() {
                    data.extend_from_slice(&f32_to_f16(c).to_le_bytes());
                }
            }
        }
    }
    CubemapSource::Faces(TextureSource {
        width: size,
        height: size,
        layers: 6,
        format: space3d_render::wgpu::TextureFormat::Rgba16Float,
        mip_level_count: 1,
        data,
        generate_mipmaps: true,
        sampler: SamplerSettings::default(),
    })
}

/// The nebula panorama (2048 × 1024: 512² faces; generated once per process).
fn nebula_panorama() -> &'static HdrImageData {
    static PANO: OnceLock<HdrImageData> = OnceLock::new();
    PANO.get_or_init(|| panorama(2048, nebula))
}

/// The studio panorama (512 × 256, generated once per process).
fn studio_panorama() -> &'static HdrImageData {
    static PANO: OnceLock<HdrImageData> = OnceLock::new();
    PANO.get_or_init(|| panorama(512, studio))
}

fn cubemap(app: &mut App, source: CubemapSource) -> CubemapHandle {
    app.resources.init::<RenderAssets>().add_cubemap(source)
}

fn shading(app: &mut App, k: &KnobValues) {
    app.insert_resource(LightingSettings {
        shading: if k.choice("shading") == 0 {
            ShadingModel::Standard
        } else {
            ShadingModel::Classic
        },
        ..LightingSettings::default()
    });
}

/// The nebula sky from a panorama (converted on the GPU) or six faces, behind a lit planet.
fn skybox() -> Story {
    Story::new("Engine/Environment/Skybox", |app, k| {
        let source = if k.choice("source") == 0 {
            CubemapSource::from_equirect_hdr(nebula_panorama())
        } else {
            faces(512, nebula)
        };
        let sky = cubemap(app, source);
        let planet = mesh(app, MeshData::sphere(1.0, 48, 24));
        app.world.spawn((
            Name("planet".into()),
            Transform::from_xyz(1.6, -0.9, -4.0),
            Mesh3d(planet),
            Material::srgb(0.55, 0.42, 0.35).with_metallic_roughness(0.0, 0.85),
        ));
        sun(app, Vec3::new(-0.8, -0.2, -0.55), 2.2);
        ambient(app, 0.02);
        let yaw = k.f32("yaw").to_radians();
        let cam = camera(
            app,
            Vec3::ZERO,
            Vec3::new(yaw.sin(), 0.12, -yaw.cos()),
            Projection::Perspective {
                fov_y: 70f32.to_radians(),
                near: 0.1,
                far: 200.0,
            },
        );
        let rotation = Quat::from_rotation_y(k.f32("rotation").to_radians());
        app.world
            .insert(
                cam,
                (
                    Skybox::new(sky)
                        .with_intensity(k.f32("intensity"))
                        .with_rotation(rotation),
                    PostProcess::default(),
                ),
            )
            .unwrap();
    })
    .description(
        "A procedural HDR nebula (warped value noise, a bright core, stars up to 8× white) as the \
         camera's `Skybox`: drawn after opaque geometry at the far plane, in linear HDR so the \
         stars bloom. Source: an equirect panorama converted to 256² faces on the GPU, or six \
         faces uploaded directly (the same picture). Knobs: source, intensity, sky rotation, \
         camera yaw.",
    )
    .knob_choice("source", &["equirect", "faces"], 0)
    .knob_f32("intensity", 1.0, 0.0..=4.0)
    .knob_f32("rotation", 0.0, -180.0..=180.0)
    .knob_f32("yaw", 25.0, -180.0..=180.0)
    .size(320, 200)
}

/// Roughness × metallic sphere grid lit only by an environment (no lights, no ambient).
fn ibl_spheres() -> Story {
    Story::new("Engine/Environment/IBL Spheres", |app, k| {
        shading(app, k);
        let env = if k.choice("environment") == 0 {
            cubemap(app, CubemapSource::from_equirect_hdr(studio_panorama()))
        } else {
            cubemap(app, CubemapSource::from_equirect_hdr(nebula_panorama()))
        };
        ambient(app, 0.0);
        let sphere = mesh(app, MeshData::sphere(0.42, 48, 24));
        let rows = [(1.0, Vec3::new(0.97, 0.74, 0.37)), (0.0, Vec3::new(0.72, 0.08, 0.06))];
        for (row, (metallic, color)) in rows.into_iter().enumerate() {
            for col in 0..5 {
                let roughness = col as f32 / 4.0;
                app.world.spawn((
                    Transform::from_xyz(-2.0 + col as f32, 0.5 - row as f32, 0.0),
                    Mesh3d(sphere),
                    Material::color(color.extend(1.0)).with_metallic_roughness(metallic, roughness),
                ));
            }
        }
        let cam = camera(
            app,
            Vec3::new(0.0, 0.25, 4.6),
            Vec3::ZERO,
            Projection::Perspective {
                fov_y: 38f32.to_radians(),
                near: 0.1,
                far: 100.0,
            },
        );
        app.world
            .insert(
                cam,
                (
                    EnvironmentLight::new(env).with_intensity(k.f32("diffuse"), k.f32("specular")),
                    Skybox::new(env),
                    // Khronos PBR Neutral keeps the base colors (gold stays gold).
                    PostProcess::default()
                        .without_bloom()
                        .with_tonemapper(Tonemapper::KhronosPbrNeutral),
                ),
            )
            .unwrap();
    })
    .description(
        "Image-based lighting alone: a gold metal row and a red dielectric row, roughness 0 → 1 \
         left to right, in a procedural photo studio (sky gradient, a 7× softbox up front-left, a \
         warm strip light on the right). Irradiance and GGX-prefiltered specular are baked on the \
         GPU from the cubemap; the split sum with multiple scattering keeps rough metals bright. \
         Knobs: environment (studio / nebula), diffuse and specular intensity, shading model.",
    )
    .knob_choice("environment", &["studio", "nebula"], 0)
    .knob_f32("diffuse", 1.0, 0.0..=2.0)
    .knob_f32("specular", 1.0, 0.0..=2.0)
    .knob_choice("shading", &["Standard", "Classic"], 0)
    .size(320, 160)
}

/// The test ship in the nebula: environment lighting, a sun, bloom on its emissive panels.
fn nebula_ship() -> Story {
    Story::new("Engine/Environment/Nebula Ship", |app, k| {
        shading(app, k);
        let env = cubemap(app, CubemapSource::from_equirect_hdr(nebula_panorama()));
        let model = space3d_assets::loaders::load_gltf(SHIP_GLB, &space3d_assets::LoadContext::new("ship.glb"), true)
            .expect("ship.glb decodes");
        let assets = app.resources.init::<RenderAssets>();
        let uploaded = upload_model(&model, assets, &ModelUploadOptions::default());
        let yaw = k.f32("yaw").to_radians();
        spawn_model(
            &mut app.world,
            &uploaded,
            Transform::IDENTITY.with_rotation(Quat::from_rotation_y(yaw)),
        );
        sun(app, Vec3::new(-0.6, -0.55, -0.5), k.f32("sun"));
        app.world.spawn((
            Transform::from_xyz(1.8, -0.6, 1.6),
            PointLight {
                color: Vec3::new(0.3, 0.7, 1.0),
                intensity: 2.5,
                range: 4.0,
            },
        ));
        ambient(app, 0.0);
        // Low and to the left: the top panels mirror the sky toward the nebula's bright core.
        let cam = camera(
            app,
            Vec3::new(-2.4, 1.0, 3.0),
            Vec3::new(0.0, -0.15, 0.0),
            Projection::Perspective {
                fov_y: 45f32.to_radians(),
                near: 0.1,
                far: 200.0,
            },
        );
        let intensity = if k.bool("environment") { k.f32("ibl") } else { 0.0 };
        app.world
            .insert(
                cam,
                (
                    EnvironmentLight::new(env).with_intensity(intensity, intensity),
                    Skybox::new(env),
                    PostProcess::default(),
                ),
            )
            .unwrap();
    })
    .description(
        "The test ship `assets/ship.glb` (baked normal, ORM and emissive maps) in the procedural \
         nebula: the same cubemap is the sky and the image-based lighting, so the metal panels \
         reflect the nebula and its stars and the dielectric hull picks up its colors; a warm sun \
         and a magenta point light add highlights, bloom lets the emissive engines and stars glow \
         (AgX). Knobs: environment lighting on/off and its intensity, sun intensity, yaw, \
         shading model.",
    )
    .knob_bool("environment", true)
    .knob_f32("ibl", 1.5, 0.0..=4.0)
    .knob_f32("sun", 2.2, 0.0..=6.0)
    .knob_f32("yaw", 150.0, -180.0..=180.0)
    .knob_choice("shading", &["Standard", "Classic"], 0)
    .size(320, 240)
}

/// An asteroid: a sphere displaced by value noise (seeded), with smooth normals.
fn asteroid(seed: u32) -> MeshData {
    let mut m = MeshData::sphere(1.0, 28, 18);
    for p in &mut m.positions {
        let d = Vec3::from_array(*p);
        let bumps = fbm(d * 1.6 + Vec3::splat(seed as f32 * 3.7), seed) - 0.5;
        let craters = (fbm(d * 3.1, seed + 40) - 0.5).abs();
        *p = (d * (1.0 + 0.55 * bumps - 0.25 * craters)).to_array();
    }
    space3d_assets::edit::mesh::recalculate_smooth_normals(&mut m);
    m
}

/// Height of the rocky ground at (x, z).
fn ground_height(x: f32, z: f32) -> f32 {
    let p = Vec3::new(x * 0.09, 0.0, z * 0.09);
    (fbm(p, 5) - 0.5) * 2.2 + (fbm(p * 3.0, 6) - 0.5) * 0.5
}

/// The rocky ground: a `size`-wide grid of `n` × `n` cells displaced by [`ground_height`],
/// colored by height (vertex colors), normals from central differences.
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
            let nx = ground_height(x - e, z) - ground_height(x + e, z);
            let nz = ground_height(x, z - e) - ground_height(x, z + e);
            m.positions.push([x, y, z]);
            m.normals.push(Vec3::new(nx, 2.0 * e, nz).normalize().to_array());
            m.uvs.push([i as f32 / n as f32, j as f32 / n as f32]);
            let t = ((y + 1.2) / 2.4).clamp(0.0, 1.0);
            let c = Vec3::new(0.05, 0.05, 0.055).lerp(Vec3::new(0.2, 0.18, 0.17), t * t);
            m.colors.push(c.extend(1.0).to_array());
        }
    }
    let row = n + 1;
    for j in 0..n {
        for i in 0..n {
            let a = j * row + i;
            m.indices
                .extend_from_slice(&[a, a + row, a + 1, a + 1, a + row, a + row + 1]);
        }
    }
    m
}

/// A top-down asteroid field under one shadow-casting sun: three test ships over drifting rocks
/// over a rocky ground, in the game's camera angle.
fn directional_shadow() -> Story {
    Story::new("Engine/Lights/Directional Shadow", |app, k| {
        app.insert_resource(LightingSettings {
            shading: ShadingModel::Standard,
            ..LightingSettings::default()
        });
        ambient(app, 0.03);
        let floor = mesh(app, ground(90.0, 120));
        app.world.spawn((
            Name("ground".into()),
            Transform::from_xyz(0.0, -4.0, 0.0),
            Mesh3d(floor),
            Material::color(Vec4::ONE).with_metallic_roughness(0.0, 0.95),
        ));
        // Rocks of three shapes at scattered heights and sizes (deterministic).
        let rocks = [0, 1, 2].map(|seed| mesh(app, asteroid(seed)));
        for i in 0..46u32 {
            let r = |c: i32| unit(hash(i as i32, c, 0, 17));
            let (x, z) = (r(1) * 44.0 - 22.0, r(2) * 30.0 - 15.0);
            let size = 0.35 + 1.4 * r(3).powi(2);
            let y = -1.5 + r(4) * 4.5;
            let tint = 0.25 + 0.2 * r(5);
            app.world.spawn((
                Transform::from_xyz(x, y, z)
                    .with_rotation(Quat::from_euler(EulerRot::YXZ, r(6) * 6.3, r(7) * 6.3, 0.0))
                    .with_scale(Vec3::new(size, size * (0.7 + 0.3 * r(8)), size)),
                Mesh3d(rocks[i as usize % 3]),
                Material::srgb(tint * 1.1, tint, tint * 0.92).with_metallic_roughness(0.0, 0.85),
            ));
        }
        // Three ships flying over the field.
        let model = space3d_assets::loaders::load_gltf(SHIP_GLB, &space3d_assets::LoadContext::new("ship.glb"), true)
            .expect("ship.glb decodes");
        let uploaded = upload_model(
            &model,
            app.resources.init::<RenderAssets>(),
            &ModelUploadOptions::default(),
        );
        for (x, y, z, yaw, scale) in [
            (-4.0, 5.0, 1.5, 200.0f32, 2.2f32),
            (5.5, 3.5, -4.0, 235.0, 1.6),
            (9.0, 6.5, 5.0, 170.0, 1.9),
        ] {
            spawn_model(
                &mut app.world,
                &uploaded,
                Transform::from_xyz(x, y, z)
                    .with_rotation(Quat::from_rotation_y(yaw.to_radians()))
                    .with_scale(Vec3::splat(scale)),
            );
        }
        // The sun: `elevation` degrees above the horizon, from the upper right of the view.
        let elevation = k.f32("elevation").to_radians();
        let direction = Vec3::new(-0.8 * elevation.cos(), -elevation.sin(), -0.6 * elevation.cos());
        let fit = match k.choice("fit") {
            0 => ShadowFit::top_down(0.0, 8.0, 200.0),
            1 => ShadowFit::Camera {
                camera: None,
                max_distance: 80.0,
                receivers: None,
            },
            _ => ShadowFit::Bounds(Aabb::new(Vec3::new(-30.0, -6.0, -24.0), Vec3::new(30.0, 9.0, 24.0))),
        };
        let filter = [
            ShadowFilter::Pcf9,
            ShadowFilter::Pcf16,
            ShadowFilter::Pcf4,
            ShadowFilter::Hard,
        ][k.choice("filter").min(3)];
        app.world.spawn((
            Name("sun".into()),
            Transform::IDENTITY.looking_at(direction, Vec3::Y),
            DirectionalLight {
                color: Vec3::new(1.0, 0.95, 0.88),
                intensity: 2.6,
            },
            DirectionalShadow::new()
                .with_map_size([2048, 1024, 512][k.choice("map size").min(2)])
                .with_fit(fit)
                .with_filter(filter)
                .with_bias(k.f32("depth bias"), k.f32("normal bias"), k.f32("slope bias"))
                .with_strength(k.f32("strength")),
        ));
        let cam = camera(
            app,
            Vec3::new(0.0, 30.0, 13.0),
            Vec3::new(0.0, 0.0, 0.5),
            Projection::Perspective {
                fov_y: 42f32.to_radians(),
                near: 0.5,
                far: 200.0,
            },
        );
        app.world.insert_one(cam, PostProcess::default()).unwrap();
    })
    .description(
        "A top-down asteroid field (the game's camera angle) under one shadow-casting sun: three \
         copies of the test ship over noise-displaced rocks over a rocky ground. The sun's \
         `DirectionalShadow` is one 2048² depth map fitted to the camera frustum within the slab \
         |y| ≤ 8 (`ShadowFit::top_down`), texel-snapped so it never shimmers while the camera \
         pans; the casters are drawn instanced in a depth-only pass culled like a camera view; \
         receivers filter it with 9-tap PCF. Knobs: map size, filter, fit (top-down / camera / \
         fixed bounds), depth / normal / slope bias, strength, sun elevation.",
    )
    .knob_choice("map size", &["2048", "1024", "512"], 0)
    .knob_choice("filter", &["PCF 9", "PCF 16", "PCF 4", "hard"], 0)
    .knob_choice("fit", &["top-down", "camera", "bounds"], 0)
    .knob_f32("depth bias", 0.02, 0.0..=0.5)
    .knob_f32("normal bias", 1.0, 0.0..=4.0)
    .knob_f32("slope bias", 1.5, 0.0..=6.0)
    .knob_f32("strength", 1.0, 0.0..=1.0)
    .knob_f32("elevation", 48.0, 10.0..=90.0)
    .size(320, 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space3d_test::{CompareOptions, compare_images, require_gpu};
    use crate::{KnobValue, run_story_headless};

    /// The panorama and six-face sources of the skybox story give the same picture, and the
    /// environment lighting changes the ship.
    #[test]
    fn sky_sources_agree_and_environment_matters() {
        require_gpu!();
        let story = skybox();
        let equirect = run_story_headless(&story, &KnobValues::default(), None).unwrap();
        let faces = KnobValues::default().with("source", KnobValue::Choice(1));
        let faces = run_story_headless(&story, &faces, None).unwrap();
        // Stars are about a texel wide: a converted panorama and faces sampled at their texel
        // centers catch them differently, so the two agree as pictures (32 × 32 block means),
        // not pixel by pixel.
        let blocks = |img: &ImageData| {
            let mut out = Vec::new();
            for by in 0..img.height / 32 {
                for bx in 0..img.width / 32 {
                    let mut sum = [0u32; 3];
                    for y in by * 32..by * 32 + 32 {
                        for x in bx * 32..bx * 32 + 32 {
                            let p = img.get(x, y);
                            for c in 0..3 {
                                sum[c] += p[c] as u32;
                            }
                        }
                    }
                    out.push(sum.map(|s| s as f32 / 1024.0));
                }
            }
            out
        };
        let (a, b) = (blocks(&equirect), blocks(&faces));
        let worst = a
            .iter()
            .zip(&b)
            .flat_map(|(p, q)| (0..3).map(move |c| (p[c] - q[c]).abs()))
            .fold(0.0, f32::max);
        assert!(worst < 6.0, "equirect vs faces: block means differ by {worst}");
        let ship = nebula_ship();
        let lit = run_story_headless(&ship, &KnobValues::default(), None).unwrap();
        let unlit = KnobValues::default().with("environment", KnobValue::Bool(false));
        let unlit = run_story_headless(&ship, &unlit, None).unwrap();
        assert!(!compare_images(&lit, &unlit, &CompareOptions::default()).passed);
    }

    /// The shadow story's shadow is there (strength 0 is another picture) and every fit and
    /// filter renders it.
    #[test]
    fn shadow_story_knobs() {
        require_gpu!();
        let story = directional_shadow();
        let shadowed = run_story_headless(&story, &KnobValues::default(), None).unwrap();
        let off = KnobValues::default().with("strength", KnobValue::Float(0.0));
        let off = run_story_headless(&story, &off, None).unwrap();
        assert!(!compare_images(&shadowed, &off, &CompareOptions::default()).passed);
        // The shadowed picture is darker overall.
        let mean = |img: &ImageData| img.pixels.iter().map(|&p| p as f64).sum::<f64>() / img.pixels.len() as f64;
        assert!(
            mean(&shadowed) < mean(&off) - 1.0,
            "{} vs {}",
            mean(&shadowed),
            mean(&off)
        );
        for (knob, choices) in [("fit", 3), ("filter", 4)] {
            for c in 1..choices {
                let k = KnobValues::default().with(knob, KnobValue::Choice(c));
                let img = run_story_headless(&story, &k, None).unwrap();
                assert!(mean(&img) < mean(&off) - 1.0, "{knob} {c}");
            }
        }
    }

    /// The generators are pure and the nebula is HDR: stars and the core exceed white, the sky
    /// stays dark on average.
    #[test]
    fn nebula_is_deterministic_and_hdr() {
        let d = Vec3::new(0.3, 0.2, -0.9).normalize();
        assert_eq!(nebula(d), nebula(d));
        let pano = nebula_panorama();
        let peak = pano.pixels.iter().map(|p| p[0].max(p[1]).max(p[2])).fold(0.0, f32::max);
        assert!(peak > 2.0, "stars and the core exceed white: {peak}");
        let n = pano.pixels.len() as f32;
        let mean = pano.pixels.iter().map(|p| p[0] + p[1] + p[2]).sum::<f32>() / n / 3.0;
        assert!(mean > 0.01 && mean < 0.5, "a dark sky with clouds: {mean}");
    }
}
