//! The graphics showcase (`Engine/Showcase/*` stories; public as `space3d_stories::showcase`):
//! every graphics feature together in the SpaceCorps look, and the content and scenes the
//! stories share with `benches/graphics.rs`, `tests/perf_graphics.rs` and
//! `examples/graphics_showcase`.
//!
//! Owner: I (graphics spec §3.6, §5.3). Everything is generated in code, deterministically
//! ([`ShowcaseContent::generate`], CPU only — run it with `spawn_compute` in a game):
//! * three hulls ([`hull_mesh`]: an interceptor, a frigate and a gunship, 40 units long and
//!   facing −Z like the game's ships, nozzles at z = +21) with MikkTSpace tangents;
//! * one tiling set of hull maps ([`hull_maps`]) shared by every hull: base color, packed ORM,
//!   normal and emissive maps — one material per faction ([`FACTIONS`]: the emissive factor
//!   colors the windows and stripes);
//! * a nebula sky ([`sky_faces`]: an HDR cubemap with violet and teal clouds below the flight
//!   plane, the galactic band, stars up to 12× white and the sun) used as `Skybox` and
//!   `EnvironmentLight`;
//! * asteroids ([`asteroid_mesh`]: mottled vertex albedo, tiling rock detail maps from
//!   [`rock_maps`]) below the flight plane, where the ships' shadows fall.
//!
//! Scenes: [`spawn_look`] (a close-up skirmish: the SpaceCorps look) and the Battle reference
//! scene of graphics spec §4.4 ([`spawn_battle`] + [`dress_battle`]), with the recommended post
//! ([`battle_look`]) and a [`GraphicsQuality`] preset applied to everything.

mod content;
#[cfg(not(target_arch = "wasm32"))]
mod perf;
mod scenes;

pub use content::{
    ContentOptions, FACTIONS, Faction, HULL_NAMES, HULL_TILE, HullMaps, RockMaps, ShowcaseAssets, ShowcaseContent,
    asteroid_mesh, hull_maps, hull_mesh, rock_maps, sky_faces, sky_panorama, sun_direction,
};
#[cfg(not(target_arch = "wasm32"))]
pub use perf::{BattleReport, headless_battle, measure_battle};
pub use scenes::{
    BattleHandles, BattleScene, ENV_DIFFUSE, ENV_SPECULAR, LookScene, SPACE, add_environment, battle_look,
    dress_battle, fill_direction, particle_settings, spawn_asteroids, spawn_battle, spawn_engines, spawn_fill,
    spawn_look, spawn_previews, spawn_ship, spawn_sun, turn_previews,
};

use crate::Story;
use space3d_core::prelude::*;
use space3d_particles::ParticlesPlugin;
use space3d_render::{DownlevelOverride, DrawPath, FrameSettings, GraphicsQuality, RenderAssets, RenderSettings};
use std::sync::OnceLock;

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    vec![
        look_story(false),
        battle_story(false),
        look_story(true),
        battle_story(true),
    ]
}

/// The quality knob's tiers.
const TIERS: [GraphicsQuality; 4] = GraphicsQuality::ALL;
const TIER_NAMES: [&str; 4] = ["Low", "Medium", "High", "Ultra"];

/// The stories' content (512² hull maps, 1024² sky faces: stars stay points at 720p and 1080p
/// in the viewer and gallery), generated once per process.
fn story_content() -> &'static ShowcaseContent {
    static CONTENT: OnceLock<ShowcaseContent> = OnceLock::new();
    CONTENT.get_or_init(|| {
        ShowcaseContent::generate(ContentOptions {
            texture_size: 512,
            sky_face_size: 1024,
        })
    })
}

/// The WebGL2 device on any GPU: no compute, no float render targets, scene depth sampled
/// without a depth attachment (`DownlevelOverride::webgl2()`), CPU-built draws.
fn webgl2_device(app: &mut App) {
    app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride::webgl2()));
    app.insert_resource(RenderSettings {
        draw_path: Some(DrawPath::CpuDirect),
        ..RenderSettings::default()
    });
}

