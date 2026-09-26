//! `Engine/Frame/Particles Bloom` (feature `particles`): particles in an HDR frame feed bloom
//! like every other scene-referred color. The `"space3d/particles"` node draws in the
//! Transparent phase into the HDR scene texture, so its additive sprites reach the composite's
//! bloom chain with no extra wiring (graphics spec §1.4, §1.9). A foundation integration story:
//! it pins the P-PART × P-POST seam, not either area's own features.

use super::{SPACE, at};
use crate::Story;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{EmitDirection, Facing, FloatRange, Gradient, SpriteRender, SpriteShape};
use space3d_render::{Camera3d, PostProcess, Projection};

/// Height of the orthographic view (world units); the sprites sit within ±7.5 of the center.
const VIEW_HEIGHT: f32 = 20.0;

/// One additive sprite of `shape` at `p`, `intensity` × `color`, moving along +X at `speed`.
fn sprite(shape: SpriteShape, facing: Facing, p: Vec3, color: Vec4, intensity: f32, speed: f32) -> ParticleLayer {
    ParticleLayer {
        lod: false,
        offset: p,
        direction: EmitDirection::Fixed(Vec3::X),
        speed: FloatRange::constant(speed),
        size: FloatRange::constant(3.2),
        color_over_life: Gradient::constant(color),
        intensity,
        render: SpriteRender {
            facing,
            ..SpriteRender::additive(shape)
        },
        ..ParticleLayer::burst(1, 100.0)
    }
}

/// Three rows of additive sprites: hot glows (`hot` ×), sparks (`hot / 2` ×) and dim glows that
/// stay below the bloom threshold (0.3 ×, every channel ≤ 1).
fn effect(hot: f32) -> ParticleEffect {
    let glow = SpriteShape::Glow {
        core: 0.35,
        hardness: 0.3,
    };
    let soft = SpriteShape::Glow {
        core: 0.0,
        hardness: 0.3,
    };
    let streak = SpriteShape::Streak { core: 0.6, taper: 1.0 };
    let stretched = Facing::VelocityStretched {
        scale: 0.25,
        min: 1.0,
        max: 6.0,
    };
    let hot_colors = [
        Vec4::new(0.35, 0.6, 1.0, 1.0),
        Vec4::new(1.0, 0.95, 0.85, 1.0),
        Vec4::new(1.0, 0.45, 0.12, 1.0),
    ];
    let mut e = ParticleEffect::new("particles bloom");
    for (i, c) in hot_colors.into_iter().enumerate() {
        let x = -5.0 + i as f32 * 5.0;
        e = e.layer(sprite(
            glow.clone(),
            Facing::Camera,
            Vec3::new(x, 5.5, 0.0),
            c,
            hot,
            0.0,
        ));
    }
    for (i, x) in [-5.5f32, 2.5].into_iter().enumerate() {
        let c = [Vec4::new(1.0, 0.55, 0.2, 1.0), Vec4::new(0.4, 0.8, 1.0, 1.0)][i];
        e = e.layer(sprite(
            streak.clone(),
            stretched,
            Vec3::new(x, 0.0, 0.0),
            c,
            hot * 0.5,
            12.0,
        ));
    }
    for (i, c) in [
        Vec4::new(0.4, 1.0, 0.5, 1.0),
        Vec4::new(1.0, 0.4, 0.8, 1.0),
        Vec4::new(0.5, 0.6, 1.0, 1.0),
    ]
    .into_iter()
    .enumerate()
    {
        let x = -5.0 + i as f32 * 5.0;
        e = e.layer(sprite(
            soft.clone(),
            Facing::Camera,
            Vec3::new(x, -5.5, 0.0),
            c,
            0.3,
            0.0,
        ));
    }
    e
}

