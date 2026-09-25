//! The engine basics: one story per core renderer/engine feature (the first engine stories;
//! ids and goldens unchanged).

use super::{ambient, camera, mesh, palette, sun};
use crate::Story;
use space3d_core::glam::EulerRot;
use space3d_core::prelude::*;
use space3d_physics::{Collider, PhysicsPlugin, Restitution, RigidBody};
use space3d_render::{
    BufferUsage, FilterMode, InstanceBuffer, InstanceBufferHandle, InstanceBuffers, InstanceTransformColor,
    InstancedMesh, Material, Mesh3d, PointLight, Projection, RenderAssets, SamplerSettings, TextureSource,
};

/// Rotates an entity around `axis` every fixed tick.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Spin {
    axis: Vec3,
    radians_per_tick: f32,
}

fn spin_system(ctx: &mut SystemCtx) {
    for (t, s) in ctx.world.query_mut::<(&mut Transform, &Spin)>() {
        t.rotation = (Quat::from_axis_angle(s.axis, s.radians_per_tick) * t.rotation).normalize();
    }
}

fn lit_cube() -> Story {
    Story::new("Engine/Basics/Lit Cube", |app, k| {
        let cube = mesh(app, MeshData::cube(1.2));
        let ground = mesh(app, MeshData::plane(8.0));
        app.world.spawn((
            Name("cube".into()),
            Transform::from_xyz(0.0, 0.1, 0.0).with_rotation(Quat::from_rotation_y(0.5)),
            Mesh3d(cube),
            Material::color(Vec4::from_array(k.color("color"))).with_metallic_roughness(0.0, k.f32("roughness")),
            Spin {
                axis: Vec3::Y,
                radians_per_tick: k.f32("spin").to_radians(),
            },
        ));
        app.world.spawn((
            Name("ground".into()),
            Transform::from_xyz(0.0, -0.5, 0.0),
            Mesh3d(ground),
            Material::srgb(0.35, 0.37, 0.4),
        ));
        sun(app, Vec3::new(-0.4, -1.0, -0.55), k.f32("sun"));
        ambient(app, 0.08);
        camera(app, Vec3::new(2.4, 1.9, 3.2), Vec3::ZERO, Projection::default());
        app.add_system(Stage::FixedUpdate, spin_system);
    })
    .description(
        "A lit cube spinning on a ground plane: mesh, material, sun, ambient light, camera and \
         fixed-tick animation end to end.",
    )
    .knob_color("color", [0.8, 0.18, 0.06, 1.0])
    .knob_f32("roughness", 0.45, 0.0..=1.0)
    .knob_f32("spin", 1.5, 0.0..=10.0)
    .knob_f32("sun", 2.5, 0.0..=8.0)
    .size(320, 240)
    .ticks(30)
}

fn material_grid() -> Story {
    Story::new("Engine/Materials/Material Grid", |app, k| {
        let n = k.i64("grid").clamp(2, 9) as usize;
        let sphere = mesh(app, MeshData::sphere(0.42, 32, 16));
        let base = Vec4::from_array(k.color("color"));
        let (metallic_max, roughness_min) = (k.f32("metallic_max"), k.f32("roughness_min"));
        let off = (n - 1) as f32 / 2.0;
        for row in 0..n {
            for col in 0..n {
                let (u, v) = (col as f32 / (n - 1) as f32, row as f32 / (n - 1) as f32);
                let roughness = roughness_min + (1.0 - roughness_min) * u;
                app.world.spawn((
                    Transform::from_xyz(col as f32 - off, row as f32 - off, 0.0),
                    Mesh3d(sphere),
                    Material::color(base).with_metallic_roughness(metallic_max * v, roughness),
                ));
            }
        }
        sun(app, Vec3::new(0.5, -0.6, -1.0), 2.2);
        app.world.spawn((
            Transform::from_xyz(-2.0, 2.5, 3.5),
            PointLight {
                color: Vec3::ONE,
                intensity: 10.0,
                range: 14.0,
            },
        ));
        ambient(app, 0.12);
        let distance = (n as f32 * 0.5 + 0.5) / 30f32.to_radians().tan();
        camera(app, Vec3::new(0.0, 0.0, distance), Vec3::ZERO, Projection::default());
    })
    .description(
        "Spheres sweeping roughness left→right (from `roughness_min` to 1) and metallic bottom→top \
         (from 0 to `metallic_max`).",
    )
    .knob_color("color", [0.9, 0.55, 0.12, 1.0])
    .knob_i64("grid", 5, 2..=9)
    .knob_f32("metallic_max", 1.0, 0.0..=1.0)
    .knob_f32("roughness_min", 0.05, 0.0..=1.0)
    .size(320, 320)
}

