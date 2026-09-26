# Post-processing: bloom, auto exposure, color grading, FXAA, lens effects

Post effects run in a camera's post group after its scene is drawn, in exposed HDR units, and
end in the one composite pass that writes the display target (see [`frame.md`](frame.md) for
frame modes, post groups and the composite). The normative design is
[`docs/design/graphics.md`](../design/graphics.md) §1.9 and §5.2 (P-POST).

This page covers bloom (milestone PO1, with the M1.1 position-stable prefilter), auto exposure
(PO2), color grading with 3D LUTs and WGSL hooks (PO4) and the display-space effects FXAA,
vignette, chromatic aberration and film grain (PO3). The distortion apply lands with PO5; its
settings already exist on `PostProcess` and are ignored until then.

```rust
use space3d::prelude::*; // PostProcess, Bloom
use space3d::render::{FilmGrain, FxaaQuality, Vignette};

// Bloom is on by default; the display-space effects are opt-in.
PostProcess::default()
    .with_fxaa(FxaaQuality::Medium)
    .with_vignette(Vignette { enabled: true, intensity: 0.3, ..Vignette::default() })
    .with_chromatic_aberration(0.15)
    .with_film_grain(FilmGrain { enabled: true, intensity: 0.05, ..FilmGrain::default() });
// The same set: PostProcess::cinematic()
```

Order in a post group (normative, spec §1.9): [auto-exposure metering → bloom chain] →
composite: scene (+ chromatic aberration taps) → exposure (manual, or 2^(E + compensation)) →
bloom → vignette → white balance × saturation, contrast → `post_hdr_hook` → tonemapper →
lift/gamma/gain, LUT → `post_ldr_hook` → **last pass**: grain → dither → output encoding. The
last pass is the composite, or the FXAA pass when FXAA is on.

## Bloom

Bloom is on by default: `PostProcess::default()` carries `Bloom::glow()`.

```rust
use space3d::prelude::*; // PostProcess, Bloom
use space3d::render::BloomComposite;

app.world.spawn((
    Transform::from_xyz(0.0, 2.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    Camera3d::default(),
    PostProcess::default().with_bloom(
        Bloom::glow()
            .with_intensity(0.5)       // additive gain
            .with_threshold(1.0, 0.5)  // exposed radiance where bloom starts, knee fraction
            .with_scatter(0.8),        // 0 tight glow … 1 wide haze
    ),
));
// Or: PostProcess::default().without_bloom()
```

What blooms is exposed radiance above the threshold: emissive materials, bright lights on shiny
surfaces, unlit colors above 1, particles and `HdrScene` overlays. A material's `emissive` of 8
at exposure 0 glows; the same at `with_exposure(-3.0)` (exposed 1.0) does not.

| `Bloom` field | default (`glow()`) | meaning |
|---|---|---|
| `enabled` | `true` | |
| `intensity` | 0.35 | additive: bloom × intensity is added; energy conserving: mix factor 0..1 |
| `threshold` | 1.0 | exposed radiance (brightest channel) where bloom starts; 0 = everything blooms |
| `soft_knee` | 0.5 | knee width as a fraction of `threshold` on either side (0 = hard cut) |
| `scatter` | 0.7 | how much the coarse levels weigh: level k gets (1 − s)·s^k, the last one the rest |
| `composite` | `Additive` | `Additive` (adds light: glow) or `EnergyConserving` (`mix(scene, bloom, intensity)`: veil) |
| `mip_count` | 6 | levels of the reference chain, 1..=8 (see "Resolution independence"); fewer when the last level would drop below 2 px |
| `max_mip_height` | 540 | height of the reference chain's level 0: a rect of twice this height or more gets half its size scaled down to it (the cost stays that of 1080p at 1440p and 4K); smaller rects keep the reference heights |

Presets: `Bloom::glow()` (game emissives: the default) and `Bloom::natural()` (threshold 0,
energy conserving 0.15, scatter 0.85: a photographic veil that keeps the image's mean
brightness). `PostProcess::with_quality(GraphicsQuality)` sets `mip_count`/`max_mip_height`
(Low 4 / 270, Medium 6 / 540, High 7 / 720). `PostProcess::with_bloom(b)` sets `b` as given,
`enabled` included (`Bloom { enabled: false, .. }` stays off; the presets are enabled).

### How it works

Per post group (camera rect), after its scene views and `HdrScene` overlays, before the
composite:

1. **Prefilter** (scene → exactly half its size): every scene pixel is multiplied by the
   camera's exposure (with auto exposure, 2^(E + compensation)), soft-thresholded on its
   brightest channel (a quadratic knee, continuous at `threshold ± knee`, so hue is kept) and
   softly limited — unchanged up to 32, compressed toward 64 above (the **firefly guard**: a
   stray specular spike of 1000 glows like a pixel of about 63) — then a separable
   [1 2 1] / 4 tent over 3 × 3 pixels filters them. Pixels outside the post group's rect
   weigh nothing: split screens and picture-in-picture views never bleed into each other.
