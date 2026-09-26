//! Post-processing stories (`Engine/Post/*`): bloom, auto exposure, FXAA and lens effects, color grading, distortion, tonemappers.
//!
//! Owner: P-POST (graphics spec §3.6).

use super::{ambient, mesh, sun};
use crate::{KnobValues, Story};
use space3d_core::prelude::*;
use space3d_render::{
    AutoExposure, Bloom, BloomComposite, Camera3d, ChromaticAberration, ColorGrading, ColorLut, CustomShader,
    FilmGrain, FxaaQuality, Material, Mesh3d, Metering, PointLight, PostProcess, RenderAssets, Vignette,
};

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    vec![bloom(), bloom_veil(), auto_exposure(), fxaa(), lens(), color_grading()]
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

const FXAA_QUALITIES: [&str; 4] = ["Low", "Medium", "High", "Ultra"];

/// Aliasing-prone geometry: thin rotated bars, a lattice of thin beams receding in depth, tilted
/// cubes and a lit sphere.
fn aliased_scene(app: &mut App) {
    let bar = mesh(app, MeshData::cube(1.0));
    let sphere = mesh(app, MeshData::sphere(0.55, 48, 24));
    for i in 0..9 {
        let a = -0.9 + i as f32 * 0.22;
        app.world.spawn((
            at(Vec3::new(-1.1 + i as f32 * 0.27, 0.55, 0.0))
                .with_rotation(Quat::from_rotation_z(a))
                .with_scale(Vec3::new(0.025 + 0.004 * i as f32, 0.9, 0.05)),
            Mesh3d(bar),
            Material::unlit(Vec4::new(0.95, 0.9, 0.8, 1.0)),
        ));
    }
    for i in 0..7 {
        let z = -0.5 - i as f32 * 0.9;
        app.world.spawn((
            at(Vec3::new(0.0, -0.75, z)).with_scale(Vec3::new(4.0, 0.03, 0.03)),
            Mesh3d(bar),
            Material::unlit(Vec4::new(0.2, 0.8, 1.0, 1.0)),
        ));
    }
    for i in 0..5 {
        let x = -1.6 + i as f32 * 0.8;
        app.world.spawn((
            at(Vec3::new(x, -0.75, -3.2)).with_scale(Vec3::new(0.03, 0.03, 5.4)),
            Mesh3d(bar),
            Material::unlit(Vec4::new(0.2, 0.8, 1.0, 1.0)),
        ));
    }
    for (i, x) in [-0.9f32, 0.9].into_iter().enumerate() {
        app.world.spawn((
            at(Vec3::new(x, -0.15, 0.3))
                .with_rotation(
                    Quat::from_rotation_x(0.5 + i as f32) * Quat::from_rotation_y(0.7) * Quat::from_rotation_z(0.2),
                )
                .with_scale(Vec3::splat(0.4)),
            Mesh3d(bar),
            Material::srgb(0.8, 0.35, 0.2).with_metallic_roughness(0.0, 0.5),
        ));
    }
    app.world.spawn((
        at(Vec3::new(0.0, -0.1, -0.8)),
        Mesh3d(sphere),
        Material::srgb(0.7, 0.72, 0.78).with_metallic_roughness(0.0, 0.3),
    ));
    sun(app, Vec3::new(-0.4, -0.7, -0.6), 2.5);
    ambient(app, 0.06);
}

fn fxaa_quality(k: &KnobValues) -> FxaaQuality {
    match k.choice("quality") {
        0 => FxaaQuality::Low,
        2 => FxaaQuality::High,
        3 => FxaaQuality::Ultra,
        _ => FxaaQuality::Medium,
    }
}

