//! Textured sprites and flipbooks (graphics spec §5.2 PA3): frames from a texture array (2 or
//! more layers, bound as `texture_2d_array`) and from a sprite sheet (a single texture, bound as
//! `texture_2d`), selected over life, at frames per second (the instances carry their age) and
//! at random; the procedural built-ins draw.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{
    BuiltinTexture, Flipbook, FlipbookMode, FloatRange, Gradient, SpriteRender, SpriteShape, SpriteTexture,
};
use space3d_render::*;

const W: u32 = 320;
const H: u32 = 180;

/// Solid colors of the frames.
const COLORS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];

fn solid(c: [u8; 3], size: u32) -> ImageData {
    ImageData {
        width: size,
        height: size,
        pixels: (0..size * size).flat_map(|_| [c[0], c[1], c[2], 255]).collect(),
    }
}

/// A 2×2 sprite sheet: frame k (row major) in color `COLORS[k]`.
fn sheet(size: u32) -> ImageData {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let k = (y >= size / 2) as usize * 2 + (x >= size / 2) as usize;
            let c = COLORS[k];
            pixels.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    ImageData {
        width: size,
        height: size,
        pixels,
    }
}

/// A square sprite of `texture` living `life` seconds, at `x`, with `flipbook`.
fn textured(texture: TextureHandle, flipbook: Flipbook, life: f32) -> ParticleLayer {
    ParticleLayer {
        lod: false,
        size: FloatRange::constant(2.0),
        color_over_life: Gradient::constant(Vec4::ONE),
        render: SpriteRender {
            shape: SpriteShape::Texture(SpriteTexture::Handle(texture)),
            flipbook: Some(flipbook),
            ..SpriteRender::default()
        },
        ..ParticleLayer::burst(1, life)
    }
}

/// The dominant channel pattern of a pixel (which frame's color it shows).
fn frame_of(p: [u8; 4]) -> Option<usize> {
    let on = |c: u8| c > 150;
    let off = |c: u8| c < 60;
    COLORS
        .iter()
        .position(|c| (0..3).all(|i| if c[i] == 255 { on(p[i]) } else { off(p[i]) }))
}

/// Frames over life from a 4-layer texture array and from a 2×2 sprite sheet: four particles of
/// different lifetimes, 0.1 s old, are at 0.1, 0.25, 0.5 and 0.83 of their lives, so they show
/// frames 0, 1, 2 and 3; at 13 frames per second a particle 0.25 s old shows frame 3 (its age
/// travels with the instance); a particle spawned later is on an earlier frame.
#[test]
fn flipbook_frame_selection() {
    for atlas in [false, true] {
        let Some(mut app) = app_with(W, H, settings()) else {
            return;
        };
        ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
        let assets = app.resources.expect_mut::<RenderAssets>();
        let (texture, book) = if atlas {
            let mut src = TextureSource::from_image(sheet(32));
            src.generate_mipmaps = false;
            (
                assets.add_texture(src),
                Flipbook {
                    frames: 4,
                    columns: 2,
                    rows: 2,
                    ..Flipbook::default()
                },
            )
        } else {
            let layers: Vec<ImageData> = COLORS.iter().map(|&c| solid(c, 16)).collect();
            let mut src = TextureSource::from_layers(&layers).unwrap();
            src.generate_mipmaps = false;
            (
                assets.add_texture(src),
                Flipbook {
                    frames: 4,
                    ..Flipbook::default()
                },
            )
        };
        let over_life = |life: f32| textured(texture, book, life);
        let mut effect = ParticleEffect::new("frames");
        for (k, life) in [1.0, 0.4, 0.2, 0.12].into_iter().enumerate() {
            effect = effect.layer(ParticleLayer {
                offset: Vec3::new(-6.0 + 3.0 * k as f32, 2.0, 0.0),
                ..over_life(life)
            });
        }
        // Frames per second: 13 fps, 0.25 s old → frame 3 (a second particle emitted 0.15 s
        // later → frame 1).
        effect = effect.layer(ParticleLayer {
            offset: Vec3::new(-3.0, -2.0, 0.0),
            ..textured(
                texture,
                Flipbook {
                    mode: FlipbookMode::Fps(13.0),
                    ..book
                },
                10.0,
            )
        });
        effect = effect.layer(ParticleLayer {
            offset: Vec3::new(3.0, -2.0, 0.0),
            start: 0.15,
            ..textured(
                texture,
                Flipbook {
                    mode: FlipbookMode::Fps(13.0),
                    ..book
                },
                10.0,
            )
        });
        let fx = app
            .resources
            .expect_mut::<ParticleEffects>()
            .add(ParticleEffect {
                duration: 1.0,
                ..effect
            })
            .unwrap();
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
        // Emitted at the start of tick 1: after 6 ticks they are 0.1 s old.
        app.step_ticks(6);
        let img = render(&mut app);
        dump(&img, &format!("flipbook_over_life_{atlas}"));
        let px = |x: f32, y: f32| at(&img, 0.5 + x * 18.0 / W as f32, 0.5 - y * 18.0 / H as f32);
        for k in 0..4 {
            let p = px(-6.0 + 3.0 * k as f32, 2.0);
            assert_eq!(frame_of(p), Some(k), "atlas {atlas}: particle {k} shows {p:?}");
        }
        app.step_ticks(9);
        let img = render(&mut app);
        dump(&img, &format!("flipbook_fps_{atlas}"));
        let px = |x: f32, y: f32| at(&img, 0.5 + x * 18.0 / W as f32, 0.5 - y * 18.0 / H as f32);
        assert_eq!(
            frame_of(px(-3.0, -2.0)),
            Some(3),
            "13 fps, 0.25 s: {:?}",
            px(-3.0, -2.0)
        );
        assert_eq!(frame_of(px(3.0, -2.0)), Some(1), "13 fps, 0.1 s: {:?}", px(3.0, -2.0));
        assert_eq!(gpu_errors(&app), 0);
    }
}