2. **Downsample** level i → i + 1 (`mip_count − 1` passes): 13 bilinear taps (Jimenez 2014),
   plain weights.
3. **Upsample** level i + 1 → i (`mip_count − 1` passes): a 3×3 tent, blended into level i as
   `src × scatter + dst × (1 − scatter)`. Level 0 ends up holding Σ (1 − s)·s^k · level_k.
4. **Composite:** level 0, magnified by a cubic B-spline (4 bilinear taps), is added after
   exposure (bloom is already exposed: never exposed twice) and before vignette, grading and
   the tonemapper: `hdr + bloom × intensity`, or `mix(hdr, bloom, intensity)` when energy
   conserving.

Every pass is a full-screen triangle with explicit-LOD sampling only (no derivatives), and each
reads one level view while writing another, which is portable to WebGL2.

The chain is a normalized weighted average: without a threshold its mean equals the scene's, so
`intensity` is the only gain (tested: energy-conserving bloom keeps the mean within 2 %).

### Stable under motion, proportional to energy (M1.1)

The prefilter's only non-linear steps (exposure, threshold, firefly limit) act on each pixel on
its own, and everything after them is linear: an even pixel weighs 1/2 in one level-0 texel per
axis, an odd one 1/4 in two, so every pixel's total weight is exactly 1/4 wherever it sits. The
composite then magnifies level 0 with a cubic B-spline, whose weights (unlike bilinear ones at a
non-integer ratio such as 720p's and 1440p's 2.67×) give every texel the same share of the frame.
Two consequences, both measured on level 0 read back in float and the composite's lookup
reproduced on the CPU (`post::bloom::tests`):

* **No shimmer.** A small bright source moving over the pixel grid adds the same light at every
  position: the spread over all phases is 0.02–0.1 % at 720p, 1080p and 1440p for sources of
  1–4 px (`bloom_is_stable_under_motion`, limit 0.3 %). M1 thresholded and Karis-weighted 2 × 2
  box averages of bilinear taps: a 2 px source's added light varied by 11 % and a 4 px source's
  by 4 % as it moved (the M1 review measured up to 16 % at 720p); stars, bolts and sparks pulsed
  while the camera panned.
* **Glow in proportion to energy.** The light bloom adds is the energy above the threshold ×
  `intensity`, whatever the source's size: 1.00 × for 1–32 px sources
  (`bloom_energy_is_proportional_and_limited`). M1 gave 1–2 px sources a third of it, 4 px ones
  43 % and 8 px ones 70 %, so small emissives (stars, sparks, laser heads, particles) now glow
  about 1.5–3× more than they did, and large ones the same. If a dense starfield now veils the
  image, keep its stars below the threshold (or the threshold above them) rather than lowering
  `intensity`, which also dims engines and lasers.

