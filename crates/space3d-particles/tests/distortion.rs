//! Distortion layers (`SpriteTarget::Distortion`, graphics spec §1.8, §2.5, §5.2 PA4): sprites
//! drawn in the particles node's Distortion slot into the post group's distortion buffer
//! (post's PO5 writer API), on both backends and the WebGL2 path; never in color; occluded by
//! closer geometry; shapes giving a constant offset (a texture), a push outward (a ring) and a
//! heat haze.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{Facing, FloatRange, Gradient, ParticleBackend, SpriteRender, SpriteShape, SpriteTexture};
use space3d_render::*;

const S: u32 = 128;
const BLACK: Vec4 = Vec4::new(0.0, 0.0, 0.0, 1.0);
/// Offset of the constant writer (view heights), and the shift it produces in pixels: the
/// encoded value round(0.04 / 0.0625 × 255) = 163 decodes to 163 / 255 × 0.0625 view heights.
const STRENGTH: f32 = 0.04;

fn expected_shift() -> f32 {
    (STRENGTH / 0.0625 * 255.0).round() / 255.0 * 0.0625 * S as f32
}

/// Post without bloom, tonemapping or dither (displayed = radiance), distortion on.
fn plain_post() -> PostProcess {
    PostProcess::passthrough().with_distortion(Distortion::default())
}

/// An orthographic camera 2 units tall looking down −Z.
fn camera(app: &mut App, post: Option<PostProcess>) -> Entity {
    let e = app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default()
            .with_clear_color(BLACK)
            .with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 100.0,
            }),
    ));
    if let Some(post) = post {
        app.world.insert_one(e, post).unwrap();
    }
    e
}

/// An unlit quad of radiance `color` centered at `center`, facing the camera.
fn quad_at(app: &mut App, color: f32, center: Vec3, size: Vec2) -> Entity {
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(1.0));
    app.world.spawn((
        Transform::from_xyz(center.x, center.y, center.z)
            .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
            .with_scale(Vec3::new(size.x, 1.0, size.y)),
        Mesh3d(mesh),
        Material::unlit(Vec4::new(color, color, color, 1.0)),
    ))
}

/// A 4 × 4 linear texture of constant red and green, opaque (offsets along U and V).
fn offset_texture(app: &mut App, r: u8, g: u8) -> TextureHandle {
    let mut img = ImageData::new(4, 4);
    for p in img.pixels.chunks_mut(4) {
        p.copy_from_slice(&[r, g, 0, 255]);
    }
    app.resources
        .expect_mut::<RenderAssets>()
        .add_texture(TextureSource::from_image_linear(img))
}

/// A layer of one long-lived, motionless sprite of `size` drawing `shape` into the distortion
/// buffer with `strength`.
fn writer_layer(shape: SpriteShape, size: f32, strength: f32) -> ParticleLayer {
    ParticleLayer {
        lod: false,
        size: FloatRange::constant(size),
        color_over_life: Gradient::constant(Vec4::ONE),
        render: SpriteRender {
            shape,
            ..SpriteRender::default()
        }
        .distortion(strength),
        ..ParticleLayer::burst(1, 100.0)
    }
}

/// The constant writer: a 1-unit textured sprite looking (+1, +1) × `STRENGTH` view heights
/// away (texture (255, 0): +U, −V, i.e. right and up).
fn constant_writer(app: &mut App) -> EffectHandle {
    let tex = offset_texture(app, 255, 0);
    effect(
        app,
        writer_layer(SpriteShape::Texture(SpriteTexture::Handle(tex)), 1.0, STRENGTH),
    )
}

/// The linear value of `channel` at `(x, y)`.
fn lin(img: &ImageData, x: u32, y: u32) -> f32 {
    let c = img.get(x, y)[0] as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Sub-pixel x of a dark → bright edge in row `y` within `[x0, x1)`.
fn edge_x(img: &ImageData, y: u32, x0: u32, x1: u32) -> f32 {
    x1 as f32 - (x0..x1).map(|x| lin(img, x, y)).sum::<f32>()
}

/// Sub-pixel y of a bright → dark edge (bright above) in column `x` within `[y0, y1)`.
fn edge_y(img: &ImageData, x: u32, y0: u32, y1: u32) -> f32 {
    y0 as f32 + (y0..y1).map(|y| lin(img, x, y)).sum::<f32>()
}

/// Mean edge positions inside the writer's full-strength region.
fn edges(img: &ImageData) -> (f32, f32) {
    let rows = 46..58u32;
    let cols = 66..80u32;
    let x = rows.clone().map(|y| edge_x(img, y, 48, 80)).sum::<f32>() / rows.len() as f32;
    let y = cols.clone().map(|x| edge_y(img, x, 48, 80)).sum::<f32>() / cols.len() as f32;
    (x, y)
}

fn render_stats(app: &App) -> RenderStats {
    app.resources.expect::<RenderStats>().clone()
}

#[track_caller]
fn assert_near(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} px, expected {expected} ± {tolerance}"
    );
}