fn material_sphere() -> Story {
    Story::new("Engine/Materials/Single Material", |app, k| {
        let sphere = mesh(app, MeshData::sphere(1.0, 48, 24));
        let emissive = k.f32("emissive");
        let color = Vec4::from_array(k.color("color"));
        app.world.spawn((
            Name("sphere".into()),
            Transform::IDENTITY,
            Mesh3d(sphere),
            Material::color(color)
                .with_metallic_roughness(k.f32("metallic"), k.f32("roughness"))
                .with_emissive(color.truncate() * emissive),
        ));
        sun(app, Vec3::new(0.6, -0.5, -1.0), 2.5);
        app.world.spawn((
            Transform::from_xyz(-2.5, 1.5, 2.0),
            PointLight {
                color: Vec3::new(0.6, 0.8, 1.0),
                intensity: 6.0,
                range: 8.0,
            },
        ));
        ambient(app, 0.1);
        camera(app, Vec3::new(0.0, 0.0, 3.3), Vec3::ZERO, Projection::default());
    })
    .description("One sphere with every material parameter on a knob.")
    .knob_color("color", [0.1, 0.35, 0.9, 1.0])
    .knob_f32("roughness", 0.35, 0.0..=1.0)
    .knob_f32("metallic", 0.0, 0.0..=1.0)
    .knob_f32("emissive", 0.0, 0.0..=3.0)
    .size(256, 256)
}

/// State of the instanced wave (resource).
#[derive(Clone, Copy, Debug)]
struct WaveField {
    handle: InstanceBufferHandle,
    side: usize,
    amplitude: f32,
    speed: f32,
}

fn write_wave(data: &mut [InstanceTransformColor], side: usize, amplitude: f32, phase: f32) {
    let spacing = 0.3;
    let half = (side as f32 - 1.0) * 0.5;
    for (i, inst) in data.iter_mut().enumerate() {
        let (x, z) = ((i % side) as f32, (i / side) as f32);
        let wave = (x * 0.21 + phase).sin() * (z * 0.17 + phase * 0.7).cos();
        let pos = Vec3::new((x - half) * spacing, wave * amplitude, (z - half) * spacing);
        let u = x / side as f32;
        let color = Vec4::new(0.08 + 0.85 * u, 0.25 + 0.3 * (wave + 1.0), 0.95 - 0.8 * u, 1.0);
        *inst = InstanceTransformColor::new(
            Mat4::from_scale_rotation_translation(Vec3::splat(0.22), Quat::IDENTITY, pos),
            color,
        );
    }
}

fn wave_system(ctx: &mut SystemCtx) {
    let field = *ctx.resources.expect::<WaveField>();
    let phase = ctx.time().tick as f32 * field.speed;
    if let Some(buf) = ctx
        .resources
        .expect_mut::<InstanceBuffers>()
        .get_mut::<InstanceTransformColor>(field.handle)
    {
        write_wave(buf.as_mut_slice(), field.side, field.amplitude, phase);
    }
}