fn fxaa() -> Story {
    Story::new("Engine/Post/FXAA", |app, k| {
        aliased_scene(app);
        // Split screen, the same view twice: without anti-aliasing (left), with FXAA (right).
        let base = PostProcess::default().without_bloom();
        let eye = Transform::from_xyz(0.0, 0.45, 2.7).looking_at(Vec3::new(0.0, 0.12, -0.6), Vec3::Y);
        for (i, post) in [base, base.with_fxaa(fxaa_quality(k))].into_iter().enumerate() {
            app.world.spawn((
                Name(if i == 0 { "camera" } else { "fxaa camera" }.into()),
                eye,
                Camera3d {
                    order: i as i32,
                    viewport: Some([0.5 * i as f32, 0.0, 0.5, 1.0]),
                    clear_color: Some(SPACE),
                    ..Default::default()
                },
                post,
            ));
        }
    })
    .description(
        "FXAA 3.11 (Lottes) after tonemapping: the same view without anti-aliasing (left) and \
         with `PostProcess::with_fxaa` (right). The composite writes an LDR intermediate; the \
         FXAA pass smooths stair-stepped edges found by local contrast, then applies grain, \
         dither and the output encoding. Qualities: Low (3 search steps), Medium (5), High \
         (12), Ultra (12, full sub-pixel filtering).",
    )
    .knob_choice("quality", &FXAA_QUALITIES, 1)
    .tag("graphics")
    .tag("post")
    .size(320, 180)
}

fn lens() -> Story {
    Story::new("Engine/Post/Lens", |app, k| {
        // A dim backdrop (the vignette darkens it toward the corners) and high-contrast stars
        // all over it (chromatic aberration grows toward the corners).
        let star = mesh(app, MeshData::cube(1.0));
        app.world.spawn((
            at(Vec3::new(0.0, 0.0, -9.0)).with_scale(Vec3::new(30.0, 20.0, 0.1)),
            Mesh3d(star),
            Material::unlit(Vec4::new(0.03, 0.035, 0.06, 1.0)),
        ));
        let mut rng = Lcg(11);
        for _ in 0..220 {
            let p = Vec3::new(rng.next() * 18.0 - 9.0, rng.next() * 10.0 - 5.3, -6.0);
            let v = 1.0 + 3.0 * rng.next();
            app.world.spawn((
                at(p).with_scale(Vec3::splat(0.06 + 0.05 * rng.next())),
                Mesh3d(star),
                Material::unlit((Vec3::new(0.9, 0.95, 1.0) * v).extend(1.0)),
            ));
        }
        aliased_scene(app);
        let post = PostProcess::default()
            .with_vignette(Vignette {
                enabled: k.f32("vignette") > 0.0,
                intensity: k.f32("vignette"),
                ..Vignette::default()
            })
            .with_chromatic_aberration(k.f32("chromatic_aberration"))
            .with_film_grain(FilmGrain {
                enabled: k.f32("grain") > 0.0,
                intensity: k.f32("grain"),
                // A fixed pattern: goldens stay stable.
                animated: false,
                seed: 1,
                ..FilmGrain::default()
            });
        let post = PostProcess {
            chromatic_aberration: ChromaticAberration {
                enabled: k.f32("chromatic_aberration") > 0.0,
                ..post.chromatic_aberration
            },
            ..post
        };
        app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, 0.45, 2.7).looking_at(Vec3::new(0.0, 0.12, -0.6), Vec3::Y),
            Camera3d::default().with_clear_color(SPACE),
            if k.bool("fxaa") {
                post.with_fxaa(FxaaQuality::Medium)
            } else {
                post
            },
        ));
    })
    .description(
        "Display-space lens effects in the composite: a vignette (darkened corners; \
         `roundness`, `smoothness` and a tint color on `Vignette`), chromatic aberration (red and \
         blue split radially, growing with r², `intensity` × 2 % of the width at the corners) and \
         film grain (value noise in the encoded space, fading in highlights, seeded: animated \
         grain follows `Time::frame`, this story's is fixed). Grain and dither run in the last \
         pass, after FXAA when it is on.",
    )
    .knob_f32("vignette", 0.45, 0.0..=1.0)
    .knob_f32("chromatic_aberration", 0.35, 0.0..=1.0)
    .knob_f32("grain", 0.06, 0.0..=0.5)
    .knob_bool("fxaa", true)
    .tag("graphics")
    .tag("post")
    .size(320, 180)
}

