//! The storybook: isolated, parameterized scenes. Browse them with `space3d stories`; every
//! story is also a golden screenshot test (`tests/screenshots.rs`).

use crate::GamePlugin;
use space3d::prelude::*;

/// All stories of the game.
pub fn storybook() -> Storybook {
    let mut book = Storybook::new();
    book.add(
        Story::new("Arena/Start", |app, _| {
            app.add_plugin(GamePlugin);
        })
        .description("The arena right after start-up")
        .size(320, 240),
    );
    book.add(
        Story::new("Arena/Settled", |app, _| {
            app.add_plugin(GamePlugin);
        })
        .description("One and a half seconds of physics later")
        .ticks(90)
        .size(320, 240),
    );
    book.add(
        Story::new("Props/Crate stack", |app, knobs| {
            let height = knobs.i64("height").max(1) as usize;
            let tint = knobs.color("tint");
            app.add_plugin(PhysicsPlugin::default());
            app.add_system(Stage::Startup, move |ctx| {
                let cube = ctx.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
                ctx.world.spawn((
                    Transform::from_xyz(0.0, -0.5, 0.0).with_scale(Vec3::new(6.0, 1.0, 6.0)),
                    Mesh3d(cube),
                    Material::srgb(0.3, 0.33, 0.37),
                    RigidBody::Fixed,
                    Collider::cuboid(Vec3::ONE),
                ));
                for level in 0..height {
                    ctx.world.spawn((
                        Transform::from_xyz(0.0, 0.5 + level as f32, 0.0)
                            .with_rotation(Quat::from_rotation_y(level as f32 * 0.3)),
                        Mesh3d(cube),
                        Material::color(Vec4::from_array(tint)).with_metallic_roughness(0.0, 0.7),
                        RigidBody::Dynamic,
                        Collider::cuboid(Vec3::ONE),
                    ));
                }
                ctx.world.spawn((
                    Transform::default().looking_at(Vec3::new(-0.5, -1.0, -0.3), Vec3::Y),
                    DirectionalLight {
                        color: Vec3::ONE,
                        intensity: 2.5,
                    },
                ));
                let top = height as f32 * 0.5;
                ctx.world.spawn((
                    Transform::from_xyz(3.0, top + 2.0, 5.0).looking_at(Vec3::new(0.0, top, 0.0), Vec3::Y),
                    Camera3d::default().with_clear_color(Vec4::new(0.07, 0.08, 0.1, 1.0)),
                ));
            });
        })
        .description("A physics-stacked tower of crates")
        .knob_i64("height", 4, 1..=8)
        .knob_color("tint", [0.8, 0.55, 0.3, 1.0])
        .ticks(30)
        .size(256, 256),
    );
    book
}
