//! `Renderer::capture_scene_hdr` (graphics spec §1.10): the primary group's scene color before
//! post, as linear floats.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;

const W: u32 = 32;
const H: u32 = 16;

/// An unlit quad of linear `color` (1 × 1 unit: the center half of the 2-unit-high view) and
/// an orthographic camera.
fn scene(app: &mut App, color: Vec3) -> Entity {
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(1.0));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
        Mesh3d(mesh),
        Material::unlit(color.extend(1.0)),
    ));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 100.0,
            }),
    ))
}

fn capture(app: &mut App) -> Result<HdrImageData, RenderError> {
    render_to_image(app).expect("capture");
    app.resources.expect_mut::<Renderer>().capture_scene_hdr_blocking()
}

use space3d_core::HdrImageData;

/// Emissive 8.0 reaches post as 8.0 (±1 %): the scene texture holds unclamped radiance.
#[test]
fn hdr_values_survive_until_post() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = scene(&mut app, Vec3::new(8.0, 0.5, 0.02));
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    let hdr = capture(&mut app).unwrap();
    assert_eq!((hdr.width, hdr.height), (W, H));
    let p = hdr.get(W / 2, H / 2);
    for (got, want) in p.iter().zip([8.0, 0.5, 0.02, 1.0]) {
        assert!((got - want).abs() <= want * 0.01, "{p:?}");
    }
    // The non-blocking capture returns the same.
    let task = app.resources.expect_mut::<Renderer>().capture_scene_hdr();
    assert_eq!(task.block().unwrap().unwrap(), hdr);
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}

/// Direct mode has no scene texture.
#[test]
fn direct_mode_has_no_scene_texture() {
    let Some(mut app) = headless_app(W, H) else { return };
    scene(&mut app, Vec3::ONE);
    let err = capture(&mut app).unwrap_err();
    assert!(
        matches!(err, RenderError::Capture(ref m) if m.contains("Direct")),
        "{err}"
    );
}

/// The Ldr fallback's scene texture holds exposed, tonemapped display-linear colors (decoded
/// from sRGB): the per-fragment tonemap of the forward shader.
#[test]
fn ldr_fallback_scene_is_tonemapped() {
    let Some(mut app) = headless_app(W, H) else { return };
    let color = Vec3::new(3.0, 1.0, 0.25);
    let cam = scene(&mut app, color);
    app.world
        .insert_one(cam, PostProcess::default().with_exposure(-1.0))
        .unwrap();
    app.world
        .insert_one(cam, CameraFrame::default().with_hdr(HdrMode::Ldr))
        .unwrap();
    let clear = Vec3::new(2.0, 0.6, 0.1);
    app.world.get::<&mut Camera3d>(cam).unwrap().clear_color = Some(clear.extend(1.0));
    let hdr = capture(&mut app).unwrap();
    // The quad: tonemapped per fragment; the clear color: the same curve on the CPU.
    for (p, linear) in [(hdr.get(W / 2, H / 2), color), (hdr.get(0, 0), clear)] {
        let want = Tonemapper::AgX.apply(linear * 0.5);
        for c in 0..3 {
            assert!((p[c] - want[c]).abs() < 0.01, "{p:?} vs {want}");
        }
    }
}