/// One auto-exposure panel at `origin`: a lit planet and fighter hull, engine glows, a small
/// nebula and stars on black (not metered), every light and radiance × `k`.
fn exposure_panel(app: &mut App, origin: Vec3, k: f32) {
    let sphere = mesh(app, MeshData::sphere(1.0, 48, 24));
    let cube = mesh(app, MeshData::cube(1.0));
    app.world.spawn((
        at(origin + Vec3::new(-0.9, -0.2, -1.5)),
        Mesh3d(sphere),
        Material::srgb(0.45, 0.55, 0.8).with_metallic_roughness(0.0, 0.55),
    ));
    app.world.spawn((
        at(origin + Vec3::new(0.9, 0.2, -0.4))
            .with_rotation(Quat::from_rotation_y(0.6) * Quat::from_rotation_x(0.3))
            .with_scale(Vec3::new(0.35, 0.18, 1.1)),
        Mesh3d(cube),
        Material::color(Vec4::new(0.6, 0.6, 0.65, 1.0)).with_metallic_roughness(0.7, 0.35),
    ));
    for side in [-0.1f32, 0.1] {
        app.world.spawn((
            at(origin + Vec3::new(0.62 + side, 0.1, -0.85)).with_scale(Vec3::splat(0.1)),
            Mesh3d(sphere),
            Material::unlit((Vec3::new(1.0, 2.6, 6.0) * k).extend(1.0)),
        ));
    }
    app.world.spawn((
        at(origin + Vec3::new(1.6, 1.4, -6.0))
            .with_rotation(Quat::from_rotation_z(0.5))
            .with_scale(Vec3::new(3.0, 1.2, 0.1)),
        Mesh3d(cube),
        Material::unlit((Vec3::new(0.012, 0.005, 0.02) * k).extend(1.0)),
    ));
    let mut rng = Lcg(3);
    for _ in 0..40 {
        let p = Vec3::new(rng.next() * 6.0 - 3.0, rng.next() * 4.0 - 2.0, -5.5);
        app.world.spawn((
            at(origin + p).with_scale(Vec3::splat(0.03)),
            Mesh3d(cube),
            Material::unlit((Vec3::new(0.9, 0.95, 1.0) * (0.5 + 2.0 * rng.next()) * k).extend(1.0)),
        ));
    }
    app.world.spawn((
        at(origin + Vec3::new(3.0, 1.5, 0.5)),
        PointLight {
            color: Vec3::new(1.0, 0.95, 0.85),
            intensity: 12.0 * k,
            range: 20.0,
        },
    ));
}

fn auto_exposure() -> Story {
    Story::new("Engine/Post/Auto Exposure", |app, k| {
        // The same scene lit 16× dimmer (left), as authored (middle) and 16× brighter (right),
        // each in its own post group: with auto exposure all three are exposed alike. No ambient
        // light (it would not scale with the panels).
        ambient(app, 0.0);
        let ae = AutoExposure {
            enabled: k.bool("auto_exposure"),
            min_ev: k.f32("min_ev"),
            max_ev: k.f32("max_ev"),
            metering: match k.choice("metering") {
                0 => Metering::Average,
                _ => Metering::CenterWeighted,
            },
            ..AutoExposure::default()
        };
        for (i, scale) in [1.0 / 16.0, 1.0, 16.0].into_iter().enumerate() {
            let origin = Vec3::new(100.0 * i as f32, 0.0, 0.0);
            exposure_panel(app, origin, scale);
            app.world.spawn((
                Name(format!("camera {i}")),
                at(origin + Vec3::new(0.0, 0.3, 3.2)).looking_at(origin + Vec3::new(0.0, 0.0, -1.0), Vec3::Y),
                Camera3d {
                    order: i as i32,
                    viewport: Some([i as f32 / 3.0, 0.0, 1.0 / 3.0, 1.0]),
                    clear_color: Some(Vec4::new(0.0, 0.0, 0.0, 1.0)),
                    ..Default::default()
                },
                PostProcess {
                    exposure: k.f32("compensation"),
                    auto_exposure: ae,
                    ..PostProcess::default()
                },
            ));
        }
    })
    .description(
        "`AutoExposure` meters each post group's rect: a 255-bin log2 luminance histogram (compute; \
         black is not metered, the darkest 10 % and the brightest 10 % are cut), exposed to middle \
         grey within `min_ev..=max_ev`, plus `PostProcess::exposure` as compensation. The same \
         scene lit 16× dimmer, as authored and 16× brighter: with auto exposure all three read \
         alike; without it the left panel is black and the right one blown out. The story uses a \
         compensation of −1 EV (a darker key suits space: the lit hull and planet, not the \
         shadows, should read as middle grey). Cameras snap on their first frame, then adapt at \
         `adapt_to_bright` / `adapt_to_dark` per second. On WebGL2 a chain of render passes \
         meters without the percentile cut.",
    )
    .knob_bool("auto_exposure", true)
    .knob_f32("compensation", -1.0, -4.0..=4.0)
    .knob_choice("metering", &["Average", "Center weighted"], 1)
    .knob_f32("min_ev", -8.0, -16.0..=0.0)
    .knob_f32("max_ev", 8.0, 0.0..=16.0)
    .ticks(180)
    .tag("graphics")
    .tag("post")
    .size(320, 180)
}