fn look_story(webgl2: bool) -> Story {
    let id = if webgl2 {
        "Engine/Showcase/WebGL2 SpaceCorps Look"
    } else {
        "Engine/Showcase/SpaceCorps Look"
    };
    let description = if webgl2 {
        "The SpaceCorps Look on a WebGL2-class device (`DownlevelOverride::webgl2()`: no compute, \
         no float render targets, CPU-built draws) at `GraphicsQuality::Low`, the tier \
         `recommended` picks there: the particles on the CPU backend (16k budget, fewer \
         spawns), the Ldr fallback (per-fragment AgX punchy, no bloom), FXAA Low, a 1024² PCF4 \
         shadow, the Classic shading model with the 128² environment bake, render scale 0.75. \
         The same scene, knobs and content as the native story."
    } else {
        "Every graphics feature in the SpaceCorps look: a close-up skirmish from the game's \
         top-down camera. Procedural hulls with all four maps (base color, packed ORM, normal, \
         emissive — window rows and faction stripes glowing in the faction's color), the nebula \
         sky as skybox and image-based lighting (metal panels mirror it), a sun whose shadows \
         fall on the ships and the textured asteroids below, lifted by the nebula's diffuse \
         light and a violet fill so shadowed sides stay readable, engines with afterburners, a \
         laser beam with hit sparks and shield ripples, a repair beam, orange bolts, a missile's smoke \
         ribbon, an explosion with debris, a light flash and a shockwave, a ship warping in \
         (distortion), bloom, AgX punchy +0.5 EV and a vignette (`battle_look`), all under a \
         `GraphicsQuality` preset (knob `quality`; High by default: MSAA 4, 7-level bloom, a \
         2048² PCF16 shadow, GGX shading). Knobs toggle shadows, image-based lighting, bloom \
         and distortion."
    };
    Story::new(id, move |app, k| {
        if webgl2 {
            webgl2_device(app);
        }
        let quality = TIERS[k.choice("quality").min(3)];
        let settings = particle_settings(&app.resources, quality);
        app.add_plugin(ParticlesPlugin::with_settings(settings));
        let assets = story_content().upload(app.resources.init::<RenderAssets>());
        spawn_look(
            app,
            &assets,
            &LookScene {
                quality,
                shadows: k.bool("shadows"),
                environment: k.bool("environment"),
                bloom: k.bool("bloom"),
                distortion: k.bool("distortion"),
                ticks: 96,
            },
        );
    })
    .description(description)
    .knob_choice("quality", &TIER_NAMES, if webgl2 { 0 } else { 2 })
    .knob_bool("shadows", true)
    .knob_bool("environment", true)
    .knob_bool("bloom", true)
    .knob_bool("distortion", true)
    .tag("showcase")
    .size(480, 270)
    .ticks(96)
    // Particles, MSAA edges and the shadow filter round differently on other drivers.
    .tolerance(0.005)
}