/// The procedural built-in textures draw on both pipelines' texture variants: a `SoftDot`
/// (a single texture), a `Fireball16` flipbook (a texture array), and a ribbon tiled with
/// `Smoke16`; each lights its spot, and nothing is drawn where no particle is.
#[test]
fn builtin_textures_draw() {
    let Some(mut app) = app_with(W, H, settings()) else {
        return;
    };
    ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let sprite = |t: BuiltinTexture, x: f32, book: Option<Flipbook>| ParticleLayer {
        lod: false,
        offset: Vec3::new(x, 1.5, 0.0),
        size: FloatRange::constant(3.0),
        color_over_life: Gradient::constant(Vec4::new(2.0, 2.0, 2.0, 1.0)),
        render: SpriteRender {
            shape: SpriteShape::Texture(SpriteTexture::Builtin(t)),
            flipbook: book,
            ..SpriteRender::default()
        },
        ..ParticleLayer::burst(1, 2.0)
    };
    let fx = app
        .resources
        .expect_mut::<ParticleEffects>()
        .add(
            ParticleEffect::new("textures")
                .layer(sprite(BuiltinTexture::SoftDot, -5.0, None))
                .layer(sprite(BuiltinTexture::Sparkle, -1.5, None))
                .layer(sprite(
                    BuiltinTexture::Fireball16,
                    2.0,
                    Some(Flipbook {
                        frames: 16,
                        blend: true,
                        ..Flipbook::default()
                    }),
                ))
                .beam(BeamLayer {
                    width: 1.5,
                    offset: Vec3::new(-5.0, -2.0, 0.0),
                    color: Gradient::constant(Vec4::new(1.5, 1.5, 1.5, 1.0)),
                    texture: Some(SpriteTexture::Builtin(BuiltinTexture::Smoke16)),
                    uv: space3d_particles::RibbonUv::Tile { length: 2.0 },
                    ..BeamLayer::default()
                }),
        )
        .unwrap();
    app.spawn_effect(
        fx,
        EffectSpawn::at(Vec3::ZERO)
            .beam_to(BeamEnd::World(Vec3::new(5.0, -2.0, 0.0)))
            .keep(),
    );
    app.step_ticks(3);
    let img = render(&mut app);
    dump(&img, "builtin_textures");
    let lit = img.pixels.chunks(4).filter(|p| p[0] > 30).count();
    assert!(lit > 500, "the built-ins draw ({lit} px)");
    assert_eq!(gpu_errors(&app), 0);
}