Karis averaging (M1's firefly guard) normalizes a pixel against its neighbours, which is exactly
what made the result depend on position; the per-pixel limit replaces it. A destination texel
of the [1 2 1] tent is centered on a scene pixel, half a pixel before the 2 × 2 block it stands
for; the composite looks bloom up half a pixel further. (A [1 3 3 1] tent over 4 × 4 pixels is as
stable but costs twice as much at 4K; a 2 × 2 box half as much but leaves blocky glows along
thin lines such as lasers.)

Cost, same GPU, loaded, A/B against the M1 prefilter in one session (`bloom_gpu_time_by_size`):
the same at 720p (0.13 ms) and 1080p (0.15 ms), 0.19 against 0.16 ms at 1440p and 0.29
against 0.20 ms at 4K — the prefilter loads 9 scene pixels per texel where M1 took 13 bilinear
taps, which matters where the scene texture is large. A compute prefilter (each pixel loaded
once through workgroup memory) is the follow-up with the compute chain (see "Cost"). The
composite's B-spline lookup takes 4 bilinear taps of level 0 instead of 1.

### Resolution independence

A level's blur spans a fixed number of its own texels, so the halo keeps its size relative to
the frame only if the level heights do not depend on the rect. The chain is therefore defined
by a **reference chain** of `mip_count` levels of heights `max_mip_height / 2^k` — what a 1080p
rect gets with the defaults (540, 270, … 17 px). A smaller rect keeps those heights: it drops the
`n` finest reference levels, where `2^n` is the power of two nearest to
`max_mip_height / (rect height / 2)`, and its level 0 (`max_mip_height / 2^n` high, within √2 of
half the rect, never above the rect's own size) stands for them; their weights merge into it
(the upsample into level 0 blends with `scatter^(n + 1)`), so every level keeps its reference
weight. The halo's size and energy relative to the frame are the same from 180p to 4K (only the
tight glow of the merged levels widens), and smaller rects cost fewer passes.

**The prefilter always halves the rect.** Its fixed 3 × 3 footprint weighs every scene pixel
alike only at exactly 2× reduction. Level 0 at the reference
height is 2.67× below the rect at 720p, 1440p and 360p and 4× at 4K: there the taps left gaps,
and a 1–2 px bright source (a star, spark, bolt head or running light) got a full halo, a double
one or none depending on its sub-pixel position, so a moving one blinked (the light bloom added
to a moving 1–2 px source went from none to 2.6× its mean at 720p, 1440p, 360p and 4K, within
1 % at 1080p). So the prefilter writes exactly half the rect (rounded up) and, when level 0 has
another size, an area resample makes level 0 from it: each level-0 pixel averages the
half-resolution pixels under its box, fractional ones by their overlap (2 × 2 bilinear taps per
pixel up to a 3-pixel box), so every pixel keeps its energy wherever it sits
(`post::bloom::tests::resample_keeps_every_pixel_energy`; on the GPU,
`bloom_halo_is_independent_of_position` in `tests/post.rs` and `tests/gl_post.rs`: a 1 px
source's halo stays within 5 % over 12 positions at every ratio, 0.5–3 % measured). It costs
one more render pass, a pooled texture (below) and GPU time where the half rect is larger than
level 0: nothing at 1080p (no resample), +4 % at 1440p, +25 % at the `GraphicsQuality::Low` rect
of T3 (1440 × 810: the prefilter writes 720 × 405 instead of 480 × 270) and +40 % at 4K (1920 ×
1080 instead of 960 × 540) — best-of chain times 0.152 → 0.158, 0.081 → 0.101 and 0.145 →
0.204 ms against the pre-change chain (M1 review round 3, interleaved sweeps; "Cost"), plus about
7 % more render-thread CPU for the extra pass at 1440p. That is the price of a halo that does not
blink.

| rect | chain (level 0, levels; prefilter target) | bloom-added mean luma, `Engine/Post/Bloom` (M1.1; M1; before the half-rect prefilter; before the reference heights) |
|---|---|---|
| 3840 × 2160 | 960 × 540, 6; 1920 × 1080 | +8.2 (+7.8; +7.6) |
| 2560 × 1440 | 960 × 540, 6; 1280 × 720 | +8.1 (+7.8; +7.7) |
| 1920 × 1080 | 960 × 540, 6; level 0 | +8.1 (+7.6; +7.6; +7.6) |
| 1600 × 900 | 960 × 540, 6; 800 × 450 | +8.2 (+7.6; +7.7) |
| 1280 × 720 | 480 × 270, 5; 640 × 360 | +8.2 (+7.4; +7.2; +9.3) |
| 640 × 360 | 240 × 135, 4; 320 × 180 | +8.1 (+7.0; +6.6; +11.7) |
| 320 × 180 (the goldens) | 121 × 68, 3; 160 × 90 | +8.5 (+6.7; +6.0; +14.1) |

With the per-pixel prefilter the added light is the same at every size within 5 %
(`engine::post::tests::bloom_added_luma_by_size` in space3d-stories, ignored: a measurement).
M1's fell below 1080p because its box averages thresholded small bright sources (stars), which
cover fewer scene pixels at lower resolutions, more harshly. Before the reference heights,
level 0 was simply half the rect with 6 levels at any size, so a 360p frame (a phone,
`render_scale` 0.5) got 1.5× and the 180p goldens about 2× the desktop veil.

### What gets bloomed

| drawn by | bloomed |
|---|---|
| scene views of the post camera (meshes, render nodes, particles) | yes |
| `OverlayStage::HdrScene` overlays | yes (they draw into the scene color before post) |
| `OverlayStage::AfterTonemap` overlays (egui, 249c874-era overlays) | no |
| display views (a later camera without post on the rect: gizmos, 3D HUD) | no |
| another post group (split screen, picture in picture) | no (taps clamp to the rect) |

A picture-in-picture post camera inside a full-screen post camera: the full group's bloom may
pick up PiP pixels at the PiP border (its scene texture holds them); the PiP's own group
overwrites its rect.

**Render targets with a transparent clear** (e.g. a ship preview shown in egui): the composite
un-premultiplies, adds bloom and re-multiplies by the scene alpha, so bloom is weighted by alpha
and no haze appears where nothing was drawn (alpha 0 stays exactly 0).

### Chain format, memory and platforms

| backend class (spec §4.1) | chain |
|---|---|
| A: Metal, Vulkan, DX12, WebGPU with `RG11B10UFLOAT_RENDERABLE` | `Rg11b10Ufloat` (4 B/px) for `Additive`; `Rgba16Float` for `EnergyConserving` |
| A without it; B: WebGL2 / GLES 3.0 with `EXT_color_buffer_float` (ANGLE) | `Rgba16Float` (8 B/px) |
| C: no float render targets (the Ldr fallback) | none: bloom is off (warned once) |

`Rg11b10Ufloat` has 6 mantissa bits (5 in blue) and converts by truncation on some GPUs (Apple):
over the chain's writes the additive glow loses about 5 % of its energy (7 % of blue), which
`intensity` absorbs. The energy-conserving mode promises the scene's mean, so its chain is
`Rgba16Float`. `HdrMode::Ldr` (or no float targets) turns bloom off: the LDR scene texture
holds tonemapped values.

The chain is one texture from the frame's pool (per-level views created once), plus the
prefilter's half-resolution target where level 0 is resampled from it. Post groups of the same
size in one frame share them (split screens cost one chain), and they are freed after 3 unused
frames. `RenderStats::target_bytes` counts them once per frame.

| rect | levels | `Rg11b10Ufloat` (chain + half-resolution target) | `Rgba16Float` |
|---|---|---|---|
| 1920 × 1080 (level 0 960 × 540) | 6 | 2.8 MB | 5.5 MB |
| 3840 × 2160 (capped to 960 × 540; half 1920 × 1080) | 6 | 2.8 + 8.3 = 11.1 MB | 22.1 MB |
| 2560 × 1440 (960 × 540; half 1280 × 720) | 6 | 2.8 + 3.7 = 6.5 MB | 12.9 MB |
| 1280 × 720 (480 × 270, the 540 level merged; half 640 × 360) | 5 | 0.7 + 0.9 = 1.6 MB | 3.2 MB |
| 1440 × 810, `GraphicsQuality::Low` (480 × 270; half 720 × 405) | 4 | 0.7 + 1.2 = 1.9 MB | 3.7 MB |

**Driver memory per render pass (Metal).** `target_bytes` counts textures only. Each of the
chain's render passes also holds GPU driver memory (IOAccelerator) while its frame is in flight —
on Metal about 2.8 MB per pass per frame in flight, whatever the attachment's size: the default
chain's 11–12 passes add about 60 MB to the process at 1080p with one frame in flight and 148 MB
with three (a window keeps two to three), per post group (split screen doubles it), against 2.7 MB
of `target_bytes` (`frame.md` "Cost"; `tests/footprint.rs` checks it). Budget it in addition to
the table above (spec §4.4's VRAM line counts textures only). Compute passes cost about 0.5 MB
each: a compute chain (one compute pass of dependent dispatches, below) would hold one pass's
worth.

### Cost

Render passes per post group: `2 × levels − 1`, one more for the resample where level 0 is not
half the rect (11 for the default 6 levels at 1080p, 12 at 1440p and 4K, 10 at 720p, 6 at 180p)
plus the composite. Their uniforms (one 64-byte slot per pass) are written when the settings or
the rect change, and the per-frame seeds of grain and dither a
window of 64 frames at a time: a still scene uploads nothing except that window (16 KiB per post
group, every 64th frame). The steady state creates no textures and reallocates nothing
(`tests/post.rs`, `tests/alloc_post.rs`); wgpu-hal on Metal allocates about 20 times per render
pass (234 allocations per frame with bloom at 320 × 180, 6 bloom passes, vs 106 for the
composite alone).

GPU time on the dev machine (Apple M4 Max, Metal; `bloom_gate_1080p` below): the GPU kept busy
(command buffers of 5 chains submitted back to back after a warm-up), each timed from the start
of its first pass to the end of its last, best of 40:

| chain | GPU (loaded) |
|---|---|
| 6 levels, 1080p (level 0 960 × 540), `Rg11b10Ufloat` | 0.14–0.17 ms |
| the same, `Rgba16Float` | 0.14–0.15 ms |
| 6 levels over a 128 × 128 scene (almost no pixels) | 0.15 ms |
| the prefilter alone (1 level), 1080p | 0.07 ms |
| `GraphicsQuality::Low` settings at 1080p (4 levels, level 0 480 × 270) | 0.08 ms |
| `GraphicsQuality::High` (7 levels; level 0 still 960 × 540 at 1080p) | 0.17 ms |

In real frames (`frame/ab`'s bloom sweep: the `post.bloom` scope of `GpuTimings`, medians of 48
frames back to back, machine load ≈ 30, so above the best-of figures): 0.196 ms at 720p (11
passes with the composite), 0.227 ms at 1080p, 0.254 ms at 1440p, 0.607 ms at 4K (the prefilter
reads the whole 4K scene) and 0.122 ms for the Low chain at 1440 × 810.

On this GPU the chain costs its 11 dependent render passes (about 14 µs each), not its pixels:
a 128² chain costs as much as a 1080p one, and the two chain formats cost the same. When
frames are isolated (the GPU idles between them and drops to low clocks) the same chain
measures about 0.45 ms; as for the composite (`frame.md` "Cost"), the gate uses the loaded
regime.

CPU (release build, same machine): bloom adds 0.15–0.17 ms of render-thread CPU per post group
over the composite alone at 1080p (the M1 record in `docs/performance.md`; an earlier estimate
here said 0.10 ms, measured on a machine busy with other builds). That is 11 render passes at
roughly 14 µs of wgpu/Metal pass overhead each (the renderer's own work per pass is a few map
lookups and allocates nothing); FXAA adds one more pass, about 0.01 ms. It is above the spec's
§4.4 line "post ≤ 0.05 ms" of the Battle CPU budget: **not met**. A compute chain (one compute
pass of dependent dispatches on class A backends, the render-pass chain kept for WebGL2) would
cut the CPU, the GPU pass overhead and the per-pass driver memory, and is the planned follow-up
(not in M2).

### Gate

Spec §4.4: "bloom 6 mips @1080p ≤ 0.20 ms" on the dev machine: **met** (0.14–0.17 ms). The test
`post::bloom::tests::bloom_gate_1080p` measures it; it is ignored by default because the other
GPU tests of the same binary run in parallel with it, so run it alone:

```sh
cargo test -p space3d-render --lib bloom_gate -- --ignored --nocapture
```

`SPACE3D_PERF_BUDGET_SCALE` relaxes the budget on slower machines; the test skips without
`TIMESTAMP_QUERY`. Each run takes the best of 40 rounds, and a run disturbed by other GPU work
is retried (best of 3 runs). The gate is only meaningful on an idle Dev GPU: at the M1 review,
with other apps holding the shared GPU at 50–60 % device utilization, runs measured 0.188–0.265 ms
and one of two invocations passed; on a quieter GPU the review measured 0.13–0.16 ms. It is not
part of `cargo test --workspace` or CI (hosted runners have software or paravirtual GPUs).

## Auto exposure

```rust
use space3d::prelude::*; // PostProcess
use space3d::render::{AutoExposure, Metering};

PostProcess::default()
    .with_auto_exposure(AutoExposure {
        min_ev: -4.0,               // darken a bright scene by 4 stops at most
        max_ev: 4.0,                // brighten a dark one by 4 at most
        metering: Metering::CenterWeighted,
        ..AutoExposure::on()
    })
    .with_exposure(-1.0);           // compensation: a darker key for space
// A cut or teleport: renderer.reset_post_history(camera) snaps on the next frame.
```

Each post group meters **its own rect** of the scene color (split screens and picture in
picture keep their own history) and writes the camera's 1 × 1 exposure texture `E` (stops); the
bloom prefilter and the composite multiply by 2^(E + `PostProcess::exposure`), so bloom's
threshold follows the metered exposure. The metered value maps the scene's log-average
luminance to middle grey (0.18) and is clamped to `min_ev..=max_ev`:

* **Black is not metered** (luminance below 2⁻¹⁶): a starfield on black is exposed for its ships,
  planets and nebulae, not brightened for the void. A frame with nothing metered holds the
  exposure.
* **Percentile window** (compute path): the darkest `low_percentile` (10 %) and the brightest
  `1 − high_percentile` (10 %) of the metered weight are cut, so a small very bright light or a
  deep shadow does not swing the exposure.
* **Metering:** `CenterWeighted` (default) weighs the center 4× the corners, linearly in the
  distance; `Average` weighs every pixel alike.
* **Adaptation:** each frame closes `1 − 2^(−dt · rate)` of the gap to the target, with
  `adapt_to_bright` (3/s: 7/8 of the gap in a second) when the scene got brighter and
  `adapt_to_dark` (1/s: half the gap in a second) when it got darker. `dt` is
  `FrameInfo::delta`: 0 when `Time::frame` did not advance (`render_to_image` twice, capture
  replays), so exposure is a pure function of the frames rendered (deterministic under
  `App::step_ticks`). `AutoExposure::instant()` (rates ∞) converges in the frame, even without
  time.
* **Snap:** a camera's first metered frame goes straight to the target (a one-shot capture is
  exposed correctly: headless rule 4 of the spec), as do the frame after auto exposure was turned
  on, after `Renderer::reset_post_history(camera)` and after a path change. Histories are freed
  60 frames after a camera stops being metered.
* **Readback:** with `FrameSettings::read_exposure`, `Renderer::camera_exposure(camera)` returns
  the exposure applied (metered + compensation, stops), 1–3 frames late and without ever waiting
  for the GPU; cameras without auto exposure report their manual exposure.
* Auto exposure needs Hdr mode: in the Ldr fallback (no float targets, `HdrMode::Ldr`) it is off
  (warned once) and the manual exposure applies. The passthrough group never meters.

### How it is metered

| path | when | passes (1080p) |
|---|---|---|
| **compute histogram** | `GraphicsCaps::compute_histogram` and an `R32Float` E (Metal, Vulkan, DX12, WebGPU; GLES 3.1+) | 1 compute pass (2 dispatches) + a 4-byte copy into E |
| **render passes** | otherwise (WebGL2, GLES 3.0, `DownlevelOverride::no_compute`) | 4 (log-luminance level, 2 reductions, adapt) |

* **Compute** (`shaders/post/exposure.wgsl`): 16 × 16 workgroups sample the rect every `stride`
  pixels per axis (1 up to 655 k samples, 2 at 1080p, 4 at 1440p and 4K). Each metered sample
  adds its weight (16 × the metering weight, an integer) to one of 255 log2-luminance bins over
  [−16, 16] (1/8 EV each) and its weight × its position inside the bin (6 bits) to a second
  counter, so a bin's samples count at their mean position, not the bin's center (no 1/8 EV
  steps as a scene brightens slowly). A second dispatch of 256 threads prefix-sums the bins,
  takes the part of each bin inside the percentile window, averages, clamps and adapts, writes
  the camera's 16-byte state and clears the histogram for the next group. Integer atomics and a
  fixed-order float reduction: bit-for-bit deterministic.
* **Render passes** (`shaders/post/exposure_dl.wgsl`, texel loads only): the rect → a level a
  quarter of its size holding Σ weight × log2 L and Σ weight of each 4 × 4 block (the same samples
  as the compute path), reductions by 8 × 8 while larger than 16 × 16, then an adapt pass that
  sums the last level and writes the camera's other E texture from the previous one (a ping-pong
  pair). No percentile window (it needs a histogram): the plain weighted log-average.

Accuracy against an exact reimplementation of the rule (every pixel, no bins,
`reference_ev` in `tests/common/post.rs`) on the test scene: the compute path within 0.01 EV
(spec: ±0.1), the render-pass path within 0.2 EV of the windowed rule (spec: ±0.5) and 0.002 EV
of the unwindowed one it computes. E is `R32Float` where renderable, else `R16Float` (about
0.004 EV of precision near ±8 EV).

Cost (Apple M4 Max, loaded, 1080p rect, `post::exposure::tests::exposure_gpu_time`): compute
0.008 ms of GPU, render passes 0.14 ms (mostly the passes' fixed overhead); spec §4.4 budgets
0.05 ms on T1. Memory: E (4 bytes, two on the render-pass path) and a 16-byte state per camera,
one shared 2 KiB histogram; the render-pass levels come from the frame's pool (1.05 MB of
`Rgba16Float` at 1080p, shared by same-size groups, counted in `RenderStats::target_bytes`). Uploads: the group's 80-byte parameters
when the settings or the rect change; per frame only the 16 bytes of adaptation factors when
`dt` changes (never with a fixed tick rate) — the steady state creates, reallocates and uploads
nothing (`auto_exposure_history_and_steady_state`, both paths).

## Color grading

```rust
use space3d::prelude::*; // PostProcess
use space3d::render::{ColorGrading, ColorLut, RenderAssets};
use glam::Vec3;

// A .cube LUT exported from Resolve, Photoshop, ... (display-referred, sRGB in and out).
let strip = ColorLut::from_cube(&std::fs::read_to_string("grade.cube")?)?;
let lut = app.resources.expect_mut::<RenderAssets>().add_texture(strip);
PostProcess::default().with_color_grading(ColorGrading {
    temperature: 0.15,                 // warmer
    saturation: 1.1,
    contrast: 1.1,                     // log2, around middle grey
    lift: Vec3::new(0.0, 0.01, 0.03),  // blue-ish blacks
    lut: Some(ColorLut { contribution: 0.8, ..ColorLut::new(lut, 33) }),
    ..ColorGrading::default()
});
```

Grading is fused into the composite (no extra pass) and costs nothing while neutral
(`ColorGrading::default()`; each part has its own effect bit). Order, and the space each step
works in:

| step | space | formula |
|---|---|---|
| white balance (`temperature`, `tint`, −1..1) | linear HDR, before the tonemapper | von Kries in LMS: the D65 white moved along the daylight locus (temperature; + is warmer) and across it (tint; + is magenta) |
| `saturation` | linear HDR | `luma + s · (c − luma)` (Rec. 709 luma; 0 grey, > 1 more saturated) |
| `contrast` | linear HDR | `0.18 · (c / 0.18)^contrast` per channel (log2, pivoting on middle grey) |
| `post_hdr_hook(c, uv)` | linear HDR | the hook shader's, if any |
| tonemapper | | |
| `gain`, `lift`, `gamma` | sRGB-encoded display color | ASC CDL: `(e · gain + lift)^(1 / gamma)` (gamma > 1 brightens the midtones) |
| LUT | sRGB-encoded display color | 3D LUT lookup, blended by `contribution` |
| `post_ldr_hook(c, uv)` | display-linear, 0..1 | the hook shader's, if any |

White balance and saturation are one 3 × 3 matrix (built on the CPU), so the HDR part costs a
matrix multiply and, with contrast, a `pow`. In the Ldr fallback everything runs on the already
tonemapped display colors (no tonemapper in the composite there).

**3D LUTs.** A `ColorLut` is an N²×N strip texture: blue slice `b` at x ∈ [b·N, (b + 1)·N), red
along x inside the slice, green along y; the composite indexes it with the encoded color and
blends two bilinear taps between the neighbouring blue slices. `ColorLut::identity(n)` gives a
neutral strip (`Rgba8Unorm`) to export, grade in an image editor and load back
(`RenderAssets::add_image`: an sRGB-format strip is re-encoded after sampling, so it behaves like
a linear one). `ColorLut::from_cube(text)` parses Adobe/Resolve `.cube` 3D LUTs (`LUT_3D_SIZE`
2..=64, `TITLE`, comments, `DOMAIN_MIN`/`DOMAIN_MAX` or `LUT_3D_INPUT_RANGE` — resampled onto
0..1 — and unknown keywords ignored; 1D LUTs are rejected with `LutError::Parse`) into an
`Rgba16Float` strip. A strip whose size is not `size² × size`, or an array texture, is ignored
(warned once). Accuracy: an identity strip changes nothing within 1/255 (`.cube`, 16-bit) or
2/255 (8-bit strips), 3/255 as an sRGB PNG (`lut_identity_is_neutral`).

**WGSL hooks.** `ColorGrading::shader` takes a `CustomShader` (registered with
`RenderAssets::add_shader`, hot-reloadable with `update_shader`) that defines `post_hdr_hook`
and/or `post_ldr_hook`, both `fn(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32>` with `uv` 0..1 over
the camera's rect; missing ones are identity. The hook may use the composite's helpers
(`s3d_luma`, `s3d_linear_to_srgb`, `s3d_srgb_to_linear`, `s3d_tonemap`, …) but not declare
resources, workgroup memory or pipeline constants without a default. The composite with the hook
is validated with naga and built inside validation and internal error scopes; one that fails —
at once, or later where the scopes answer asynchronously (browser WebGPU) — grades without its
hooks, logs the error once and reports it through `Renderer::shader_error(handle)`
(`invalid_grading_hook_falls_back`). Hook pipelines are cached per (output format, hook
version); a new version compiles on first use (prewarm by rendering a frame with it on a loading
screen).

```rust
// Night vision: green monochrome with scanlines (the Engine/Post/Color Grading story).
let hook = assets.add_shader(CustomShader::new("night vision", r#"
fn post_ldr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> {
    let l = s3d_luma(c);
    let lines = 0.85 + 0.15 * step(0.5, fract(uv.y * 90.0));
    return vec3<f32>(0.15, 1.0, 0.25) * pow(l, 0.8) * 1.4 * lines;
}"#));
PostProcess::default().with_color_grading(ColorGrading { shader: Some(hook), ..Default::default() });
```

## FXAA

`PostProcess::with_fxaa(FxaaQuality)` (or `anti_aliasing: AntiAliasing::Fxaa(q)`): FXAA 3.11
(Timothy Lottes), the PC quality path, after tonemapping. The composite then writes an 8-bit
intermediate `L` (`Rgba8UnormSrgb`, the target's size, from the frame's texture pool: post groups
of one size share it) instead of the target, and one more full-screen pass reads `L`, finds
edges by local contrast (luma = √(Rec. 601 luma) of the display-linear color, about perceptual),
blends along them and writes the target — with grain, dither and the output encoding applied
there, after the anti-aliasing, so FXAA never smears the grain.

| `FxaaQuality` | Lottes preset | search steps | edge threshold | sub-pixel |
|---|---|---|---|---|
| `Low` | 10 | 3 | 1/4 | 0.75 |
| `Medium` (default) | 12 | 5 | 1/6 | 0.75 |
| `High` | 29 | 12 | 1/8 | 0.75 |
| `Ultra` | 39 | 12 | 1/8 | 1.0 |

Every tap is clamped to the post group's rect (split screens never bleed into each other), the
pass uses explicit-LOD sampling only (WebGL2), and it runs in the Ldr fallback too. Render
targets with a transparent clear keep their alpha: FXAA filters premultiplied colors and alpha
together, and grain and dither are weighted by alpha. `PostProcess::with_quality` sets the
FXAA quality to the tier's (Low, Medium, High) when FXAA is on; the Low tier (WebGL2) uses FXAA
Low instead of MSAA (spec §4.4). Cost: one pass, and `L` (8.3 MB at 1080p).

## Vignette

`Vignette` darkens (or tints) toward the corners, in HDR before the tonemapper:
`color × mix(1, vignette.color, intensity × f)`, where `f` rises smoothly from 0 to 1 over the
last `smoothness` of the distance to the corners (1 at the corners of a centered vignette).
`roundness` 1 is circular whatever the aspect, 0 follows the rect's shape; `center` moves it;
`color` black darkens (a color tints). At the defaults (intensity 0.3) the corners are 30 %
darker.

## Chromatic aberration

`ChromaticAberration { enabled, intensity }` (or `with_chromatic_aberration(i)`): red and blue
are sampled apart, radially from the rect's center and growing with r²; at the corners the split
is `intensity × 2 %` of the rect's width (resolution-independent). The taps are bilinear and
clamped to the rect. Off in `GraphicsQuality::Low`.

## Film grain

`FilmGrain`: value noise in the encoded (sRGB) space, one value for the three channels, cells of
`size` pixels at 1080p (scaled with the target's height, at least one pixel), amplitude
`intensity` fading in the highlights with `luminance_response` (1: no grain on white). It is
seeded (spec §4.3): `animated` grain uses `pcg(seed ^ Time::frame)` — a new pattern each frame,
the same pattern for the same frame (`render_to_image` twice is identical, capture replays
repeat it) — and still grain (`animated: false`) always `seed` (use it for goldens). The seeds
of animated grain and dither are uploaded for 64 frames at once (one aligned uniform slot per
frame and post group, selected with a dynamic offset); everything else of a still scene uploads
nothing.

## Display-space effects and overlays

Grain, dither, vignette, chromatic aberration and FXAA run inside the post group: `HdrScene`
overlays get them, `AfterTonemap` overlays (egui, 249c874-era overlays) and display views
(gizmo cameras without post) do not (tested:
`after_tonemap_overlays_untouched_by_grain_ca_vignette`). The passthrough group (render scale
without post) never applies them.

## Determinism

Bloom and grading have no temporal state and no noise: the same scene gives bit-identical
images (tested with `render_to_image` twice). Capture replays re-run the chain from the scene
texture. Auto exposure adapts with `FrameInfo::delta` (0 when `Time::frame` did not advance) and
its histogram reduction is order-independent, so the same ticks give the same exposure and
capture replays never meter (`render_to_image_is_idempotent_with_ae_and_grain`: two apps
stepped through the same ticks give identical images). Grain and dither are seeded from
`Time::frame` (never the wall clock or the renderer's own frame count); their seeds are hashed on
the CPU (bit-identical to hashing per pixel), a window of frames at a time — each frame's seed is
still a pure function of that frame.

## Tests and stories

* `tests/post.rs` (and `tests/gl_post.rs` on GL/ANGLE): bloom — `bloom_only_above_threshold`,
  `bloom_is_in_exposed_units`, `energy_conserving_preserves_mean`,
  `bloom_does_not_bleed_across_viewports`, `hdr_scene_overlays_feed_bloom`,
  `bloom_off_in_ldr_fallback`, `transparent_rt_bloom_weighted_by_alpha`, `bloom_steady_state`,
  `bloom_downlevel_parity`, `bloom_halo_is_independent_of_position`; auto exposure —
  `auto_exposure_converges_and_matches_reference` (and `auto_exposure_downlevel_matches_reference`
  for the render-pass path on the default backend; on GL it is the only path),
  `instant_converges_frame_1`, `render_to_image_is_idempotent_with_ae_and_grain`,
  `auto_exposure_feeds_bloom`, `auto_exposure_per_post_group`,
  `auto_exposure_history_and_steady_state` (both paths); grading — `grading_matches_formulas`,
  `lut_identity_is_neutral`, `lut_changes_colors`, `invalid_grading_hook_falls_back`; FXAA and
  lens effects — `fxaa_softens_edges`, `film_grain_is_seeded`, `grain_after_fxaa`,
  `after_tonemap_overlays_untouched_by_grain_ca_vignette`, `vignette_darkens_corners`,
  `chromatic_aberration_splits_toward_corners`, `lens_effects_do_not_bleed_across_viewports`,
  `fxaa_ldr_fallback_and_steady_state`, `transparent_rt_lens_effects_keep_alpha`;
  `gl_bloom_matches_default_backend`.
* `tests/alloc_post.rs`: allocations per frame (Metal, 320 × 180 unless noted) with bloom on one
  post group (234) and on two (423), `PostProcess::cinematic()` (261: the FXAA pass on top of
  bloom), auto exposure with the readback on the compute path (289) and on render passes (308),
  bloom with every grading step, a LUT and a hook (234: grading adds nothing), and bloom at 1080p
  (332).
* Unit tests in `src/post/`: chain sizing, the soft threshold's and the firefly limit's CPU
  mirrors, pass uniforms, the prefilter's stability and energy on the GPU
  (`bloom_is_stable_under_motion`, `bloom_energy_is_proportional_and_limited`), WGSL layouts
  (`PostParams`, `BloomPass`, `ExposureParams`), the exposure histogram's CPU mirror, strides,
  adaptation factors and sanitizing, the grading matrices and CDL, hook composition, the `.cube`
  parser (`cube_parser_*`) and the strip lookup's CPU mirror, FXAA presets, the pre-hashed seeds,
  and GPU-time measurements run alone with `--ignored` (`bloom_gate_1080p`,
  `bloom_gpu_time_by_size`, `exposure_gpu_time`).
* Stories: `Engine/Post/Bloom` (engines, laser bolts, sparks, stars and a threshold ramp, with
  every `Bloom` field on a knob), `Engine/Post/Bloom Veil` (`Bloom::natural()`),
  `Engine/Post/Auto Exposure` (the same scene lit 16× dimmer, as authored and 16× brighter in
  three post groups, exposed alike; knobs for every `AutoExposure` field that matters and the
  compensation), `Engine/Post/FXAA` (the same view without and with FXAA side by side; quality
  knob), `Engine/Post/Lens` (vignette, chromatic aberration and still grain on knobs, FXAA on)
  and `Engine/Post/Color Grading` (ungraded and graded side by side: white balance, saturation,
  contrast, lift/gamma/gain, generated teal-and-orange and bleach-bypass `.cube` LUTs, and a
  night-vision `post_ldr_hook`).
