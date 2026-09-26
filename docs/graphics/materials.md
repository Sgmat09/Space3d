# Materials: maps, blend modes, glTF import

`Material` renders lit or unlit surfaces with a base color, metallic/roughness, emissive and — since
the graphics upgrade — normal, metallic-roughness, occlusion and emissive maps, five blend modes,
per-material UV transforms and a second UV set. glTF models "just light up": every standard
material slot is read and converted. The normative design is
[`docs/design/graphics.md`](../design/graphics.md) §2.2.8, §2.4 and §5.2 (P-MAT); this page covers
milestones PM1 (blend modes and maps) and PM2 (glTF import and tangents). Skyboxes, image-based
lighting and shadows (PM3–PM5) come next.

A material without maps, with `BlendMode::Opaque` (or the legacy `alpha_blend`), renders exactly —
bit for bit — as before the upgrade: every map sits behind a uniform flag, and new settings default
to their neutral values.

## Maps

```rust
use space3d::prelude::*;
use space3d::render::{MapMask, TextureTransform};

let assets = app.resources.expect_mut::<RenderAssets>();
let base = assets.add_image(albedo);            // sRGB (color)
let normal = assets.add_image_linear(normals);  // linear (data)
let orm = assets.add_image_linear(orm);         // linear: R occlusion, G roughness, B metallic
let glow = assets.add_image(emissive);          // sRGB (color)
let material = Material::color(Vec4::ONE)
    .with_texture(base)
    .with_normal_map(normal, 1.0)               // scale of the normal's XY
    .with_orm_map(orm)                          // = metallic-roughness + occlusion, one sample
    .with_emissive_map(glow, Vec3::splat(4.0))  // factor × map; > 1 feeds bloom with post
    .with_uv_transform(TextureTransform { scale: Vec2::splat(2.0), ..TextureTransform::IDENTITY });
```

| field | texture | meaning |
|---|---|---|
| `base_color_texture` | sRGB | × `base_color` (alpha too) |
| `normal_texture`, `normal_scale` | **linear**, tangent space, +Y green up (glTF / OpenGL) | XY × scale, Z reconstructed (two-channel maps work) |
| `metallic_roughness_texture` | **linear**, G roughness, B metallic | × `roughness`, × `metallic` |
| `occlusion_texture`, `occlusion_strength` | **linear**, R | darkens ambient (and image-based) light only: `1 + strength × (R − 1)` |
| `emissive_texture` | sRGB | × `emissive` (so `emissive` must be non-zero, as in glTF) |
| `uv_transform` | — | `KHR_texture_transform` (offset, rotation, scale) of every map, before the per-instance `UvRect` |
| `uv1_maps` | — | maps (`MapMask`) sampled with the mesh's second UV set (`MeshExtras::uvs1`; UV0 without one) |

* **ORM packing.** When `occlusion_texture` is the same handle as `metallic_roughness_texture`,
  the texture is sampled once (R occlusion, G roughness, B metallic).
* **Color spaces.** Upload color maps (base color, emissive) with `add_image` (`Rgba8UnormSrgb`),
  data maps (normal, metallic-roughness, occlusion) with `add_image_linear`
  (`TextureSource::from_image_linear`, `Rgba8Unorm`). A data map uploaded as sRGB by mistake still
  works — the shader re-encodes the sample, losing precision, and a warning is logged.
* **Texture arrays.** The color maps (base color, emissive) and the data maps (normal,
  metallic-roughness, occlusion) each share one texture kind: single-layer or array. Arrays sample
  the per-instance `TextureLayer`, so per-instance skins can share one normal/ORM array. A map of
  the other kind within a group is not sampled (warned once).
* **Missing textures** (not uploaded yet, removed, unsupported format) bind a white default and
  their map is switched off in the material uniform: the factors alone apply.
* `LightingSettings::normal_maps = false` ignores normal maps globally (low-end devices).

### Tangents

Normal maps need a tangent frame:

* **Vertex tangents** (MikkTSpace, as baked maps expect): register the mesh with
  `RenderAssets::add_mesh_with(data, MeshExtras::new().with_tangents(t))`. The tangents (and a
  second UV set) live in the arena's **extension stream**: 16 bytes per vertex (`Snorm16x4`
  tangent, `Float32x2` UV1) in lockstep with the mesh arena, created with the first mesh that has
  extras. `space3d_assets::edit::mesh::compute_mikktspace_tangents(&mesh)` computes them (glTF
  orientation: `cross(normal, tangent) × w` points up the image, toward −v), splitting vertices
  where mirrored UVs meet. Run it off the frame (`spawn_compute`).
* **Per-pixel frames**: without vertex tangents the frame comes from screen-space derivatives
  (a cotangent frame) — correct for most maps, slightly different from a MikkTSpace bake along
  seams.

Instance layouts with more than 10 vertex locations leave no room for the extension stream: their
draws use per-pixel frames and UV0 (warned once per layout). Double-sided materials flip the whole
frame on back faces (glTF).

## Blend modes

```rust
use space3d::render::BlendMode;
Material::unlit(color).with_blend(BlendMode::Additive);
Material::additive(Vec4::new(1.0, 0.5, 0.2, 1.0));      // unlit, additive, double-sided glow
Material::premultiplied(Vec4::new(0.4, 0.2, 0.1, 0.5)); // unlit; a = 0 adds light
Material::default().with_alpha_mask(0.5);               // cut-outs
```

| `BlendMode` | color | alpha | depth write | phase |
|---|---|---|---|---|
| `Opaque` (default) | src | src (written as is) | yes | opaque |
| `AlphaMask` | src where alpha ≥ `alpha_cutoff`, else discarded | 1 | yes | alpha mask (after opaque) |
| `AlphaBlend` (= legacy `alpha_blend: true`) | `src·a + dst·(1−a)` | `a + dst·(1−a)` | no | transparent |
| `Premultiplied` | `src + dst·(1−a)` (store color × alpha) | `a + dst·(1−a)` | no | transparent |
| `Additive` | `src·a + dst` | dst (kept) | no | transparent |

`BlendMode::blend_state()` is shared with the particle renderer, so meshes and particles blend
identically. With MSAA, `AlphaMask` becomes **alpha to coverage** (smooth, sorted-free edges);
without it, it discards.

**Draw order** of a view (spec §1.7): opaque batches, alpha-masked batches, (sky), then
transparent batches by `Material::render_order` (lower first), alpha-blended and premultiplied
before additive at equal order, then by pipeline and material. `render_order` orders meshes only
(particles have their own order and draw after meshes).

**Per-instance sorting** is opt-in: `LightingSettings::transparent_sort =
TransparentSort::PerInstance { max_instances }` sorts the instances of alpha-blended and
premultiplied ECS batches (up to `max_instances` each) back to front per view, on every draw path
(those batches are culled on the CPU). The default `TransparentSort::None` keeps batch (spawn)
order. Additive batches never need sorting; explicit `InstanceBuffer`s keep their order.

## Shading models

`LightingSettings::shading` selects the lighting model of lit materials, frame-wide:

* `ShadingModel::Classic` (default): the original Blinn-Phong-style model, pixel-identical.
* `ShadingModel::Standard`: GGX distribution, height-correlated Smith visibility, Schlick Fresnel,
  Lambert diffuse — in the same light units (a white Lambert surface facing a light of intensity 1
  reflects 1). Use it with image-based lighting (PM4) and for the "High" look.

## glTF import

The glTF loader (`space3d::assets::GltfLoader`, `load_gltf`) reads every material slot into
`MaterialData`: base color, metallic-roughness, normal (+ `scale`), occlusion (+ `strength`),
emissive (+ `emissiveFactor` × `KHR_materials_emissive_strength`), `KHR_materials_unlit`, alpha
mode and cutoff, double-sidedness, and per slot its `texCoord`, sampler and
`KHR_texture_transform`. Primitives' `TANGENT` and `TEXCOORD_1` go to `ModelMesh::extras`.