pub(super) fn particles_bloom() -> Story {
    Story::new("Engine/Frame/Particles Bloom", |app, k| {
        app.add_plugin(ParticlesPlugin::default());
        let fx = app
            .resources
            .expect_mut::<ParticleEffects>()
            .add(effect(k.f32("hot")))
            .expect("valid effect");
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(7).keep());
        // Split screen over the same sprites: HDR without bloom (left), `Bloom::glow()` (right).
        let post = PostProcess::default();
        for (i, (name, post)) in [
            ("camera", post.without_bloom()),
            (
                "bloom camera",
                post.with_bloom(post.bloom.with_intensity(k.f32("bloom"))),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            app.world.spawn((
                Name(name.into()),
                at(Vec3::new(0.0, 0.0, 50.0)),
                Camera3d {
                    order: i as i32,
                    viewport: Some([0.5 * i as f32, 0.0, 0.5, 1.0]),
                    clear_color: Some(SPACE),
                    projection: Projection::Orthographic {
                        height: VIEW_HEIGHT,
                        near: 1.0,
                        far: 100.0,
                    },
                    ..Default::default()
                },
                post,
            ));
        }
    })
    .description(
        "Particles feed bloom in an HDR frame: the particles node draws into the HDR scene texture \
         (Transparent phase), so its additive sprites glow through the composite's bloom like \
         emissive meshes. Split screen over the same sprites, both HDR + AgX: bloom off (left), \
         `Bloom::glow()` (right). Top: hot glows at `hot` ×; middle: velocity-stretched sparks at \
         `hot / 2` ×; bottom: dim glows at 0.3 ×, below the threshold (no halo of their own; the \
         lift around them is the hot row's wide scatter).",
    )
    .knob_f32("hot", 6.0, 0.0..=20.0)
    .knob_f32("bloom", 0.35, 0.0..=1.0)
    .tag("graphics")
    .tag("particles")
    .size(320, 180)
    .ticks(3)
}

#[cfg(test)]
mod tests {
    use crate::space3d_test::require_gpu;
    use crate::{KnobValue, KnobValues, run_story_headless};
    use space3d_core::ImageData;

    /// Mean luma (0..255) of rows `y0..y1`, columns `x0..x1` of `img`.
    fn mean_luma(img: &ImageData, (x0, x1): (u32, u32), (y0, y1): (u32, u32)) -> f32 {
        let mut sum = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                let [r, g, b, _] = img.get(x, y);
                sum += 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
            }
        }
        sum / ((x1 - x0) * (y1 - y0)) as f32
    }

    /// The right half (bloom) minus the left half (no bloom) over the same half-local region.
    fn glow(img: &ImageData, x: (u32, u32), y: (u32, u32)) -> f32 {
        let half = img.width / 2;
        mean_luma(img, (x.0 + half, x.1 + half), y) - mean_luma(img, x, y)
    }

    /// Additive particles above the threshold bloom in HDR; below it, both halves match.
    #[test]
    fn hdr_particles_feed_bloom() {
        require_gpu!();
        let story = super::particles_bloom();
        let hot = run_story_headless(&story, &KnobValues::default(), None).unwrap();
        let (w, h) = (hot.width / 2, hot.height);
        // Rows: hot glows at y 5.5 of a 20-unit view (pixel ≈ 0.225 h), dim glows at ≈ 0.775 h.
        let band = |c: f32| ((h as f32 * (c - 0.1)) as u32, (h as f32 * (c + 0.1)) as u32);
        let halo_hot = glow(&hot, (0, w), band(0.225));
        let halo_dim = glow(&hot, (0, w), band(0.775));
        // The sprites themselves are drawn on both sides (bright cores in the left half too).
        assert!(mean_luma(&hot, (0, w), band(0.225)) > 8.0, "hot row not drawn");
        assert!(
            halo_hot > 6.0,
            "no bloom around the hot particles: +{halo_hot:.2} levels (dim row +{halo_dim:.2})"
        );
        assert!(
            halo_hot > 2.0 * halo_dim,
            "bloom must come from the hot row: hot +{halo_hot:.2}, dim +{halo_dim:.2}"
        );
        // Everything below the threshold: bloom adds (almost) nothing.
        let cold = KnobValues::default().with("hot", KnobValue::Float(0.3));
        let cold = run_story_headless(&story, &cold, None).unwrap();
        let all = glow(&cold, (0, w), (0, h));
        eprintln!("particles bloom: hot row +{halo_hot:.2}, dim row +{halo_dim:.2}, all cold +{all:.3} levels");
        assert!(all.abs() < 0.5, "below-threshold particles bloomed: {all:.3} levels");
    }
}