fn instanced_cubes() -> Story {
    Story::new("Engine/Instancing/10k Cubes", |app, k| {
        let count = k.i64("count").max(1) as usize;
        let side = (count as f64).sqrt().ceil() as usize;
        let cube = mesh(app, MeshData::cube(1.0));
        let amplitude = k.f32("amplitude");
        let mut data = vec![InstanceTransformColor::new(Mat4::IDENTITY, Vec4::ONE); count];
        write_wave(&mut data, side, amplitude, 0.0);
        let handle = app
            .resources
            .init::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
        let field = WaveField {
            handle,
            side,
            amplitude,
            speed: k.f32("speed"),
        };
        app.world.spawn((
            Name("wave".into()),
            InstancedMesh {
                mesh: cube,
                material: Material::default().with_metallic_roughness(0.0, 0.6),
                instances: field.handle,
            },
        ));
        app.insert_resource(field);
        app.add_system(Stage::FixedUpdate, wave_system);
        sun(app, Vec3::new(-0.3, -1.0, -0.4), 2.2);
        ambient(app, 0.1);
        let extent = side as f32 * 0.3;
        camera(
            app,
            Vec3::new(0.0, extent * 0.62, extent * 0.95),
            Vec3::new(0.0, -extent * 0.12, 0.0),
            Projection::default(),
        );
    })
    .description(
        "`count` cubes in one explicit InstanceBuffer (InstanceTransformColor), rewritten every \
         fixed tick as a wave: one instanced draw, dirty-range uploads, GPU culling where supported.",
    )
    .knob_i64("count", 10_000, 100..=100_000)
    .knob_f32("amplitude", 0.6, 0.0..=2.0)
    .knob_f32("speed", 0.08, 0.0..=0.5)
    .size(320, 240)
    .ticks(20)
    .tag("instancing")
    // Thousands of sub-pixel edges: allow a little rasterization variance between GPUs.
    .tolerance(0.005)
}

fn checker_plane() -> Story {
    Story::new("Engine/Textures/Checker Plane", |app, k| {
        let cell = k.i64("cell").clamp(1, 128) as u32;
        let image = ImageData::checker(256, cell, [235, 235, 235, 255], [38, 42, 52, 255]);
        let filter = if k.bool("nearest") {
            FilterMode::Nearest
        } else {
            FilterMode::Linear
        };
        let source = TextureSource {
            sampler: SamplerSettings {
                filter,
                ..Default::default()
            },
            ..TextureSource::from_image(image)
        };
        let texture = app.resources.init::<RenderAssets>().add_texture(source);
        let tint = Vec4::from_array(k.color("tint"));
        let material = if k.bool("unlit") {
            Material::unlit(tint).with_texture(texture)
        } else {
            Material::color(tint).with_texture(texture)
        };
        let plane = mesh(app, MeshData::plane(6.0));
        let cube = mesh(app, MeshData::cube(1.0));
        app.world.spawn((
            Name("plane".into()),
            Transform::IDENTITY,
            Mesh3d(plane),
            material.clone(),
        ));
        app.world.spawn((
            Name("cube".into()),
            Transform::from_xyz(0.6, 0.5, 0.4).with_rotation(Quat::from_rotation_y(0.6)),
            Mesh3d(cube),
            material,
        ));
        sun(app, Vec3::new(-0.5, -1.0, -0.3), 2.0);
        ambient(app, 0.15);
        camera(
            app,
            Vec3::new(0.0, 2.2, 4.8),
            Vec3::new(0.0, 0.0, -0.5),
            Projection::default(),
        );
    })
    .description("A checkerboard ImageData texture on a plane and a cube: sRGB upload, mipmaps, sampler filter.")
    .knob_i64("cell", 16, 1..=128)
    .knob_color("tint", [1.0, 1.0, 1.0, 1.0])
    .knob_bool("nearest", true)
    .knob_bool("unlit", false)
    .size(320, 240)
    .tolerance(0.005)
}

fn point_lights() -> Story {
    Story::new("Engine/Lights/Point Lights", |app, k| {
        let count = k.i64("count").clamp(1, 16) as usize;
        let (intensity, range, height) = (k.f32("intensity"), k.f32("range"), k.f32("height"));
        let floor = mesh(app, MeshData::plane(10.0));
        let cube = mesh(app, MeshData::cube(0.9));
        let ball = mesh(app, MeshData::sphere(0.6, 32, 16));
        let marker = mesh(app, MeshData::sphere(0.08, 12, 6));
        app.world.spawn((
            Name("floor".into()),
            Transform::IDENTITY,
            Mesh3d(floor),
            Material::srgb(0.75, 0.75, 0.75).with_metallic_roughness(0.0, 0.7),
        ));
        app.world.spawn((
            Transform::from_xyz(-0.8, 0.45, 0.0).with_rotation(Quat::from_rotation_y(0.4)),
            Mesh3d(cube),
            Material::srgb(0.9, 0.9, 0.9),
        ));
        app.world.spawn((
            Transform::from_xyz(0.9, 0.6, 0.3),
            Mesh3d(ball),
            Material::srgb(0.9, 0.9, 0.9).with_metallic_roughness(0.0, 0.3),
        ));
        for i in 0..count {
            let a = std::f32::consts::TAU * i as f32 / count as f32;
            let color = palette(i);
            let pos = Vec3::new(a.cos() * 2.6, height, a.sin() * 2.6);
            app.world.spawn((
                Name(format!("light {i}")),
                Transform::from_xyz(pos.x, pos.y, pos.z),
                PointLight {
                    color,
                    intensity,
                    range,
                },
            ));
            app.world.spawn((
                Transform::from_xyz(pos.x, pos.y, pos.z),
                Mesh3d(marker),
                Material::unlit(color.extend(1.0)),
            ));
        }
        ambient(app, 0.02);
        camera(
            app,
            Vec3::new(0.0, 5.5, 7.0),
            Vec3::new(0.0, 0.0, 0.3),
            Projection::default(),
        );
    })
    .description("A ring of colored point lights (with unlit markers) over a floor; no sun.")
    .knob_i64("count", 6, 1..=16)
    .knob_f32("intensity", 8.0, 0.0..=30.0)
    .knob_f32("range", 4.5, 0.5..=12.0)
    .knob_f32("height", 1.0, 0.1..=3.0)
    .size(320, 240)
}

