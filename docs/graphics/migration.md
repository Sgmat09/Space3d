# Migrating SpaceCorps2027 to the graphics upgrade

A step-by-step guide for the game (`~/git/spacecorps/SpaceCorps2027`, `client/src`) to adopt every
feature of graphics milestones 1 and 2: HDR, tonemapping and bloom, quality presets and render
scale, material maps straight from glTF, a cubemap sky with image-based lighting, the sun's
shadow, the particle library (engines, lasers, explosions, shields, warps), distortion, and the
post chain. Each step is independent and shippable on its own; the order below gives the biggest
visual win first. The overview of every feature is [`../graphics.md`](../graphics.md).

**Where the game is.** At its HEAD (b120d9b) the game pins Space3d `80ab72b` (graphics M1 and
main's keyboard-layout work up to the pinned fake layouts and the macOS ISO / JIS keys, plus the
headless pacing; the graphics branch has merged all of it) and has already done parts of this
guide: the flight, backdrop, viewer and preview cameras have a `PostProcess` (`game/look.rs`: `KhronosPbrNeutral` over a
full-frame lift, `TONE_LIFT`, bloom, FXAA), the graphics-quality setting drives
`FrameSettings::render_scale` (`ui/render_scale.rs` is gone), `game/additive.rs` draws in
`OverlayStage::HdrScene`, the effects are particles (`game/fx.rs` with its own `.pfx.ron`
exhaust, explosion, warp, hit and muzzle effects), and previews below 120 pt skip bloom. What
M2 adds for it: material maps from glTF (step 3), the cubemap sky and image-based lighting
(step 4), the sun's shadow (step 5), quality presets (step 2), and the M2 fixes to the game's
M1 feedback (game#1–#15, marked below).

**Step 0 changes nothing.** The game's code compiles unchanged against M2 and renders the same
pixels, except where a fix changes behavior it asked for: particle emitters moved in
`Stage::Update` no longer bead at 30 and 45 fps (game#2), sprites soft-clip at render scales
below 1 without post (game#3), a lowered `ParticleSettings::max_particles` applies at once
(game#14), and the built-in effects are retuned for the top-down view (game#11; the game
mostly plays its own effects). Checked: a scratch copy of the game's workspace pointed at this
tree type-checks with 0 warnings; `crates/space3d/tests/api_compat_249c874.rs` compiles its forms
of every touched type. One behavior differs by design since M1: with **more than 16 point
lights**, each view is lit by the 16 nearest to its camera instead of the first 16 in query
order (spec D19).

| step | game files | engine feature |
|---|---|---|
| [0](#0-update-the-engine) | `Cargo.toml` | the upgrade, the `particles` feature |
| [1](#1-hdr-and-the-battle-look) | `game/look.rs` (done at M1) | `PostProcess`: the look; M2: `KhronosPbrNeutralNoToe` replaces Neutral + `TONE_LIFT` |
| [2](#2-quality-presets) | `session.rs`, `game/look.rs`, `ui/pages/settings.rs` | `GraphicsQuality`, `ParticleSettings::for_quality_on` (render scale done at M1) |
| [3](#3-ship-materials-from-gltf) | `game/models.rs` | normal, ORM, emissive maps; tangents |
| [4](#4-the-sky-as-a-cubemap-and-image-based-lighting) | `game/env.rs`, `game/camera.rs` | `Skybox`, `EnvironmentLight`, `ShadingModel::Standard` |
| [5](#5-the-suns-shadow) | `game/camera.rs` | `DirectionalShadow` |
| [6](#6-effects-particles-and-the-additive-overlay) | `game/fx.rs`, `game/additive.rs`, `game/fx/*`, lasers in `ui/hud` | particles; `OverlayStage::HdrScene` |
| [7](#7-distortion) | — | built into the particle effects; `DistortionMaterial` |
| [8](#8-ui-previews) | `ui/preview.rs`, `game/look.rs` | MSAA render targets, post on previews, `Bloom::min_rect_height` |
| [9](#9-loading-screens-prewarm) | map loading | `GraphicsQuality::prewarm_request` |
| [10](#10-web) | `web/`, settings | Low on WebGL2 |
| [11](#11-tests-and-performance) | `client/tests` | goldens per backend, `DownlevelOverride::webgl2()`, `GpuTimings` |

## 0. Update the engine

```toml
# Cargo.toml (workspace): from rev 80ab72b (M1 + main) to the M2 merge
space3d = { git = "https://github.com/SpaceCorps/Space3d", rev = "<graphics M2 merge>", default-features = false,
            features = ["render", "platform", "assets", "audio", "net", "ui", "github", "particles"] }
```

`particles` (already on) adds `space3d::particles` (its own prelude; it is not in
`space3d::prelude`). Build, run, compare. A lockfile pinned before `hyper-util` 0.1.21 needs
`cargo update -p hyper-util` (game#7, noted on main).

## 1. HDR and the battle look

**Done at M1** (`game/look.rs`: the flight camera renders HDR with `KhronosPbrNeutral`, bloom and
FXAA). What M2 changes for it:

* **Drop the lift.** Neutral's toe subtracts `x − 6.25·x²` from every channel of a color whose
  weakest channel `x` is under 0.08, nearly all of a dim sky's weakest channel (sRGB (21, 47, 65)
  → (1, 40, 60)); the game lifts the whole scene by `TONE_LIFT` 0.018 to compensate, at the cost
  of blacks at sRGB ~7 (game#9). `Tonemapper::KhronosPbrNeutralNoToe` passes colors unchanged
  below 0.8 and rolls the highlights off like Neutral: use it for `FLIGHT_TONEMAPPER`,
  `showcase`, `viewer` and `preview`, and remove `lift()` and the overlay's `Kind::Fill` (the
  exposures tuned against the lift, `FLIGHT_EXPOSURE` 0.15, want a second look: without the lift
  and the toe the image is brighter by the toe's 0.04 above it).
* **If you want AgX.** `AgXPunchy` + 0.5 EV (below, the reference Battle's look) skews dim
  saturated nebulae toward their strongest channel — the reason the game rejected it (game#6;
  M-1's magenta: (54, 0, 21), green crushed). Plain `AgX` with `ColorGrading::vivid()` keeps the
  hue and adds saturation (the same magenta (78, 8, 47); frame.md, "Choose the look
  explicitly", has the numbers).

For a new camera: give it a `PostProcess`: the scene then renders in linear HDR, bloom makes
everything above 1 glow, and a tonemapper maps it to the display. egui still draws after
tonemapping, unchanged.

```rust
use space3d::render::Vignette; // PostProcess and Tonemapper are in space3d::prelude

let entity = ctx.world.spawn((
    Transform::from_xyz(0.0, 400.0, 150.0).looking_at(Vec3::ZERO, Vec3::Y),
    Camera3d::default()
        .with_clear_color(Vec4::new(0.004, 0.006, 0.012, 1.0))
        .with_projection(Projection::Perspective { fov_y: FOV_Y_DEG.to_radians(), near: 2.0, far: FAR }),
    // The reference Battle's look: AgX punchy half a stop up keeps faction colors, lasers and
    // fire saturated (plain AgX turns them pastel) but skews dim saturated skies (above), bloom
    // on everything above 1, a light vignette. Distortion is on by default (free until
    // something writes it).
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
* The `additive.rs` overlay draws in the HDR scene (step 6a, done at M1), so it blooms too.

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

**Render scale: done at M1.** The game sets `FrameSettings::render_scale` from
`GameSettings::render_scale()` (`ui/render_scale.rs`'s render-target path is gone): the scene
renders smaller into the primary scene texture, the composite upscales it, and egui stays sharp.
Since M2 sprites also soft-clip at render scales below 1 when post is off, as they do at 1
(game#3: the "low" quality with post off no longer flattens HDR sparks).

**The particle budget.** `game/look.rs` works around M1's grow-only pool
(`session_particle_budget`: "Space3d M1's pool only grows"). Since M2 a lowered
`ParticleSettings::max_particles` applies at once (new rings stop at it; the free tail is given
back), and `ParticleStats` reports `pool_capacity` (the budget in force), `pool_allocated` and
`pool_high_water` (game#14): the workaround can go.

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
* **Emissive.** The painted hulls carry `emissiveFactor` 1.0 with the emissive map as the mask,
  and bake their nav lights into it (187–304 texels on `SC_Hull` of Protos, Wraith, Echelon and
  Ostirion, 275 on Paragon's `SC_Trim`). `apply_material_maps` multiplies the material's
  `emissive` by the map; it does not set the factor. `painted_material` gives Hull and Trim no
  emissive, so their map would contribute nothing: **seed the factor wherever there is a map**
  before `apply_material_maps` (the engine's own `upload_model` does this):

  ```rust
  if data.emissive_texture.is_some() && material.emissive == Vec3::ZERO {
      material.emissive = Vec3::from_array(data.emissive); // the map is the mask
  }
  apply_material_maps(&mut material, &data, &mut |i, role| texture(assets, i, role));
  ```

  Setting `emissive` from the factor *without* the map washes every hull out white (measured on
  Protos.glb and Wraith.glb). With the map, glowing roles can drop their separate uniform
  emissive materials.
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

and load it like any asset (`AssetsPlugin` registers the `.skybox` and `.hdr` loaders and their
asset types, `CubemapData` and `HdrImageData`; before M2's fix the asset types were missing and
`load::<CubemapData>` panicked):

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
  ambient (the environment's irradiance replaces most of it).
* **Shadows need a fill.** A dark nebula lights little: at diffuse 1 the sides the sun misses
  and every cast shadow get 1–2 % of a sunlit surface's light and read as black (the M2 final
  review's finding in the showcase). Aim for 10–20 % on a side facing away from the sun: raise
  the diffuse intensity until `irradiance × intensity` is 7–10 % of the sun (a sky whose
  irradiance is about 0.03 needs about 6), and add a shadowless `DirectionalLight` from the side
  opposite the sun and a little from below, in the nebula's hue, at about 15 % of the sun — the
  showcase's rig (`space3d_stories::showcase::{add_environment, spawn_fill}`: diffuse 6,
  specular 2.5, a 0.35 lavender fill against a 2.2 sun, all raised × 1.3 at Medium and × 1.8 at
  Low by `fill_lift`) gives 15 %. Keep the fill's color muted (a saturated one outlines crease
  walls) and the ambient faint.
  [`materials.md`](materials.md#fill-light-dark-skies) explains the ratio. The backdrop variant
  (`spawn_backdrop_sky`, softened highlights under the station panels) becomes a second cubemap
  built from softened images, or the same one with a lower `Skybox::intensity`.
* **Orientation.** `Skybox::rotation` / `EnvironmentLight::rotation` turn the sky if a sector's
  sun ends up in the wrong place.
* **What stays.** The nebula veils, star layers, planet, debris and grid are geometry in the scene
  (parallax): keep them. Remove the six `SkyFace` quads and the `SKYBOX_HALF` constraint on `FAR`.
* **Memory.** An 8-bit 1024² sky is 25 MB of faces and 33.5 MB with its mips (six faces × 1024²
  × 4 B × 4/3); the environment bake adds about 4.2 MB, so one sector's sky costs about 38 MB
  (`RenderStats::lighting_bytes`: 37.8 MB for the game's M-1 sky loaded from its `.skybox`
  manifest). An HDR sky costs the same where the device renders `Rg11b10Ufloat` and compute
  shaders write it (`GraphicsCaps::hdr_sky_format`: desktop Metal, most Vulkan and D3D12
  drivers; its conversion and mips take about 1–2 ms of GPU once) and twice as much where it is
  kept `Rgba16Float` (GL, WebGL2 and WebGPU: 67 MB at 1024²).
  The presets' recommended face sizes: 512 (Low, 8.4 MB with mips) / 1024 / 2048.

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

**a) Keep `additive.rs`, draw it into the HDR scene** (done at M1: the world copy draws in
`OverlayStage::HdrScene`) so the sprites bloom and tonemap with the scene: four steps in
[`frame.md`](frame.md#migrating-an-additive-overlay-the-games-additivers) — stage per instance
(`HdrScene` for the world copy, `AfterTonemap` for the `UiParticles` copy over egui), encoding
from `prepare_scene`, no `1 − e^−x` soft clip outside Direct mode, `Rgba16Float` pipelines —
plus the engine render scale of step 2.

**b) Replace the sprites with the particle library** (GPU simulated, 100k+, depth-tested, soft
against hulls, lit flashes, distortion, deterministic on the fixed tick). Written against the
game's `fx::Effect` at b120d9b (`client/src/game/fx.rs`; the functions below compile in a module
next to it, with `use super::fx::{self, Effect}` and `super::scene::AvatarIndex`); the game's own
`fx::play` already does this with its own effects:

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
        Effect::ShieldHit { center, radius, dir, target } => {
            // `target` is the ship's net id (0: none). World `center` and `dir` on a moving ship:
            // `follow_world` converts them into the ship's space (plain `follow` takes the
            // transform as local: at(center).follow(ship) would play at twice the ship's
            // position, game#1).
            let ship = (target != 0)
                .then(|| ctx.resources.expect::<AvatarIndex>().by_id.get(&target).copied())
                .flatten();
            let spawn = EffectSpawn::at(center).facing(-dir).scale(radius / 20.0);
            let spawn = match ship {
                Some(ship) => spawn.follow_world(ship, ctx.world),
                None => spawn,
            };
            ctx.spawn_effect(builtin::SHIELD_RIPPLE, spawn);
        }
        Effect::Warp { at, scale, arriving } => {
            let fx = if arriving { builtin::WARP_IN } else { builtin::WARP_OUT };
            ctx.spawn_effect(fx, EffectSpawn::at(at).scale(scale));
        }
        Effect::Muzzle { at, dir, color } => {
            // The game's own `spacecorps/muzzle` (`Effects::muzzle`); or a hit spark's flash.
            ctx.spawn_effect(builtin::HIT_SPARK, EffectSpawn::at(at).facing(dir).tint(color.extend(1.0)).scale(0.5));
        }
    }
}

// Hitscan lasers (drawn with egui today): a 0.12 s beam that despawns itself.
fn laser(ctx: &mut SystemCtx, from: Vec3, to: Vec3, color: Vec3) {
    ctx.spawn_effect(builtin::LASER_SHOT, EffectSpawn::at(from).beam_to(BeamEnd::World(to)).tint(color.extend(1.0)));
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

M2 changes for the game's effects (it plays its own `.pfx.ron` effects, so most of this is for
its authoring):

* **Moving ships in `Stage::Update`** (`fx::animate`, the exhaust emitters): emission no longer
  beads at 30 and 45 fps — each fixed tick extrapolates the steady motion measured over the
  previous frame to its own time (game#2; particles.md, "Moving emitters").
* **Ships easing toward 20 Hz server updates**: the built-in `ENGINE_EXHAUST` and
  `ENGINE_AFTERBURNER` now smooth their emitters' velocity over 0.1 s by default
  (`ParticleEffect::velocity_smoothing`): the time rate follows each tick's distance against the
  smoothed speed and particles inherit the smoothed velocity, so the plume keeps an even density
  and its length does not depend on the step pattern (game#8; particles.md, "Uneven motion").
  For the game's own exhaust effects, add `velocity_smoothing: 0.1` to the `.pfx.ron` (or
  `ParticleEmitter::with_velocity_smoothing(0.1)` on the emitters). Its `EXHAUST_FOLLOW` easing
  of the emitter poses can stay. The game's exhaust recipe (mostly per distance flown, cores
  wider than their spacing, a small time rate) is what the built-in `ENGINE_EXHAUST` does now
  too, and what particles.md recommends (game#12).
* **Fast emitters**: `rate_per_distance` rings are sized for `Spawn::max_speed` (100 units/s by
  default); looping effects now grow them with the emitter's speed, so the exhaust's
  `capacity: Some(192)` override can go; one-shots on fast bolts set `max_speed` (game#10).
* **The built-ins' reference view** is the game's: 1.4 px per unit with 40-unit hulls at
  `scale` 1 (particles.md, "Built-in effects"): warps about 4 hulls across, explosions whose
  fireball burns out within a second, 3-px sparks, no thin crisp rings (game#11).

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
    PostProcess::default().with_tonemapper(Tonemapper::KhronosPbrNeutralNoToe),
    EnvironmentLight::new(studio_cube),                     // studio reflections instead of the rig swap
));
```

**Bloom on small previews costs passes, not pixels** (game#15): every render-target camera with
post is its own post group, and its bloom chain is about 9 render passes whatever its size —
four 320 × 192 shop cards made the frame 47 passes against 11 without them. Keep the game's rule
(no bloom under 120 pt) or move it into the engine with `Bloom::min_rect_height` (post.md,
"Small views": 240 px is a good threshold); level 0 is never resampled up any more, which saves
a pass on every small group.

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
  1024² PCF4 shadow, the CPU particle backend with 8 192 particles and thinner spawns (quality
  0.1 on the web; the built-ins leave their engines' sparks and heat haze out below 0.3, and
  under a full budget the plumes outrank the explosions' smoke and embers: the Battle holds the
  budget at 16:9 without dropping a plume), the IBL bake spread over frames. WebGPU browsers get
  Medium or High.
* The game's own effects can do the same: `ParticleLayer::min_quality` (a detail layer left out
  below that quality × LOD level) and `ParticleLayer::priority` (a layer's budget priority,
  overriding the effect's) are new, both defaulting to the old behavior.
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
| 0 | the game builds against M2 (from rev 80ab72b) and looks the same |
| 1 | done at M1 (`PostProcess` on every camera); M2: `KhronosPbrNeutralNoToe` replaces Neutral + `TONE_LIFT` (or AgX + `ColorGrading::vivid()`) |
| 2 | Settings › Graphics drives `GraphicsQuality::apply` + `ParticleSettings::for_quality_on` (render scale done at M1); the grow-only budget workaround removed |
| 3 | ship materials get their maps (`apply_material_maps`, emissive factor seeded where there is a map) and tangents; the maps are no longer discarded |
| 4 | the sky is a `Skybox` cubemap and an `EnvironmentLight`; `ShadingModel::Standard`; shadowed sides get a 10–20 % fill |
| 5 | the key light has a `DirectionalShadow` (`ShadowFit::top_down`); grid and walls opt out |
| 6 | done at M1 (particles, the overlay in `HdrScene`); M2: `follow_world` for hits on moving ships, `velocity_smoothing: 0.1` in the game's exhaust effects |
| 8 | previews use MSAA render targets and post; bloom only above ~240 px (`Bloom::min_rect_height`) |
| 9 | map loads prewarm |
| 11 | goldens re-blessed; GL variants where needed |
