# Graphics API: compatibility policy and ownership

This is the contract of the graphics upgrade (normative spec: [`docs/design/graphics.md`](../design/graphics.md),
§2 and §3.6). The public API of every area was frozen at milestone F2: every type, trait and
function exists and compiles, with neutral stubs (`STUB(<owner>)` comments) where the owning area
has not implemented the behaviour yet. Stubs never panic.

## Semver policy (spec §2.7)

Code written against Space3d 249c874 (what SpaceCorps2027 pins) keeps compiling and renders the
same pixels by default. `crates/space3d/tests/api_compat_249c874.rs` compiles the game's own forms
of every touched type; `crates/space3d/tests/prelude_policy.rs` checks the prelude additions
against every type name of the game.

* **Config structs** (`FrameSettings`, `CameraFrame`, `RenderTargetDesc`, `DownlevelOverride`,
  `PostProcess` and its effect structs, `LightingSettings`, `IblSettings`, `DirectionalShadow`,
  `Skybox`, `EnvironmentLight`, `TextureTransform`, `ParticleSettings`, the particle data model,
  `ModelUploadOptions`) are plain `pub`-field structs with `Default`. Construct them with
  functional record update (`..Default::default()`) or the builders: new fields may be added.
  Exhaustive struct literals of them are not covered by the compatibility guarantee.
* **Enums** added by the upgrade are `#[non_exhaustive]` (new variants may be added): match them
  with a wildcard arm.
* **Read-only info and context structs** (`GraphicsCaps`, `GpuTimings`, `GpuScope`, `FrameInfo`,
  `ViewInfo`, `PassTarget`, `MeshRef`, `TextureRef`, `SceneDepthRef`, the node contexts,
  `FrameLayouts`, `OverlayScene`, `RenderModel`, `RenderPart`, `MeshExtras`, `EffectState`) are
  `#[non_exhaustive]`: read their fields, never construct them.
* **Bit sets** (`SceneReads`, `SlotMask`, `MapMask`) are newtypes with `const`s and `BitOr`.
* **Existing types that gain fields** — the only ones, and all "FRU-only" downstream:
  `Material`, `RenderStats`, `space3d_assets::MaterialData`, `space3d_assets::Primitive`. Their
  new fields default to the legacy behaviour (`MaterialData`/`Primitive` fields are
  `#[serde(default)]`, so old RON/JSON still loads).
* **Frozen existing types** (unchanged): `Camera3d`, `Projection`, `RenderTarget`,
  `DirectionalLight`, `PointLight`, `AmbientLight`, `MeshData`, `InstancedMesh`,
  `InstanceLayout`, `RenderSettings`, `RendererConfig`, `RenderCapabilities`, `DrawPath`,
  `SamplerSettings`, `OverlayTarget`'s fields, `OverlayTargetKind`, `RenderPlugin`,
  `DefaultPlugins` / `MinimalPlugins`, `GltfLoader`. New configuration lives in new components and
  resources.
* **Traits**: `RenderOverlay` gains provided methods only (`stage`, `scene_reads`,
  `prepare_scene`): existing impls compile and draw exactly as before (`AfterTonemap`).
* **Preludes**: `space3d_render::prelude` (and so `space3d::prelude`) gains exactly `PostProcess`,
  `Tonemapper`, `Bloom`, `CameraFrame`, `Skybox`, `EnvironmentLight`, `DirectionalShadow`,
  `CubemapHandle`. Everything else is reached through `space3d_render::*` /
  `space3d::render::*`. Particles have their own prelude (`space3d::particles::prelude`, facade
  feature `particles`) and are never in the facade prelude.