fn camera_projection() -> Story {
    Story::new("Engine/Camera/Projection", |app, k| {
        let projection = if k.choice("projection") == 0 {
            Projection::Perspective {
                fov_y: k.f32("fov").to_radians(),
                near: 0.1,
                far: 100.0,
            }
        } else {
            Projection::Orthographic {
                height: k.f32("ortho_height"),
                near: 0.1,
                far: 100.0,
            }
        };
        let cube = mesh(app, MeshData::cube(1.0));
        let floor = mesh(app, MeshData::plane(1.0));
        app.world.spawn((
            Transform::from_xyz(0.0, -0.5, -7.0).with_scale(Vec3::new(6.0, 1.0, 20.0)),
            Mesh3d(floor),
            Material::srgb(0.3, 0.32, 0.36),
        ));
        for i in 0..8 {
            let z = -2.0 * i as f32;
            let c = palette(i).extend(1.0);
            for x in [-1.6, 1.6] {
                app.world
                    .spawn((Transform::from_xyz(x, 0.0, z), Mesh3d(cube), Material::color(c)));
            }
        }
        sun(app, Vec3::new(-0.4, -1.0, -0.2), 2.2);
        ambient(app, 0.12);
        camera(app, Vec3::new(0.0, 1.6, 4.0), Vec3::new(0.0, 0.2, -8.0), projection);
    })
    .description("Two rows of cubes receding into the distance, seen through a perspective or orthographic camera.")
    .knob_choice("projection", &["Perspective", "Orthographic"], 0)
    .knob_f32("fov", 60.0, 20.0..=120.0)
    .knob_f32("ortho_height", 7.0, 1.0..=30.0)
    .size(320, 240)
}

fn hierarchy() -> Story {
    Story::new("Engine/Transforms/Hierarchy", |app, k| {
        let speed = k.f32("speed").to_radians();
        let planets = k.i64("planets").clamp(1, 6) as usize;
        let sphere = mesh(app, MeshData::sphere(1.0, 32, 16));
        app.world.spawn((
            Name("star".into()),
            Transform::IDENTITY.with_scale(Vec3::splat(0.6)),
            Mesh3d(sphere),
            Material::unlit(Vec4::new(1.0, 0.75, 0.2, 1.0)),
        ));
        app.world.spawn((
            Transform::IDENTITY,
            PointLight {
                color: Vec3::new(1.0, 0.85, 0.6),
                intensity: 14.0,
                range: 16.0,
            },
        ));
        for p in 0..planets {
            let pivot = app.world.spawn((
                Name(format!("orbit {p}")),
                Transform::IDENTITY.with_rotation(Quat::from_rotation_y(p as f32 * 1.7)),
                Spin {
                    axis: Vec3::Y,
                    radians_per_tick: speed / (p as f32 + 1.0),
                },
            ));
            let radius = 0.24 + 0.07 * (p % 3) as f32;
            let planet = app.world.spawn((
                Name(format!("planet {p}")),
                Transform::from_xyz(1.4 + p as f32 * 0.9, 0.0, 0.0).with_scale(Vec3::splat(radius)),
                Parent(pivot),
                Mesh3d(sphere),
                Material::color(palette(p + 3).extend(1.0)),
            ));
            if p % 2 == 0 {
                // Moon orbit: child of the planet, compensating the planet's scale.
                let moon_pivot = app.world.spawn((
                    Transform::IDENTITY.with_scale(Vec3::splat(1.0 / radius)),
                    Parent(planet),
                    Spin {
                        axis: Vec3::Y,
                        radians_per_tick: speed * 3.0,
                    },
                ));
                app.world.spawn((
                    Name(format!("moon {p}")),
                    Transform::from_xyz(0.42, 0.0, 0.0).with_scale(Vec3::splat(0.07)),
                    Parent(moon_pivot),
                    Mesh3d(sphere),
                    Material::srgb(0.7, 0.7, 0.72),
                ));
            }
        }
        app.add_system(Stage::FixedUpdate, spin_system);
        ambient(app, 0.12);
        let far = 1.4 + planets as f32 * 0.9;
        camera(
            app,
            Vec3::new(0.0, far * 1.25, far * 1.5),
            Vec3::ZERO,
            Projection::default(),
        );
    })
    .description("Planets orbiting a star through Parent pivots (and moons two levels deep), spun every fixed tick.")
    .knob_f32("speed", 2.0, 0.0..=10.0)
    .knob_i64("planets", 3, 1..=6)
    .size(256, 256)
    .ticks(45)
}