/// The bright upper-right quadrant behind (z = −1), the camera, and a constant writer at the
/// origin; rendered after two ticks (twice: the texture uploads on the first frame). Returns the
/// app, the image, the writer and the image without it (rendered first).
fn shifted(backend: ParticleBackend, setup: fn(&mut App)) -> Option<(App, ImageData, Entity, ImageData)> {
    let mut app = app_with(S, S, ParticleSettings { backend, ..settings() })?;
    setup(&mut app);
    quad_at(&mut app, 1.0, Vec3::new(0.5, 0.5, -1.0), Vec2::ONE);
    camera(&mut app, Some(plain_post()));
    let fx = constant_writer(&mut app);
    app.step_ticks(1);
    let plain = render(&mut app);
    assert_eq!(render_stats(&app).distortion_passes, 0, "no writer, no pass");
    let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
    app.step_ticks(2);
    render(&mut app);
    let img = render(&mut app);
    Some((app, img, e, plain))
}

fn none(_: &mut App) {}

fn webgl2(app: &mut App) {
    app.resources.expect_mut::<FrameSettings>().downlevel = DownlevelOverride::webgl2();
}

/// A distortion layer moves the image under it by its offset (post's ±1 px accuracy, spec PO5)
/// and leaves the rest bit-identical to the scene without it; one Distortion pass, no color
/// drawn. GPU and CPU backends, and the WebGL2 path (CPU backend, Ldr fallback, sampled depth).
#[test]
fn distortion_layer_shifts_the_background() {
    for (backend, setup, what) in [
        (ParticleBackend::Gpu, none as fn(&mut App), "gpu"),
        (ParticleBackend::Cpu, none, "cpu"),
        (ParticleBackend::Auto, webgl2, "webgl2"),
    ] {
        let Some((app, img, _, plain)) = shifted(backend, setup) else {
            return;
        };
        dump(&img, &format!("distortion_shift_{what}"));
        assert_eq!(render_stats(&app).distortion_passes, 1, "{what}: one Distortion pass");
        let shift = expected_shift();
        let (x, y) = edges(&img);
        eprintln!(
            "{what}: edges at ({x}, {y}), expected ({}, {})",
            64.0 - shift,
            64.0 + shift
        );
        assert_near(x, 64.0 - shift, 1.0, &format!("{what}: vertical edge"));
        assert_near(y, 64.0 + shift, 1.0, &format!("{what}: horizontal edge"));
        // Away from the writer the edge is where it was.
        assert_near(
            edge_x(&img, 10, 48, 80),
            64.0,
            0.02,
            &format!("{what}: undistorted edge"),
        );
        assert_near(edges(&plain).0, 64.0, 0.02, &format!("{what}: without the writer"));
        for y in (0..28).chain(100..S) {
            for x in 0..S {
                assert_eq!(
                    img.get(x, y),
                    plain.get(x, y),
                    "{what}: ({x}, {y}) is away from the writer"
                );
            }
        }
        assert_eq!(gpu_errors(&app), 0);
    }
}

/// Distortion layers never draw in color: without post, or with post's distortion off, the
/// image is bit-identical to the scene without them, and there is no Distortion pass.
#[test]
fn distortion_layers_never_draw_in_color() {
    for post in [
        None,
        Some(PostProcess::passthrough()),
        Some(PostProcess {
            dither: false,
            ..PostProcess::default().with_distortion(Distortion {
                enabled: false,
                ..Distortion::default()
            })
        }),
    ] {
        let Some(mut app) = app_with(S, S, settings()) else {
            return;
        };
        quad_at(&mut app, 1.0, Vec3::new(0.5, 0.5, -1.0), Vec2::ONE);
        camera(&mut app, post);
        let fx = constant_writer(&mut app);
        app.step_ticks(1);
        render(&mut app);
        let plain = render(&mut app);
        let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
        app.step_ticks(2);
        render(&mut app);
        let img = render(&mut app);
        assert!(app.world.contains(e));
        assert_eq!(render_stats(&app).distortion_passes, 0, "{post:?}");
        assert!(
            img.pixels == plain.pixels,
            "{post:?}: the distortion layer drew in color"
        );
        assert!(stats(&app).alive >= 1 || stats(&app).backend == Some(ParticleBackend::Gpu));
        assert_eq!(gpu_errors(&app), 0);
    }
}

/// A distortion sprite behind closer geometry writes nothing there: the closer panel's interior
/// is bit-identical to the render without the writer (read-only depth, and the GL depth path).
#[test]
fn distortion_layer_occluded_by_closer_geometry() {
    for gl in [false, true] {
        let Some((mut app, _, e, _)) = shifted(ParticleBackend::Auto, none) else {
            return;
        };
        app.resources.expect_mut::<FrameSettings>().downlevel.no_read_only_depth = gl;
        // A panel over the quadrant's edge, in front of the writer (z = 1).
        quad_at(&mut app, 0.5, Vec3::new(0.0, 0.2, 1.0), Vec2::new(0.5, 0.5));
        let img = render(&mut app);
        assert_eq!(render_stats(&app).distortion_passes, 1);
        // Without the writer (hidden: not drawn, its particle kept).
        app.world.insert_one(e, Visibility::Hidden).unwrap();
        app.step_ticks(1);
        let plain = render(&mut app);
        assert_eq!(render_stats(&app).distortion_passes, 0, "hidden: no pass");
        // The panel covers x 48..80, y 35..67; its interior 2 px in.
        let mut d = 0u8;
        for y in 38..64 {
            for x in 51..77 {
                let (p, q) = (img.get(x, y), plain.get(x, y));
                for c in 0..3 {
                    d = d.max(p[c].abs_diff(q[c]));
                }
            }
        }
        assert_eq!(d, 0, "gl {gl}: the closer panel is not distorted");
        assert_eq!(gpu_errors(&app), 0);
    }
}

