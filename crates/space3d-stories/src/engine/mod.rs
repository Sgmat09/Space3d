//! Built-in engine storybook: one story per major renderer/engine feature.
//!
//! One file per area (graphics spec §2.8): `basics` (the original stories), `frame`,
//! `materials` + `lighting`, `post`, `particles` (feature `particles`), `showcase` and `ui`
//! (feature `ui`: `Engine/UI/Theme/*` and `Engine/UI/Tools/*`). Each exports
//! `pub(crate) fn stories() -> Vec<Story>`; [`engine_storybook`] concatenates them in that order.

mod basics;
mod frame;
mod lighting;
mod materials;
#[cfg(feature = "particles")]
mod particles;
mod post;
mod showcase;
#[cfg(feature = "ui")]
mod ui;

use crate::Storybook;
use space3d_core::prelude::*;
use space3d_render::{
    AmbientLight, Camera3d, DirectionalLight, Material, Mesh3d, MeshHandle, Projection, RenderAssets, RenderTargetDesc,
    Renderer, TextureHandle,
};

/// Linear clear color shared by the engine stories.
pub(super) const CLEAR: Vec4 = Vec4::new(0.012, 0.015, 0.024, 1.0);

/// Eight saturated linear colors for lights and markers.
pub(super) const PALETTE: [[f32; 3]; 8] = [
    [1.0, 0.15, 0.1],
    [1.0, 0.5, 0.05],
    [0.9, 0.85, 0.1],
    [0.1, 1.0, 0.3],
    [0.1, 0.8, 1.0],
    [0.2, 0.3, 1.0],
    [0.7, 0.2, 1.0],
    [1.0, 0.2, 0.6],
];

pub(super) fn palette(i: usize) -> Vec3 {
    Vec3::from_array(PALETTE[i % PALETTE.len()])
}

pub(super) fn mesh(app: &mut App, data: MeshData) -> MeshHandle {
    app.resources.init::<RenderAssets>().add_mesh(data)
}

pub(super) fn camera(app: &mut App, eye: Vec3, target: Vec3, projection: Projection) -> Entity {
    app.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(target, Vec3::Y),
        Camera3d::default().with_clear_color(CLEAR).with_projection(projection),
    ))
}

/// Directional light travelling along `direction`.
pub(super) fn sun(app: &mut App, direction: Vec3, intensity: f32) {
    app.world.spawn((
        Name("sun".into()),
        Transform::IDENTITY.looking_at(direction, Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.96, 0.9),
            intensity,
        },
    ));
}

/// A render target the size of the primary target (320 × 180 before a renderer exists), shown
/// full-screen by the primary camera: a quad at `at`, far from the story's scene, and an
/// orthographic camera looking at it. For stories that render their scene into a render target
/// (MSAA is chosen per render target). A system keeps its size, and the quad's aspect, in step
/// with the primary target: the gallery and the editor's viewer render stories at other sizes
/// than their goldens, and the scene must not be an upscaled 320 × 180 image there.
pub(super) fn screen_target(app: &mut App, desc: RenderTargetDesc, at: Vec3, clear: Vec4) -> TextureHandle {
    let size = app.resources.get::<Renderer>().map_or((320, 180), Renderer::size);
    let rt = app
        .resources
        .init::<RenderAssets>()
        .add_render_target_with(size.0, size.1, desc);
    let plane = mesh(app, MeshData::plane(1.0));
    let aspect = |(w, h): (u32, u32)| w.max(1) as f32 / h.max(1) as f32;
    let quad = app.world.spawn((
        Transform::from_xyz(at.x, at.y, at.z)
            .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
            .with_scale(Vec3::new(2.0 * aspect(size), 1.0, 2.0)),
        Mesh3d(plane),
        Material::unlit(Vec4::ONE).with_texture(rt),
    ));
    app.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(at.x, at.y, at.z + 5.0),
        Camera3d::default()
            .with_clear_color(clear)
            .with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 10.0,
            }),
    ));
    let mut current = size;
    app.add_system(Stage::PreUpdate, move |ctx: &mut SystemCtx| {
        let Some(size) = ctx.resources.get::<Renderer>().map(Renderer::size) else {
            return;
        };
        if size == current {
            return;
        }
        current = size;
        if let Some(assets) = ctx.resources.get_mut::<RenderAssets>() {
            assets.resize_render_target(rt, size.0, size.1);
        }
        if let Ok(mut t) = ctx.world.get::<&mut Transform>(quad) {
            t.scale.x = 2.0 * aspect(size);
        }
    });
    rt
}

pub(super) fn ambient(app: &mut App, intensity: f32) {
    app.insert_resource(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity,
    });
}

/// The built-in engine storybook (feature `engine`, on by default): lit cube, material grid,
/// single material, 10k instanced cubes, textured checker plane, point lights, camera
/// projections, transform hierarchy and a physics drop.
///
/// With feature `ui` (off by default; the editor and the facade's `stories` feature turn it on)
/// it also holds the UI stories: `Engine/UI/Theme/Default`
/// and `Engine/UI/Theme/Compact` (the same tool panel under egui's default style and the opt-in
/// compact density) and `Engine/UI/Tools/*` (debug overlay, profiler, console, and all three
/// under a roomy game style).
///
/// Every story spawns its own camera and lights, uses only engine plugins, and is
/// deterministic for a given tick count, so the whole book doubles as a golden screenshot
/// suite (`crates/space3d-stories/tests/__screenshots__/Engine/`).
pub fn engine_storybook() -> Storybook {
    let mut book = Storybook::new();
    let areas = [
        basics::stories(),
        frame::stories(),
        materials::stories(),
        lighting::stories(),
        post::stories(),
        #[cfg(feature = "particles")]
        particles::stories(),
        showcase::stories(),
        #[cfg(feature = "ui")]
        ui::stories(),
    ];
    for story in areas.into_iter().flatten() {
        book.add(story.tag("engine"));
    }
    book
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KnobValue, KnobValues};

    #[test]
    fn engine_book_is_well_formed() {
        let book = engine_storybook();
        assert!(book.len() >= 8);
        for story in book.iter() {
            assert!(story.id.starts_with("Engine/"), "{}", story.id);
            assert!(!story.description.is_empty(), "{} lacks a description", story.id);
            for knob in &story.knobs {
                assert_eq!(
                    knob.clamp(knob.default.clone()),
                    knob.default,
                    "{}: {}",
                    story.id,
                    knob.name
                );
            }
        }
    }

    /// Every story sets up and simulates without a renderer (knob extremes too).
    #[test]
    fn stories_simulate_without_renderer() {
        for story in engine_storybook().iter() {
            let mut extremes = KnobValues::default();
            for knob in &story.knobs {
                let v = match &knob.kind {
                    crate::KnobKind::Float { max, .. } => KnobValue::Float(*max),
                    crate::KnobKind::Int { min, .. } => KnobValue::Int(*min),
                    crate::KnobKind::Choice { options } => KnobValue::Choice(options.len() - 1),
                    _ => knob.default.clone(),
                };
                extremes.set(knob.name.clone(), v);
            }
            for knobs in [KnobValues::default(), extremes] {
                let mut app = space3d_test::TestApp::no_render();
                story.apply(&mut app, &knobs);
                app.step_ticks(story.ticks.min(5));
                assert!(app.count::<Camera3d>() >= 1, "{} must spawn a camera", story.id);
            }
        }
    }
}