/// A `.cube` 3D LUT of size `n` computed from `f` (sRGB-encoded in and out).
fn cube_lut(n: u32, f: impl Fn(Vec3) -> Vec3) -> String {
    let mut s = format!("TITLE \"generated\"\nLUT_3D_SIZE {n}\n");
    let last = (n - 1) as f32;
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let v = f(Vec3::new(r as f32, g as f32, b as f32) / last).clamp(Vec3::ZERO, Vec3::ONE);
                s += &format!("{:.5} {:.5} {:.5}\n", v.x, v.y, v.z);
            }
        }
    }
    s
}

/// "Teal and orange": shadows pushed toward teal, highlights toward orange, skin-like mids kept.
fn teal_orange(c: Vec3) -> Vec3 {
    let luma = c.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    let t = (luma * 1.4 - 0.2).clamp(0.0, 1.0);
    let t = t * t * (3.0 - 2.0 * t);
    let tint = Vec3::new(0.86, 1.0, 1.1).lerp(Vec3::new(1.12, 0.98, 0.84), t);
    let graded = c * tint;
    let g = graded.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    Vec3::splat(g).lerp(graded, 1.15)
}

/// "Bleach bypass": half desaturated with a contrasty luminance overlay.
fn bleach_bypass(c: Vec3) -> Vec3 {
    let luma = c.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    let overlay = |base: f32, blend: f32| {
        if base < 0.5 {
            2.0 * base * blend
        } else {
            1.0 - 2.0 * (1.0 - base) * (1.0 - blend)
        }
    };
    let o = Vec3::new(overlay(c.x, luma), overlay(c.y, luma), overlay(c.z, luma));
    Vec3::splat(luma).lerp(o, 0.55)
}

/// A post-display "night vision" hook: green monochrome, scanlines and a round mask (`uv`).
const NIGHT_VISION: &str = "fn post_ldr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> {
    let l = s3d_luma(c);
    let lines = 0.85 + 0.15 * step(0.5, fract(uv.y * 90.0));
    let d = length((uv - 0.5) * vec2<f32>(1.6, 1.0));
    let mask = 1.0 - smoothstep(0.62, 0.72, d);
    return vec3<f32>(0.15, 1.0, 0.25) * pow(l, 0.8) * 1.4 * lines * mask;
}";

/// A colorful space scene for grading: a planet, a nebula backdrop, ships with engine glows, a
/// row of saturated tiles and a grey ramp from black to white.
fn grading_scene(app: &mut App) {
    let sphere = mesh(app, MeshData::sphere(1.0, 48, 24));
    let cube = mesh(app, MeshData::cube(1.0));
    app.world.spawn((
        at(Vec3::new(-1.5, 0.6, -3.0)).with_scale(Vec3::splat(1.4)),
        Mesh3d(sphere),
        Material::srgb(0.3, 0.55, 0.85).with_metallic_roughness(0.0, 0.6),
    ));
    for (i, color) in [Vec3::new(0.5, 0.05, 0.4), Vec3::new(0.05, 0.3, 0.45)]
        .into_iter()
        .enumerate()
    {
        app.world.spawn((
            at(Vec3::new(-2.0 + 4.0 * i as f32, 0.8, -7.0))
                .with_rotation(Quat::from_rotation_z(0.4 - 0.8 * i as f32))
                .with_scale(Vec3::new(5.0, 3.0, 0.1)),
            Mesh3d(cube),
            Material::unlit((color * 0.35).extend(1.0)),
        ));
    }
    fighter(
        app,
        Vec3::new(1.1, 0.55, -1.2),
        Vec3::new(-1.0, 0.1, 0.3),
        Vec3::new(0.7, 0.66, 0.6),
        Vec3::new(14.0, 6.0, 1.6),
    );
    for i in 0..8 {
        let x = -1.4 + i as f32 * 0.4;
        app.world.spawn((
            at(Vec3::new(x, -0.35, 0.0)).with_scale(Vec3::new(0.3, 0.25, 0.05)),
            Mesh3d(cube),
            Material::unlit((super::palette(i) * 0.8).extend(1.0)),
        ));
        let v = i as f32 / 7.0;
        app.world.spawn((
            at(Vec3::new(x, -0.72, 0.0)).with_scale(Vec3::new(0.38, 0.3, 0.05)),
            Mesh3d(cube),
            Material::unlit(Vec4::new(v * v, v * v, v * v, 1.0)),
        ));
    }
    sun(app, Vec3::new(0.6, -0.4, -0.7), 2.2);
    ambient(app, 0.05);
}

