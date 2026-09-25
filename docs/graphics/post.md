# Post-processing: bloom, FXAA, vignette, chromatic aberration, film grain

Post effects run in a camera's post group after its scene is drawn, in exposed HDR units, and
end in the one composite pass that writes the display target (see [`frame.md`](frame.md) for
frame modes, post groups and the composite). The normative design is
[`docs/design/graphics.md`](../design/graphics.md) §1.9 and §5.2 (P-POST).

This page covers bloom (milestone PO1) and the display-space effects FXAA, vignette, chromatic
aberration and film grain (PO3). Auto exposure, color grading and the distortion apply land
with the next post milestones; their settings already exist on `PostProcess` and are ignored
until then.

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

Order in a post group (normative, spec §1.9): scene (+ chromatic aberration taps) → exposure →
bloom → vignette → tonemapper → [grading, LUT: PO4] → **last pass**: grain → dither → output
encoding. The last pass is the composite, or the FXAA pass when FXAA is on.

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

1. **Prefilter** (scene → level 0, half resolution): 13 bilinear taps (Jimenez 2014) in five
   2×2 boxes. Each box is multiplied by the camera's exposure, soft-thresholded on its brightest
   channel (a quadratic knee, continuous at `threshold ± knee`, so hue is kept) and weighted by
   its Karis factor 1 / (1 + luma), normalized: a single hot pixel cannot dominate its
   neighbourhood, so sub-pixel sparks do not flicker into blobs. Taps are clamped to the post
   group's rect: split screens and picture-in-picture views never bleed into each other.
2. **Downsample** level i → i + 1 (`mip_count − 1` passes): the same 13 taps, plain weights.
3. **Upsample** level i + 1 → i (`mip_count − 1` passes): a 3×3 tent, blended into level i as
   `src × scatter + dst × (1 − scatter)`. Level 0 ends up holding Σ (1 − s)·s^k · level_k.
4. **Composite:** level 0, sampled bilinearly, is added after exposure (bloom is already
   exposed: never exposed twice) and before vignette, grading and the tonemapper:
   `hdr + bloom × intensity`, or `mix(hdr, bloom, intensity)` when energy conserving.

Every pass is a full-screen triangle with explicit-LOD sampling only (no derivatives), and each
reads one level view while writing another, which is portable to WebGL2.

The chain is a normalized weighted average: without a threshold its mean equals the scene's, so
`intensity` is the only gain (tested: energy-conserving bloom keeps the mean within 2 %).

### Resolution independence