fn physics_drop() -> Story {
    Story::new("Engine/Physics/Falling Bodies", |app, k| {
        app.add_plugin(PhysicsPlugin::default());
        let bodies = k.i64("bodies").clamp(1, 80) as usize;
        let restitution = k.f32("restitution");
        let floor = mesh(app, MeshData::plane(12.0));
        let cube = mesh(app, MeshData::cube(0.6));
        let ball = mesh(app, MeshData::sphere(0.32, 24, 12));
        app.world.spawn((
            Name("ground".into()),
            Transform::from_xyz(0.0, -0.5, 0.0),
            RigidBody::Fixed,
            Collider::cuboid(Vec3::new(12.0, 1.0, 12.0)),
        ));
        app.world.spawn((
            Transform::IDENTITY,
            Mesh3d(floor),
            Material::srgb(0.32, 0.34, 0.38).with_metallic_roughness(0.0, 0.8),
        ));
        for i in 0..bodies {
            let (layer, slot) = (i / 4, i % 4);
            let x = (slot as f32 - 1.5) * 0.9 + if layer % 2 == 1 { 0.3 } else { 0.0 };
            let z = ((i * 7) % 5) as f32 * 0.15 - 0.3;
            let transform = Transform::from_xyz(x, 1.0 + layer as f32 * 1.1, z).with_rotation(Quat::from_euler(
                EulerRot::XYZ,
                i as f32 * 0.37,
                i as f32 * 0.61,
                i as f32 * 0.23,
            ));
            let material = Material::color(palette(i).extend(1.0)).with_metallic_roughness(0.0, 0.5);
            if i % 3 == 2 {
                app.world.spawn((
                    transform,
                    RigidBody::Dynamic,
                    Collider::ball(0.32),
                    Restitution(restitution),
                    Mesh3d(ball),
                    material,
                ));
            } else {
                app.world.spawn((
                    transform,
                    RigidBody::Dynamic,
                    Collider::cuboid(Vec3::splat(0.6)),
                    Restitution(restitution),
                    Mesh3d(cube),
                    material,
                ));
            }
        }
        sun(app, Vec3::new(-0.3, -1.0, -0.5), 2.3);
        ambient(app, 0.1);
        camera(
            app,
            Vec3::new(0.0, 4.5, 7.5),
            Vec3::new(0.0, 0.8, 0.0),
            Projection::default(),
        );
    })
    .description(
        "Boxes and balls dropped onto the ground with space3d-physics (rapier), captured after a \
         deterministic number of fixed ticks.",
    )
    .knob_i64("bodies", 16, 1..=80)
    .knob_f32("restitution", 0.3, 0.0..=1.0)
    .size(320, 240)
    .ticks(90)
    .tag("physics")
}

/// The basics, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    vec![
        lit_cube(),
        material_grid(),
        material_sphere(),
        instanced_cubes(),
        checker_plane(),
        point_lights(),
        camera_projection(),
        hierarchy(),
        physics_drop(),
    ]
}