fn color_grading() -> Story {
    Story::new("Engine/Post/Color Grading", |app, k| {
        grading_scene(app);
        let lut = match k.choice("lut") {
            1 => Some(cube_lut(17, teal_orange)),
            2 => Some(cube_lut(17, bleach_bypass)),
            _ => None,
        }
        .map(|text| {
            let strip = ColorLut::from_cube(&text).expect("a generated LUT parses");
            let texture = app.resources.init::<RenderAssets>().add_texture(strip);
            ColorLut {
                contribution: k.f32("lut_contribution"),
                ..ColorLut::new(texture, 17)
            }
        });
        let shader = k.bool("night_vision").then(|| {
            app.resources
                .init::<RenderAssets>()
                .add_shader(CustomShader::new("night vision", NIGHT_VISION))
        });
        let grading = ColorGrading {
            temperature: k.f32("temperature"),
            tint: k.f32("tint"),
            saturation: k.f32("saturation"),
            contrast: k.f32("contrast"),
            lift: Vec3::new(0.0, 0.01, 0.03) * k.f32("lift"),
            gamma: Vec3::splat(k.f32("gamma")),
            gain: Vec3::new(1.0, 0.98, 0.94) * k.f32("gain"),
            lut,
            shader,
        };
        // The same view ungraded (left) and graded (right).
        let eye = Transform::from_xyz(0.0, 0.2, 3.0).looking_at(Vec3::new(0.0, 0.0, -1.0), Vec3::Y);
        let base = PostProcess::default();
        for (i, post) in [base, base.with_color_grading(grading)].into_iter().enumerate() {
            app.world.spawn((
                Name(if i == 0 { "camera" } else { "graded camera" }.into()),
                eye,
                Camera3d {
                    order: i as i32,
                    viewport: Some([0.5 * i as f32, 0.0, 0.5, 1.0]),
                    clear_color: Some(SPACE),
                    ..Default::default()
                },
                post,
            ));
        }
    })
    .description(
        "`ColorGrading`, fused into the composite: white balance (`temperature`, `tint`), \
         `saturation` and `contrast` (log2, around middle grey) in linear HDR before the \
         tonemapper; lift/gamma/gain (ASC CDL on the sRGB-encoded color) and a 3D LUT after it, \
         then the `post_ldr_hook` of an optional WGSL hook shader (`night_vision`). Left: \
         ungraded; right: graded. The LUTs are generated `.cube` files (`ColorLut::from_cube`): \
         teal & orange and bleach bypass.",
    )
    .knob_f32("temperature", 0.2, -1.0..=1.0)
    .knob_f32("tint", 0.0, -1.0..=1.0)
    .knob_f32("saturation", 1.15, 0.0..=2.0)
    .knob_f32("contrast", 1.1, 0.5..=2.0)
    .knob_f32("lift", 1.0, 0.0..=4.0)
    .knob_f32("gamma", 1.0, 0.5..=2.0)
    .knob_f32("gain", 1.0, 0.5..=1.5)
    .knob_choice("lut", &["None", "Teal & orange", "Bleach bypass"], 1)
    .knob_f32("lut_contribution", 0.8, 0.0..=1.0)
    .knob_bool("night_vision", false)
    .tag("graphics")
    .tag("post")
    .size(320, 180)
}