/// A ring-shaped distortion sprite (a shockwave) pushes the image under its band outward: a
/// bright disc whose rim lies under the band grows, and the pixels inside the ring's hole are
/// untouched.
#[test]
fn ring_distortion_pushes_outward() {
    for backend in [ParticleBackend::Gpu, ParticleBackend::Cpu] {
        let Some(mut app) = app_with(S, S, ParticleSettings { backend, ..settings() }) else {
            return;
        };
        // A bright disc of radius 0.4 (26 px) behind, drawn as a hard particle in color.
        let disc = effect(
            &mut app,
            dot(Vec4::ONE, space3d_particles::ParticleBlend::Additive, 0.8),
        );
        app.spawn_effect(disc, EffectSpawn::at(Vec3::new(0.0, 0.0, -1.0)).keep());
        camera(&mut app, Some(plain_post()));
        let ring = effect(
            &mut app,
            writer_layer(
                SpriteShape::Ring {
                    thickness: 0.2,
                    focus: 0.0,
                    fill: 0.0,
                },
                1.2,
                0.03,
            ),
        );
        app.step_ticks(2);
        let plain = render(&mut app);
        let e = app.spawn_effect(ring, EffectSpawn::at(Vec3::ZERO).keep());
        app.step_ticks(1);
        let img = render(&mut app);
        dump(&img, "distortion_ring");
        let bright = |img: &ImageData| (0..S).filter(|&x| img.get(x, 64)[0] > 128).count();
        let (before, after) = (bright(&plain), bright(&img));
        eprintln!("{backend:?}: bright pixels on the center row {before} → {after}");
        assert!(
            after >= before + 4,
            "{backend:?}: the disc grows under the ring ({before} → {after})"
        );
        // The ring's hole (radius < 0.3) and the far corners are untouched.
        for (x, y) in [(64, 64), (60, 62), (2, 2), (125, 125)] {
            assert_eq!(img.get(x, y), plain.get(x, y), "{backend:?}: ({x}, {y})");
        }
        assert!(app.world.contains(e));
        assert_eq!(gpu_errors(&app), 0);
    }
}

/// Heat haze (a glow-shaped distortion sprite) bends a striped background, deterministically:
/// the same ticks render the same image, on either backend.
#[test]
fn heat_haze_is_deterministic() {
    let mut images = Vec::new();
    for backend in [ParticleBackend::Gpu, ParticleBackend::Gpu, ParticleBackend::Cpu] {
        let Some(mut app) = app_with(S, S, ParticleSettings { backend, ..settings() }) else {
            return;
        };
        for i in 0..8 {
            quad_at(
                &mut app,
                1.0,
                Vec3::new(-0.875 + i as f32 * 0.25, 0.0, -1.0),
                Vec2::new(0.125, 2.0),
            );
        }
        camera(&mut app, Some(plain_post()));
        let haze = effect(
            &mut app,
            ParticleLayer {
                render: SpriteRender {
                    facing: Facing::Camera,
                    ..writer_layer(
                        SpriteShape::Glow {
                            core: 0.0,
                            hardness: 0.0,
                        },
                        1.5,
                        0.02,
                    )
                    .render
                },
                ..writer_layer(
                    SpriteShape::Glow {
                        core: 0.0,
                        hardness: 0.0,
                    },
                    1.5,
                    0.02,
                )
            },
        );
        app.step_ticks(1);
        let plain = render(&mut app);
        app.spawn_effect(haze, EffectSpawn::at(Vec3::ZERO).seed(3).keep());
        app.step_ticks(3);
        let img = render(&mut app);
        dump(&img, &format!("distortion_haze_{backend:?}"));
        let changed = img
            .pixels
            .chunks(4)
            .zip(plain.pixels.chunks(4))
            .filter(|(a, b)| a[0].abs_diff(b[0]) > 16)
            .count();
        assert!(changed > 200, "{backend:?}: the haze bends the stripes ({changed} px)");
        assert_eq!(gpu_errors(&app), 0);
        images.push(img);
    }
    assert!(images[0].pixels == images[1].pixels, "same ticks, same image");
    // CPU vs GPU: the same particles; positions agree within float rounding.
    let diff = images[0]
        .pixels
        .iter()
        .zip(&images[2].pixels)
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    assert!(diff < 20, "the backends agree ({diff} channels differ)");
}
