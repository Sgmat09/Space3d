//! Post-processing stories (`Engine/Post/*`): bloom, auto exposure, FXAA and lens effects, color grading, distortion, tonemappers.
//!
//! Owner: P-POST (graphics spec §3.6).

use super::{ambient, mesh, sun};
use crate::{KnobValues, Story};
use space3d_core::prelude::*;
use space3d_render::{Bloom, BloomComposite, Camera3d, Material, Mesh3d, PostProcess};

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    vec![bloom(), bloom_veil()]
}

/// Dark-space backdrop (linear).
const SPACE: Vec4 = Vec4::new(0.004, 0.005, 0.012, 1.0);

fn at(p: Vec3) -> Transform {
    Transform::from_xyz(p.x, p.y, p.z)
}

/// Deterministic pseudo-random numbers in 0..1 (a 32-bit LCG; stories must not depend on a
/// random seed).
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// A fighter at `p` heading along `dir`: hull, wings and two engine glows of linear radiance
/// `engine`.
fn fighter(app: &mut App, p: Vec3, dir: Vec3, tint: Vec3, engine: Vec3) {
    let hull = mesh(app, MeshData::cube(1.0));
    let nozzle = mesh(app, MeshData::sphere(0.5, 16, 8));
    let yaw = Quat::from_rotation_arc(Vec3::Z, dir.normalize());
    app.world.spawn((
        at(p).with_rotation(yaw).with_scale(Vec3::new(0.3, 0.16, 1.1)),
        Mesh3d(hull),
        Material::color(tint.extend(1.0)).with_metallic_roughness(0.6, 0.35),
    ));
    app.world.spawn((
        at(p + yaw * Vec3::new(0.0, -0.03, -0.1))
            .with_rotation(yaw)
            .with_scale(Vec3::new(1.5, 0.04, 0.38)),
        Mesh3d(hull),
        Material::color((tint * 0.6).extend(1.0)).with_metallic_roughness(0.4, 0.5),
    ));
    for side in [-0.12f32, 0.12] {
        app.world.spawn((
            at(p + yaw * Vec3::new(side, 0.0, -0.58)).with_scale(Vec3::splat(0.14)),
            Mesh3d(nozzle),
            Material::unlit(engine.extend(1.0)),
        ));
    }
}

/// A laser bolt from `a` to `b`: a thin unlit box of radiance `color` (its white-hot core is the
/// tonemapper's, its glow the bloom's).
fn laser(app: &mut App, a: Vec3, b: Vec3, width: f32, color: Vec3) {
    let beam = mesh(app, MeshData::cube(1.0));
    let d = b - a;
    app.world.spawn((
        at((a + b) * 0.5)
            .with_rotation(Quat::from_rotation_arc(Vec3::Z, d.normalize()))
            .with_scale(Vec3::new(width, width, d.length())),
        Mesh3d(beam),
        Material::unlit(color.extend(1.0)),
    ));
}