fn battle_story(webgl2: bool) -> Story {
    let id = if webgl2 {
        "Engine/Showcase/WebGL2 Battle"
    } else {
        "Engine/Showcase/Battle"
    };
    let description = if webgl2 {
        "The Battle reference scene on a WebGL2-class device (`DownlevelOverride::webgl2()`) at \
         `GraphicsQuality::Low`: the particles' CPU backend holds the battle in its 16k budget \
         (fewer spawns, nothing off screen), no float targets (no bloom), FXAA Low, render \
         scale 0.75 — what the web build shows."
    } else {
        "The graphics spec's Battle reference scene (§4.4) at `GraphicsQuality::High`, 2 s in, \
         through the game's battle camera (432 units away, 70° pitch): 300 ships of three \
         procedural hulls with ORM, normal and emissive maps (instanced), 60 asteroids, the \
         nebula skybox with image-based lighting, a shadow-casting sun, 100k particles \
         (engines, afterburner ribbons, held lasers and repair beams, bolts, missiles, 30 \
         explosions a second with light flashes, warps' shockwaves), bloom and MSAA 4, and two \
         256² render-target ship previews (transparent clear, post) shown by a HUD camera in \
         the lower right corner. `benches/graphics.rs` and `tests/perf_graphics.rs` measure it \
         at 1080p against the Dev gates (≤ 3.0 ms GPU, ≤ 1.5 ms CPU)."
    };
    Story::new(id, move |app, k| {
        if webgl2 {
            webgl2_device(app);
        }
        let quality = TIERS[k.choice("quality").min(3)];
        let settings = particle_settings(&app.resources, quality);
        app.add_plugin(ParticlesPlugin::with_settings(settings));
        let scene = BattleScene {
            quality,
            previews: k.bool("previews"),
            ..BattleScene::default()
        };
        let mut handles = spawn_battle(app, &scene);
        let assets = story_content().upload(app.resources.init::<RenderAssets>());
        dress_battle(&mut app.world, &mut app.resources, &assets, &scene, &mut handles);
        app.add_system(Stage::FixedUpdate, turn_previews);
    })
    .description(description)
    .knob_choice("quality", &TIER_NAMES, if webgl2 { 0 } else { 2 })
    .knob_bool("previews", true)
    .tag("showcase")
    .tag("perf")
    .size(320, 180)
    .ticks(120)
    // 100k particles: other drivers round a few of them differently.
    .tolerance(0.005)
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_particles::ParticleSettings;
    use space3d_test::TestApp;

    /// The showcase's lighting rig keeps the sides the sun does not reach readable (M2 final
    /// review: with the sky's diffuse lighting at 0.7 and no fill they got about 1.6 % of a
    /// sunlit top and read as black): a gray quad facing away from the sun (horizontally) gets
    /// 12–30 % of the light of one facing up, the sun's own direction 0 % of its light — the
    /// rest is the nebula-tinted fill of [`add_environment`] and [`spawn_fill`] — and a top in
    /// a cast shadow (the sun's light taken away) 8–30 %. Direct output (no post), linear
    /// luminance of the center pixel.
    #[test]
    fn fill_keeps_shadowed_sides_readable() {
        use space3d_render::{Camera3d, EnvironmentLight, Material, Mesh3d, Projection, srgb_to_linear};
        if !space3d_test::gpu_available() {
            eprintln!("skipping GPU test: no adapter available");
            return;
        }
        let content = ShowcaseContent::generate(ContentOptions {
            texture_size: 16,
            sky_face_size: 32,
        });
        // The horizontal normal facing away from the sun (along the light's travel).
        let sun = sun_direction();
        let away = Vec3::new(sun.x, 0.0, sun.z).normalize();
        let luminance = |n: Vec3, rig: bool, sunlit: bool| -> f32 {
            let mut t = TestApp::headless(8, 8);
            let app = t.app_mut();
            let assets = content.upload(app.resources.init::<RenderAssets>());
            let plane = app
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(space3d_core::MeshData::plane(4.0));
            app.world.spawn((
                Transform::IDENTITY.with_rotation(Quat::from_rotation_arc(Vec3::Y, n)),
                Mesh3d(plane),
                Material::srgb(0.5, 0.5, 0.5).with_metallic_roughness(0.0, 1.0),
            ));
            let up = if n.y.abs() > 0.9 { Vec3::Z } else { Vec3::Y };
            let camera = app.world.spawn((
                Transform::from_xyz(n.x * 5.0, n.y * 5.0, n.z * 5.0).looking_at(Vec3::ZERO, up),
                Camera3d::default().with_projection(Projection::Orthographic {
                    height: 1.0,
                    near: 0.1,
                    far: 20.0,
                }),
            ));
            let sun = spawn_sun(&mut app.world, space3d_render::ShadowFit::default());
            let _ = app.world.remove_one::<space3d_render::DirectionalShadow>(sun);
            if !sunlit {
                // What a cast shadow leaves: everything but the sun's direct light.
                let _ = app.world.despawn(sun);
            }
            if rig {
                add_environment(&mut app.world, &mut app.resources, &assets, camera);
                spawn_fill(&mut app.world);
            } else {
                // Before the M2 final review.
                let _ = app.world.insert_one(
                    camera,
                    EnvironmentLight::new(assets.sky).with_intensity(0.7, ENV_SPECULAR),
                );
                app.resources.insert(space3d_render::AmbientLight {
                    color: Vec3::new(0.45, 0.55, 1.0),
                    intensity: 0.015,
                });
            }
            t.screenshot().unwrap();
            let img = t.screenshot().unwrap();
            let [r, g, b, _] = img.get(4, 4);
            let lin = |c: u8| srgb_to_linear(c as f32 / 255.0);
            0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
        };
        let top = luminance(Vec3::Y, true, true);
        let side = luminance(away, true, true);
        let toward = luminance(-away, true, true);
        let shadowed_top = luminance(Vec3::Y, true, false);
        let (old_top, old_side) = (luminance(Vec3::Y, false, true), luminance(away, false, true));
        let old_shadowed_top = luminance(Vec3::Y, false, false);
        eprintln!(
            "fill: side away / top {side:.4} / {top:.4} = {:.3} (before: {:.3}); side toward the sun / top {:.3}; \
             shadowed top / top {:.3} (before: {:.3})",
            side / top,
            old_side / old_top,
            toward / top,
            shadowed_top / top,
            old_shadowed_top / old_top
        );
        assert!(top > 0.2 && top < 0.9, "a sunlit top in range: {top}");
        assert!((0.12..=0.3).contains(&(side / top)), "side / top {}", side / top);
        assert!(toward > side, "the sun's side is brighter: {toward} / {side}");
        assert!(
            (0.08..=0.3).contains(&(shadowed_top / top)),
            "shadowed top / top {}",
            shadowed_top / top
        );
        assert!(old_side / old_top < 0.04, "the old rig: {}", old_side / old_top);
    }

    /// The WebGL2 stories get the CPU particle backend's settings whatever the tier, and the
    /// native ones the tier's.
    #[test]
    fn stories_pick_the_particle_tier_for_the_device() {
        let mut app = TestApp::no_render();
        webgl2_device(&mut app);
        assert_eq!(
            particle_settings(&app.resources, GraphicsQuality::High),
            ParticleSettings::for_quality(GraphicsQuality::High),
            "no renderer: the tier's settings"
        );
        if space3d_test::gpu_available() {
            let mut app = TestApp::builder().size(32, 32).render_on_demand().build();
            app.with_renderer(|app| {
                webgl2_device(app);
                assert_eq!(
                    particle_settings(&app.resources, GraphicsQuality::High),
                    ParticleSettings::for_quality(GraphicsQuality::Low)
                );
            });
        }
    }
}
