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
    PostProcess::default()            // AgX, exposure 0, dither on
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
| `AcesFitted` | Stephen Hill's filmic ACES fit (contrasty) |
| `Reinhard` | luminance Reinhard: hue-preserving, never reaches white |
| `KhronosPbrNeutral` | faithful base colors (UI and product previews) |
| `None` | clamp to 0..1 (with exposure 0: the legacy look through the HDR path, `PostProcess::passthrough()`) |

`Tonemapper::apply` is the CPU mirror of every curve (tests, UI previews). Radiance 1.0 is
diffuse white at exposure 0; clear colors are radiance too, so they are exposed and tonemapped
like everything else (`Tonemapper::None` reproduces them).

Switches: `PostProcess::enabled` per camera, `FrameSettings::post` for all cameras (graphics
menus, A/B comparisons). With either off the camera renders as if it had no `PostProcess`.

The composite dithers by default (±1 LSB, seeded by `Time::frame`: `render_to_image` twice gives
the same image; `dither: false` turns it off). Bloom, auto exposure, FXAA, vignette, chromatic
aberration, film grain, color grading and distortion are configured on the same `PostProcess`
and arrive with the post milestones.

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

Group 0 (`FrameLayouts::view`) holds only binding 0, the `View` uniform, for now. The lighting
bindings 1–6 (environment cubes, BRDF LUT, shadow map and their samplers) join it with the
lighting wiring (F7 / P-MAT PM4). Until then, a node shader that declares any of them fails
pipeline validation.

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
(slight edge aliasing instead of halos). Group 1 samples depth through a nearest sampler
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

The four steps to draw it into the HDR scene (bloomed and tonemapped with the scene):

1. **Stage.** `fn stage(&self) -> OverlayStage { OverlayStage::HdrScene }`.
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

Nothing else changes: priorities, the per-kind buffers keyed by `OverlayTarget::kind`, the
render pass contract. The `Engine/Frame/Overlay Stages` story draws the same sprites both ways
(`crates/space3d-stories/src/engine/frame/demo.rs`, `AdditiveSprites`: `migrated` switches the
four steps). Lasers become particle beams instead (particles area).

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
  `fullscreen_passes`, `textures_created` (0 in the steady state), `target_bytes`,
  `node_draws`, `node_dispatches`, `scene_read_passes`, `depth_resolves`, `distortion_passes`.
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

Measured on the dev machine (Apple M4 Max, 1080p, `benches/frame.rs`): the HDR path with the
composite only adds about 10 µs of CPU and about 0.06 ms of GPU per frame over Direct mode; the
steady state allocates nothing in the renderer and creates no textures (see
[`docs/performance.md`](../performance.md)).