/// The bloom scene: two fighters with engine glows exchanging laser fire, a spray of hot
/// sparks, a lit planet, a starfield and a threshold ramp (unlit squares of radiance 0.5 … 16)
/// along the bottom.
fn battle(app: &mut App, bloom: Bloom) {
    // Blue engines (left, heading away to the right), orange ones (right, turning in).
    fighter(
        app,
        Vec3::new(-1.25, 0.3, -0.2),
        Vec3::new(1.0, 0.0, -0.6),
        Vec3::new(0.55, 0.58, 0.65),
        Vec3::new(4.0, 10.4, 24.0),
    );
    fighter(
        app,
        Vec3::new(1.35, -0.15, -1.4),
        Vec3::new(-1.0, 0.05, 0.4),
        Vec3::new(0.62, 0.42, 0.33),
        Vec3::new(18.0, 6.6, 1.8),
    );
    // Red bolts from the left fighter, a green beam from the right one.
    let red = Vec3::new(20.0, 1.4, 0.9);
    laser(
        app,
        Vec3::new(-0.75, 0.26, -0.5),
        Vec3::new(-0.3, 0.18, -0.72),
        0.04,
        red,
    );
    laser(app, Vec3::new(0.15, 0.1, -0.94), Vec3::new(0.6, 0.02, -1.16), 0.04, red);
    laser(
        app,
        Vec3::new(0.95, -0.12, -1.25),
        Vec3::new(-0.35, 0.2, -0.55),
        0.03,
        Vec3::new(1.2, 16.0, 3.5),
    );
    // Sparks where a red bolt hits the right fighter: tiny hot flecks (particles) of 3 … 30.
    let spark = mesh(app, MeshData::sphere(0.5, 8, 4));
    let mut rng = Lcg(7);
    let hit = Vec3::new(1.05, -0.02, -1.28);
    for _ in 0..30 {
        let d = Vec3::new(rng.next() - 0.5, rng.next() - 0.3, rng.next() - 0.5).normalize_or_zero();
        let r = 0.05 + 0.3 * rng.next();
        let heat = 3.0 + 27.0 * rng.next() * rng.next();
        app.world.spawn((
            at(hit + d * r).with_scale(Vec3::splat(0.02 + 0.025 * rng.next())),
            Mesh3d(spark),
            Material::unlit((Vec3::new(1.0, 0.55, 0.18) * heat).extend(1.0)),
        ));
    }
    // A planet in the lower right corner: lit, its sunlit side just above 1.
    let planet = mesh(app, MeshData::sphere(1.0, 64, 32));
    app.world.spawn((
        at(Vec3::new(12.0, -13.5, -13.0)).with_scale(Vec3::splat(4.2)),
        Mesh3d(planet),
        Material::srgb(0.35, 0.5, 0.8).with_metallic_roughness(0.0, 0.6),
    ));
    // Stars: points of 1.5 … 8 at the back.
    let star = mesh(app, MeshData::cube(1.0));
    for _ in 0..90 {
        let p = Vec3::new(rng.next() * 40.0 - 20.0, rng.next() * 27.0 - 23.0, -16.0);
        let v = 1.5 + 6.5 * rng.next() * rng.next();
        app.world.spawn((
            at(p).with_scale(Vec3::splat(0.04)),
            Mesh3d(star),
            Material::unlit((Vec3::new(0.9, 0.95, 1.0) * v).extend(1.0)),
        ));
    }
    // Threshold ramp along the bottom: 0.5, 1, 2, 4, 8, 16.
    let tile = mesh(app, MeshData::cube(1.0));
    for i in 0..6 {
        let v = 0.5 * 2f32.powi(i);
        app.world.spawn((
            at(Vec3::new(-1.2 + i as f32 * 0.36, -0.55, 1.0))
                .with_rotation(Quat::from_rotation_x(-0.38))
                .with_scale(Vec3::new(0.11, 0.11, 0.01)),
            Mesh3d(tile),
            Material::unlit(Vec4::new(v, v, v, 1.0)),
        ));
    }
    sun(app, Vec3::new(0.7, -0.3, -0.65), 2.5);
    ambient(app, 0.04);
    app.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(0.0, 1.3, 3.0).looking_at(Vec3::new(0.0, -0.2, -0.7), Vec3::Y),
        Camera3d::default().with_clear_color(SPACE),
        PostProcess::default().with_bloom(bloom),
    ));
}

/// Bloom from the knobs (defaults: `Bloom::glow()`).
fn bloom_knobs(k: &KnobValues) -> Bloom {
    Bloom {
        enabled: k.bool("enabled"),
        intensity: k.f32("intensity"),
        threshold: k.f32("threshold"),
        soft_knee: k.f32("soft_knee"),
        scatter: k.f32("scatter"),
        composite: match k.choice("composite") {
            1 => BloomComposite::EnergyConserving,
            _ => BloomComposite::Additive,
        },
        mip_count: k.i64("mips") as u32,
        ..Bloom::glow()
    }
}

fn bloom() -> Story {
    let glow = Bloom::glow();
    Story::new("Engine/Post/Bloom", |app, k| battle(app, bloom_knobs(k)))
        .description(
            "`Bloom::glow()`, the `PostProcess` default: engine glows, laser bolts, hot sparks and \
             stars bloom; the lit planet and the hulls barely do. The prefilter exposes the scene \
             and keeps what is above `threshold`, with a soft knee of `soft_knee × threshold` on \
             either side (the ramp of 0.5 … 16 along the bottom). A chain of `mips` levels (13-tap \
             downsamples, tent upsamples blended by `scatter`) is added after exposure with \
             `intensity`, or mixed in when energy conserving.",
        )
        .knob_bool("enabled", true)
        .knob_f32("intensity", glow.intensity, 0.0..=2.0)
        .knob_f32("threshold", glow.threshold, 0.0..=4.0)
        .knob_f32("soft_knee", glow.soft_knee, 0.0..=1.0)
        .knob_f32("scatter", glow.scatter, 0.0..=1.0)
        .knob_choice("composite", &["Additive", "Energy conserving"], 0)
        .knob_i64("mips", glow.mip_count as i64, 1..=8)
        .tag("graphics")
        .tag("post")
        .size(320, 180)
}

fn bloom_veil() -> Story {
    Story::new("Engine/Post/Bloom Veil", |app, k| {
        battle(app, Bloom::natural().with_intensity(k.f32("intensity")))
    })
    .description(
        "`Bloom::natural()`: threshold 0 and the energy-conserving composite (`mix(scene, bloom, \
         intensity)`), a photographic veil that moves light around instead of adding it: the \
         image keeps its mean brightness.",
    )
    .knob_f32("intensity", Bloom::natural().intensity, 0.0..=1.0)
    .tag("graphics")
    .tag("post")
    .size(320, 180)
}
