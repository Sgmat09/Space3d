//! The storybook: isolated, parameterized scenes. Browse them with `space3d stories`; every
//! story is also a golden screenshot test (`tests/screenshots.rs`).

use crate::{GamePlugin, Player};
use space3d::prelude::*;

/// All stories of the game.
pub fn storybook() -> Storybook {
    let mut book = Storybook::new();
    book.add(
        Story::new("Game/Start", |app, _knobs| {
            app.add_plugin(GamePlugin);
        })
        .description("The scene right after start-up")
        .size(320, 240),
    );
    book.add(
        Story::new("Game/Player position", |app, knobs| {
            app.add_plugin(GamePlugin);
            let (x, z) = (knobs.f32("x"), knobs.f32("z"));
            app.add_system(Stage::Update, move |ctx| {
                for (transform, _) in ctx.world.query_mut::<(&mut Transform, &Player)>() {
                    transform.translation.x = x;
                    transform.translation.z = z;
                }
            });
        })
        .description("The player placed anywhere in the arena")
        .knob_f32("x", 2.5, -crate::ARENA..=crate::ARENA)
        .knob_f32("z", 1.5, -crate::ARENA..=crate::ARENA)
        .size(320, 240),
    );
    book.add(
        Story::new("Props/Material", |app, knobs| {
            let [r, g, b, _] = knobs.color("color");
            let (metallic, roughness) = (knobs.f32("metallic"), knobs.f32("roughness"));
            app.add_system(Stage::Startup, move |ctx| {
                let cube = ctx.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
                ctx.world.spawn((
                    Transform::default().with_rotation(Quat::from_euler(EulerRot::XYZ, 0.4, 0.7, 0.0)),
                    Mesh3d(cube),
                    Material::color(Vec4::new(r, g, b, 1.0)).with_metallic_roughness(metallic, roughness),
                ));
                ctx.world.spawn((
                    Transform::default().looking_at(Vec3::new(-0.5, -1.0, -0.4), Vec3::Y),
                    DirectionalLight {
                        color: Vec3::ONE,
                        intensity: 2.5,
                    },
                ));
                ctx.world.spawn((
                    Transform::from_xyz(0.0, 0.8, 2.6).looking_at(Vec3::ZERO, Vec3::Y),
                    Camera3d::default().with_clear_color(Vec4::new(0.08, 0.08, 0.1, 1.0)),
                ));
            });
        })
        .description("One prop with an editable material")
        .knob_color("color", [0.9, 0.3, 0.1, 1.0])
        .knob_f32("metallic", 0.2, 0.0..=1.0)
        .knob_f32("roughness", 0.4, 0.0..=1.0)
        .size(256, 256),
    );
    book
}