The render bridge (`space3d::assets::render`, facade feature `render`) turns a model into render
assets:

```rust
use space3d::assets::render::{ModelUploadOptions, prepare_model_async, spawn_model};

// Off the frame: MikkTSpace tangents for normal-mapped primitives without TANGENT.
let task = prepare_model_async(model, ModelUploadOptions::default());
// Later, when `task.poll()` is ready, on the main thread (registration only):
let render_model = prepared.upload(&mut assets);
spawn_model(&mut world, &render_model, Transform::IDENTITY);
```

| glTF | `Material` |
|---|---|
| `baseColorFactor` / `baseColorTexture` (sRGB) | `base_color` / `base_color_texture` |
| `metallicFactor`, `roughnessFactor` / `metallicRoughnessTexture` (linear) | `metallic`, `roughness` / `metallic_roughness_texture` |
| `normalTexture` (+`scale`, linear) | `normal_texture`, `normal_scale` |
| `occlusionTexture` (+`strength`, linear); same image as metallic-roughness | `occlusion_texture` (= the same handle: ORM packed), `occlusion_strength` |
| `emissiveFactor` × `KHR_materials_emissive_strength` / `emissiveTexture` (sRGB) | `emissive` (unbounded, HDR) / `emissive_texture` |
| `alphaMode` OPAQUE / MASK (+`alphaCutoff`) / BLEND | `BlendMode::Opaque` / `AlphaMask` + `alpha_cutoff` / `AlphaBlend` |
| `doubleSided`, `KHR_materials_unlit` | `double_sided`, `unlit` |
| `texCoord: 1` on a slot | its bit in `uv1_maps` |
| `KHR_texture_transform` | `uv_transform`: the base color's, else the first slot's (different transforms per slot: warned, the first applies) |
| sampler wrap / filter | the texture's `SamplerSettings` (one upload per image × color space × sampler) |

Games that build their own materials (e.g. a paint-scheme material per role) keep them and add the
maps with `apply_material_maps(&mut material, &data, &mut upload)`: normal, metallic-roughness,
occlusion and emissive maps, their scales and strengths, ORM packing, `emissive_strength` × the
material's `emissive` (only where it is non-zero or the slot exists), `uv1_maps` and the UV
transform. Base color, blend mode and the surface factors are left alone (roughness and metallic
factors multiply the map). `upload` should use `add_image` for `TextureRole::is_srgb()` roles and
`add_image_linear` otherwise.

The storybook shows the path end to end: `Engine/Materials/glTF Import` loads
`crates/space3d-stories/assets/ship.glb`, a small hull with baked base color, normal, ORM and
emissive maps and no `TANGENT` (generated by `space3d-assets/tests/gltf_materials.rs`).

## WebGL2 and GLES

Everything above works on WebGL2 / GLES 3.0: a material binds 5 textures + the defaults (9 of the
16 texture units the fragment stage has, counting the view's), no storage buffers, and the
extension stream is rebound with the mesh arena where `BASE_VERTEX` is missing. Alpha to coverage
needs MSAA (usually off on the web). Verified on ANGLE (GLES 3.0): `tests/materials.rs` with
`WGPU_BACKEND=gl` and `tests/gl_materials.rs` (GL vs Metal).

## Performance

* Materials are interned per entity per frame; a material equal to the previous entity's is
  answered by one comparison (`Material: PartialEq`), others by a small cache. Texture sets get
  one bind group each, rebuilt only when a texture or the materials buffer changes.
* Maps cost only the materials that have them: the samples sit behind uniform branches.
* Steady state: no allocation in the renderer's code with every feature above in use
  (`tests/alloc_materials.rs`); a static scene uploads nothing.
* `benches/materials.rs`: `materials/ecs_pbr/100k` (100k static entities, 8 PBR materials with
  maps; gate ≤ 2.5 ms per frame on the dev machine), `materials/ecs_plain/100k`,
  `materials/transparent_sort/4096`. Measured values are in `docs/performance.md`.