* After F2, renames and removals of these items need the foundation maintainer's sign-off and an
  update of the compatibility fixtures; additions (fields on the FRU types above, variants of
  non-exhaustive enums, provided trait methods, new items in an area's hub) are free.

## Where the API lives

`space3d-render`'s crate root re-exports each area's hub, so areas never edit `lib.rs`:

| hub | area | contents |
|---|---|---|
| `settings.rs` | frame (F) | `FrameSettings`, `CameraFrame`, `HdrMode`, `DownlevelOverride`, `RenderTargetDesc`, `RenderTargetFormat` |
| `caps.rs` | F | `GraphicsCaps` |
| `output.rs` | F | `OutputEncoding`, `Tonemapper`, `OUTPUT_WGSL` |
| `node.rs` | F | the render-node API (`RenderNode`, `RenderNodes`, contexts, `PassTarget`, `FrameLayouts`, `VIEW_WGSL`, `scene_wgsl`, `FULLSCREEN_WGSL`) |
| `overlay.rs` | F | `OverlayStage`, `OverlayScene`, `RenderOverlay` provided methods |
| `timing.rs`, `quality.rs` | F (quality: I) | `GpuTimings`, `GpuScope`, `GraphicsQuality` |
| `material.rs` | P-MAT | `BlendMode`, `ShadowMode`, `MapMask`, `TextureTransform`, `MeshExtras` |
| `lighting/api.rs` | P-MAT | cubemaps, `Skybox`, `EnvironmentLight`, `DirectionalShadow`, `LightingSettings` |
| `post/api.rs` | P-POST | `PostProcess` and every post type, `DISTORTION_WGSL` |

Crate-internal contracts between the frame and the areas (spec §2.6) have fixed signatures and
stub bodies: `lighting::Lighting` (+ `DepthViewRequest`, `ViewResources`, `LightingPrepare`),
`post::PostState` (+ `PostGroupInput`, `PostGroupPlan`, `PostGroupNeeds`, `PostEncode`),
`frame::extract::source_casts`, `frame::targets::TexturePool`.

Additions made by the foundation since F2 (additive):

* F4: `PostState` has a working composite-only chain (exposure, tonemapper, dither,
  alpha, rect mapping, upscale). `PostState::plan_group(world, plan, reallocs)` writes the
  group's `GpuPost` slot (in call order) and `PostState::flush(device, encoder, uploader)`
  uploads the slots before any `encode_group`. `PostGroupPlan` carries unclipped `[i32; 4]`
  rects plus `scene_size`, `output_size`, `scene_alpha`, `transparent` and `passthrough`.
  `PostGroupInput` adds `index` (the plan slot), `scene_generation` (bind-group cache key) and
  `output_viewport` (the drawn rect; `output_rect` is its scissor). `PostEncode` adds `pool`
  (the frame's `TexturePool`, released after each group).

* F5: `PostGroupPlan::distortion` (the group's Distortion phase runs this frame: the composite's
  `DISTORTION` bit), `PostState::distortion_target` counts created buffers (reported through
  `end_frame` in `textures_created`), `PostState::distortion_view(camera)` returns the existing
  buffer (a cheap view clone, its size and generation) for the frame's passes and replays, and
  `PostState::prewarm(device, format, manual_srgb)` builds a composite pipeline ahead of use
  (`Renderer::prewarm`). The composite binds `PostGroupInput::distortion` at group 1 binding 4
  (input bind groups are keyed by `(scene_generation, distortion generation)`).
  `FrameLayouts::scene` has a third binding: a non-filtering nearest sampler (binding 2) through
  which `scene_wgsl`'s `s3d_scene_depth` samples: WebGL2 depth textures read as 0 through
  `textureLoad` (it uses the texture's own linear filter state there, and GLES 3.0 depth
  textures are incomplete unless filtered nearest). Node shaders declaring only bindings 0–1
  keep working.
* F5: `frame::extract::material_class(&Material) -> u8` is the one place pipeline class bits
  come from (P-MAT extends it for bits 2..=5; `Renderer::prewarm` uses it).

## Ownership map (spec §3.6)

A builder edits only the files it owns; everything else goes through the foundation maintainer
(`F`), additively. `F→X` files are written by F and owned by X afterwards.

| files | owner |
|---|---|
| root `Cargo.toml` (workspace deps), `crates/space3d-core/src/data.rs` (`HdrImageData`) | F |
| `crates/space3d-render/Cargo.toml`, `ARCHITECTURE.md` | F (I merges area summaries) |
| `src/{lib, settings, caps, output, node, overlay, timing, renderer, stats}.rs` | F |
| `src/quality.rs` | F→I |
| `src/components.rs` (`Material` fields and docs) | F |
| `src/material.rs` | F→P-MAT |
| `src/frame/{mod, plan, views, targets, passes, nodes, tests}.rs` | F |
| `src/frame/{extract, draws}.rs` | F→P-MAT |
| `src/pipeline/{mod, view}.rs` | F |
| `src/pipeline/{forward, instance}.rs`, `src/{arena, assets, texture, handles}.rs` | F→P-MAT |
| `src/lighting/**` | F→P-MAT |
| `src/post/{mod, api, chain}.rs` | F→P-POST |
| `src/post/{bloom, exposure, fxaa, lut, grading, distortion, distortion_mesh, readback, pipelines}.rs` | P-POST |
| `src/shaders/{cull, compact, blit, clear}.wgsl`, `src/shaders/common/*`, `src/shaders/frame/*` | F |
| `src/shaders/forward.wgsl` | F→P-MAT (F owns the `//#VIEW` and `//#OUTPUT` regions) |
| `src/shaders/{depth, sky, ibl_bake}.wgsl`, `brdf_lut.bin` | F→P-MAT |
| `src/shaders/post/{composite, distortion}.wgsl` | F→P-POST |
| `src/shaders/post/{bloom, exposure, exposure_dl, fxaa, distortion_mesh}.wgsl` | P-POST |
| `tests/{frame_modes, nodes, overlay_stages, depth_views, capture_hdr, alloc_frame, gl, alloc, headless, custom_shader}.rs`, `tests/common/mod.rs` | F |
| `tests/{materials, environment, shadows, gltf_render, gl_materials, alloc_materials}.rs`, `tests/common/materials.rs` | P-MAT |
| `tests/{post, post_distortion, gl_post, alloc_post}.rs`, `tests/common/post.rs` | P-POST |
| `benches/{instancing, frame}.rs` / `materials.rs` / `post.rs` / `graphics.rs` | F / P-MAT / P-POST / I |
| `examples/frame_nodes.rs` | F |
| `crates/space3d-assets/**` (incl. the `render` feature) | F (skeleton) → P-MAT |
| `crates/space3d-particles/**` | F (skeleton) → P-PART |
| `crates/space3d-stories/src/engine/{mod, basics, frame}.rs`, `Cargo.toml` | F |
| `crates/space3d-stories/src/engine/{materials, lighting}.rs` / `post.rs` / `particles.rs` / `showcase.rs` | P-MAT / P-POST / P-PART / I |
| goldens `tests/__screenshots__/Engine/{Frame}` / `{Materials, Environment, Lights/Directional_Shadow}` / `Post` / `Particles` / `Showcase` | F / P-MAT / P-POST / P-PART / I |
| `crates/space3d/{Cargo.toml, src/lib.rs, tests/api_compat_249c874.rs, tests/prelude_policy.rs}` | F |
| `crates/space3d-editor/src/components.rs` | F → I (reprs) |
| `examples/{pbr_showcase}` / `{post_processing}` / `{particles, particles_stress}` / `{graphics_showcase}` | P-MAT / P-POST / P-PART / I |
| `docs/graphics/{api, frame}.md` / `materials.md` / `post.md` / `particles.md` / `migration.md` | F / P-MAT / P-POST / P-PART / I |

Change requests to F files: `scratchpad/graphics/change-requests.md` (file, change, reason), or
through the orchestrator.
