# Frame: HDR, tonemapping, render scale, cameras, render nodes, overlay stages

How a frame turns cameras into pixels once graphics features are on. The normative design is
[`docs/design/graphics.md`](../design/graphics.md) §1; the renderer internals are in
[`crates/space3d-render/ARCHITECTURE.md`](../../crates/space3d-render/ARCHITECTURE.md) ("Frame
modes, scene textures and the composite").

## Nothing changes by default

A camera without `PostProcess` renders exactly as before (Direct mode): straight into its target,
no extra texture, no extra pass, the same pixels (the engine goldens are checked bit for bit).

## Turning on HDR and tonemapping

```rust
use space3d::prelude::*; // PostProcess, Tonemapper, CameraFrame, Bloom
use space3d::render::{FrameSettings, HdrMode, RenderTargetDesc};

app.world.spawn((
    Transform::from_xyz(0.0, 2.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    Camera3d::default(),
    PostProcess::default()            // AgX, exposure 0, Bloom::glow(), dither on
        .with_exposure(0.5)           // stops: radiance × 2^0.5
        .with_tonemapper(Tonemapper::AcesFitted),
));
```

The camera's target group now renders into an HDR scene texture (`Rgba16Float`): lighting,
emissive materials and clear colors keep values above 1. One composite pass per post group then
applies the exposure and the tonemapper and writes the display target. Tonemappers:

| `Tonemapper` | look |
|---|---|
| `AgX` (default) | saturated lights go to white without hue skew |
| `AgXPunchy` | AgX with Sobotka's "punchy" look (power 1.35, saturation 1.4 in AgX's log space): saturated emissives and effects keep their hue (a red laser stays red, blue exhaust blue) at the cost of darker mid tones (0.18 → 0.12 display-linear, 0.21 with `AgX`); a game look for effect-heavy scenes |
| `AcesFitted` | Stephen Hill's filmic ACES fit (contrasty) |
| `Reinhard` | luminance Reinhard: hue-preserving, never reaches white |
| `KhronosPbrNeutral` | faithful base colors (UI and product previews), highlights rolled off from 0.76; its **toe** crushes dim saturated colors (below) |
| `KhronosPbrNeutralNoToe` | Neutral without the toe: unchanged below 0.8, the same roll-off above — display-authored art (skies, paint, previews) keeps its exact values |
| `None` | clamp to 0..1 (with exposure 0: the legacy look through the HDR path, `PostProcess::passthrough()`) |

