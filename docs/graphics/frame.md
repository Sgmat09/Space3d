# Frame: HDR, tonemapping, render scale, cameras

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
written. The image matches the HDR path up to blending and 8-bit precision; bloom and auto
exposure are off there. `FrameSettings::downlevel` (`DownlevelOverride::webgl2()`) forces the
WebGL2-class paths on any device for testing.

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

## Capture and tools

* `Renderer::capture()` / `render_to_image` return the final, tonemapped sRGB image in every mode.
  Windowed captures of an intermediate primary group composite again from the last frame's scene
  texture (no scene pass is re-rendered).
* `Renderer::capture_scene_hdr()` returns the primary group's scene texture before post as
  `space3d_core::HdrImageData` (linear floats; `Err` in Direct mode).
* `RenderStats`: `intermediate_groups`, `post_groups`, `post_passes`, `render_passes`,
  `fullscreen_passes`, `textures_created` (0 in the steady state), `target_bytes`.

## Cost

Measured on the dev machine (Apple M4 Max, 1080p, `benches/frame.rs`): the HDR path with the
composite only adds about 10 µs of CPU and about 0.06 ms of GPU per frame over Direct mode; the
steady state allocates nothing in the renderer and creates no textures (see
[`docs/performance.md`](../performance.md)).
