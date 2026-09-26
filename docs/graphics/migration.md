# Migrating SpaceCorps2027 to the graphics upgrade

A step-by-step guide for the game (`~/git/spacecorps/SpaceCorps2027`, `client/src`) to adopt every
feature of graphics milestones 1 and 2: HDR, tonemapping and bloom, quality presets and render
scale, material maps straight from glTF, a cubemap sky with image-based lighting, the sun's
shadow, the particle library (engines, lasers, explosions, shields, warps), distortion, and the
post chain. Each step is independent and shippable on its own; the order below gives the biggest
visual win first. The overview of every feature is [`../graphics.md`](../graphics.md).

**Step 0 changes nothing.** The game's code (pinned at the pre-upgrade API) compiles unchanged
against the upgrade and renders the same pixels (checked: the game's workspace type-checks against
this tree with 0 warnings; `crates/space3d/tests/api_compat_249c874.rs` compiles its forms of every
touched type). One behavior differs by design: with **more than 16 point lights**, each view is
lit by the 16 nearest to its camera instead of the first 16 in query order (spec D19).

| step | game files | engine feature |
|---|---|---|
| [0](#0-update-the-engine) | `Cargo.toml` | the upgrade, the `particles` feature |
| [1](#1-hdr-and-the-battle-look) | `game/camera.rs` | `PostProcess`: HDR, AgX punchy, exposure, bloom, vignette |
| [2](#2-quality-presets) | `session.rs`, `ui/render_scale.rs`, `ui/pages/settings.rs` | `GraphicsQuality`, render scale, `ParticleSettings::for_quality_on` |
| [3](#3-ship-materials-from-gltf) | `game/models.rs` | normal, ORM, emissive maps; tangents |
| [4](#4-the-sky-as-a-cubemap-and-image-based-lighting) | `game/env.rs`, `game/camera.rs` | `Skybox`, `EnvironmentLight`, `ShadingModel::Standard` |
| [5](#5-the-suns-shadow) | `game/camera.rs` | `DirectionalShadow` |
| [6](#6-effects-particles-and-the-additive-overlay) | `game/fx.rs`, `game/additive.rs`, `game/fx/*`, lasers in `ui/hud` | particles; `OverlayStage::HdrScene` |
| [7](#7-distortion) | — | built into the particle effects; `DistortionMaterial` |
| [8](#8-ui-previews) | `ui/preview.rs` | MSAA render targets, post on previews |
| [9](#9-loading-screens-prewarm) | map loading | `GraphicsQuality::prewarm_request` |
| [10](#10-web) | `web/`, settings | Low on WebGL2 |
| [11](#11-tests-and-performance) | `client/tests` | goldens per backend, `DownlevelOverride::webgl2()`, `GpuTimings` |

## 0. Update the engine

```toml
# Cargo.toml (workspace)
space3d = { git = "https://github.com/SpaceCorps/Space3d", rev = "<graphics merge>", default-features = false,
            features = ["render", "platform", "assets", "audio", "net", "ui", "github", "particles"] }
```

`particles` adds `space3d::particles` (its own prelude; it is not in `space3d::prelude`). Build,
run, compare: nothing looks different yet.

## 1. HDR and the battle look

`game/camera.rs` spawns the game camera. Give it a `PostProcess`: the scene now renders in linear
HDR, bloom makes everything above 1 glow, and a tonemapper maps it to the display. egui still draws
after tonemapping, unchanged.

```rust
use space3d::render::Vignette; // PostProcess and Tonemapper are in space3d::prelude

let entity = ctx.world.spawn((
    Transform::from_xyz(0.0, 400.0, 150.0).looking_at(Vec3::ZERO, Vec3::Y),
    Camera3d::default()
        .with_clear_color(Vec4::new(0.004, 0.006, 0.012, 1.0))
        .with_projection(Projection::Perspective { fov_y: FOV_Y_DEG.to_radians(), near: 2.0, far: FAR }),
    // The recommended battle look: AgX punchy half a stop up keeps faction colors, lasers and
    // fire saturated (plain AgX turns them pastel), bloom on everything above 1, a light
    // vignette. Distortion is on by default (free until something writes it).
    PostProcess::default()
        .with_tonemapper(Tonemapper::AgXPunchy)
        .with_exposure(0.5)
        .with_vignette(Vignette { enabled: true, intensity: 0.24, ..Vignette::default() }),
));
```

What to retune afterwards:
* **Lights may go above 1 now.** `FLIGHT_LIGHTS`' comment ("the engine has no tonemapper, so the
  total stays under ~1 on light paint") no longer applies: the tonemapper rolls highlights off.
  Keep the values first; raise the key light later if hulls look dull.
* **The clear color is exposed and tonemapped** like the scene (spec R13): the backdrop tint
  shifts slightly. `PostProcess::passthrough()` reproduces the old look exactly (to A/B).
* **Emissive above 1 glows.** Engines, canopies and running lights (`Surface::glows()` roles with
  their glTF emissive factor) bloom; scale the factors (2–5 glows clearly, 5–16 reads white-hot).
* The `additive.rs` overlay still draws after tonemapping, exactly as before (step 6 moves it
  into the HDR scene so it blooms too).

## 2. Quality presets

The game's `graphicsQuality` (`low` / `medium` / `high`) maps onto `GraphicsQuality`, which sets
every area's performance knobs coherently (render scale, MSAA / FXAA, bloom length, the shadow
map, the environment bake, the shading model; table in [`../graphics.md`](../graphics.md#quality-presets)):

```rust
use space3d::particles::prelude::*;
use space3d::render::{FrameSettings, GraphicsQuality, Renderer};

/// `settingsJson.graphicsQuality` → engine tier. WebGL2-class devices (GL, no float render
/// targets or no compute) get Low whatever the setting: that is what `recommended` says there.
fn tier(setting: &str, renderer: &Renderer) -> GraphicsQuality {
    if GraphicsQuality::recommended_for(renderer) == GraphicsQuality::Low {
        return GraphicsQuality::Low;
    }
    match setting {
        "low" => GraphicsQuality::Low,
        "medium" => GraphicsQuality::Medium,
        _ => GraphicsQuality::High,
    }
}

/// Run when the renderer appears and whenever Settings › Graphics changes.
fn apply_graphics_quality(ctx: &mut SystemCtx) {
    let Some(renderer) = ctx.resources.get::<Renderer>() else { return };
    let settings = &ctx.resources.expect::<Session>().settings;
    let q = tier(&settings.graphics_quality, renderer);
    let mut particles = ParticleSettings::for_quality_on(q, renderer.graphics_caps());
    particles.quality *= settings.particle_scale(); // `particleQuality`: 0.25 / 0.6 / 1
    let render_scale = settings.render_scale();     // the game's 50 % / 75 % / 100 %
    q.apply(ctx.world, ctx.resources);              // settings, MSAA, cameras, the sun's shadow
    *ctx.resources.expect_mut::<ParticleSettings>() = particles;
    // The preset renders Low at 75 %; the game's "low" is 50 %.
    ctx.resources.expect_mut::<FrameSettings>().render_scale = render_scale;
}
```

**Replace `ui/render_scale.rs`'s render-target path with the engine's render scale.** Today the
game camera renders into an offscreen `RenderTarget::Texture` at 50 % / 75 % and egui paints it
behind the UI. With `FrameSettings::render_scale` (or `CameraFrame::render_scale` on the game
camera) the camera stays on `RenderTarget::Primary`, the scene renders smaller into the primary
scene texture, the composite upscales it (bloom and post at scene resolution), and egui stays
sharp. It also keeps HdrScene overlays working (step 6: with the render-target path the window
has no scene view, so they would draw nothing). Delete `RenderScalePlugin` and its egui image;
keep `GameSettings::render_scale()` as the value.

`apply` is lossless: switching Low → High restores what Low turned off (chromatic aberration,
grain, auto exposure) from the camera's authored `PostProcess`. Cameras spawned later (previews)
take the tier with `q.post_for(post, msaa)`, or call `apply` again.

## 3. Ship materials from glTF

The ships are painted with `normalTexture`, `occlusionTexture`, `metallicRoughnessTexture` (ORM
packed) and `emissiveTexture` + `emissiveFactor`; the loader reads them all
(`space3d::assets::MaterialData`) and the renderer draws them. `game/models.rs::to_parts` today
samples the base color only and fakes the rest per role. Keep the game's own mesh handling (the
coincident-face fix, per-kind transforms) and add the maps with `apply_material_maps`:

```rust
use space3d::assets::render::{TextureRole, apply_material_maps};
use space3d::render::MeshExtras;

fn to_parts(ctx: &mut SystemCtx, model: &ModelData, tangents: &[Vec<Vec<[f32; 4]>>], albedo: f32) -> Vec<Part> {
    let assets = ctx.resources.expect_mut::<RenderAssets>();
    // One upload per (image, color space): base color and emissive sRGB, the rest linear.
    let mut textures: HashMap<(usize, bool), TextureHandle> = HashMap::new();
    let mut texture = |assets: &mut RenderAssets, i: usize, role: TextureRole| -> Option<TextureHandle> {
        let image = model.images.get(i)?;
        let srgb = role.is_srgb();
        Some(*textures.entry((i, srgb)).or_insert_with(|| {
            if srgb { assets.add_image(image.clone()) } else { assets.add_image_linear(image.clone()) }
        }))
    };
    let mut parts = Vec::new();
    for (m, mesh) in model.meshes.iter().enumerate() {
        for (p, prim) in mesh.primitives.iter().enumerate() {
            let data = prim.material.and_then(|i| model.materials.get(i)).cloned().unwrap_or_default();
            let surface = data.name.as_deref().and_then(Surface::from_material_name);
            let base = data.base_color_texture.and_then(|i| texture(assets, i, TextureRole::BaseColor));
            let mut material = match surface {
                Some(surface) => painted_material(&data, surface, base), // + the albedo scale, as today
                None => legacy_material(&data, base),
            };
            // Normal, ORM, occlusion, emissive maps + normal scale, occlusion strength, texCoord
            // sets and the texture transform, exactly as authored.
            apply_material_maps(&mut material, &data, &mut |i, role| texture(assets, i, role));
            if material.metallic_roughness_texture.is_some() {
                // The ORM map carries metallic and roughness: the factors multiply it.
                (material.metallic, material.roughness) = (data.metallic, data.roughness);
            }
            // Normal maps need tangents: computed off the frame (below), else per-pixel frames.
            let t = tangents.get(m).and_then(|t| t.get(p)).cloned().unwrap_or_default();
            parts.push(Part {
                mesh: assets.add_mesh_with(prim.mesh.clone(), MeshExtras::new().with_tangents(t)),
                material,
                surface,
            });
        }
    }
    parts
}
```

* **Tangents.** Compute them in the game's compute-pool `prepare` (after `resolve_coincident`,
  which rewrites the primitives): `space3d::assets::edit::mesh::compute_mikktspace_tangents(&mesh)`
  returns the mesh (vertices split where mirrored UVs need it) and its tangents — use both. Or
  keep baked glTF `TANGENT`s: `ModelMesh::primitive_extras(i)`, but `resolve_coincident`'s `retain`
  + `extend` reorders primitives, so `ModelMesh::extras` must be kept in step or cleared
  ([`api.md`](api.md)). Without tangents the shader builds per-pixel frames (correct, a little
  softer).
* **Emissive.** The painted hulls carry `emissiveFactor` 1.0 with the emissive map as the mask:
  keep both (`apply_material_maps` does). Setting `emissive` from the factor *without* the map
  washes every hull out white (measured on Protos.glb and Wraith.glb). `painted_material`'s
  "emissive factor only on glowing roles" stays right; with the map, glowing roles can drop their
  separate uniform emissive materials.
* **Stop discarding the maps.** `models.rs` removes the model from `Assets<ModelData>` "because
  ORM, normal and emissive maps the engine can't use yet would stay in memory": they are used now.
* **Whole models without custom code**: `space3d::assets::render::prepare_model_async(model,
  ModelUploadOptions::default())` (tangents off the frame) → `PreparedModel::upload(assets)` →
  `spawn_model(world, &model, transform)` converts every slot, blend mode and sampler.

`Engine/Materials/glTF Import` and `Engine/Environment/Nebula Ship` show a baked ship lit this way.

## 4. The sky as a cubemap and image-based lighting

`game/env.rs` draws the skybox as six camera-following quads (unlit, `SKYBOX_HALF` inside `FAR`).
Replace them with one cubemap: the `Skybox` component draws it at infinity behind everything, and
the same cubemap lights the hulls (`EnvironmentLight`: diffuse irradiance and GGX reflections, baked
once on the GPU) — metal panels mirror the sector's nebula.

The faces are three.js-authored (`right, left, top, bottom, front, back`). Either write a manifest
next to them, `assets/skyboxes/G-1/sky.skybox`:

```json
{ "faces": ["right.jpg", "left.jpg", "top.jpg", "bottom.jpg", "front.jpg", "back.jpg"], "convention": "threejs" }
```

and load it like any asset (`SkyboxLoader` is registered by `AssetsPlugin`):

```rust
use space3d::assets::{AssetServer, Assets, CubemapData, Handle};
use space3d::render::{EnvironmentLight, Skybox};

let sky: Handle<CubemapData> = ctx.resources.expect_mut::<AssetServer>().load(format!("skyboxes/{map}/sky.skybox"));
// …once loaded (poll `Assets<CubemapData>` as `models.rs` polls models):
let cube = ctx.resources.expect_mut::<RenderAssets>().add_cubemap(data.to_source());
let camera = ctx.resources.expect::<GameCamera>().entity;
ctx.world.insert(camera, (Skybox::new(cube), EnvironmentLight::new(cube).with_intensity(0.8, 1.0)))?;
```

or build it from the six images the game already loads:
`CubemapSource::from_faces_threejs([right, left, top, bottom, front, back])`.

* **Shading model.** Image-based lighting is designed for `ShadingModel::Standard` (GGX); the
  presets set it (Medium and up), or set `LightingSettings::shading` yourself. `Classic` (the
  default, pixel-identical to the old shader) keeps working with IBL, approximately.
* **Brightness.** An LDR sky is radiance ≤ 1: `Skybox::intensity` scales the background,
  `EnvironmentLight::with_intensity(diffuse, specular)` the lighting. Lower `FLIGHT_LIGHTS`'
  ambient (the environment's irradiance replaces most of it). The backdrop variant
  (`spawn_backdrop_sky`, softened highlights under the station panels) becomes a second cubemap
  built from softened images, or the same one with a lower `Skybox::intensity`.
* **Orientation.** `Skybox::rotation` / `EnvironmentLight::rotation` turn the sky if a sector's
  sun ends up in the wrong place.
* **What stays.** The nebula veils, star layers, planet, debris and grid are geometry in the scene
  (parallax): keep them. Remove the six `SkyFace` quads and the `SKYBOX_HALF` constraint on `FAR`.
* **Memory.** An 8-bit 1024² sky is 25 MB with mips; the environment bake adds ~4 MB. The presets'
  recommended face sizes: 512 (Low) / 1024 / 2048.

`Engine/Environment/Skybox`, `Engine/Environment/IBL Spheres`, `Engine/Showcase/*`.

## 5. The sun's shadow

One directional light casts shadows; put `DirectionalShadow` next to the key light
(`FLIGHT_LIGHTS.lights[0]` in `CameraPlugin`), fitted to the part of the camera's view where ships
and stations live:

```rust
use space3d::render::{DirectionalShadow, ShadowFit};

let lights = FLIGHT_LIGHTS.lights.map(|(from, color, intensity)| {
    ctx.world.spawn((light_transform(from), DirectionalLight { color, intensity }))
});
// The key light casts: the camera's frustum within ±150 units of the flight plane, up to 1500
// units away. The quality preset sets the map size and filter (Low 1024 / PCF4 … Ultra 4096).
ctx.world.insert_one(
    lights[0],
    DirectionalShadow::new().with_fit(ShadowFit::top_down(0.0, 150.0, 1500.0)),
)?;
```

* `Material::shadows` (`ShadowMode::{Auto, CastAndReceive, CastOnly, ReceiveOnly, None}`) opts
  meshes out: the holo grid, the boundary wall, unlit sky geometry and effects should not cast
  (`ShadowMode::None` for the grid quads; unlit and transparent materials never cast by default).
* The light rig's studio swap (`LightRig::studio`) changes lights per frame; keep the shadow on
  the flight key light only.

## 6. Effects: particles and the additive overlay

Two ways, both supported; move effect by effect.

**a) Keep `additive.rs`, draw it into the HDR scene** so the sprites bloom and tonemap with the
scene: four steps in [`frame.md`](frame.md#migrating-an-additive-overlay-the-games-additivers) —
stage per instance (`HdrScene` for the world copy, `AfterTonemap` for the `UiParticles` copy over
egui), encoding from `prepare_scene`, no `1 − e^−x` soft clip outside Direct mode, `Rgba16Float`
pipelines — plus the engine render scale of step 2.

**b) Replace the sprites with the particle library** (GPU simulated, 100k+, depth-tested, soft
against hulls, lit flashes, distortion, deterministic on the fixed tick):

```rust
use space3d::particles::prelude::*;

app.add_plugin(ParticlesPlugin::default()); // settings: step 2

fn play(ctx: &mut SystemCtx, effect: fx::Effect) {
    match effect {
        Effect::Explosion { at, scale } => {
            ctx.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(at).scale(scale));
        }
        Effect::Impact { at, dir, color } => {
            ctx.spawn_effect(builtin::HIT_SPARK, EffectSpawn::at(at).facing(-dir).tint(color.extend(1.0)));
        }
        Effect::ShieldHit { center, radius, dir } => {
            ctx.spawn_effect(builtin::SHIELD_RIPPLE, EffectSpawn::at(center).facing(-dir).scale(radius / 20.0));
        }
        Effect::Warp { at, scale, arriving } => {
            let fx = if arriving { builtin::WARP_IN } else { builtin::WARP_OUT };
            ctx.spawn_effect(fx, EffectSpawn::at(at).scale(scale));
        }
        Effect::Tracer { from, to, color } => {
            // Hitscan lasers leave egui: a 0.12 s beam that despawns itself.
            ctx.spawn_effect(builtin::LASER_SHOT, EffectSpawn::at(from).beam_to(BeamEnd::World(to)).tint(color.extend(1.0)));
        }
        Effect::Muzzle { .. } => { /* the shot's beam starts with a flash */ }
    }
}

// Engines: one looping emitter per nozzle, following the ship; throttle → rate, faction color → tint.
let e = ctx.spawn_effect(
    builtin::ENGINE_EXHAUST,
    EffectSpawn::from_transform(Transform::from_xyz(nozzle.x, nozzle.y, nozzle.z)).follow(ship).tint(engine_color).keep(),
);
if let Ok(mut em) = ctx.world.get::<&mut ParticleEmitter>(e) {
    em.rate_scale = throttle;
}
// Boost: ENGINE_AFTERBURNER on the same nozzles. Bolts: LASER_TRAIL following the bolt entity.
// Held lasers / repairs: LASER_BEAM / REPAIR_BEAM with `beam_to(BeamEnd::Entity(target))`.
// Missiles: MISSILE_TRAIL; heavy hits: DEBRIS_SPARKS; EMP: EMP.
```

The full mapping of the game's sprite kinds, fades and effects (smoke = `ParticleBlend::Premultiplied`,
`Fade` → `Curve::Preset`, …) is in [`particles.md`](particles.md#migrating-the-games-effects). Space
dust (`SpriteQueue`) can stay in the overlay or become a looping emitter around the camera.
Explosions spawn `PointLight` flashes (their light touches nearby hulls); with more than 16 lights a
view keeps the 16 nearest.

## 7. Distortion

Nothing to do: `EXPLOSION`, `WARP_IN` / `WARP_OUT`, `EMP`, `ENGINE_EXHAUST` and
`ENGINE_AFTERBURNER` bend the image behind them through the camera's `PostProcess`
(`distortion.enabled`, on by default; `strength` scales every writer). Custom shockwaves and heat
haze: `DistortionMaterial` meshes (a ring that grows, `DistortionMode::Radial`) or
`SpriteRender::distortion(strength)` in an effect. ([`post.md`](post.md#distortion))

## 8. UI previews

`ui/preview.rs` renders each preview into a render target "at twice the display resolution and
filtered down (the offscreen targets have no MSAA)". Render targets take MSAA now, and a camera
with `PostProcess` targeting one gets the same look as the flight view, alpha preserved for egui:

```rust
use space3d::render::RenderTargetDesc;

let target = assets.add_render_target_with(size.0, size.1, RenderTargetDesc::default().with_samples(4));
sys.world.spawn((
    transform,
    Camera3d::default()
        .with_target(RenderTarget::Texture(target))
        .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 0.0)), // transparent: the composite keeps alpha
    PostProcess::default().with_tonemapper(Tonemapper::AgXPunchy),
    EnvironmentLight::new(studio_cube),                     // studio reflections instead of the rig swap
));
```

Render at display size instead of 2× (a quarter of the pixels). Previews live far from the maps,
so the particles and their light flashes stay out of them (particle draws are culled per view).

## 9. Loading screens: prewarm

WebGL2 links each shader program when a pipeline is created (50–150 ms). After a map's content is
loaded and one frame of it has rendered (behind the loading screen):

```rust
let materials: Vec<Material> = lib.ready.values().flatten().map(|p| p.material.clone()).collect();
let request = quality.prewarm_request(&materials);
ctx.resources.expect_mut::<Renderer>().prewarm(&request);
```

It builds the forward, depth and shadow pipelines of those materials for the frame's pass
signatures, every node's pipelines (particles, sky) and the post pipelines.

## 10. Web

* WebGL2 gets `GraphicsQuality::Low` (`recommended`): render scale 0.75, FXAA, a 4-level bloom
  (off where the browser has no float color buffers: the Ldr fallback tonemaps per fragment), a
  1024² PCF4 shadow, the CPU particle backend with 8 192 particles and thinner spawns, the IBL bake
  spread over frames. WebGPU browsers get Medium or High.
* Settings › Graphics can still offer every tier; clamp to Low on WebGL2 (`tier` above).
* Try the web look on the desktop: `FrameSettings::downlevel = DownlevelOverride::webgl2()` and
  `RenderSettings::draw_path = Some(DrawPath::CpuDirect)`.

## 11. Tests and performance

* **Goldens.** Screenshots change with post: re-bless once, intentionally
  (`SPACE3D_UPDATE_SNAPSHOTS=1`). GL / WebGL2 renders differ slightly: per-backend goldens with
  `SPACE3D_SNAPSHOT_VARIANT=backend`, or `CompareOptions::perceptual()`.
* **Determinism.** Particles, bloom, grain and exposure are deterministic for a given tick count
  (`render_to_image` twice gives the same image).
* **Timings.** `FrameSettings::gpu_timing = true` fills `GpuTimings` (per-pass GPU ms, 2–5 frames
  late); `RenderStats` and `ParticleStats` count the rest. The Battle reference scene (300 ships,
  100k particles, every feature) costs ~1.5 ms GPU at 1080p on an M4 Max
  ([`../performance.md`](../performance.md)).

## Checklist

| | done when |
|---|---|
| 0 | the game builds against the upgrade and looks the same |
| 1 | the game camera has `PostProcess` (AgX punchy, +0.5 EV); lights and emissive retuned |
| 2 | Settings › Graphics drives `GraphicsQuality::apply` + `ParticleSettings::for_quality_on`; `ui/render_scale.rs` uses `FrameSettings::render_scale` |
| 3 | ship materials get their maps (`apply_material_maps`) and tangents; the maps are no longer discarded |
| 4 | the sky is a `Skybox` cubemap and an `EnvironmentLight`; `ShadingModel::Standard` |
| 5 | the key light has a `DirectionalShadow` (`ShadowFit::top_down`); grid and walls opt out |
| 6 | effects are particles (or the overlay is `HdrScene`); lasers are `LASER_SHOT` / `LASER_BEAM` |
| 8 | previews use MSAA render targets and post |
| 9 | map loads prewarm |
| 11 | goldens re-blessed; GL variants where needed |