**Choose the look explicitly.** `AgX` is the default because it is safe for any content: dim
scenes keep their brightness and nothing clips. In an effect-heavy scene it is pastel: in a
composed battle, faction exhaust (an alien red `(2.05, 0.63, 0.34)`) comes out as a near-white
peach core and fire as cream. `AgXPunchy` keeps those hues (the red exhaust core `(243, 177,
158)` instead of `(234, 197, 186)`, the shockwave orange instead of khaki) but darkens dim
content (the battle's mean luma 46 → 29 at 720p, a dim laser story 12 → 2.6); half a stop of
exposure restores most of it (37 with `+0.5`, 42 with `+0.75`). For SpaceCorps2027's battles
the recommended look is

```rust
PostProcess::default()
    .with_tonemapper(Tonemapper::AgXPunchy)
    .with_exposure(0.5)
```

(the exposure also lowers the bloom threshold by half a stop: bloom works in exposed units);
keep `AgX` for dim, atmospheric scenes and menus.

**Dim saturated colors (skies, nebulae).** The punchy look's power and saturation work in AgX's
curve space, where a dim color sits in the toe: they push its channels apart and crush the
weakest one. KhronosPbrNeutral's toe subtracts `x − 6.25·x²` from every channel of a color
whose weakest channel `x` is below 0.08 (0.04 above), nearly all of that channel. Measured
(sRGB out; `output::tests::dim_saturated_colors`, the GPU composite matches within 2 levels):

| input | `AgXPunchy` +0.5 EV | `AgX` +0.5 EV | `AgX` + `ColorGrading::vivid()` +0.5 EV | `KhronosPbrNeutral` | `KhronosPbrNeutralNoToe` |
|---|---|---|---|---|---|
| dim magenta nebula, linear (0.84, 0.10, 0.40) × 0.04 (R:B 2.1) | (54, 0, 21): R:B 4.8 linear, green crushed (4.8 at × 0.04, 6.3 at × 0.02, 8.1 at × 0.01) | (70, 10, 41) | (78, 8, 47) | — | — |
| dim teal, sRGB (21, 47, 65) | (1, 36, 69) | (25, 62, 89) | (19, 66, 95) | (1, 40, 60) | (21, 47, 65) |
| red laser, linear (2.5, 0.625, 0.5) | (253, 180, 167) | (241, 200, 193) | (241, 193, 183) | — | — |

So a scene whose backdrop is a dim saturated sky has three honest choices: `AgXPunchy` + 0.5 EV
for effects that stay saturated, knowing the sky skews toward its strongest channel (the
reference Battle does this); `AgX` with `ColorGrading::vivid()` (saturation 1.2, contrast 0.95
before the tonemapper: the sky keeps its hue and gains saturation, bright emissives gain less
than with Punchy); or `KhronosPbrNeutralNoToe` for art authored for the display, which passes
through unchanged below 0.8 (no full-frame lift needed to undo Neutral's toe; post's
`ColorGrading::lift` runs after the tonemapper on encoded color, so it cannot restore a crushed
channel: +0.018 lifts the teal to about (6, 44, 65), not 21 red).

`Tonemapper::apply` is the CPU mirror of every curve (tests, UI previews). Radiance 1.0 is
diffuse white at exposure 0; clear colors are radiance too, so they are exposed and tonemapped
like everything else (`Tonemapper::None` reproduces them).

Switches: `PostProcess::enabled` per camera, `FrameSettings::post` for all cameras (graphics
menus, A/B comparisons). With either off the camera renders as if it had no `PostProcess`.

The composite dithers by default (±1 LSB, seeded by `Time::frame`: `render_to_image` twice gives
the same image; `dither: false` turns it off). Bloom (`Bloom::glow()` by default), FXAA,
vignette, chromatic aberration, film grain, auto exposure, color grading and distortion are
configured on the same `PostProcess` (`docs/graphics/post.md`). Whatever is drawn into the HDR scene texture feeds bloom: emissive materials,
render nodes in the scene phases, `OverlayStage::HdrScene` overlays and particles (the
`Engine/Frame/Particles Bloom` story: additive sprites above the threshold glow, dim ones do not).

## Scene storage: `CameraFrame::hdr`

```rust
CameraFrame::default().with_hdr(HdrMode::Compact)
```

| `HdrMode` | scene texture |
|---|---|
| `Auto` (default) | `Rgba16Float` where float render targets exist, else the Ldr fallback |
| `Ldr` | the Ldr fallback everywhere (cheapest post path) |
| `Compact` | `Rg11b10Ufloat` (half the bandwidth, no alpha) on an opaque-cleared, single-sample primary target where the device renders it; `Auto` elsewhere |

**The Ldr fallback** (devices without float render targets — WebGL2 without
`EXT_color_buffer_float` — or `HdrMode::Ldr`): the scene texture is `Rgba8UnormSrgb`, and every
fragment and clear color is exposed and tonemapped with the camera's operator before it is
written. Bloom and auto exposure are off there. `FrameSettings::downlevel`
(`DownlevelOverride::webgl2()`) forces the WebGL2-class paths on any device for testing.

The image matches the HDR path up to 8-bit precision where every pixel is one opaque surface at
one sample: opaque single-sample scenes stay within `CompareOptions::perceptual()` (at most 1 %
of pixels off by more than 8/255). Two differences are inherent to tonemapping per fragment:

* **MSAA:** each sample is tonemapped before the resolve, where HDR resolves radiance and then
  tonemaps. Silhouettes between bright and dark surfaces resolve differently, up to about
  100/255 at high-contrast edges.
* **Blending:** translucent surfaces (and additive ones) blend tonemapped colors, where HDR
  blends radiance. Overlaps are darker or brighter, most visibly over bright or emissive
  backgrounds.

With these, a scene stays within `perceptual()` with up to 8 % of pixels allowed off. Measured:
MSAA 4× 0.1 % on Metal and 1.1 % on ANGLE, a translucent emissive quad 5.3 %, and both together
3.6 % on Metal and 4.3 % on ANGLE, with SSIM ≥ 0.99. Every pixel that is off lies on a silhouette or under a translucent surface
(`tests/frame_modes.rs` `webgl2_override_parity_msaa_and_blending`).

## Render scale

```rust
app.insert_resource(FrameSettings::default().with_render_scale(0.75));
// or per camera group: CameraFrame::default().with_render_scale(0.5)
```

The primary target's cameras render into a scene texture scaled by 0.25..=1 and the composite
upscales it bilinearly (without post: a plain copy, "passthrough"). Overlays (egui) stay at full
resolution.

## Several cameras

Cameras render in `order`; cameras sharing a target form its *target group*:

* The group's **lead camera** — its highest-order post camera, else its first camera — sets the
  group's `CameraFrame` (HDR mode, render scale). Differing values on other cameras are ignored
  (warned once).
* Every distinct viewport with a post camera is a **post group** with its own settings (the
  viewport's highest-order post camera) and its own composite: a split screen of two post
  cameras gets two exposures and tonemappers, and filtering never bleeds across the split.
* A post camera in a smaller viewport over a full-screen post camera (picture-in-picture) is
  composited after it and overwrites its rectangle.
* A later camera **without** post on a post camera's viewport (gizmos, a 3D HUD) — or on a
  viewport without any post camera (a minimap) — is a **display view**: it draws after the
  composites, LDR and without tonemapping, straight into the target, at 1 sample.

## Render targets

```rust
let rt = assets.add_render_target_with(512, 512, RenderTargetDesc::hdr().with_samples(4));
```

* `RenderTargetDesc::hdr()` stores linear, unclamped radiance (`Rgba16Float` where the device can
  render it): a monitor screen sampled by an emissive material glows through the sampling
  camera's post. Post on cameras rendering *into* an HDR target is ignored (warned once).
  `render_target_view` returns the float view (egui shows it unclamped).
* `samples: 4` multisamples the cameras rendering into the target (resolved into it).
* A render-target camera with post and a transparent clear (a ship preview) keeps alpha: the
  composite un-premultiplies before tonemapping and re-premultiplies after, so no haze appears
  where nothing was drawn.

## The sky does not depend on the camera's position

A `Skybox` looks the same wherever the camera is: 85 000 or 430 000 units from the origin, the
sky moves only when the camera *turns* (~0.01 px of error, measured below), never when it
translates.

The sky pass gets each pixel's world direction from `View::inv_proj_rot`, the inverse of the
projection times the view's *rotation* only (the view matrix with its translation dropped),
computed in `f64` on the CPU and rounded to `f32` once. With `R` the upper-left 3×3 of the view
matrix and `P` the projection as drawn (viewport fit included):

```text
inv_proj_rot   = [R⁻¹ 0; 0 1] · P⁻¹                          (f64, then to f32)
unproject(z)   = (inv_proj_rot · (ndc.x, ndc.y, z, 1)).xyz / w
direction      = unproject(1) − unproject(0)                  (world space; perspective and
                                                               orthographic alike)
cube lookup    = view.sky_rotation · direction
```

Nothing in it depends on where the camera is: `R` and `P` have the same entries anywhere. The
pass used to multiply by `inv_view_proj`, the `f32` inverse of `proj · view`, whose translation
column is the camera position; that inverse is badly conditioned far from the origin (its
entries carry ~0.01 of rounding at 85 000 units) and a pixel's direction came out up to 8 mrad
off (~3 px at 640 wide, ~10 px at 3456) with near 2 and far 300 000, differently for every
camera pose: a still camera was fine, every small move shifted the nebula and stars by several
pixels ("the textures flicker when the camera moves"). `inv_view_proj` is still filled, unchanged.

**Custom shaders, nodes and overlays.** `View` (in `VIEW_WGSL`) has a new last member,
`inv_proj_rot: mat4x4<f32>` (1312 → 1376 bytes); every older member keeps its offset, so shaders
written before still compile and read the same values. Rebuild a pixel's world *direction* or
offset from the camera with it, not with `inv_view_proj`:

```wgsl
let near = view.inv_proj_rot * vec4<f32>(ndc, 0.0, 1.0);
let far = view.inv_proj_rot * vec4<f32>(ndc, 1.0, 1.0);
let dir = far.xyz / far.w - near.xyz / near.w;                // world direction (not normalised)
let world = view.camera_pos.xyz + near.xyz / near.w;          // the pixel's point on the near plane
```

(`view.inv_proj`, which reconstructs *view-space* positions from depth, has no translation in it
and is unaffected.) On the CPU the same math is `space3d_render::inverse_proj_rot(view, proj)`
and `unproject_ray(view, proj, ndc)` (a ray's origin on the near plane and its unit direction,
`f64` inside; the editor's `ViewCamera::ray` and the model editor's picking ray use it, and the
shadow fit builds its frustum corners as `camera_pos + inv_proj_rot · ndc`).

**The camera's rotation is taken as the `Transform` holds it.** `Transform::looking_at(target)`
derives it from the `f32` difference of two absolute positions, which far from the origin is
quantized: the positions round to 0.008 at 85 000 units, so a target 50 units away moves the
direction by up to ~1.6e-4 rad in steps (worst case ~0.3 px at 3456 × 2234; measured ~0.05 px
on average for an eye drifting 0.03 units per frame) as the eye drifts, and the sky
follows that rotation faithfully. For a camera far from the origin build the rotation from
yaw/pitch (as `tests/sky_precision.rs` does) or from an offset relative to a local origin.

**What this does not fix: depth.** Geometry is still transformed in absolute `f32` world space
(`clip = view_proj · model · position` in `forward.wgsl` and `depth.wgsl`), so at ~430 000 units
the product carries ~0.03 clip units of rounding that changes with every camera pose, and the
Depth32Float compare picks different winners for surfaces a few units apart (z-fighting of thin
plates, station pops). Camera-relative rendering (subtracting the camera position from model
translations in `f64`/split floats) is planned next; until it lands, scenes that need fine depth
resolution should still sit near the origin (`migration.md` §12).

**How it is guarded.** `crates/space3d-render/tests/sky_precision.rs` renders a high-frequency
procedural star/nebula cubemap (512² faces: nebula noise, grain and single-texel stars, so there
is detail down to the pixel) behind a sphere for 64 frames, capturing each with
`render_to_image`, with the camera at `(85000, 0, 85000)` (near 2), at `(1200, −430000, 900)`
(near 10) and, as a control, at the origin, far 300 000, moving 0.03 units per frame. The sky
patch (128² pixels at 1x, 256² at 2x, in the top of the view) of each consecutive pair of frames
is registered to sub-pixel accuracy (`tests/common/subpixel.rs`: both frames Gaussian-blurred
σ 1.2 px, phase correlation for the integer shift, then Gauss-Newton on a bicubic resampling for
the fraction, ~0.005 px noise at 8-bit) and compared with the shift the camera's rotation
predicts: **translate**, constant rotation (the sky must not move: 0 px) and **yaw**, a slow turn
(0.06 px per frame; the truth is a fixed world direction projected through both rotations).
A run passes with a mean residual < 0.1 px, no frame beyond 0.3 px and no *reversal* (two
consecutive residuals of opposite x sign, both above 0.05 px: the sky jumping back and forth).
It runs at 1x (640 × 360) and 2x (1280 × 720), in Direct and HDR (passthrough post) frame modes,
under `DownlevelOverride::webgl2()` and, where wgpu's ANGLE is available (see `gl.rs`), on the
GL backend. Mean residual, px (Metal; 63 frame pairs; 2x doubles the pixel numbers):

| camera, motion | before (73cc7da), 1x | before, 2x | now, 1x | now, 2x |
|---|---|---|---|---|
| origin (control), translate | 0.000 | 0.000 | 0.000 | 0.000 |
| origin (control), yaw | 0.006 | 0.005 | 0.006 | 0.005 |
| (85000, 0, 85000) near 2, translate | 1.63 (max 5.5, 47 reversals) | 3.27 (max 10.9, 47) | 0.000 | 0.000 |
| (85000, 0, 85000) near 2, yaw | 2.04 (max 5.8, 40) | 3.34 (max 9.3, 41) | 0.006 | 0.005 |
| y = −430000 near 10, translate | 0.69 (max 2.1, 12) | 1.37 (max 4.3, 23) | 0.000 | 0.000 |
| y = −430000 near 10, yaw | 1.71 (max 5.8, 19) | 3.86 (max 10.9, 29) | 0.006 | 0.005 |

On the GL backend (ANGLE, GLES 3.0, 640 × 360, Direct and Ldr-fallback HDR alike) the same runs
gave, before: 1.62 px at 85 000 (translate; 2.04 turning, 47 and 40 reversals) and 0.70 px at
y = −430 000 (translate; 1.73 turning); now 0.000 (translate) and 0.006 (turning), like the
origin control. `webgl2()` on Metal gave the default backend's numbers to the digit.

The origin rows are the noise floor of the method (the yaw residual is registration bias, not
the renderer); the far rows equal them: the sky rendered far from the origin is bit-identical to
the one rendered at the origin for the same camera rotation and clip planes (a different near
plane changes the last bit of the `f32` direction: at most one level of 8-bit output).
`Engine/Frame/Sky Far From Origin` shows it: the same picture at the origin, at 85 000 and at
430 000 units.

## Render nodes: drawing into the frame's passes

A `RenderNode` draws into passes the frame owns — the view's scene pass, its scene-depth pass,
a post group's distortion buffer — with the view's uniform, its encoding and (optionally) its
scene depth. Particles, the skybox and distortion meshes are nodes; so can be any user draw.
Register one in the `RenderNodes` resource (`RenderPlugin` inserts it, with the built-ins
`"space3d/skybox"` and `"space3d/distortion-meshes"`):

```rust
use space3d::render::*;

struct DepthTint { /* layouts captured in `prepare`, pipelines per `PassTarget` */ }
static SLOTS: [PhaseSlot; 1] =
    [PhaseSlot::new(RenderPhase::Transparent, 100).reading(SceneReads::DEPTH)];

impl RenderNode for DepthTint {
    fn slots(&self) -> &[PhaseSlot] { &SLOTS }
    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        // once per frame: keep ctx.layouts.{view, scene} (rebuild when ctx.frame.renderer changes),
        // upload with ctx.upload, record compute into ctx.encoder
    }
    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        // build a pipeline per target: VIEW_WGSL + scene_wgsl(&target) + OUTPUT_WGSL + your code
    }
    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        pass.set_pipeline(/* the pipeline for ctx.target */);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
app.resources.expect_mut::<RenderNodes>().insert("depth-tint", DepthTint { /* … */ });
```

`crates/space3d-render/examples/frame_nodes.rs` is this node in full; the `Engine/Frame/Node
Phases` and `Engine/Frame/Soft Depth` stories show every phase and the depth paths.

Group 0 (`FrameLayouts::view`, the forward pipelines' group 0) holds binding 0, the `View`
uniform (dynamic offset `ctx.view.view_offset`), and bindings 1–6, the camera's lighting inputs:
the environment specular and irradiance cubes (`texture_cube<f32>`), the BRDF LUT
(`texture_2d<f32>`), their filtering sampler, the shadow map (`texture_depth_2d`) and its
comparison sampler, all fragment-only. A node shader declares only what it uses (`VIEW_WGSL`
declares binding 0); without an `EnvironmentLight` or a shadow they are neutral 1×1 dummies.

**When nodes are called.** Every rendered frame: `prepare` once per node (after the frame is
planned, before GPU culling and every pass; compute allowed), then per camera view
`prepare_view`, which returns the `SlotMask` of slots with work in that view — slots without
work cost nothing (no pass, no depth store, no distortion write). `prewarm` runs before the first
draw with a new `(slot, PassTarget)` (a new node, a new mode, MSAA or format) and from
`Renderer::prewarm` (loading screens: WebGL2 links programs when pipelines are created). `draw`
records into the pass: pipelines must match `ctx.target` exactly; viewport and scissor are set to
the view's rect; bind groups and pipeline are unset. Windowed captures call `draw` again with
`FrameKind::CaptureReplay` and never `prepare`/`prepare_view`: keep what `draw` needs from
`prepare`. `FrameInfo` gives the frame number (seed noise with it), the fixed tick, render time
`(tick + alpha) × fixed_delta`, `delta` (0 when `Time::frame` did not advance and in replays),
the renderer id (a new renderer is a new device: rebuild GPU state) and `device_lost`.

**Order.** Per view, phases run Opaque → Skybox → Transparent; within a phase slots draw by
`(order, registration, slot index)`. The built-in batches sit at order 0 and draw before nodes
of the same order, so negative orders draw before them:

| phase | draws |
|---|---|
| Opaque | slots of order < 0, built-in opaque batches, slots of order ≥ 0 (may write depth) |
| Skybox | slots (the built-in skybox node at 0), where depth is still at the far plane |
| Transparent | slots of order < 0, built-in blended batches, slots of order ≥ 0 not reading depth; then the depth-reading slots (below) |
| Distortion | only in a post group's settings camera, into its distortion buffer |

`RenderNodes::schedule()` lists `(node, slot, phase, order)` in this order.

**Scene depth.** A Transparent slot declared `.reading(SceneReads::DEPTH)` draws — as a whole —
after the view's scene pass, in a single-sample *SceneRead* pass on the view's color with the
view's depth in group 1 (`scene_wgsl(&ctx.target)` declares it and the helpers
`s3d_view_depth_at`, `s3d_soft_fade`, `s3d_scene_occluded`, …). How depth is available depends
on the device and the view (`PassTarget::depth_access`):

| samples | read-only depth (`GraphicsCaps::read_only_depth`) | views in the target | depth readers draw in | `depth_access` | soft fade |
|---|---|---|---|---|---|
| 1 | yes (Metal, Vulkan, DX12, WebGPU) | any | SceneRead pass, depth attached read-only and sampled | `ReadOnlyAttached` | yes |
| 1 | no (WebGL2 / GLES) | any | SceneRead pass, no depth attachment: test manually (`s3d_scene_occluded`) | `SampledOnly` | yes |
| 4 | yes | 1 | the depth is resolved (minimum of the samples), then the SceneRead pass on the resolved color | `ReadOnlyAttached` | yes |
| 4 | yes | > 1 | inside the multisampled scene pass (a later view would load multisampled content without the single-sample pass's draws) | `None` (far dummy) | no; hardware depth test |
| 4 | no | any | inside the multisampled scene pass (GLES 3.0 cannot sample multisampled depth) | `None` | no |

Write shaders that handle all three accesses: `s3d_depth_valid()` is false with `None`,
`s3d_soft_fade` then returns 1 and `s3d_scene_occluded` false; use
`target.depth_stencil(compare, false)` (it is `None` without a depth attachment). The minimum of
the samples is conservative: readers behind partially covered silhouette pixels are hidden
(slight edge aliasing instead of halos: a silhouette in front of a large, bright additive glow
stair-steps where the same edge over the sky is anti-aliased — the Look's enemy wing over its
shield glow at High; an edge-aware fade or drawing such glows inside the MSAA pass is an M3
item, graphics spec §6.2). Group 1 samples depth through a nearest sampler
(binding 2): WebGL2 reads depth textures as 0 through `textureLoad`. `DownlevelOverride {
no_read_only_depth: true, .. }` forces the WebGL2 rows on any device.

**Distortion.** Distortion slots draw only in the view of a post group's settings camera whose
`PostProcess::distortion` is enabled, and only when one has work there: into the group's
persistent `DISTORTION_FORMAT` buffer — half the group's rect in the scene texture (all of it with
`DistortionResolution::Full`), so it follows the render scale, and scene depth maps onto it at a
fixed ratio of 2 (or 1); `DrawContext::target_size` is its size — cleared to 0, blend One/One, writing
`distortion_encode(offset, weight)` from `DISTORTION_WGSL`, with the view's single-sample depth
sampled where it exists (`SampledOnly`; `None` with MSAA on GL). The composite binds the buffer;
applying it is the post area's (PO5). Registering a writer changes nothing by itself.

**Cost.** A node with no work in a view costs one `prepare_view` call. Each SceneRead pass,
depth resolve and distortion pass is one render pass (`RenderStats::{scene_read_passes,
depth_resolves, distortion_passes, node_draws}`); the renderer allocates nothing for nodes in the
steady state (`tests/alloc_frame.rs` "nodes").

## Overlay stages: HDR overlays

`RenderOverlay`s draw after the frame by default (`OverlayStage::AfterTonemap`): into the final
target, after tonemapping, exactly where and how they always did — egui and every overlay written
before stages existed. An overlay can instead draw into the scene before post:

```rust
fn stage(&self) -> OverlayStage { OverlayStage::HdrScene }
```

| stage | when | target | gets |
|---|---|---|---|
| `AfterTonemap` (default) | after every post group, display view and FXAA of the primary target | the display target (surface, headless target, capture target), display format | nothing: exactly the old placement and pixels |
| `HdrScene` | after the primary target's last scene view (and its SceneRead / Distortion passes), before its post groups; in Direct mode into the target after its last view, before every `AfterTonemap` overlay | the scene color (`Rgba16Float`, `Rg11b10Ufloat`, or `Rgba8UnormSrgb` in the Ldr fallback) at scene size, `scale_factor × render_scale` | exposure, bloom, tonemapping, grading, FXAA and grain with the scene (and distortion) |

`render()`'s contract is unchanged for both: one single-sample pass, no depth attachment,
`target.format`, viewport and scissor reset per overlay, `(priority, insertion)` order within a
stage. `OverlayTarget::kind` still says which frame it is (`Surface`, `Offscreen`, `Capture`).
HdrScene overlays also get `prepare_scene` (before `prepare`) with an `OverlayScene`: the last
primary scene view, the frame mode, the encoding to write (`output`, with `exposure` and
`tonemapper` for the Ldr fallback's `Tonemapped`), the view's bind groups and layouts, and — if
`scene_reads()` contains `DEPTH` — that view's single-sample depth. Build the overlay's shaders from
`VIEW_WGSL`, `scene.scene_wgsl()` (the group-1 declarations and helpers for this pass:
`s3d_scene_depth`, `s3d_scene_occluded`, `s3d_soft_fade`, …; `scene.depth_access()` is
`SampledOnly` with depth, `None` without — key pipelines by it) and `OUTPUT_WGSL`, with
`scene.layouts.view` / `scene.layouts.scene` as groups 0 and 1 and the view's dynamic offset
`scene.view.view_offset`; the pass has no depth attachment, so test depth with
`s3d_scene_occluded`. Frames without a primary view skip HdrScene overlays. Captures: a Direct
primary group calls them again (`Capture`); an intermediate one does not (they are already in the
scene color, which the capture composites again) — unless the frame skipped its scene views
(below), which the capture then renders, HdrScene overlays included (`Capture`).

Use `OverlayTarget::shader_encodes_srgb()` (or `OverlayScene::output`) to decide whether to
encode sRGB in the shader: the old `!target.format.is_srgb()` test is wrong for float targets.

### Migrating an additive overlay (the game's `additive.rs`)

An LDR-era additive sprite overlay: premultiplied additive blending, no depth, a pipeline per
target format, sRGB encoded in the shader for non-sRGB surfaces, and values above 1 squeezed
into the 8-bit target with `1 − e^−x`:

```rust
impl RenderOverlay for Additive {
    fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
               _: &mut wgpu::CommandEncoder, target: &OverlayTarget) {
        let encode_srgb = !target.format.is_srgb();                 // (2) wrong for float targets
        self.write_params(queue, target.size, encode_srgb, /* soft_clip */ true); // (3)
        self.ensure_pipeline(device, target.format);                 // (4) per-format cache
    }
    fn render(&self, pass: &mut wgpu::RenderPass<'static>, target: &OverlayTarget) {
        pass.set_pipeline(self.pipeline(target.format));
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..6, 0..self.count);
    }
}
```

The steps to draw it into the HDR scene (bloomed and tonemapped with the scene):

1. **Stage, per instance.** `stage()` takes `&self` and is read every frame, so keep the stage
   in the overlay and return it: `fn stage(&self) -> OverlayStage { self.stage }`. Construct the
   in-world copy with `OverlayStage::HdrScene`. **An instance drawn over the UI keeps
   `OverlayStage::AfterTonemap`**: `HdrScene` draws before post and before every `AfterTonemap`
   overlay, egui included, so moving it there would put it under the UI. The game registers
   its `ParticleOverlay` type twice — the world sprites and `UiParticles`
   (`"fx-additive-ui-particles"`, priority 1100, drawn over egui inside the wiki model viewer's
   clip rect); only the first becomes `HdrScene`. Steps 2–4 apply to the `HdrScene` instance;
   the `AfterTonemap` one keeps its 249c874 path (`prepare_scene` is only called for
   `HdrScene` overlays).
2. **Encoding from the scene.** Keep what `prepare_scene` says and apply it with `s3d_output`
   from `OUTPUT_WGSL` (`Linear` into float or sRGB scene colors, `ManualSrgb` for an 8-bit
   non-sRGB window in Direct mode, `Tonemapped` with `exposure`/`tonemapper` in the Ldr
   fallback) instead of `!target.format.is_srgb()`:
   ```rust
   fn prepare_scene(&mut self, scene: &OverlayScene<'_>) {
       self.scene = Some((scene.output, scene.exposure, scene.tonemapper, scene.mode));
   }
   ```
   ```wgsl
   return s3d_output(vec4<f32>(c, 0.0), params.encoding, params.exposure, params.tonemapper);
   ```
3. **No soft clip outside Direct mode.** Values above 1 now feed bloom and the tonemapper: apply
   `1 − e^−x` only when `scene.mode == FrameMode::Direct` (no post camera: the target is still
   8-bit display color).
4. **`Rgba16Float` pipelines.** The per-format pipeline cache now also sees the scene color's
   format (`Rgba16Float`; `Rg11b10Ufloat` with `HdrMode::Compact`; `Rgba8UnormSrgb` in the Ldr
   fallback). Blend state, depth (none) and sample count (1) stay the same.

5. **Render scale through the engine, not a render target.** At Low and Medium quality the
   game's `client/src/ui/render_scale.rs` points the game camera at an offscreen
   `RenderTarget::Texture` of 50 % or 75 % and egui paints that texture over the window. The
   window then has no camera, so the frame has no primary scene view, and HdrScene overlays —
   which draw after the primary target's last scene view — are skipped: after steps 1–4 the
   in-world copy draws nothing at those settings. Replace that path with the engine's render
   scale — `CameraFrame::default().with_render_scale(0.5)` on the game camera, or
   `FrameSettings::render_scale` — and keep the camera on `RenderTarget::Primary`: the scene
   renders at the scale into the primary scene texture, the HdrScene overlays draw into it
   (bloomed and tonemapped with the scene), the composite upscales, and egui stays at full
   resolution. (Measured with the game's overlay verbatim at 320 × 180: with the render-target
   path the migrated copy's sprite pixel is `[0, 0, 0]`; with `render_scale(0.5)` it is
   `[251, 221, 180]`, bloomed. Unmigrated, the priority −100 copy draws under egui's
   full-screen background image at those settings already — a game issue the engine render
   scale fixes as well.)

Nothing else changes: priorities (within each stage), the per-kind buffers keyed by
`OverlayTarget::kind`, the render pass contract. The `Engine/Frame/Overlay Stages` story draws the same sprites both ways
(`crates/space3d-stories/src/engine/frame/demo.rs`, `AdditiveSprites`: `migrated` switches the
four steps). Lasers become particle beams instead (particles area).

## Depth views (shadow maps)

A shadow map is a *depth view*: a view the lighting requests for a light
(`DirectionalShadow`, materials area) that the frame draws before every camera view. Each one
has its own view uniform (its `view_proj` is the light matrix), its own culling frustum and its
own draws of the opaque casters (`Material` blending off; the materials' `ShadowMode` decides
the rest), culled against the *light's* frustum — a ship outside the camera's view still casts
into the map. On GPU draw paths a depth view is one more frustum in the camera views' culling
dispatches (up to 64 views share one mark and one scatter dispatch per instance buffer): it adds
no compute dispatch. On `CpuDirect` (WebGL2) its casters are culled on the CPU like a camera's.
The pass clears the map to 1.0 and stores it; casters draw with depth-only pipelines (no
fragment stage; alpha-masked materials sample their base color and discard below the cutoff),
back faces culled unless double-sided, with the light's depth bias. Depth views never take part
in node phases, overlays, post or capture replays (a capture reuses the frame's maps).

Camera views then sample the map in their forward pass: group 0 binds each camera's lighting
resources (environment cubes, BRDF LUT, shadow map and comparison sampler); depth views bind the
neutral ones (a texture cannot be sampled while it is the pass's depth attachment).

Cost (dev machine, `benches/frame.rs` `depth_view`: one 2048² map of 400 cubes and a ground
plane over the 1080p frame scene): 0.02 ms of GPU (`GpuTimings` scope `shadow`), +37 to +46 µs of
render-thread CPU per frame (the culling of the casters, the pass and its draws), 16.8 MB of map
and about 23 MB of process memory in all with the pass's Metal driver memory ("Cost" below). The
steady state uploads nothing for a still scene and allocates nothing in the renderer
(`tests/depth_views.rs`, `tests/alloc_frame.rs` "depth_views"). `RenderStats::{depth_views,
shadow_casters, shadow_draw_calls}` count them (the camera counters `instances`,
`instances_drawn`, `draw_calls` do not include them).

## GPU timings

```rust
app.insert_resource(FrameSettings::default().with_gpu_timing(true));
// later, in any system:
let t = app.resources.expect::<GpuTimings>();
println!("bloom {:.3} ms of {:.3}", t.scope_ms("post.bloom"), t.total_ms);
```

With `FrameSettings::gpu_timing` on and `GraphicsCaps::timestamps` (Vulkan, DX12, Metal,
browser WebGPU with `timestamp-query`; not WebGL2), every pass writes a GPU timestamp at its
start and end, and `GpuTimings` (the resource and `Renderer::gpu_timings`) lists one `GpuScope`
per pass: `label`, the `camera` it belongs to, `ms`.

| label | pass |
|---|---|
| `cull` | the GPU culling compute pass (all views) |
| `shadow` | each depth view |
| `scene` | each camera view's scene pass (display views included) |
| `depth_resolve`, `scene_read`, `distortion` | the passes of §"Render nodes" |
| `overlays.hdr` | the HdrScene overlays |
| `post.bloom` | a post group's bloom chain (all its passes) |
| `post.composite`, `post.fxaa` | a post group's composite and FXAA |
| `overlays` | the AfterTonemap overlays (egui) |
| `particles.sim`, `particles.compact`, `particles.sort`, `ibl.bake`, … | the areas' own passes |

Nodes time their own passes: `PrepareContext::compute_timestamps(label)` /
`render_timestamps(label)` (and `ViewPrepareContext::compute_timestamps`, with the view's
camera) return the writes for one pass — they borrow the frame's query set, not the context,
so they go straight into `ctx.encoder.begin_compute_pass`.

Timings are 2–5 frames old (`GpuTimings::frame` names the frame): a frame's timestamps are
resolved only once its GPU work has completed, by a later frame, because Metal writes a render
pass's boundary samples asynchronously (a resolve in the same command buffer read the scene
pass's start and end from different frames). Nothing waits for the GPU; the renderer allocates
nothing for it after the first timed frame (`tests/gpu_timings.rs`, `tests/alloc_frame.rs`
"timings"). On tile-based GPUs (Apple, mobile) consecutive passes overlap, so scopes can
overlap and `total_ms` is a sum, not the frame's GPU time. Measure on a loaded GPU (frames back
to back): an idle GPU drops to low clocks and every pass costs about 3× more. Off (the
default), timing costs nothing.

## Capture and tools

* `Renderer::capture()` / `render_to_image` return the final, tonemapped sRGB image in every mode.
  Windowed captures of an intermediate primary group composite again from the last frame's scene
  texture (no scene pass is re-rendered). A window whose frame got no surface texture (minimized
  or covered — every frame of a background-mode window on macOS) draws no primary view at all; its
  captures render the skipped scene views into the scene texture first (nodes see
  `FrameKind::CaptureReplay`), then composite, so they equal the unoccluded frame.
* `Renderer::capture_scene_hdr()` returns the primary group's scene texture before post as
  `space3d_core::HdrImageData` (linear floats; `Err` in Direct mode or when the last frame had no
  primary view; after a skipped windowed frame the scene views are rendered first).
* `RenderStats`: `intermediate_groups`, `post_groups`, `post_passes`, `render_passes`,
  `fullscreen_passes`, `textures_created` (0 in the steady state), `target_bytes` (the
  frame's and post's own textures; see "Cost" for the driver memory of their passes),
  `node_draws`, `node_dispatches`, `scene_read_passes`, `depth_resolves`, `distortion_passes`,
  `depth_views`, `shadow_casters`, `shadow_draw_calls`.
* **MSAA on Apple GPUs (Metal):** the first frames after a pipeline's first draw can differ from
  later frames by a few levels on some silhouette pixels. The renderer's inputs and commands
  are the same in every frame (a static scene uploads nothing after its first frame). What
  changes is the wall-clock time since the first draw. Once the first frame finished about
  20 ms earlier, every later frame is identical. Prewarmed pipelines do not help, and 1× is
  stable from the first frame. The behaviour is the same on `main` (eb77fef).
  Measured at 4× and 160×120 with 1,100 to 1,900 instances in 3 to 5 batches: without waiting,
  25 to 69 of the 19,200 pixels differ, by up to 10 levels, for 1 to 4 frames. After a 20 ms
  wait, none do.
  For exact MSAA captures, render one warm-up frame, wait for the GPU, then wait about 20 ms
  before the captured frame. Otherwise compare with a tolerance.

## Cost

Measured on the dev machine (Apple M4 Max, 1080p, `benches/frame.rs` `frame/ab`, which interleaves
the modes and reports paired deltas; the records are in [`docs/performance.md`](../performance.md),
"Gates at M1 close" and "F7 / F8 and M1.1"):

| over Direct | render-thread CPU per frame | GPU (loaded) |
|---|---|---|
| HDR, composite only (`without_bloom()`) | +13 to +17 µs quiet, +19 to +32 µs under heavy load (gate +20 µs) | +32 to +38 µs throughput (gate +0.10 ms) |
| default bloom (`Bloom::glow()`), per post group, on top of the composite | 0.15–0.18 ms (up to 0.45 ms under very heavy load) | the chain 0.14–0.17 ms at 1080p (`bloom_gate_1080p`; post.md "Cost") |
| one 2048² depth view (400 casters) | +37 to +46 µs | 0.02 ms (`shadow`) |

The steady state allocates nothing in the renderer and creates no textures.

**Memory beyond `target_bytes`.** `RenderStats::target_bytes` counts the frame's and post's own
textures (the scene texture, depths, the bloom chain, …). Every render pass also holds GPU driver
memory while its frame is in flight — on Metal about 2.8 MB per render pass per frame in flight,
whatever its attachments' size (`tests/footprint.rs`: 11 empty passes into a 32 × 32 texture
against 1, raw wgpu). A feature's memory is therefore about `target_bytes` plus
`render_passes × 2.8 MB × frames in flight`: the composite's pass is small next to its scene
texture, but the default bloom chain's 11–12 passes are about 20× its 2.8 MB of textures. Measured
at 1080p (process footprint after 600 frames, one scene per process):

| scene | footprint, one frame in flight | three frames in flight (an unpaced loop) | `target_bytes` | render passes |
|---|---|---|---|---|
| Direct | 490 MB | — | 0 | 0 |
| HDR, composite only | 512 MB | 541 MB | 24.9 MB | 1 |
| default bloom | 572 MB (+60) | 688 MB (+148) | 27.6 MB (+2.7) | 12 |
| one 2048² depth view | 513 MB (+23) | — | 0 (the map is the lighting's: 16.8 MB) | 1 |

A window keeps two to three frames in flight (`desired_maximum_frame_latency` 2), so budget about
100–150 MB for a post group's default bloom on Apple GPUs; split screens multiply it. Fewer bloom
levels (`Bloom::mip_count`, `GraphicsQuality::Low`: 4 levels, 8 passes) cost proportionally less.
Run `cargo test -p space3d-render --release --test footprint -- --ignored --nocapture` to measure
(it fails when bloom holds more than 80 MB at one frame in flight or 200 MB at three).