A level's blur spans a fixed number of its own texels, so the halo keeps its size relative to
the frame only if the level heights do not depend on the rect. The chain is therefore defined
by a **reference chain** of `mip_count` levels of heights `max_mip_height / 2^k` — what a 1080p
rect gets with the defaults (540, 270, … 17 px). A smaller rect keeps those heights: it drops the
`n` finest reference levels, where `2^n` is the power of two nearest to
`max_mip_height / (rect height / 2)`, and its level 0 (resampled from the rect to
`max_mip_height / 2^n`, within √2 of half the rect, never above the rect's own size) stands for
them; their weights merge into it (the upsample into level 0 blends with `scatter^(n + 1)`), so
every level keeps its reference weight. The halo's size and energy relative to the frame are
the same at 1080p, 720p and 360p (only the tight glow of the merged levels widens), and smaller
rects cost fewer passes:

| rect | chain (level 0, levels) | bloom-added mean luma, `Engine/Post/Bloom` (before this rule) |
|---|---|---|
| 1920 × 1080 | 960 × 540, 6 | +7.6 (+7.6) |
| 1280 × 720 | 480 × 270, 5 | +7.3 (+9.3) |
| 640 × 360 | 240 × 135, 4 | +6.6 (+11.7) |
| 320 × 180 (the goldens) | 121 × 68, 3 | +6.0 (+14.1) |

Below 1080p the added light falls slightly because small bright sources (stars) cover fewer
scene pixels before the threshold, not because of the chain. Before, level 0 was simply half
the rect with 6 levels at any size, so a 360p frame (a phone, `render_scale` 0.5) got 1.5× and
the 180p goldens about 2× the desktop veil.

**Known limitation: sub-pixel shimmer.** The prefilter's soft threshold and Karis weights are
non-linear, so the light a 2-px source of radiance 10 adds varies by about 10 % (a 4-px source
of radiance 4 by about 6 %) as it moves by quarter pixels: no fireflies, but small bright
sources (stars, bolts, sparks) shimmer slightly while the camera pans. Temporal smoothing of the
prefilter (with the TAA work) is the fix; until then keep emissive sources above ~4 px or their
radiance moderate.

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

The chain is one texture from the frame's pool (per-level views created once). Post groups of
the same size in one frame share it (split screens cost one chain), and it is freed after 3
unused frames. `RenderStats::target_bytes` counts it once per frame.

| rect | levels | `Rg11b10Ufloat` | `Rgba16Float` |
|---|---|---|---|
| 1920 × 1080 (level 0 960 × 540) | 6 | 2.8 MB | 5.5 MB |
| 3840 × 2160 (capped to 960 × 540) | 6 | 2.8 MB | 5.5 MB |
| 1280 × 720 (480 × 270, the 540 level merged) | 5 | 0.7 MB | 1.4 MB |
| 1440 × 810, `GraphicsQuality::Low` (480 × 270) | 4 | 0.7 MB | 1.4 MB |

### Cost

Render passes per post group: `2 × levels − 1` (11 for the default 6 levels at 1080p and
above, 9 at 720p, 5 at 180p) plus the composite. Their uniforms (one 64-byte slot per pass) are
written when the settings or the rect change, and the per-frame seeds of grain and dither a
window of 64 frames at a time: a still scene uploads nothing except that window (16 KiB per post
group, every 64th frame). The steady state creates no textures and reallocates nothing
(`tests/post.rs`, `tests/alloc_post.rs`); wgpu-hal on Metal allocates about 20 times per render
pass (211 allocations per frame with bloom at 320 × 180, 5 bloom passes, vs 106 for the
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

On this GPU the chain costs its 11 dependent render passes (about 14 µs each), not its pixels:
a 128² chain costs as much as a 1080p one, and the two chain formats cost the same. When
frames are isolated (the GPU idles between them and drops to low clocks) the same chain
measures about 0.45 ms; as for the composite (`frame.md` "Cost"), the gate uses the loaded
regime.

CPU (release build, same machine, busy with other builds): with bloom the render stage grows by
about 0.10 ms per post group over the composite alone, 11 render passes at roughly 10 µs of
wgpu/Metal pass overhead each (the renderer's own work per pass is a few map lookups and
allocates nothing); FXAA adds one more pass, about 0.01 ms. That is above the spec's §4.4 line
"post ≤ 0.05 ms" of the Battle CPU budget; a compute chain (one compute pass of dependent
dispatches on class A backends, the render-pass chain kept for WebGL2) would cut both the CPU
and the GPU pass overhead and is the planned follow-up.

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

Bloom has no temporal state and no noise: the same scene gives bit-identical chains (tested with
`render_to_image` twice). Capture replays re-run the chain from the scene texture. Grain and
dither are seeded from `Time::frame` (never the wall clock or the renderer's own frame count);
their seeds are hashed on the CPU (bit-identical to hashing per pixel), a window of frames at a
time — each frame's seed is still a pure function of that frame.

## Tests and stories

* `tests/post.rs` (and `tests/gl_post.rs` on GL/ANGLE): bloom — `bloom_only_above_threshold`,
  `bloom_is_in_exposed_units`, `energy_conserving_preserves_mean`,
  `bloom_does_not_bleed_across_viewports`, `hdr_scene_overlays_feed_bloom`,
  `bloom_off_in_ldr_fallback`, `transparent_rt_bloom_weighted_by_alpha`, `bloom_steady_state`,
  `bloom_downlevel_parity`; FXAA and lens effects — `fxaa_softens_edges`, `film_grain_is_seeded`,
  `grain_after_fxaa`, `after_tonemap_overlays_untouched_by_grain_ca_vignette`,
  `vignette_darkens_corners`, `chromatic_aberration_splits_toward_corners`,
  `lens_effects_do_not_bleed_across_viewports`, `fxaa_ldr_fallback_and_steady_state`,
  `transparent_rt_lens_effects_keep_alpha`; `gl_bloom_matches_default_backend`.
* `tests/alloc_post.rs`: allocations per frame with bloom on one and on two post groups, and with
  `PostProcess::cinematic()` (240 on Metal at 320 × 180: the FXAA pass on top of bloom's 211).
* Unit tests in `src/post/`: chain sizing, the soft threshold's CPU mirror, pass uniforms, WGSL
  layouts, FXAA presets and their WGSL for every quality, the pre-hashed seeds, and the GPU-time
  gate (`bloom_gate_1080p`, run alone with `--ignored`).
* Stories: `Engine/Post/Bloom` (engines, laser bolts, sparks, stars and a threshold ramp, with
  every `Bloom` field on a knob), `Engine/Post/Bloom Veil` (`Bloom::natural()`),
  `Engine/Post/FXAA` (the same view without and with FXAA side by side; quality knob) and
  `Engine/Post/Lens` (vignette, chromatic aberration and still grain on knobs, FXAA on).
