# Space3d graphics upgrade: final unified specification

Status: **normative** build spec. It supersedes `design-frame.md`, `design-particles.md`, `design-materials.md`
and `design-post.md` wherever they disagree, and resolves every finding in `critique.md` (Appendix A maps each
finding to the section that resolves it). Where this spec is silent, the area design still applies, as long as it
does not contradict this spec.

Inputs:
* `request.md`: the SpaceCorps2027 priorities and follow-ups.
* The four area designs and the critique.
* Code checked for this spec:
  * the `render-fixes` worktree (HEAD 31f8414 plus the in-flight viewport-clear edits: `clear.wgsl`,
    `Pipelines::viewport_clear`, the new `Camera3d::viewport` docs, ARCHITECTURE "Cameras and viewports");
  * `main` at 5c96110 (`overlay.rs`, stories, test harness, editor, facade);
  * the game at 4159bb8, which pins 249c874;
  * the wgpu/wgpu-hal/wgpu-types 30.0.1 sources.

Reading guide. §1 is the architecture and the normative frame rules. §2 is the complete public API contract and the
compatibility proof. §3 covers shaders and file layout (with owners). §4 covers downlevel, headless,
determinism and performance. §5 is the build plan. §6 lists risks and deferred items.

Conventions. MUST / MUST NOT / SHOULD are normative. "F", "P-PART", "P-POST", "P-MAT" and "I" name the builders
(§5). `S` is a target group's scene color, `D` its display target, `DB` a post group's distortion buffer, `E` a
post camera's 1×1 exposure texture and `L` the LDR intermediate before FXAA.

---

## 0. Decision register (one line each; details in the referenced sections)

| id | decision | § |
|---|---|---|
| D1 | **One composite pass** per post group. The frame owns the slot and plumbing (targets, formats, alpha, render-scale upscale, capture replay). Post owns the composite shader and parameters (`GpuPost`). Frame's own Output WGSL and `OutputInputs` are dropped. The "no post" path is the same shader with every effect off. | 1.9 |
| D2 | **One opt-in:** an enabled `PostProcess` component on a camera means HDR storage (or the LDR fallback), exposure, tonemapper and bloom. There is one `Tonemapper` enum. Dropped: `Tonemapping`, `Exposure`, `CameraFrame.post`, `PostSettings` and `FrameSettings.hdr`. Exactly two switches: `PostProcess.enabled` and `FrameSettings.post`. | 1.3, 2.2 |
| D3 | **Grouping:** a *target group* is all views of one `RenderTarget`. A *post group* is `(target, viewport rect)` with at least one post camera. Its settings come from the highest-order post camera. Views ordered after it in that rect are *display views* (LDR, drawn on top). | 1.3 |
| D4 | **One distortion system:** post's half-resolution `Rgba8Unorm` split-sign offset buffer `DB`, written in the `Distortion` phase and applied fused in the composite. It is triggered by camera state plus visible writers, never by node registration. The frame's scene-color copy is dropped (refraction deferred). Particles drop `Rg16Float`. `DistortionMaterial` is drawn by a built-in node owned by P-POST. | 1.8 |
| D5 | **One forward output tail:** `s3d_output()` driven by the override `S3D_OUTPUT` (encodings `Linear`, `ManualSrgb`, `Tonemapped`). Exposure and operator for the fallback live in the single `GpuView`. F owns the tail, `GpuView` and `PipelineKey`, all frozen in F. | 3.2–3.4 |
| D6 | **One view uniform** (`GpuView`, extended once by F). Bind groups: 0 = view, 1 = frame scene inputs, 2–3 = node-owned. Time is **not** in `GpuView`. | 3.3, 3.4 |
| D7 | **`RenderNode` API:** `&mut Resources`, per-slot `SlotMask`, `DrawContext::slot`, `SceneDepthRef` with a generation, `RenderCapabilities` + `GraphicsCaps`, renderer id / device-lost flag, `mesh()`/`texture()` accessors, prewarm hook. Particles adopt these names. | 2.2.4 |
| D8 | **Scene depth access:** 1× on RODS attaches depth read-only and samples it. On GL, depth is sampled with no attachment and tested manually. MSAA on RODS with a single scene view resolves depth (min of samples), then runs the 1× pass. Anything else (GL + MSAA, or multi-view MSAA) draws depth readers *in* the MSAA pass with the fade off. No linearize pass, no `R16F`/`R32F` copies, no unresolve blits. | 1.6 |
| D9 | **Determinism:** all noise is seeded from `Time::frame`. `delta = 0` when `Time::frame` did not advance. Capture replays never advance temporal state. `GpuView` carries no per-frame data. | 4.3 |
| D10 | **API policy:** new fields are allowed only on `Material`, `MaterialData`, `ModelMesh` and `RenderStats` *(amended at the M1 review: `ModelMesh` replaces `Primitive`, which the game builds with exhaustive literals)*. Every other existing struct is frozen. New config structs are plain `pub` field structs with `Default` (FRU-friendly). New enums and read-only info structs are `#[non_exhaustive]`. Prelude additions come from a curated, collision-checked list. | 2.7 |
| D11 | **Transparent order:** built-in blended meshes (`render_order`, then alpha/premultiplied before additive) → non-depth-reading node slots → depth-reading node slots. Particles use a single node, so their internal order is never split. | 1.7 |
| D12 | **Budgets:** one tier table (T1 dGPU, T2 Apple M1, T3 WebGL2 iGPU at scale 0.75), one "Battle" scene budget per tier, a WebGL2 preset (`GraphicsQuality::Low`), and wasm CPU and VRAM budgets. Dev-machine (M4 Max) numbers are the CI gates. | 4.4 |
| D13 | **Infrastructure consolidated:** one caps struct (`GraphicsCaps`), one texture pool (frame), one timestamp system (`GpuTimings`), one stats struct (`RenderStats` + particles' own `ParticleStats`), one HDR image type (`space3d_core::HdrImageData`). | 2.2 |
| D14 | **Overlays:** `stage()` provided method. `AfterTonemap` (default) behaves exactly as today. `HdrScene` draws into `S` after all scene views and before post, and is distorted, metered, bloomed and tonemapped. In Direct mode it draws into `D` before the AfterTonemap overlays. | 1.5 |
| D15 | **Capture:** Direct groups replay as today. Intermediate groups do not re-render scenes: `S`/`DB`/`E` persist, so the replay re-runs only post (with no adaptation), display views and AfterTonemap overlays. | 1.10 |
| D16 | **HDR render targets** render in Direct mode into `Rgba16Float` (linear, unclamped). Post on cameras targeting them is ignored (warned once). No passthrough composite variant. | 1.3 |
| D17 | **Shadows and environment plumbing** (depth-only views in culling, view bind groups keyed by environment) are built by F. The content (fit, PCF, bake, sky) is P-MAT's. Skybox is a built-in node written against the public node API. | 2.6, 5 |
| D18 | **`PostEffect` public trait deferred.** v1 extension points are `RenderNode`, `RenderOverlay` stages and the WGSL grading hooks. | 6 |
| D19 | **Point lights:** with ≤ 16 point lights, today's query order (bit-identical). With > 16, the 16 nearest to each view, kept in query order. | 1.12 |
| D20 | **Defaults stay pixel-identical:** no `PostProcess` means Direct mode, `TransparentSort::None`, `ShadingModel::Classic`, no maps and no new passes. Verified against goldens re-baselined in F0. | 1.2, 5.1 |

---

## 1. Architecture

### 1.1 Invariants (kept from today, extended)

1. **One command encoder and one submit per frame.** Every new pass (post, node compute, shadow, bake) records
   into the frame encoder.
2. **Zero allocations in our code in the steady state.** `RenderStats::cpu_reallocations == 0`,
   `RenderStats::textures_created == 0` after warm-up, and in the particles crate
   `ParticleStats::cpu_reallocations == 0`. wgpu-internal allocations stay bounded per mode (`tests/alloc*.rs`).
3. **Static scenes upload 0 bytes.** Animated post (grain, auto exposure) may add ≤ 16 B per post group. Live
   particles upload their own per-frame data.
4. **Deterministic** on one machine for identical inputs, in every mode, on every draw path and backend (§4.3).
5. **Headless-first.** Everything records against offscreen targets. Nothing touches windows or focus.
6. **GPU errors never panic in release.** Device loss degrades.
7. **Every feature has a WebGL2 path** that needs no compute and no storage buffers in vertex shaders (§4.1).
8. **249c874-era code compiles and renders unchanged** (§2.7).

### 1.2 Frame modes

Each target group runs in one **frame mode**, chosen per frame from components and settings only (never from
content: a warp effect appearing never flips a mode).

| mode | when | scene color `S` | forward output encoding | who writes `D` |
|---|---|---|---|---|
| **Direct** | no post camera in the group and render scale 1 (**the default**) | none: views render straight into `D` as today | `Linear` (sRGB/float `D`) or `ManualSrgb` (8-bit non-sRGB `D`) | scene passes |
| **Hdr** | at least one post camera, float targets available, and the lead camera's `HdrMode` is not `Ldr` | `Rgba16Float`, or `Rg11b10Ufloat` for `HdrMode::Compact` on an opaque primary target | `Linear` (unclamped radiance) | composite (+ FXAA), then display views |
| **Ldr** | (a) post cameras, but no float targets or `HdrMode::Ldr`; or (b) no post camera and render scale < 1 ("passthrough") | `Rgba8UnormSrgb` | (a) `Tonemapped` (exposure + tonemapper per fragment); (b) `Linear` | composite, then display views |

The `Ldr` fallback (a) keeps the look of one image: meshes, particles and HdrScene overlays all tonemap per
fragment with the camera's operator and exposure. Clear colors are exposed and tonemapped on the CPU with the same
curve (`Tonemapper::apply`). Bloom and auto exposure are off (warned once). Display-space effects (vignette,
grading, LUT, FXAA, grain, dither) still run in the composite.

### 1.3 Groups, views and settings (normative)

Definitions (per frame; views are active `Camera3d`s sorted by `(order, entity)` as today):

* **Post camera.** A view whose camera has `PostProcess { enabled: true, .. }`, while `FrameSettings::post` is true
  and its target is not an HDR render target (D16).
* **Target group `G`.** All views with the same `RenderTarget`. They share the storage parameters: mode, scene
  format, sample count and render scale.
* **Lead camera of `G`.** The highest-order post camera of `G`, else `G`'s first view. Its `CameraFrame` sets
  `G`'s `HdrMode` and render scale. Other views' `CameraFrame` values that differ are ignored, with a warning once
  per (camera, field).
* **Rect.** A view's viewport rounded to whole pixels (`viewport_rect`, unchanged), in `D` pixels. In `S` it is
  the same normalized viewport rounded on the scene size.
* **Post group `PG`.** For each distinct rect that has at least one post camera: the **settings camera** `P` (the
  highest-order post camera with that rect) and its **scene views** (views with that rect and `order ≤ P`,
  ties broken by entity).
* **Scene view.** A view of an intermediate `G` that belongs to some `PG`. In a passthrough group (Ldr b), every
  view is a scene view of one implicit full-rect group. Scene views render into `S`.
* **Display view.** Any other view of an intermediate `G`: later cameras on a post camera's rect (gizmos, 3D
  HUD), or rects with no post camera (a minimap with no post over a post-processed main view). Display views
  render **LDR, Direct-style, straight into `D`**, after all of `G`'s post groups.
* Mode selection for `G`:

```text
post = views of G that are post cameras
scale = CameraFrame(lead).render_scale ?? (G is Primary ? FrameSettings.render_scale : 1.0), clamped 0.25..=1
if post.is_empty() && scale == 1.0             → Direct
elif post.is_empty()                            → Ldr (passthrough, output Linear, implicit full-rect group)
elif HdrMode(lead) != Ldr && caps.hdr_format    → Hdr (Rg11b10Ufloat iff HdrMode::Compact && caps.compact
                                                   && target is Primary && lead clear alpha == 1; else Rgba16Float)
else                                            → Ldr (output Tonemapped)
```

Multi-camera cases (all tested, §5.1 F4):

| setup | groups | result |
|---|---|---|
| game camera with post | primary: Hdr, 1 PG | scene → HdrScene overlays → post → AfterTonemap overlays (egui) |
| + gizmo camera (order 10, `clear_color: None`, no post) | same PG; gizmo is a display view | gizmos drawn LDR over the tonemapped image, not bloomed |
| split screen, two post cameras | 2 PGs (left, right) | each has its own exposure history and bloom chain; sampling is clamped to its rect in `S` |
| PiP post camera inside a full post camera | 2 PGs (full, pip) | full PG runs first (lower settings order), pip PG overwrites its rect in `D`; bloom of the full PG may sample PiP pixels at the PiP border (documented) |
| PiP camera without post over a post camera | display view | drawn LDR on top |
| ship preview in an LDR RT with a transparent clear and post | RT: Hdr | composite preserves alpha (§1.9); egui shows it as today |
| `render_scale = 0.75`, no post | primary: Ldr passthrough | scene at 75 %, composite upscales; display views and overlays at full resolution |
| monitor RT (`RenderTargetDesc::hdr()`) sampled by an emissive material | RT: Direct into `Rgba16Float` | linear radiance, blooms in the sampling camera's post |

### 1.4 Frame diagram (normative order)

```
render_frame(world, resources)                                                   [F: frame/mod.rs]
 0  frame_info: Time snapshot (frame, tick, alpha, delta rule §4.3), settings snapshot, caps overrides,
    readback collect, move RenderAssets/InstanceBuffers out (as today)
 1  upload assets: meshes (+ext stream), textures, cubemaps                     [F; P-MAT extends]
 2  extract + batch ECS → sources                                               [P-MAT: frame/extract.rs]
 3  plan: cameras → ViewInfo, target groups, modes, post groups, roles, PassTargets;
    group textures (re)allocated only on key change                             [F: frame/plan.rs, targets.rs]
 4  lighting.prepare: cubemap conversion, IBL bake passes (budgeted), shadow light pick   [P-MAT: lighting/]
 5  lighting.depth_views → depth views (shadow maps) appended to the view list  [F plumbing / P-MAT content]
 6  post.begin_frame + post.plan_group per PG                                   [P-POST: post/]
 7  RenderNode::prepare for every node, once (particles sim compute, uploads, DistortionMaterial extract)
 8  per camera view: RenderNode::prepare_view → SlotMask; built-in draws + culling jobs; GpuView writes
    (incl. lighting.fill_view, exposure/tonemap for Tonemapped)                 [F: frame/views.rs]
 9  encode_compute: GPU culling of every view (camera + depth views; same dispatch count rule as today)
10  acquire surface (unchanged)
11  depth views: one depth-only pass each (Clear 1.0, Store) — shadow maps      [F: frame/passes.rs]
12  for V in camera views (camera order):
      Direct G   : scene pass V → D   (Opaque → Skybox → Transparent; viewport-clear rules unchanged)
                   SceneRead(V) if depth-reading slots have work (§1.6)
                   if V is G's last view && G is Primary: HdrScene overlays → D
      scene view : scene pass V → S (same rules applied to S) ; SceneRead(V)
                   if V is the settings camera of its PG and distortion is enabled and writers have work:
                       Distortion pass → DB(PG)   (writers of the Distortion phase; §1.8)
                   if V is G's last scene view: finish(G)
      display view: if G is finished → display pass V → D (Direct-style) else queue it for finish(G)
    finish(G): [Primary] HdrScene overlays → S
               for PG in G by settings-camera order: post.encode_group(PG): meter(E) → bloom → composite
                   → D   (or → L → FXAA → D); grain + dither in the last pass
               queued display views → D
13  AfterTonemap overlays → primary D (unchanged contract; clear-if-no-primary-view rule unchanged)
14  clear surface if nothing drew (unchanged); resolve timestamps → readback ring
15  submit, present, poll; post.end_frame; stats publish (unchanged + new counters)
```

Scene pass internals (per view):

```
color: S_msaa→resolve S | S | D (Direct: today's attachments, MSAA stored as today)
depth: group depth (Clear 1.0; Store iff a SceneRead/Distortion/HdrScene-depth consumer needs it this view)
  [viewport clear triangle when the rules require it — unchanged]
  Opaque      : built-in opaque + alpha-mask batches (order 0) + node slots by (order, seq)
  Skybox      : built-in skybox node (order 0) + node slots
  Transparent : built-in blended batches (order 0) + non-reading node slots by (order, seq)
                [+ depth-reading slots here only in the in-pass fallback, §1.6]
Depth resolve (MSAA + RODS only; when any consumer of V's depth has work: V's SceneRead, the Distortion writers of
V's PG, or HdrScene overlays with DEPTH when V is the last primary scene view): min of samples → depth_1x
SceneRead(V) (only when some depth-reading slot has work in V and the in-pass fallback is not in use):
  1× pass on the resolved color (Load), depth_1x / 1× depth read-only (RODS) or unattached (GL):
  depth-reading Transparent slots by (order, seq)
```

### 1.5 RenderOverlay placement (answers request follow-up 1)

| stage | when | target and format | depth | receives |
|---|---|---|---|---|
| `OverlayStage::AfterTonemap` (**default**; every 249c874 overlay and egui) | once per frame, after every post group, FXAA and display view of the primary target | `D` (surface view / headless offscreen / capture target), display format, 1 sample, no depth, `Load`; viewport reset per overlay | none | nothing: exactly today's placement and pixels |
| `OverlayStage::HdrScene` (opt-in) | once per frame, **after all scene views of the primary target** (incl. their SceneRead and Distortion passes) and **before its post groups** | intermediate: `S` (`Rgba16Float` / `Rg11b10Ufloat` / `Rgba8UnormSrgb` in Ldr), size = scene size, `scale_factor × render_scale`. Direct: `D`, after the primary group's last view, before all AfterTonemap overlays | none attached; the last primary scene view's 1× depth is available through `prepare_scene` when `scene_reads()` contains `DEPTH` (valid inside that view's rect) | distortion, exposure metering, **bloom**, tonemapping, grading, FXAA, grain |

Rules:
* `render()`'s contract is literally unchanged: single sample, no depth attachment, `target.format`, viewport and
  scissor reset per overlay. Ordering within a stage is today's `(priority, insertion)`.
* `OverlayTarget.kind` keeps its meaning (which frame: `Surface`, `Offscreen`, `Capture`). No variant is added.
* HdrScene overlays MUST write values in the encoding given by `OverlayScene::output`:
  * `Linear` for float `S`, and for sRGB-format `S`/`D`;
  * `ManualSrgb` for 8-bit non-sRGB `D` in Direct mode;
  * `Tonemapped` in the Ldr fallback (apply `OverlayScene::{exposure, tonemapper}` via `s3d_output` from
    `OUTPUT_WGSL`).
* The `!format.is_srgb()` heuristic is wrong for float formats: use `OverlayTarget::shader_encodes_srgb()` or
  `OverlayScene::output`.
* Frames with no primary view skip HdrScene overlays entirely (no `prepare`/`render`). AfterTonemap overlays keep
  the clear-to-black rule.
* Capture replays (§1.10): AfterTonemap overlays are replayed (`OverlayTargetKind::Capture`), as today. HdrScene
  overlays are replayed only for Direct primary groups. In intermediate mode they are already in `S`, which
  persists, so they are not called again — unless the frame skipped its scene views (no surface texture) and the
  replay renders them into `S` (§1.10): then they are called again (`Capture`).

Game migration of `client/src/game/additive.rs` (worked through in `docs/graphics/frame.md`,
"Migrating an additive overlay"):
1. Stage per instance, not per type: keep an `OverlayStage` field and return it,
   `fn stage(&self) -> OverlayStage { self.stage }`. The game registers its `ParticleOverlay`
   type twice: the in-world copy (`"fx-additive-particles"`, priority −100) becomes `HdrScene`;
   the copy drawn over egui (`"fx-additive-ui-particles"`, priority 1100, the wiki model
   viewer's `UiParticles`) stays `AfterTonemap` — `HdrScene` draws before every `AfterTonemap`
   overlay, egui included, so a type-level `HdrScene` would hide it under the UI. Steps 2–4
   apply to the `HdrScene` instance.
2. Pick the encoding from `prepare_scene` (`scene.output`) instead of `!target.format.is_srgb()`.
3. Drop the `1 − e^−x` soft clip when `scene.mode != FrameMode::Direct` (values > 1 feed bloom).
4. Add `Rgba16Float` to its per-format pipeline cache.

Lasers leave egui and become particle beams (P-PART).

### 1.6 Scene depth access (D8; normative matrix)

"RODS" = `DownlevelFlags::READ_ONLY_DEPTH_STENCIL` (absent on WebGL2/GLES: wgpu-types `limits.rs:1127`), masked by
`FrameSettings::downlevel.no_read_only_depth`.

| group samples | RODS | scene views in G | depth readers draw in | `PassTarget.depth_access` | fade |
|---|---|---|---|---|---|
| 1 | yes | any | 1× SceneRead pass; depth attached **read-only** and sampled via group 1 | `ReadOnlyAttached` | on |
| 1 | no (GL) | any | 1× SceneRead pass, **no depth attachment**; `Depth32Float` sampled as unfilterable float; shader depth-tests manually (`s3d_scene_occluded`) | `SampledOnly` | on |
| N>1 | yes | 1 | depth resolve (fragment pass, `texture_depth_multisampled_2d`, `frag_depth = min over samples`) → `depth_1x`; then 1× SceneRead pass on the resolved color with `depth_1x` read-only | `ReadOnlyAttached` | on |
| N>1 | yes | >1 | inside the MSAA scene pass, after the non-reading Transparent slots (a later view would load stale MSAA content otherwise) | `None` (far dummy bound) | off, occlusion by hardware test |
| N>1 | no (GL) | any | inside the MSAA scene pass (GLES 3.0 cannot sample multisampled depth) | `None` | off |

* **Usage rule (normative, verified in wgpu-hal `gles/device.rs:870-1010`):**
  * A 1× `Depth32Float` texture gets `TEXTURE_BINDING` usage: surface/offscreen `Attachments` at 1 sample,
    render-target depth, and group depth. On GL this turns the renderbuffer into a depth texture, which WebGL2
    supports.
  * A **multisampled** depth texture gets `TEXTURE_BINDING` **only when `GraphicsCaps::msaa_depth_resolve`**. On
    GL, a sampled multisampled texture needs `glTexStorage2DMultisample` (GLES 3.1+), which WebGL2 lacks.
  * `S_msaa` never gets `TEXTURE_BINDING`.
* **Other depth consumers** (a PG's Distortion writers, HdrScene overlays with `DEPTH`) write `DB` or the resolved
  `S` after the MSAA passes. They therefore use the resolved `depth_1x` in **every** RODS+MSAA group, including
  multi-view ones: the in-pass restriction applies only to Transparent reading slots. On GL + MSAA they get
  `DepthAccess::None` (unoccluded distortion, no overlay depth).
* The scene pass stores depth only in views where a consumer has work. Otherwise it keeps `Discard` (today).
* Min-of-samples is conservative: particles behind partially covered silhouette pixels are hidden (slight edge
  aliasing instead of halos). Documented.
* **Depth is per view.** Everything that reads a view's depth (its SceneRead pass, its PG's Distortion pass) runs
  before the next view's scene pass clears depth. HdrScene overlays see the last primary scene view's depth.
* Per-slot routing: a `Transparent` slot declared `.reading(SceneReads::DEPTH)` moves **as a whole** (all its
  draws, hard and soft) to the SceneRead pass. Particles therefore register one Transparent slot, so their internal
  back-to-front order is never split (critique P0-6.2).

### 1.7 Transparent ordering (D11)

In a view's pass sequence:
1. Built-in blended batches, sorted by `(render_order, blend class: AlphaBlend/Premultiplied before Additive,
   pipeline, material, mesh, …)`. With every `render_order == 0` this is today's order.
2. Non-depth-reading node Transparent slots, by `(order, registration seq)`.
3. Depth-reading node Transparent slots (SceneRead pass, or in-pass fallback), by `(order, seq)`.

Particles (one node) always come after meshes. Inside the node, particles order by their own
`SpriteRender::order`, then class, then depth. `Material::render_order` orders meshes only, and
`SpriteRender::order` orders particles only: two documented, independent order spaces. Interleaving meshes and
particles needs OIT (deferred, §6).

### 1.8 Distortion (D4)

* `DB` is per post group and **persistent** (keyed by the settings camera, freed 60 frames after last use, like
  `E`). Format `DISTORTION_FORMAT = Rgba8Unorm` with split signs (R = +x, G = +y, B = −x, A = −y). Size = PG rect
  **in `S`** / 2 (`DistortionResolution::Half`, default) or full. Cleared to 0 when written. *(Amended at the
  F-core review: the rect in `S`, i.e. the `D` rect × render scale, not the `D` rect. The buffer follows the
  render scale like the scene it distorts, and writers occluding against scene depth (`S`-sized) sample it at a
  fixed ratio of 2 (or 1) instead of `2 × render_scale`.)*
* **Distortion phase:** `RenderPhase::Distortion` node slots draw into `DB` right after the settings camera's
  scene pass (and its SceneRead). They use that camera's `ViewInfo`, with `PassTarget { color: Rgba8Unorm,
  samples: 1, depth: None, depth_access: SampledOnly | None }`, blend `One/One` on color and alpha, and viewport =
  `DB` size.
* Writers output `distortion_encode(offset_in_view_heights, weight)` from `DISTORTION_WGSL` and fade or occlude
  against scene depth manually.
* **Trigger:** `PostProcess::distortion.enabled` on the settings camera **and** at least one Distortion slot
  reporting work in `prepare_view` for that camera. Otherwise there is no `DB` write and the composite's
  `DISTORTION` bit is off. Registering a node with a Distortion slot changes nothing by itself.
* **Apply:** fused in the composite (`S` sampled at `uv + offset`, CA split, and the bloom lookup offset by the
  same vector).
* **Writers:**
  * built-in `DistortionMaterial` meshes, drawn by the `space3d/distortion-meshes` node (P-POST) through the
    public node API;
  * particle layers with `SpriteTarget::Distortion` (P-PART);
  * user nodes.
* Views without post never draw distortion writers (the slots are skipped).
* HdrScene overlays are part of `S`, so they **are** distorted (critique P2-6).

### 1.9 Composite (D1) and post-chain conventions

The same `composite.wgsl` runs for every intermediate post group, including the passthrough group (all effect bits
off, exposure 1, `Tonemapper::None`, no dither: a bilinear copy and upscale). Its order is normative:

```
uv (0..1 over the PG rect in D) → uv_s (PG rect in S; + distortion offset; taps clamped to the rect)
c = S(uv_s) [+ CA taps];                  a = c.a (1 for Rg11b10)
if TRANSPARENT (RT group whose lead clear alpha < 1) and a > 1/255: c.rgb /= a
e = exposure multiplier: manual 2^ev, or 2^(E + compensation) with auto exposure;  (Ldr: e = 1)
hdr = c.rgb * e
if BLOOM: hdr += B0(uv_s + off) * intensity     // bloom chain is built from EXPOSED, thresholded S
          (EnergyConserving: hdr = mix(hdr, B0, intensity))
vignette → white balance/saturation/contrast → post_hdr_hook → tonemap (Ldr: None)
→ lift/gamma/gain → LUT → post_ldr_hook
if last pass (no FXAA): grain → dither (encoded space) → manual sRGB if D is non-sRGB 8-bit
if TRANSPARENT: rgb *= a (bloom, grain and dither are thereby weighted by a)
out = vec4(rgb, a)                                   // alpha is ALWAYS preserved
```

Conventions (all normative):
* **Bloom is in exposed units:** the prefilter multiplies by exposure before the threshold, and the composite adds
  bloom after exposure (never exposed twice).
* **Grain and dither run in the last pass:** in the composite when there is no FXAA, else at the end of the FXAA
  pass.
* **Alpha is preserved.** Compact HDR (`Rg11b10Ufloat`) has no alpha, so it is used only for primary targets whose
  lead clear alpha is 1.
* **Dither** is on by default (`PostProcess::dither`). Its seed is `pcg(0x9E3779B9 ^ frame)` with
  `frame = FrameInfo::frame` (§4.3). The passthrough group never dithers.
* The **first write into `D` this frame** by a composite uses `LoadOp::Clear(black)`, drawing only its rect.
  Later composites and display views `Load`.

### 1.10 Capture, headless, replay (D15)

* **Headless:** `D` of the primary group is the offscreen `Rgba8UnormSrgb` target. `capture()` and
  `render_to_image()` are unchanged. No window, no focus change.
* **Windowed capture** (`encode_primary_to_offscreen`, offscreen sized to `last_primary_size`):
  * **Direct primary group:** re-record its views as today (node `draw` with `FrameKind::CaptureReplay`, SceneRead
    passes included), then HdrScene overlays (kind `Capture`).
  * **Intermediate primary group:** `S`, the depth-independent `DB` and `E` persist from the frame (group textures
    are only reallocated at the start of the next frame, the same rule as `ensure_offscreen`). The replay re-runs
    `post.encode_group` with `replay = true` (no metering, no adaptation; bloom recomputed from `S`; output =
    offscreen, composite pipeline for `Rgba8UnormSrgb`), then the display views, then nothing else.
  * **Skipped frame** *(amended at the F-core review)*: a window whose surface texture was unavailable
    (`Occluded`/`Timeout`: minimized or covered, and every frame of a background-mode window on macOS) draws no
    primary view, so `S` does not hold the frame. The group's textures record the frame whose scene views `S`
    holds; when it is not the last frame, the replay first re-records the scene views into `S` (node `draw` with
    `CaptureReplay`, SceneRead and Distortion passes included, then HdrScene overlays with kind `Capture`), then
    composites as above.
  * **Both:** AfterTonemap overlays (kind `Capture`).
* **`Renderer::capture_scene_hdr()`** (new; tests and tools): the primary group's `S` after HdrScene overlays and
  before post, as `HdrImageData`. `Rg11b10` decodes with alpha 1; `Rgba8UnormSrgb` is decoded. It returns
  `Err(RenderError::Capture(..))` in Direct mode, and when the last frame had no primary scene view (`S` is an
  older frame's). After a skipped windowed frame it renders the scene views into `S` first, like the replay.
  `S` has `COPY_SRC` usage.

### 1.11 Extension API overview

| extension | where it runs | used by |
|---|---|---|
| `RenderNode` (`RenderNodes` resource) | Opaque / Skybox / Transparent / Distortion phases of camera views, plus once-per-frame `prepare` (compute allowed) and per-view `prepare_view` | particles, skybox (built-in), distortion meshes (built-in), user draws |
| `RenderOverlay` (+ `stage()`) | HdrScene (pre-post) or AfterTonemap (post) | egui, game `additive.rs`, gizmos |
| `CustomShader` hooks (unchanged) + `ColorGrading::shader` (`post_hdr_hook` / `post_ldr_hook`) | forward shader / composite | user materials / grading |
| WGSL modules: `VIEW_WGSL`, `scene_wgsl()` (nodes) / `OverlayScene::scene_wgsl()` (overlays), `OUTPUT_WGSL`, `FULLSCREEN_WGSL`, `DISTORTION_WGSL` | any node/overlay shader | particles, distortion writers, game overlays |

`PostEffect` (user full-screen passes) is deferred (D18, §6).

### 1.12 Lights, IBL, shadows in the frame

* **Directional lights:** today's first 4. The shadow caster is sorted to index 0 (P-MAT).
* **Point lights:** D19. Selection happens per view in `frame/views.rs` (F). Particle light flashes insert
  ordinary `PointLight`s.
* **IBL/sky** are per camera (`EnvironmentLight`, `Skybox` components). View bind groups (group 0 bindings 1–6)
  are cached per `(environment key, shadow-map generation, views buffer generation)`.
* **Shadow maps** are **depth views**: extra views that take part in GPU/CPU culling (their own frustum slot,
  jobs and output region, and no extra dispatches on GPU paths), drawn in depth-only passes before the first
  camera view, never in color, overlay or post passes, and never replayed for captures.

---

## 2. Public API contract (additive)

All items below are **new** unless marked *(existing)*. Docs (`#![warn(missing_docs)]`) are mandatory. Signatures are
normative. Private fields are elided as `/* … */`. F writes every type in this section in F2, with complete
builders and trivial methods, and with **neutral stubs** (no panics) for behaviour owned by a P builder, tagged
`// STUB(P-XXX)` (§5.1). Policy (proven in §2.7):
* config structs are plain `pub` field structs with `Default` (FRU-friendly, **not** `#[non_exhaustive]`);
* enums, and info/context structs that users only read, are `#[non_exhaustive]`;
* bit sets are newtypes with `const`s and `BitOr`.

### 2.1 Crate root layout of `space3d-render`

`lib.rs` (F-owned) re-exports each area's API hub, so builders never edit `lib.rs`:

```rust
pub use crate::settings::*;          // FrameSettings, CameraFrame, HdrMode, DownlevelOverride, RenderTargetDesc, RenderTargetFormat
pub use crate::caps::GraphicsCaps;
pub use crate::output::*;            // OutputEncoding, Tonemapper, OUTPUT_WGSL
pub use crate::node::*;              // RenderNode API, FrameLayouts, WGSL consts
pub use crate::timing::{GpuScope, GpuTimings};
pub use crate::quality::GraphicsQuality;
pub use crate::material::*;          // BlendMode, ShadowMode, MapMask, TextureTransform, MeshExtras   (P-MAT hub)
pub use crate::lighting::api::*;     // cubemaps, Skybox, EnvironmentLight, shadows, LightingSettings  (P-MAT hub)
pub use crate::post::api::*;         // PostProcess and every post type                               (P-POST hub)
pub use overlay::{OverlayScene, OverlayStage /* + existing overlay exports */};
```

Hubs may gain items later without touching `lib.rs`. A name clash between hubs is a compile error; builders
resolve it in their own hub.

### 2.2 `space3d-render`

#### 2.2.1 Settings and camera components (`settings.rs`, F)

```rust
/// Global frame options (resource, inserted by `RenderPlugin`). Changes apply next frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSettings {
    /// Scene resolution scale of Primary-target cameras without `CameraFrame::render_scale` (clamped
    /// 0.25..=1). < 1 forces an intermediate mode (Ldr passthrough without post). Default 1.
    pub render_scale: f32,
    /// Global post-processing switch (graphics menu, A/B). `false`: every camera renders as if it had
    /// no `PostProcess`. Default true.
    pub post: bool,
    /// Record GPU pass timestamps into `GpuTimings` (needs `TIMESTAMP_QUERY`). Default false.
    pub gpu_timing: bool,
    /// Read back each post camera's exposure (`Renderer::camera_exposure`), 1–3 frames late. Default false.
    pub read_exposure: bool,
    /// Force WebGL2-class code paths on any device (tests, parity, debugging). Default: nothing forced.
    pub downlevel: DownlevelOverride,
}
impl Default for FrameSettings { /* 1.0, true, false, false, default */ }
impl FrameSettings {
    pub fn with_render_scale(self, scale: f32) -> Self;
    pub fn with_post(self, on: bool) -> Self;
    pub fn with_gpu_timing(self, on: bool) -> Self;
    pub fn with_downlevel(self, d: DownlevelOverride) -> Self;
}

/// Capabilities to pretend are missing (masks `GraphicsCaps`; never adds any).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DownlevelOverride {
    /// No float render targets: post cameras use the Ldr fallback; no bloom; LDR IBL bake.
    pub no_float_targets: bool,
    /// No compute: downlevel auto exposure; particles' `Auto` backend picks the CPU backend. (The draw
    /// path is still chosen by `RenderSettings::draw_path`.)
    pub no_compute: bool,
    /// No read-only depth-stencil: scene depth is sampled without attachment (the GL path, §1.6).
    pub no_read_only_depth: bool,
}
impl DownlevelOverride { pub fn webgl2() -> Self; /* all true */ }

/// Per-camera frame options (optional component next to `Camera3d`, which is unchanged).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraFrame {
    /// Scene storage of this camera's target group (read from the group's lead camera, §1.3).
    pub hdr: HdrMode,
    /// Scene resolution scale (0.25..=1) of this camera's target group; `None` = `FrameSettings`.
    pub render_scale: Option<f32>,
}
impl CameraFrame {
    pub fn with_hdr(self, hdr: HdrMode) -> Self;
    pub fn with_render_scale(self, scale: f32) -> Self;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HdrMode {
    /// `Rgba16Float` scene for groups with a post camera where float targets exist; Ldr fallback elsewhere.
    #[default]
    Auto,
    /// LDR scene with per-fragment tonemapping even where HDR exists (cheapest post path).
    Ldr,
    /// `Rg11b10Ufloat` (half bandwidth, no alpha) on opaque-cleared primary targets where renderable; else `Auto`.
    Compact,
}

/// Render-target options for `RenderAssets::add_render_target_with`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderTargetDesc {
    pub format: RenderTargetFormat,
    /// MSAA samples of cameras rendering into it (1 or 4; clamped to support). Default 1.
    pub samples: u32,
}
impl Default for RenderTargetDesc { /* Ldr, 1 */ }
impl RenderTargetDesc { pub fn hdr() -> Self; pub fn with_samples(self, samples: u32) -> Self; }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RenderTargetFormat {
    /// `Rgba8UnormSrgb` (= `add_render_target`): display-referred.
    #[default]
    Ldr,
    /// `GraphicsCaps::hdr_format` (else LDR): linear radiance, rendered Direct (D16); `render_target_view`
    /// returns the float view (egui shows it unclamped: documented).
    Hdr,
}
```

#### 2.2.2 Capabilities (`caps.rs`, F)

```rust
/// Graphics capabilities of the device after `FrameSettings::downlevel` masking
/// (`Renderer::graphics_caps`). One probe for every area (frame, post, materials, particles).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GraphicsCaps {
    /// `Rgba16Float` if RENDER_ATTACHMENT + TEXTURE_BINDING, FILTERABLE + BLENDABLE. WebGL2 needs
    /// `EXT_color_buffer_float` (wgpu-hal gles/adapter.rs:668-679, 1285).
    pub hdr_format: Option<wgpu::TextureFormat>,
    /// `hdr_format` also supports MULTISAMPLE_X4 + MULTISAMPLE_RESOLVE. Without it, MSAA groups in Hdr mode
    /// drop to 1 sample (logged once), never to LDR.
    pub hdr_msaa: bool,
    /// `Rg11b10Ufloat` with the same flags (`Features::RG11B10UFLOAT_RENDERABLE` requested where offered).
    pub compact_hdr_format: Option<wgpu::TextureFormat>,
    /// Bloom chain format: compact, else hdr, else `None` (no bloom).
    pub bloom_format: Option<wgpu::TextureFormat>,
    /// 1×1 exposure texture format: `R32Float` if renderable, else `R16Float`, else `None`.
    pub exposure_format: Option<wgpu::TextureFormat>,
    /// `DownlevelFlags::READ_ONLY_DEPTH_STENCIL`.
    pub read_only_depth: bool,
    /// Multisampled depth can be resolved with a fragment pass (`read_only_depth` backends).
    pub msaa_depth_resolve: bool,
    /// `RenderCapabilities::compute && indirect` (culling-class compute).
    pub compute: bool,
    /// `compute` and ≥ 256 invocations per workgroup and ≥ 1 KiB workgroup storage (post histogram).
    pub compute_histogram: bool,
    /// `Features::TIMESTAMP_QUERY` granted.
    pub timestamps: bool,
    /// `DEPTH_BIAS_CLAMP` downlevel flag (shadow bias clamp).
    pub depth_bias_clamp: bool,
    pub max_texture_2d: u32,
    pub max_texture_cube: u32,
    pub max_bind_groups: u32,
}
```

The renderer's wanted features grow by `TIMESTAMP_QUERY | RG11B10UFLOAT_RENDERABLE`. They stay optional and are
intersected with the adapter's features, as today. `RenderCapabilities` *(existing)* is unchanged.

#### 2.2.3 Output encoding and tonemapping (`output.rs`, F)

```rust
/// How a fragment shader drawing into a pass must encode its color (see `PassTarget::output`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OutputEncoding {
    /// Linear, unclamped (float targets; sRGB-format targets encode in hardware).
    #[default]
    Linear,
    /// Clamp to 0..1 and encode sRGB in the shader (8-bit non-sRGB targets without an sRGB view).
    ManualSrgb,
    /// LDR fallback of a post camera: exposure × tonemapper in the shader, then linear (sRGB target).
    Tonemapped,
}
impl OutputEncoding { pub const fn id(self) -> u32; /* 0, 1, 2: value of the S3D_OUTPUT override */ }

/// HDR → display mapping (one enum for the whole engine).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Tonemapper {
    /// Clamp to 0..1.
    None,
    /// Luminance Reinhard `c / (1 + L)`: hue-preserving, never white.
    Reinhard,
    /// Stephen Hill's ACES fit.
    AcesFitted,
    /// Sobotka AgX (minimal fit, no look): saturated lights go to white without hue skew. Default.
    #[default]
    AgX,
    /// Khronos PBR Neutral: faithful base colors (UI/product previews).
    KhronosPbrNeutral,
}
impl Tonemapper {
    pub const fn id(self) -> u32;
    /// CPU mirror of `s3d_tonemap` (linear in → display-linear 0..1 out). Used by tests, by the Ldr
    /// clear-color path and by UI previews.
    pub fn apply(self, linear: glam::Vec3) -> glam::Vec3;
}

/// WGSL without bindings: `S3D_OUTPUT_LINEAR/MANUAL_SRGB/TONEMAPPED`, `s3d_linear_to_srgb`, `s3d_srgb_to_linear`,
/// `s3d_luma`, `s3d_tonemap(op: u32, c: vec3<f32>) -> vec3<f32>`,
/// `s3d_output(c: vec4<f32>, encoding: u32, exposure: f32, op: u32) -> vec4<f32>`.
pub const OUTPUT_WGSL: &str;
```

#### 2.2.4 Render nodes (`node.rs`, F) — the extension API

```rust
/// Scene phases a node can draw in (per camera view, in this order; Distortion per post group).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RenderPhase { Opaque, Skybox, Transparent, Distortion }

/// Scene inputs a slot samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SceneReads(u8);
impl SceneReads {
    pub const NONE: Self;
    /// Scene depth of the view. On a `Transparent` slot this moves the whole slot to the SceneRead pass (§1.6).
    /// Implied for `Distortion` slots when depth exists.
    pub const DEPTH: Self;
    pub const fn union(self, other: Self) -> Self;
    pub const fn contains(self, other: Self) -> bool;
}
impl std::ops::BitOr for SceneReads { type Output = Self; }

/// One phase a node draws in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PhaseSlot { pub phase: RenderPhase, pub order: i32, pub reads: SceneReads }
impl PhaseSlot {
    /// Built-in work sits at order 0 of its phase and draws before nodes at the same order.
    pub const fn new(phase: RenderPhase, order: i32) -> Self;
    pub const fn reading(self, reads: SceneReads) -> Self;
}

/// Per-view work report: bit `i` = `slots()[i]` has draws in this view (≤ 32 slots per node).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SlotMask(pub u32);
impl SlotMask {
    pub const NONE: Self; pub const ALL: Self;
    pub const fn only(slot: usize) -> Self; pub const fn with(self, slot: usize) -> Self;
    pub const fn contains(self, slot: usize) -> bool;
}

/// Draws into frame-owned passes. Registered in `RenderNodes`. Everything `draw` needs must be captured in
/// `prepare*`: capture replays call `draw` again without a `World` (FrameKind::CaptureReplay).
pub trait RenderNode: Send + Sync + 'static {
    /// Slots (read at registration and after `RenderNodes::invalidate`, never per frame). ≤ 32.
    fn slots(&self) -> &[PhaseSlot];
    /// Once per rendered frame (never for replays), after uploads/extraction/view planning and before GPU
    /// culling and every view pass: extract, upload (`ctx.upload`), record compute (particle simulation).
    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) { let _ = ctx; }
    /// Once per camera view per frame, after every node's `prepare`, in camera order. Returns the slots with
    /// work in this view: slots without work cost nothing (no SceneRead pass, no depth store, no DB write).
    fn prepare_view(&mut self, ctx: &mut ViewPrepareContext<'_>) -> SlotMask { let _ = ctx; SlotMask::ALL }
    /// Pipelines for the pass signatures this node will be drawn with; called before the first draw with a
    /// new `(slot, PassTarget)` (registration, mode change, MSAA change). Build pipelines here (WebGL2 link).
    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) { let _ = (device, targets); }
    /// Record draws for `ctx.slot` of `ctx.view`. Pipelines MUST match `ctx.target`. Viewport/scissor are set
    /// to the view's rect (Distortion: the DB rect); bind groups and pipeline are unset on entry.
    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>);
}
pub type SharedNode = std::sync::Arc<std::sync::Mutex<dyn RenderNode>>;
/// Lock recovering from poisoning (a panicking node never takes the renderer down).
pub fn lock_node(node: &SharedNode) -> std::sync::MutexGuard<'_, dyn RenderNode>;

/// Registered nodes (resource, inserted by `RenderPlugin` with the built-ins "space3d/skybox" and
/// "space3d/distortion-meshes"). Snapshot re-cloned only when the generation changes (like `OverlayCache`).
#[derive(Default)]
pub struct RenderNodes { /* … */ }
impl RenderNodes {
    pub fn insert<T: RenderNode>(&mut self, name: impl Into<String>, node: T) -> std::sync::Arc<std::sync::Mutex<T>>;
    pub fn insert_shared(&mut self, name: impl Into<String>, node: SharedNode) -> Option<SharedNode>;
    pub fn remove(&mut self, name: &str) -> Option<SharedNode>;
    pub fn get(&self, name: &str) -> Option<&SharedNode>;
    pub fn contains(&self, name: &str) -> bool;
    /// Re-read every node's `slots()` next frame.
    pub fn invalidate(&mut self);
    /// `(node name, slot index, phase, order)` in execution order.
    pub fn schedule(&self) -> impl Iterator<Item = (&str, usize, RenderPhase, i32)>;
    pub fn names(&self) -> impl Iterator<Item = &str>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

/// Deterministic per-frame facts.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct FrameInfo {
    /// `Time::frame` (App frames), or the renderer's frame count when no `Time` exists. Seeds all noise.
    pub frame: u64,
    pub tick: u64,
    pub alpha: f32,
    /// Render time in seconds: `(tick + alpha) × fixed_delta` (never wall clock).
    pub time: f64,
    /// `Time::delta` if `Time::frame` advanced since this renderer's previous frame, else 0 (and 0 in replays).
    pub delta: f32,
    pub kind: FrameKind,
    /// Unique id of this `Renderer` instance (a new renderer/device = new id: rebuild GPU state).
    pub renderer: u64,
    /// The device was lost (resources created now are invalid; skip work).
    pub device_lost: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameKind { Frame, CaptureReplay }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameMode { Direct, Hdr, Ldr }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ViewRole { Scene, Display }

/// One camera view of this frame.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct ViewInfo {
    pub camera: space3d_core::Entity,
    pub order: i32,
    pub target: RenderTarget,
    pub primary: bool,
    pub mode: FrameMode,
    pub role: ViewRole,
    pub view: glam::Mat4, pub proj: glam::Mat4, pub view_proj: glam::Mat4,
    pub camera_pos: glam::Vec3,
    pub near: f32, pub far: f32, pub orthographic: bool,
    pub frustum: Frustum,
    /// Rect in pixels of the texture the view renders into (`S` for scene views, `D` otherwise).
    pub viewport: [u32; 4],
    pub scene_size: (u32, u32),
    pub target_size: (u32, u32),
    pub render_scale: f32,
    pub samples: u32,
    /// Color format of the view's scene pass.
    pub color_format: wgpu::TextureFormat,
    pub output: OutputEncoding,
    /// For `OutputEncoding::Tonemapped` (1.0 / `None` otherwise).
    pub exposure: f32,
    pub tonemapper: Tonemapper,
    /// Dynamic offset of this view's `View` uniform (bind group 0).
    pub view_offset: u32,
}

/// How scene depth is available to a pass (§1.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DepthAccess {
    /// Far dummy bound in group 1 (fade must treat it as "no occluder").
    None,
    /// Real depth in group 1 and attached read-only: hardware depth test works.
    ReadOnlyAttached,
    /// Real depth in group 1, no depth attachment: test manually (`s3d_scene_occluded`).
    SampledOnly,
}

/// Exact attachment signature of the pass a draw is recorded into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PassTarget {
    pub color: wgpu::TextureFormat,
    pub samples: u32,
    /// `Some(Depth32Float)` in scene and RODS SceneRead passes; `None` in GL SceneRead and Distortion.
    pub depth: Option<wgpu::TextureFormat>,
    pub depth_read_only: bool,
    pub depth_access: DepthAccess,
    pub output: OutputEncoding,
}
impl PassTarget {
    pub fn color_target(&self, blend: Option<wgpu::BlendState>) -> wgpu::ColorTargetState;
    /// `None` when `depth` is `None`. `depth_write` is forced off when `depth_read_only`. Nodes outside the
    /// Opaque phase MUST pass `false` (the scene pass depth is writable, but only opaque work may write it).
    pub fn depth_stencil(&self, compare: wgpu::CompareFunction, depth_write: bool) -> Option<wgpu::DepthStencilState>;
    pub fn multisample(&self) -> wgpu::MultisampleState;
}

/// A mesh of `RenderAssets` for node draws (cheap clones; valid until the mesh is removed).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MeshRef {
    pub vertex_buffer: wgpu::Buffer,   // the mesh arena (layout: MESH_VERTEX_LAYOUT)
    pub index_buffer: wgpu::Buffer,    // u32 indices
    pub first_index: u32, pub index_count: u32,
    pub base_vertex: i32,
    /// Without `RenderCapabilities::base_vertex` (WebGL2) bind `vertex_buffer` at `base_vertex × stride` and
    /// draw with base 0 (see `direct_vertex_base`).
    pub bounds: Sphere,
}
/// Vertex layout of the mesh arena (position, normal, uv, color; locations 0..=3).
pub const MESH_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static>;

/// A texture of `RenderAssets` (flipbooks, sprites, LUTs).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TextureRef {
    pub view: wgpu::TextureView, pub sampler: wgpu::Sampler,
    pub format: wgpu::TextureFormat, pub size: (u32, u32), pub layers: u32,
    /// Bound as `texture_2d_array` iff true (more than one layer; ARCHITECTURE "Textures").
    pub layered: bool,
    /// Bumped whenever the GPU texture is replaced: rebuild bind groups on change.
    pub version: u32,
}

/// Raw scene depth for custom bind groups.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct SceneDepthRef<'a> {
    pub view: &'a wgpu::TextureView,  // Depth32Float, bind as unfilterable float texture_2d
    pub size: (u32, u32),
    /// Changes whenever the texture is recreated (cache bind groups by it).
    pub generation: u64,
}

#[non_exhaustive]
pub struct PrepareContext<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: &'a mut wgpu::CommandEncoder,
    pub world: &'a space3d_core::World,
    /// `RenderAssets` and `InstanceBuffers` are moved out during the frame: use `assets`.
    pub resources: &'a mut space3d_core::Resources,
    pub assets: &'a RenderAssets,
    pub frame: FrameInfo,
    /// Camera views of this frame (depth views excluded).
    pub views: &'a [ViewInfo],
    pub caps: &'a RenderCapabilities,
    pub graphics: &'a GraphicsCaps,
    pub layouts: &'a FrameLayouts,
    /* private: uploader, timing, stats */
}
impl PrepareContext<'_> {
    /// Copy through the renderer's staging ring (recorded in the frame encoder; no per-call allocation;
    /// counted in `RenderStats::bytes_uploaded`). `buffer` needs `COPY_DST`.
    pub fn upload(&mut self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]);
    pub fn mesh(&self, handle: MeshHandle) -> Option<MeshRef>;
    pub fn texture(&self, handle: TextureHandle) -> Option<TextureRef>;
    /// Timestamp writes for a compute / render pass the caller records (`None` without timing).
    pub fn compute_timestamps(&mut self, label: &'static str) -> Option<wgpu::ComputePassTimestampWrites<'_>>;
    pub fn render_timestamps(&mut self, label: &'static str) -> Option<wgpu::RenderPassTimestampWrites<'_>>;
    /// Add to `RenderStats::node_*` counters.
    pub fn count_dispatches(&mut self, n: u32);
}

#[non_exhaustive]
pub struct ViewPrepareContext<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: &'a mut wgpu::CommandEncoder,
    pub world: &'a space3d_core::World,
    pub view: &'a ViewInfo,
    pub frame: FrameInfo,
    /// Pass target each slot will be drawn with in this view (index = slot). Slots that cannot draw in this
    /// view (a Distortion slot on a camera that is not a post group's settings camera, or without post) get
    /// `None`-access targets and their mask bits are ignored.
    pub targets: &'a [PassTarget],
    pub graphics: &'a GraphicsCaps,
    /* private */
}
impl ViewPrepareContext<'_> {
    pub fn upload(&mut self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]);
    pub fn compute_timestamps(&mut self, label: &'static str) -> Option<wgpu::ComputePassTimestampWrites<'_>>;
}

#[non_exhaustive]
pub struct DrawContext<'a> {
    pub device: &'a wgpu::Device,
    /// Index into `slots()` being drawn, and its phase.
    pub slot: usize,
    pub phase: RenderPhase,
    pub view: &'a ViewInfo,
    pub target: PassTarget,
    /// Size of the attachment (Distortion: the DB size).
    pub target_size: (u32, u32),
    /// Group 0 (`FrameLayouts::view`): `pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset])`.
    pub view_bind_group: &'a wgpu::BindGroup,
    /// Group 1 (`FrameLayouts::scene`): scene depth per `target.depth_access` (far dummy otherwise) and the
    /// view's post exposure texture E (0 EV dummy without auto exposure; last frame's value during scene passes).
    pub scene_bind_group: &'a wgpu::BindGroup,
    pub scene_depth: Option<SceneDepthRef<'a>>,
    pub frame: FrameInfo,
}

/// Shared layouts (renderer lifetime, never recreated).
#[non_exhaustive]
pub struct FrameLayouts {
    /// Group 0: binding 0 `View` uniform (dynamic offset, VERTEX|FRAGMENT); bindings 1–6 lighting (env specular
    /// cube, irradiance cube, BRDF LUT, env sampler, shadow map, comparison sampler; FRAGMENT). Identical to
    /// the forward pipelines' group 0 (a shader may declare only the bindings it uses). Compute passes of
    /// nodes use their own layouts (upload the few view values they need).
    pub view: wgpu::BindGroupLayout,
    /// Group 1: binding 0 scene depth `texture_2d<f32>` (unfilterable), binding 1 exposure `texture_2d<f32>`
    /// (unfilterable, 1×1, EV in .r). FRAGMENT.
    pub scene: wgpu::BindGroupLayout,
    /// Full-screen triangle (`vs_fullscreen`, `FULLSCREEN_WGSL`).
    pub fullscreen: wgpu::ShaderModule,
}
/// `struct DirLight`, `struct PointLight`, `struct View` (exact forward layout, §3.3) +
/// `@group(0) @binding(0) var<uniform> view: View;`. Node shaders must not redefine those names.
pub const VIEW_WGSL: &str;
/// Group 1 declarations and helpers matching `target.depth_access` (requires `VIEW_WGSL` first):
/// `s3d_scene_uv(frag_xy)`, `s3d_scene_depth(px) -> f32` (device depth; 1.0 without depth),
/// `s3d_view_depth_at(px) -> f32`, `s3d_view_depth_of(world) -> f32`, `s3d_depth_valid() -> bool`,
/// `s3d_soft_fade(px, frag_view_depth, softness) -> f32` (1 without depth),
/// `s3d_scene_occluded(px, frag_device_depth) -> bool` (false without depth), `s3d_exposure_ev() -> f32`.
pub fn scene_wgsl(target: &PassTarget) -> &'static str;
/// The same for a depth access (amended at the F-core review, additive): shaders drawn outside node
/// passes — HdrScene overlays use `OverlayScene::scene_wgsl()` = `scene_wgsl_for(scene.depth_access())`.
pub fn scene_wgsl_for(access: DepthAccess) -> &'static str;
/// `struct FullscreenOut { @builtin(position) pos, @location(0) uv }` + `vs_fullscreen(vertex_index)`.
pub const FULLSCREEN_WGSL: &str;
```

Example (docs, the "Engine/Frame/Node Phases" story, and `crates/space3d-render/examples/frame_nodes.rs`):

```rust
struct DepthTint { pipelines: Vec<(PassTarget, wgpu::RenderPipeline)> }
static SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Transparent, 100).reading(SceneReads::DEPTH)];
impl RenderNode for DepthTint {
    fn slots(&self) -> &[PhaseSlot] { &SLOTS }
    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) { /* build per target */ }
    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let p = self.pipeline(ctx.device, ctx.target);            // VIEW_WGSL + scene_wgsl(&ctx.target) + OUTPUT_WGSL
        pass.set_pipeline(p);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
app.resources.expect_mut::<RenderNodes>().insert("depth-tint", DepthTint { pipelines: vec![] });
```

#### 2.2.5 Overlay additions (`overlay.rs`, F)

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OverlayStage {
    /// Into the primary scene color before post (bloom, tonemap); see §1.5.
    HdrScene,
    /// Into the final target after tonemapping (today's behaviour).
    #[default]
    AfterTonemap,
}

pub trait RenderOverlay: Send + Sync + 'static {
    fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
               encoder: &mut wgpu::CommandEncoder, target: &OverlayTarget);          // (existing)
    fn render(&self, pass: &mut wgpu::RenderPass<'static>, target: &OverlayTarget);   // (existing)
    /// NEW (provided). Read every frame.
    fn stage(&self) -> OverlayStage { OverlayStage::AfterTonemap }
    /// NEW (provided). HdrScene only: scene inputs sampled (DEPTH keeps the last primary view's depth stored).
    fn scene_reads(&self) -> SceneReads { SceneReads::NONE }
    /// NEW (provided). HdrScene only: called before `prepare` with the scene inputs of this pass.
    fn prepare_scene(&mut self, scene: &OverlayScene<'_>) { let _ = scene; }
}

#[non_exhaustive]
pub struct OverlayScene<'a> {
    /// The last primary scene view (matrices, near/far, rect).
    pub view: &'a ViewInfo,
    pub mode: FrameMode,
    /// Encoding the overlay MUST apply (Tonemapped in the Ldr fallback).
    pub output: OutputEncoding,
    pub exposure: f32,
    pub tonemapper: Tonemapper,
    pub scene_depth: Option<SceneDepthRef<'a>>,
    pub view_bind_group: &'a wgpu::BindGroup,
    pub scene_bind_group: &'a wgpu::BindGroup,
    pub layouts: &'a FrameLayouts,
}

impl OverlayScene<'_> {                    // amended at the F-core review (additive)
    /// `SampledOnly` when `scene_depth` is bound in group 1 (no depth attachment), else `None`.
    pub fn depth_access(&self) -> DepthAccess;
    /// `scene_wgsl_for(self.depth_access())`: the group-1 module for this pass's shaders.
    pub fn scene_wgsl(&self) -> &'static str;
}

impl OverlayTarget {                       // (existing struct; fields unchanged)
    /// Shader must encode sRGB itself: only 8-bit non-sRGB unorm formats (false for sRGB AND float formats).
    pub fn shader_encodes_srgb(&self) -> bool;
    /// `format` is a float (HDR) format.
    pub fn is_hdr(&self) -> bool;
}
```

#### 2.2.6 Render targets, renderer, stats, timing, quality (F)

```rust
impl RenderAssets {
    pub fn add_render_target_with(&mut self, width: u32, height: u32, desc: RenderTargetDesc) -> TextureHandle;
    pub fn render_target_desc(&self, handle: TextureHandle) -> Option<RenderTargetDesc>;
}

impl Renderer {
    pub fn graphics_caps(&self) -> &GraphicsCaps;
    pub fn frame_layouts(&self) -> &FrameLayouts;
    pub fn renderer_id(&self) -> u64;
    pub fn gpu_timings(&self) -> &GpuTimings;
    /// Primary scene color before post (§1.10); Err in Direct mode.
    pub fn capture_scene_hdr(&mut self) -> Task<Result<space3d_core::HdrImageData, RenderError>>;
    #[cfg(not(target_arch = "wasm32"))]
    pub fn capture_scene_hdr_blocking(&mut self) -> Result<space3d_core::HdrImageData, RenderError>;
    /// Last read-back exposure (stops, incl. compensation) of a post camera (`FrameSettings::read_exposure`).
    pub fn camera_exposure(&self, camera: space3d_core::Entity) -> Option<f32>;                 // P-POST fills
    /// Forget a camera's temporal post history (auto exposure snaps next frame).
    pub fn reset_post_history(&mut self, camera: space3d_core::Entity);                          // P-POST fills
    /// Build pipelines ahead of use (loading screens; WebGL2 program links).
    pub fn prewarm(&mut self, request: &PrewarmRequest);                                          // F + P-MAT + P-POST
    pub fn environment_state(&self, cubemap: CubemapHandle) -> EnvironmentState;                  // P-MAT fills
}

/// What `Renderer::prewarm` builds, in addition to post pipelines of current post cameras and every
/// node's `prewarm` for the pass targets reachable in the current modes.
#[derive(Clone, Debug, Default)]
pub struct PrewarmRequest {
    pub materials: Vec<Material>,
    pub layouts: Vec<&'static InstanceLayout>,
    /// Extra color formats to prewarm for (e.g. the capture format when windowed).
    pub formats: Vec<wgpu::TextureFormat>,
}

/// GPU timestamps per pass, 1–3 frames late (resource + `Renderer::gpu_timings`). One query set (256
/// timestamps) shared by frame, post, materials and nodes; labels in §4.4.
/// (Amended at F8: one 256-timestamp query set per timed frame in a ring of 5, resolved once the frame's
/// GPU work completed — Metal writes render-pass boundary samples asynchronously, so a same-frame resolve
/// read stale values — hence 2–5 frames late. `PrepareContext`/`ViewPrepareContext` timestamp writes
/// borrow the query set (lifetime `'a`), not the context.)
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct GpuTimings {
    pub supported: bool,
    pub frame: Option<u64>,
    pub total_ms: f32,
    pub scopes: Vec<GpuScope>,
}
impl GpuTimings { pub fn scope_ms(&self, label: &str) -> f32; /* sum over matching scopes */ }
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct GpuScope { pub label: &'static str, pub camera: Option<space3d_core::Entity>, pub ms: f32 }

/// Coherent quality tiers (graphics menus, device defaults). Implemented in Phase I.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum GraphicsQuality { Low, #[default] Medium, High }
impl GraphicsQuality {
    /// WebGL2 / no float targets → Low; integrated / Apple / mobile → Medium; discrete → High.
    pub fn recommended(caps: &RenderCapabilities, graphics: &GraphicsCaps) -> Self;
    pub fn frame_settings(self, base: FrameSettings) -> FrameSettings;
    pub fn msaa_samples(self, caps: &RenderCapabilities) -> u32;
    pub fn post(self, base: PostProcess) -> PostProcess;
    pub fn lighting(self, base: LightingSettings) -> LightingSettings;
    pub fn shadow(self, base: DirectionalShadow) -> DirectionalShadow;
}
```

`RenderStats` *(existing)* gains these `pub` counters. Each is 0 in Direct mode without new features; F adds all of
them, and builders increment their own:

```rust
pub render_passes: u32, pub compute_passes: u32, pub fullscreen_passes: u32,
pub intermediate_groups: u32, pub post_groups: u32, pub post_passes: u32,
pub scene_read_passes: u32, pub depth_resolves: u32, pub distortion_passes: u32,
pub node_draws: u32, pub node_dispatches: u32,
pub depth_views: u32, pub shadow_draw_calls: u32, pub shadow_casters: u64,
pub sorted_instances: u32, pub ibl_bake_passes: u32,
pub textures_created: u32, pub target_bytes: u64,
```

`RenderPlugin::build` additionally `init_resource`s `FrameSettings`, `LightingSettings`, `RenderNodes` (with
the built-ins) and `GpuTimings`. `RenderOverlays` is already there on main. `RenderPlugin` itself is unchanged.

#### 2.2.7 Post-processing API (`post/api.rs`; types written by F, behaviour P-POST)

These are post design §3.1–3.6 with these normative changes:
* `PostSettings`, `PostStats`, `PostCapabilities` and `PostTimings` are **removed**. Their replacements:
  `FrameSettings::{post, read_exposure, gpu_timing, downlevel.no_compute}`, `RenderStats` counters, `GraphicsCaps`,
  `GpuTimings` (labels `post.*`) and `Renderer::camera_exposure`.
* `PostQuality` is removed in favour of `GraphicsQuality` (`PostProcess::with_quality(GraphicsQuality)`).
* `Tonemapper` is the one from §2.2.3.
* Config structs are plain `pub`-field structs (FRU-friendly). Enums are `#[non_exhaustive]`.

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PostProcess {
    pub enabled: bool,                      // true
    pub exposure: f32,                      // stops; compensation with auto exposure. 0
    pub auto_exposure: AutoExposure,        // off
    pub tonemapper: Tonemapper,             // AgX
    pub bloom: Bloom,                       // Bloom::glow() (enabled)
    pub anti_aliasing: AntiAliasing,        // None
    pub vignette: Vignette,                 // off
    pub chromatic_aberration: ChromaticAberration, // off
    pub film_grain: FilmGrain,              // off
    pub distortion: Distortion,             // enabled (free without writers)
    pub color_grading: ColorGrading,        // neutral
    pub dither: bool,                       // true
}
impl Default for PostProcess;
impl PostProcess {
    pub fn new() -> Self;
    /// Tonemapper None, everything off, no dither: the legacy look through the HDR path.
    pub fn passthrough() -> Self;
    /// Default + FXAA(Medium) + vignette 0.3 + CA 0.15 + grain 0.05.
    pub fn cinematic() -> Self;
    pub fn with_quality(self, q: GraphicsQuality) -> Self;
    pub fn with_exposure(self, stops: f32) -> Self;
    pub fn with_auto_exposure(self, a: AutoExposure) -> Self;
    pub fn with_tonemapper(self, t: Tonemapper) -> Self;
    pub fn with_bloom(self, b: Bloom) -> Self;
    pub fn without_bloom(self) -> Self;
    pub fn with_fxaa(self, q: FxaaQuality) -> Self;
    pub fn with_vignette(self, v: Vignette) -> Self;
    pub fn with_chromatic_aberration(self, intensity: f32) -> Self;
    pub fn with_film_grain(self, g: FilmGrain) -> Self;
    pub fn with_distortion(self, d: Distortion) -> Self;
    pub fn with_color_grading(self, g: ColorGrading) -> Self;
    pub fn with_lut(self, lut: ColorLut) -> Self;
}
// Fields and defaults exactly as design-post §3.2–3.6:
pub struct AutoExposure { pub enabled: bool, pub min_ev: f32, pub max_ev: f32, pub adapt_to_bright: f32,
    pub adapt_to_dark: f32, pub low_percentile: f32, pub high_percentile: f32, pub metering: Metering }
impl AutoExposure { pub fn on() -> Self; pub fn instant(self) -> Self; }
#[non_exhaustive] pub enum Metering { Average, #[default] CenterWeighted }
pub struct Bloom { pub enabled: bool, pub intensity: f32, pub threshold: f32, pub soft_knee: f32,
    pub scatter: f32, pub composite: BloomComposite, pub mip_count: u32, pub max_mip_height: u32 }
impl Bloom { pub fn glow() -> Self; pub fn natural() -> Self; pub fn with_intensity(self, i: f32) -> Self;
    pub fn with_threshold(self, t: f32, knee: f32) -> Self; pub fn with_scatter(self, s: f32) -> Self; }
#[non_exhaustive] pub enum BloomComposite { #[default] Additive, EnergyConserving }
#[non_exhaustive] pub enum AntiAliasing { #[default] None, Fxaa(FxaaQuality) }
#[non_exhaustive] pub enum FxaaQuality { Low, #[default] Medium, High, Ultra }
pub struct Vignette { pub enabled: bool, pub intensity: f32, pub smoothness: f32, pub roundness: f32,
    pub center: glam::Vec2, pub color: glam::Vec3 }
pub struct ChromaticAberration { pub enabled: bool, pub intensity: f32 }
pub struct FilmGrain { pub enabled: bool, pub intensity: f32, pub size: f32, pub luminance_response: f32,
    pub animated: bool, pub seed: u32 }
pub struct Distortion { pub enabled: bool, pub strength: f32, pub chromatic: f32, pub resolution: DistortionResolution }
#[non_exhaustive] pub enum DistortionResolution { #[default] Half, Full }
/// Component: draws the entity's mesh (`Mesh3d` + `GlobalTransform`, optional `Visibility`) into the DB of
/// every post camera that sees it (node "space3d/distortion-meshes"). Occluded by scene depth where available.
pub struct DistortionMaterial { pub strength: f32, pub mode: DistortionMode, pub texture: Option<TextureHandle>,
    pub scroll: glam::Vec2, pub soft_depth: f32 }
#[non_exhaustive] pub enum DistortionMode { #[default] Normal, Radial { inner: f32, outer: f32 } }
pub const DISTORTION_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Writer prelude: `const DISTORTION_RANGE = 0.0625;` `distortion_encode(offset_view_heights, weight)`,
/// `distortion_depth_fade(frag_xy, db_size, softness)` (uses `scene_wgsl` helpers). Blend One/One.
pub const DISTORTION_WGSL: &str;
pub struct ColorGrading { pub temperature: f32, pub tint: f32, pub saturation: f32, pub contrast: f32,
    pub lift: glam::Vec3, pub gamma: glam::Vec3, pub gain: glam::Vec3, pub lut: Option<ColorLut>,
    pub shader: Option<ShaderHandle> }
impl ColorGrading { pub fn is_neutral(&self) -> bool; }
pub struct ColorLut { pub texture: TextureHandle, pub size: u32, pub contribution: f32 }
impl ColorLut { pub fn new(texture: TextureHandle, size: u32) -> Self; pub fn identity(size: u32) -> TextureSource;
    pub fn from_cube(text: &str) -> Result<TextureSource, LutError>; }
#[derive(Debug, thiserror::Error)] #[non_exhaustive] pub enum LutError { /* design-post §3.6 */ }
```

(Derives follow design-post: `Clone, Copy, Debug, PartialEq` on structs, `+ Default, Eq, Hash` on enums.)

#### 2.2.8 Materials API (`components.rs` fields by F; `material.rs`/`assets.rs` behaviour P-MAT)

`Material` *(existing)* gains these fields. Defaults reproduce today exactly (D20):

```rust
pub struct Material {
    // existing: base_color, base_color_texture, metallic, roughness, emissive, unlit, double_sided, alpha_blend, shader
    pub normal_texture: Option<TextureHandle>,             // None   (linear; +Y green up)
    pub normal_scale: f32,                                 // 1.0
    pub metallic_roughness_texture: Option<TextureHandle>, // None   (linear; G rough, B metal)
    pub occlusion_texture: Option<TextureHandle>,          // None   (linear R; same handle as MR → ORM packed)
    pub occlusion_strength: f32,                           // 1.0
    pub emissive_texture: Option<TextureHandle>,           // None   (sRGB; × emissive)
    pub blend: BlendMode,                                  // Opaque (legacy alpha_blend → AlphaBlend)
    pub alpha_cutoff: f32,                                 // 0.5
    pub uv_transform: TextureTransform,                    // IDENTITY
    pub uv1_maps: MapMask,                                 // NONE
    pub shadows: ShadowMode,                               // Auto
    pub render_order: i16,                                 // 0 (transparent meshes only)
}
impl Material {
    pub fn with_normal_map(self, t: TextureHandle, scale: f32) -> Self;
    pub fn with_metallic_roughness_map(self, t: TextureHandle) -> Self;
    pub fn with_occlusion_map(self, t: TextureHandle, strength: f32) -> Self;
    pub fn with_orm_map(self, t: TextureHandle) -> Self;
    pub fn with_emissive_map(self, t: TextureHandle, factor: glam::Vec3) -> Self;
    pub fn with_blend(self, b: BlendMode) -> Self;
    pub fn with_alpha_mask(self, cutoff: f32) -> Self;
    pub fn with_uv_transform(self, t: TextureTransform) -> Self;
    pub fn with_uv1_maps(self, m: MapMask) -> Self;
    pub fn with_shadows(self, m: ShadowMode) -> Self;
    pub fn with_render_order(self, o: i16) -> Self;
    pub fn effective_blend(&self) -> BlendMode;
    /// Unlit, Additive, double-sided glow.
    pub fn additive(color: glam::Vec4) -> Self;
    /// Unlit premultiplied (a = 0 adds light; a > 0 also covers).
    pub fn premultiplied(color: glam::Vec4) -> Self;
}

// material.rs (hub)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum BlendMode { #[default] Opaque, AlphaMask, AlphaBlend, Premultiplied, Additive }
impl BlendMode {
    pub fn is_transparent(self) -> bool;
    pub fn writes_depth(self) -> bool;
    /// Opaque/AlphaMask → None; AlphaBlend → ALPHA_BLENDING; Premultiplied → PREMULTIPLIED_ALPHA_BLENDING;
    /// Additive → color (SrcAlpha, One, Add), alpha (Zero, One, Add). Shared with particles.
    pub fn blend_state(self) -> Option<wgpu::BlendState>;
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShadowMode { #[default] Auto, CastAndReceive, CastOnly, ReceiveOnly, None }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MapMask(pub u8);
impl MapMask { pub const NONE: Self; pub const BASE_COLOR: Self; pub const NORMAL: Self;
    pub const METALLIC_ROUGHNESS: Self; pub const OCCLUSION: Self; pub const EMISSIVE: Self; pub const ALL: Self;
    pub const fn union(self, o: Self) -> Self; pub const fn contains(self, o: Self) -> bool; }
impl std::ops::BitOr for MapMask { type Output = Self; }
/// KHR_texture_transform: uv' = T(offset)·R(rotation)·S(scale)·uv (one per material; all maps).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextureTransform { pub offset: glam::Vec2, pub rotation: f32, pub scale: glam::Vec2 }
impl TextureTransform { pub const IDENTITY: Self; pub fn matrix(&self) -> glam::Mat3; }
impl Default for TextureTransform { /* IDENTITY */ }

/// Per-vertex data beyond `MeshData` (which stays frozen).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct MeshExtras { pub tangents: Vec<[f32; 4]>, pub uvs1: Vec<[f32; 2]> }
impl MeshExtras { pub fn new() -> Self; pub fn with_tangents(self, t: Vec<[f32; 4]>) -> Self;
    pub fn with_uvs1(self, uv: Vec<[f32; 2]>) -> Self; }

impl TextureSource {                                       // (existing type; new constructors)
    pub fn from_image_linear(image: ImageData) -> Self;    // Rgba8Unorm, GPU mips
    pub fn from_layers_linear(layers: &[ImageData]) -> Option<Self>;
    pub fn from_hdr(image: &space3d_core::HdrImageData) -> Self;   // Rgba16Float
}
impl RenderAssets {
    pub fn add_image_linear(&mut self, image: ImageData) -> TextureHandle;
    pub fn add_mesh_with(&mut self, data: MeshData, extras: MeshExtras) -> MeshHandle;
    pub fn update_mesh_with(&mut self, h: MeshHandle, data: MeshData, extras: MeshExtras) -> bool;
}
```

Behaviour: design-materials §3–§6, with these normative changes:
* `LightingSettings::transparent_sort` defaults to **`TransparentSort::None`**. `PerInstance` is opt-in.
* New WGSL helpers are prefixed `s3d_`.
* Compare presets use `CompareOptions::perceptual()`.
* The glTF bridge also offers `apply_material_maps` (§2.4).

#### 2.2.9 Lighting API (`lighting/api.rs`; types F, behaviour P-MAT)

```rust
define_handle!(/// Handle to a cubemap in RenderAssets.
               CubemapHandle);
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CubemapSource { Faces(TextureSource), Equirect { texture: TextureSource, face_size: u32 } }
impl CubemapSource {
    pub fn from_faces(faces: [ImageData; 6]) -> Option<Self>;          // +X −X +Y −Y +Z −Z, sRGB
    pub fn from_faces_threejs(faces: [ImageData; 6]) -> Option<Self>;
    pub fn from_equirect_image(image: ImageData) -> Self;
    pub fn from_equirect_hdr(image: &space3d_core::HdrImageData) -> Self;
    pub fn with_face_size(self, size: u32) -> Self;
}
impl RenderAssets {
    pub fn add_cubemap(&mut self, source: CubemapSource) -> CubemapHandle;
    pub fn update_cubemap(&mut self, h: CubemapHandle, source: CubemapSource) -> bool;
    pub fn remove_cubemap(&mut self, h: CubemapHandle) -> bool;
    pub fn contains_cubemap(&self, h: CubemapHandle) -> bool;
    pub fn cubemap_face_size(&self, h: CubemapHandle) -> Option<u32>;
}
/// Camera component: cubemap background (Skybox phase, far plane).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Skybox { pub cubemap: CubemapHandle, pub intensity: f32, pub rotation: glam::Quat }
impl Skybox { pub fn new(c: CubemapHandle) -> Self; pub fn with_intensity(self, i: f32) -> Self;
    pub fn with_rotation(self, r: glam::Quat) -> Self; }
/// Camera component: diffuse irradiance + GGX prefiltered specular from `cubemap`, on top of AmbientLight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvironmentLight { pub cubemap: CubemapHandle, pub diffuse_intensity: f32,
    pub specular_intensity: f32, pub rotation: glam::Quat }
impl EnvironmentLight { pub fn new(c: CubemapHandle) -> Self; pub fn with_intensity(self, d: f32, s: f32) -> Self;
    pub fn with_rotation(self, r: glam::Quat) -> Self; }
/// Light component next to a `DirectionalLight` (which is unchanged). At most one casts (lowest entity bits).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionalShadow { pub map_size: u32, pub fit: ShadowFit, pub filter: ShadowFilter,
    pub depth_bias: f32, pub normal_bias: f32, pub slope_bias: f32, pub strength: f32, pub caster_depth: f32 }
impl Default for DirectionalShadow { /* 2048, Camera{None, 200, None}, Pcf9, 0.02, 1.0, 1.5, 1.0, 500 */ }
impl DirectionalShadow { pub fn new() -> Self; pub fn with_map_size(self, s: u32) -> Self;
    pub fn with_fit(self, f: ShadowFit) -> Self; pub fn with_filter(self, f: ShadowFilter) -> Self;
    pub fn with_bias(self, depth: f32, normal: f32, slope: f32) -> Self; pub fn with_strength(self, s: f32) -> Self; }
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum ShadowFit { Camera { camera: Option<space3d_core::Entity>, max_distance: f32, receivers: Option<Aabb> },
    Bounds(Aabb) }
impl ShadowFit { pub fn top_down(plane_y: f32, half_thickness: f32, max_distance: f32) -> Self; }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShadowFilter { Hard, Pcf4, #[default] Pcf9, Pcf16 }

/// Global lighting options (resource).
#[derive(Clone, Debug, PartialEq)]
pub struct LightingSettings {
    pub shading: ShadingModel,             // Classic (pixel-identical)
    pub normal_maps: bool,                 // true
    pub transparent_sort: TransparentSort, // None (opt-in PerInstance)
    pub ibl: IblSettings,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)] #[non_exhaustive]
pub enum ShadingModel { #[default] Classic, Standard }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)] #[non_exhaustive]
pub enum TransparentSort { #[default] None, PerInstance { max_instances: u32 } }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IblSettings { pub specular_size: u32, pub irradiance_size: u32, pub specular_samples: u32,
    pub irradiance_samples: u32, pub bake_passes_per_frame: Option<u32> }   // 256, 32, 64, 256, None
#[derive(Clone, Copy, Debug, PartialEq, Eq)] #[non_exhaustive]
pub enum EnvironmentState { Missing, Uploading, Baking { passes_left: u32 }, Ready, Failed }
```

IBL with `ShadingModel::Classic` (Blinn–Phong highlights and GGX reflections) is documented as approximate.
Showcase stories and the High preset use `Standard` (critique P2-11).

#### 2.2.10 Preludes

`space3d_render::prelude` (and therefore `space3d::prelude`, which globs it) gains **exactly**:
`PostProcess, Tonemapper, Bloom, CameraFrame, Skybox, EnvironmentLight, DirectionalShadow, CubemapHandle`.

Every other new name is reachable through `space3d_render::*` / `space3d::render::*` only. `BlendMode` stays out
of the prelude: `space3d_assets::edit::texture::BlendMode` exists. Particles are **never** in the facade prelude
(the game has `Particle`, `Effect`, `Fade`, `Trail`). The list is enforced by `crates/space3d/tests/prelude_policy.rs`
(§2.7).

### 2.3 `space3d-core` (F)

```rust
/// Linear floating-point RGBA image (HDR environments, HDR captures). Not in the core prelude.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HdrImageData { pub width: u32, pub height: u32, pub pixels: Vec<[f32; 4]> }
impl HdrImageData {
    pub fn new(width: u32, height: u32, pixels: Vec<[f32; 4]>) -> Self;   // panics if len mismatches
    pub fn filled(width: u32, height: u32, c: [f32; 4]) -> Self;
    pub fn get(&self, x: u32, y: u32) -> [f32; 4];
}
```

Exported as `space3d_core::HdrImageData`, next to `ImageData` in `data.rs`.

### 2.4 `space3d-assets` (P-MAT)

These are design-materials §3.9, plus the P1-12 helper:
* `MaterialData` *(existing)* gains these fields, all `#[serde(default)]` with legacy defaults:
  `metallic_roughness_texture`, `occlusion_texture`, `emissive_texture: Option<usize>`,
  `normal_scale`, `occlusion_strength`, `emissive_strength: f32`, `unlit: bool`,
  `texture_info: MaterialTextureInfo`.
* `ModelMesh` *(existing)* gains `extras: Vec<PrimitiveExtras>` (`#[serde(default)]`; `extras[i]` belongs to
  `primitives[i]`), `PrimitiveExtras { tangents: Vec<[f32;4]>, uvs1: Vec<[f32;2]> }` and
  `ModelMesh::primitive_extras(i)` (the streams whose length matches the primitive's vertex count). *(Amended at
  the M1 review: F2 had put `tangents`/`uvs1` on `Primitive`, which broke the game's exhaustive
  `Primitive { mesh, material }` literals (5 × E0063 at the game's a614a02 pin). `Primitive` is frozen.)*
* New types:
  * `MaterialTextureInfo`, `TextureInfo { tex_coord, transform: Option<UvTransformData>, sampler: SamplerData }`,
    `UvTransformData`, `SamplerData`;
  * loaders `HdrImageLoader` (`.hdr` → `HdrImageData`) and `SkyboxLoader` (`.skybox` JSON → `CubemapData`),
    `CubemapData`, `CubeConvention`;
  * `edit::mesh::compute_mikktspace_tangents(&MeshData) -> (MeshData, Vec<[f32; 4]>)` (the `bevy_mikktspace`
    workspace dependency; latest stable checked at build time, AGENTS rule 1).
* glTF loader reads:
  * every slot, `texCoord`;
  * `KHR_texture_transform`, `KHR_materials_emissive_strength`, `KHR_materials_unlit`;
  * `TANGENT`, `TEXCOORD_1`.
  `GltfLoader` *(existing)* is unchanged.
* Feature `render` (enabled by the facade `render` feature), module `space3d_assets::render`:

```rust
pub struct ModelUploadOptions { pub generate_tangents: bool, pub sampler_override: Option<SamplerSettings> }
pub fn prepare_model(model: &ModelData, o: &ModelUploadOptions) -> PreparedModel;          // CPU: MikkTSpace
pub fn prepare_model_async(model: ModelData, o: ModelUploadOptions) -> Task<PreparedModel>; // spawn_compute
pub struct PreparedModel { /* … */ }
impl PreparedModel { pub fn upload(self, assets: &mut RenderAssets) -> RenderModel; }
pub fn upload_model(model: &ModelData, assets: &mut RenderAssets, o: &ModelUploadOptions) -> RenderModel;
#[non_exhaustive] pub struct RenderModel { pub parts: Vec<RenderPart>, pub textures: Vec<TextureHandle> }
#[non_exhaustive] pub struct RenderPart { pub name: Option<String>, pub node: Option<usize>,
    pub material_index: Option<usize>, pub mesh: MeshHandle, pub material: Material, pub transform: Mat4 }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)] #[non_exhaustive]
pub enum TextureRole { BaseColor, Normal, MetallicRoughness, Occlusion, Emissive }
impl TextureRole { pub fn is_srgb(self) -> bool; }   // BaseColor, Emissive
/// Full MaterialData → Material conversion.
pub fn material_from_data(data: &MaterialData,
    texture: &mut dyn FnMut(usize, TextureRole) -> Option<TextureHandle>) -> Material;
/// NEW (P1-12): add only the map-related state of `data` to a material built by custom code (the game's
/// `painted_material`/`legacy_material`): normal/MR/occlusion/emissive maps, normal_scale, occlusion_strength,
/// ORM packing, emissive_strength × emissive (only if the material's emissive is non-zero or the slot exists),
/// uv1_maps, uv_transform. Base color, blend and surface factors are left untouched.
pub fn apply_material_maps(material: &mut Material, data: &MaterialData,
    texture: &mut dyn FnMut(usize, TextureRole) -> Option<TextureHandle>);
pub fn spawn_model(world: &mut World, model: &RenderModel, root: Transform) -> Entity;
impl CubemapData { pub fn to_source(&self) -> CubemapSource; }
```

Game migration note (`docs/graphics/migration.md`, Phase I):
1. In `game/models.rs`, after building `material`, call `apply_material_maps(&mut material, &data, &mut upload)`.
   `upload` uses `add_image` for sRGB roles and `add_image_linear` for linear roles.
2. For primitives with a normal map but no `TANGENT`, run `prepare_model_async` or `compute_mikktspace_tangents`
   off-thread, then call `add_mesh_with`.

### 2.5 `space3d-particles` (new crate, P-PART; skeleton by F)

This is design-particles §4 verbatim, except for these normative deltas:
1. **Names:**
   * The render hook is the frame's `RenderNode`, registered as `"space3d/particles"` in `RenderNodes`. Its slots
     are `[Transparent @100 (.reading(DEPTH) iff settings.soft_particles), Distortion @100]`.
   * `RenderFeature`, `requirements()` and `ViewInfo::scene_depth` are gone. Use `DrawContext::{target,
     scene_bind_group, scene_depth, slot}`.
2. **`Fade` is renamed `FadeCurve`** (`Curve::Preset(FadeCurve)`).
3. **Output:** particle shaders call `s3d_output()` (`OUTPUT_WGSL`) with the view's encoding, exposure and
   tonemapper, so they match meshes in every mode.
   * `ParticleSettings::ldr_soft_clip` becomes **`direct_soft_clip: bool`** (default true). It applies `1 − e^−x`
     only in `FrameMode::Direct` (no tonemapper exists there). *(Amended at the M2 review, game#3: also in the
     Ldr passthrough of a render scale below 1 without post — `FrameMode::Ldr` with no tonemapper — which has no
     tonemapper either; the look no longer changes with the render scale. Never in Hdr mode or a post camera's Ldr
     fallback, whose tonemapper runs per fragment.)*
4. **Distortion:** `SpriteRender::target: SpriteTarget { Color, Distortion { strength: f32 } }` (default
   `Color`). Distortion layers draw in the Distortion slot with `DISTORTION_WGSL` into `DB`. The `Rg16Float`
   target is dropped.
5. **Bind groups:**
   * 0 = frame view (`VIEW_WGSL`: `view_proj`, `view`, `camera_pos`, viewport, depth params);
   * 1 = frame scene (`scene_wgsl`);
   * 2 = particles per-frame (LUT texture + sampler, `GpuParticleFrame` uniform: render time, flags; dynamic
     draw params);
   * 3 = sprite/flipbook texture.
   The vertex shader reads no storage buffers.
6. **Soft particles:**
   * `DepthAccess::ReadOnlyAttached` → `s3d_soft_fade`;
   * `SampledOnly` → soft fade and a `s3d_scene_occluded` discard;
   * `None` → hard test only (in-pass fallback).
7. **Backend choice:** `Auto` picks `Gpu` iff `GraphicsCaps::compute` and `RenderCapabilities::indirect`. Kernels
   use `@workgroup_size(64)` and ≤ 4 storage buffers.
8. **Stats:** `ParticleStats` stays in this crate. GPU timing labels are `particles.sim`, `particles.compact` and
   `particles.sort`.
9. **Prelude:** `space3d::particles::prelude` only. It is not in the facade prelude and not in `DefaultPlugins`.
   Facade feature `particles = ["render", "dep:space3d-particles"]`, not default in v1.
10. **Blend:** `ParticleBlend::from_blend_mode(BlendMode) -> Option<ParticleBlend>`. The premultiplied equation is
    as designed; visual parity with `BlendMode` is tested.
11. **Stories** live in `space3d-stories/src/engine/particles.rs` (feature `particles`, part of `engine`), so they
    join the engine regression test, the CLI `stories` command and the editor viewer (critique P2-3). There is no
    `particles_storybook()` in the particles crate.

### 2.6 Crate-internal contracts (binding between builders, not public)

F writes these signatures with stub bodies. The P builders own the bodies. A signature changes only through the
foundation maintainer (§5.0).

```rust
// lighting/mod.rs (P-MAT owns the bodies)
pub(crate) struct Lighting { /* cubemaps, env cache, bake queue, shadow state */ }
impl Lighting {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, caps: &GraphicsCaps) -> Self;       // stub: dummies
    /// Step 4: cubemap uploads/conversion, IBL bake passes (budgeted; all at once when headless).
    pub fn prepare(&mut self, cx: &mut LightingPrepare<'_>);                                    // stub: no-op
    /// Step 5: shadow depth views for this frame (after camera planning).
    pub fn depth_views(&mut self, world: &World, cameras: &[ViewInfo], settings: &LightingSettings)
        -> &[DepthViewRequest];                                                                 // stub: &[]
    /// Step 8: fill env/sky/shadow fields of a camera's GpuView (zero = off).
    pub fn fill_view(&self, world: &World, camera: Entity, view: &mut GpuView);                 // stub: no-op
    /// Bindings 1–6 of group 0 for a camera, plus a stable key for bind-group caching.
    pub fn view_resources(&self, camera: Option<Entity>) -> ViewResources<'_>;                  // stub: dummies, key 0
    pub fn register_nodes(nodes: &mut RenderNodes);                                             // stub: skybox node no-op
}
pub(crate) struct DepthViewRequest {
    pub light: Entity, pub view_proj: Mat4, pub frustum: Frustum,
    pub target: wgpu::TextureView /* Depth32Float, size² */, pub size: u32,
    pub bias: wgpu::DepthBiasState,
}
// frame/extract.rs (P-MAT): which sources cast (MatEntry cast flag); F's depth-view draw loop calls it.
pub(crate) fn source_casts(materials: &MaterialTable, material: u32) -> bool;                  // stub: false

// post/mod.rs (P-POST owns the bodies; F writes a working composite-only version)
pub(crate) struct PostState { /* pool users, per-camera history (E, DB), pipelines, readback */ }
impl PostState {
    pub fn new(device: &wgpu::Device, caps: &GraphicsCaps) -> Self;
    pub fn begin_frame(&mut self, frame: &FrameInfo, settings: &FrameSettings);
    /// Step 6: per post group — what this group needs (bloom, auto exposure, FXAA, distortion, depth).
    pub fn plan_group(&mut self, world: &World, group: &PostGroupPlan) -> PostGroupNeeds;
    /// Persistent DB of a post group (created on demand), cleared by the caller's pass.
    pub fn distortion_target(&mut self, device: &wgpu::Device, camera: Entity, rect: [u32; 4],
        res: DistortionResolution) -> (&wgpu::TextureView, (u32, u32));
    /// Record meter → bloom → composite (→ FXAA) for one post group into the frame encoder.
    pub fn encode_group(&mut self, cx: &mut PostEncode<'_>, input: &PostGroupInput<'_>);
    pub fn end_frame(&mut self, stats: &mut RenderStats);
    pub fn register_nodes(nodes: &mut RenderNodes);                    // "space3d/distortion-meshes"
    pub fn camera_exposure(&self, camera: Entity) -> Option<f32>;
    pub fn reset_history(&mut self, camera: Entity);
}
pub(crate) struct PostGroupInput<'a> {
    pub camera: Entity, pub settings: PostProcess, pub mode: FrameMode, pub scene_output: OutputEncoding,
    pub scene: &'a wgpu::TextureView, pub scene_format: wgpu::TextureFormat, pub scene_size: (u32, u32),
    pub scene_rect: [u32; 4], pub scene_alpha: bool, pub transparent: bool,
    pub depth: Option<&'a wgpu::TextureView>, pub distortion: Option<&'a wgpu::TextureView>,
    pub output: &'a wgpu::TextureView, pub output_format: wgpu::TextureFormat, pub output_size: (u32, u32),
    pub output_rect: [u32; 4], pub output_manual_srgb: bool, pub output_clear: bool,
    pub passthrough: bool, pub replay: bool, pub frame: FrameInfo,
}
// frame/targets.rs (F): the one texture pool (post bloom chain, L, downlevel log-lum chain).
pub(crate) struct TexturePool;   // acquire(desc) → PoolTexture (per-mip views created once), release at group end,
                                 // evict after 3 unused frames, `generation` per entry for bind-group caches
```

### 2.7 Compatibility proof (249c874-era code keeps compiling)

**Evidence base.**
* The game at 4159bb8 pins 249c874.
* Exhaustive literals were located by grep:
  * `Material { .. }` in the game always ends with `..Material::default()` / `..Material::color(..)`
    (`ui/landing.rs:407`, `ui/pages/wiki/viewer.rs:497/657`, …);
  * the only exhaustive `Material` literal is in-workspace: `space3d-editor/src/components.rs:148`;
  * `Camera3d { .. }` literals exist only in-workspace (the editor);
  * the game uses `Camera3d::default()` plus builders;
  * `OverlayTarget { .. }` is built only inside `overlay.rs`;
  * `RenderStats { .. }` is built only with FRU (`space3d-test`);
  * the game matches `OverlayTargetKind` by `==`.

**Every public item touched**:

| item | change | why it is safe |
|---|---|---|
| `Material` | +12 `pub` fields (defaults = legacy), + builder methods, `Default` extended | Downstream uses FRU or field mutation only. The in-workspace editor literal is converted to FRU in F0. It is not `#[non_exhaustive]`, which would forbid FRU. |
| `RenderStats` | +19 `pub` counters (`staging_chunks_created` *added at the M1 review*) | FRU-only construction downstream. `PartialEq`/`Default` still derive. |
| `RenderOverlay` trait | +3 provided methods (`stage`, `scene_reads`, `prepare_scene`) | Existing impls compile unchanged. Default = AfterTonemap = today's placement and pixels (test `legacy_overlay_unchanged`). |
| `OverlayTarget` | +2 inherent methods | Fields unchanged. |
| `OverlayTargetKind` | **unchanged** (no variant) | The game compares kinds and keeps per-kind buffers. HdrScene reuses the kinds. |
| `RenderAssets` | + methods (`add_image_linear`, `add_render_target_with`, `render_target_desc`, `add_mesh_with`, `update_mesh_with`, cubemap methods) | Additive. `render_target_view` is unchanged (it returns a float view only for opt-in HDR RTs). |
| `TextureSource` | + constructors | Fields unchanged. |
| `Renderer` | + methods | Additive. Behaviour in Direct mode is unchanged. |
| `RenderPlugin` | **unchanged struct**; `build` inits more resources and built-in nodes | The facade literal `RenderPlugin { headless }` still compiles. |
| `space3d_render::prelude` / `space3d::prelude` | + 8 names (§2.2.10) | Checked against every type name in the game (`prelude_policy.rs` fixture of 793 names) and the workspace preludes. No collision. |
| `MaterialData`, `ModelMesh` (assets) | + `#[serde(default)]` fields | FRU-only downstream (grep of the game at every pin: never built literally). Old RON/JSON still deserialize. `Primitive` is frozen (the game builds it literally; *amended at the M1 review*). |
| `space3d-core` | + `HdrImageData` (root export, not prelude); `App` + `add_system_ordered(stage, order, name, system)`, and `add_system` / `add_system_named` insert at order 0 by `partition_point` (*added at the M1 review* for `ParticlesPlugin::TICK_ORDER`) | Additive. Systems added with `add_system` all have order 0, so they keep running in registration order, as before (`space3d-core` app tests; `docs/graphics/api.md`); only an ordered system (the particles' tick at 1000) runs after them. |
| `engine_storybook()` | returns more stories | Additive. |
| **Frozen, unchanged:** `Camera3d`, `Projection`, `RenderTarget`, `DirectionalLight`, `PointLight`, `AmbientLight`, `MeshData`, `InstancedMesh`, `InstanceLayout`, `RenderSettings`, `RendererConfig`, `RenderCapabilities`, `DrawPath`, `SamplerSettings`, `OverlayTarget` fields, `DefaultPlugins`/`MinimalPlugins` fields, `GltfLoader`, `space3d_assets::Primitive` | — | New configuration lives in new components/resources (`CameraFrame`, `DirectionalShadow`, `Skybox`, `EnvironmentLight`, `FrameSettings`, `LightingSettings`, `MeshExtras`). |

**Behavioural compatibility.**
* Direct mode is the default. `TransparentSort::None`, `ShadingModel::Classic` and no maps leave the forward
  shader output bit-identical (every new multiply is by exact 1.0 or `select`ed off).
* The output tail for `Linear`/`ManualSrgb` is byte-for-byte today's `MANUAL_SRGB` block.
* `MANUAL_SRGB` stays available to custom shaders as an `override` derived from the encoding.
* Point-light selection is unchanged for ≤ 16 lights.
* Adding `ParticlesPlugin` or any node changes no mode and no pixels of views where the node reports no work.
* Verified by the engine goldens re-baselined in F0 (`direct_mode_matches_goldens`).

**Compile fixtures (F0/F2, `crates/space3d/tests/`).**
* `api_compat_249c874.rs`: verbatim 249c874-style code that must compile:
  * the game's `impl RenderOverlay` (prepare/render only);
  * the `Material` FRU forms;
  * `DirectionalLight { color, intensity }`, `PointLight { color, intensity, range }`, `AmbientLight { .. }`;
  * `MeshData { positions, normals, uvs, colors, indices }`, `InstancedMesh { mesh, instances, material }`;
  * `RenderPlugin { headless: None }`, `RenderSettings { draw_path, frustum_culling }`,
    `RendererConfig { .. Default::default() }`;
  * `RenderStats { draw_calls: 3, ..Default::default() }`;
  * `MaterialData { name, emissive, double_sided, ..Default::default() }`;
  * `TextureSource { sampler: SamplerSettings { address, ..SamplerSettings::default() }, ..TextureSource::from_image(i) }`;
  * an exhaustive `match` on `OverlayTargetKind`;
  * `use space3d::prelude::*;` together with `use space3d::render::{OverlayTarget, OverlayTargetKind,
    RenderOverlay, RenderOverlays};`.
* `prelude_policy.rs`: a fixture list of the game's type names (generated from SpaceCorps2027 at 4159bb8).
  * It asserts that the facade prelude's new names don't intersect the fixture list.
  * It compiles a module with `use space3d::prelude::*; use fixture::*;` referencing every *new* prelude name, so
    any ambiguity fails the build.

**Semver policy (documented in `docs/graphics/api.md`, F).**
* New config structs support FRU (`..Default::default()`). Exhaustive literals of them are not covered by
  compatibility guarantees.
* New enums and info structs are `#[non_exhaustive]`.
* Future fields may be added only to types listed as "FRU-only" in that doc.

### 2.8 Facade, stories, editor, CLI (F unless noted)

* Facade:
  * `Cargo.toml` gets feature `particles` (not default) and optional dep `space3d-particles`;
  * `lib.rs`: `#[cfg(feature = "particles")] pub use space3d_particles as particles;`;
  * the `render` feature enables `space3d-assets?/render`.
* `space3d-stories`:
  * `src/engine.rs` becomes `src/engine/mod.rs` (registry) + `basics.rs` (existing 9 stories, unchanged ids);
  * per-area files `frame.rs` (F), `materials.rs` + `lighting.rs` (P-MAT), `post.rs` (P-POST),
    `particles.rs` (P-PART, `#[cfg(feature = "particles")]`), `showcase.rs` (I);
  * each exports `pub(crate) fn stories() -> Vec<Story>`, and the registry concatenates them (F pre-wires
    empty functions);
  * feature `engine = ["dep:space3d-physics", "dep:space3d-particles"]`;
  * goldens under `tests/__screenshots__/Engine/<Area>/`.
* Editor: the F0 literal fix. `CameraFrame`/`PostProcess`/`Skybox`/`DirectionalShadow` reprs and inspector rows are
  added in Phase I (optional, additive).
* CLI `space3d stories` and the editor storybook viewer consume `engine_storybook()`, so new stories appear with no
  CLI change (verified in Phase I).

---

## 3. Shader composition and file layout

### 3.1 Composition strategy (three tiers; no naga_oil)

1. **String composition** with the existing `//#MARKER` scheme, for structural variation only. Markers in
   `forward.wgsl`:

   | marker | owner | content |
   |---|---|---|
   | `//#CONSTS` | P-MAT | structural consts (e.g. `S3D_COLOR_LAYERED`, `S3D_DATA_LAYERED`, `S3D_EXT`) |
   | `//#VIEW` | F | `VIEW_WGSL`'s `struct View` (shared source `shaders/common/view.wgsl`) |
   | `//#OUTPUT` | F | `OUTPUT_WGSL` (tonemap + `s3d_output`) |
   | `//#TEXTURE` | P-MAT | texture-set declarations + `sample_base` (existing name) and `s3d_sample_normal/_mr/_occlusion/_emissive` |
   | `//#EXT` | P-MAT | ext vertex stream fields and accessors (`s3d_vertex_tangent`, `s3d_vertex_uv1`) or constant defaults |
   | `//#INSTANCE`, `//#VARYINGS`, `//#HOOKS` | existing (P-MAT may extend) | unchanged meaning |

   User hook code is still spliced last (markers inside it are left alone).
2. **WGSL `override` constants**, specialized at pipeline creation (naga `process_overrides` runs on GL,
   wgpu-hal `gles/device.rs:316`; browsers natively). F declares **all** of them in F3 with defaults, so module keys
   never change shape:
   * `override S3D_OUTPUT: u32 = 0u;` (§2.2.3 ids) and `override MANUAL_SRGB: bool = false;` (derived: kept for
     custom-shader compatibility);
   * `S3D_ALPHA_MASK: bool = false`, `S3D_ALPHA_TO_COVERAGE: bool = false`, `S3D_SHADING_STANDARD: bool = false`,
     `S3D_SHADOW_TAPS: u32 = 0u`, `S3D_NORMAL_MAPS: bool = true`.
   Every override has a default. Custom-shader validation (which rejects overrides *without* defaults) keeps
   accepting the template.
3. **Uniform branches:** per-material data (which maps exist, unlit, ORM, uv1 mask, sRGB fixups, receives shadows,
   IBL on). Uniform control flow keeps implicit-derivative samples legal. All implicit-LOD samples and derivatives
   come first, unconditionally (design-materials §5.2).

The forward `fs_main` tail (F3) is normative. With `S3D_OUTPUT == 0` it is today's output; with `1` it is
today's `MANUAL_SRGB` block:

```wgsl
var color = fragment_hook(in, vec4<f32>(rgb, base.a));
return s3d_output(color, S3D_OUTPUT, view.exposure.x, view.output.y);
```

### 3.2 Shared WGSL modules (F)

| file | public const / fn | contents |
|---|---|---|
| `shaders/common/view.wgsl` | `VIEW_WGSL` | `struct DirLight`, `struct PointLight`, `struct View` (§3.3). `VIEW_WGSL` appends the group-0 binding-0 declaration; forward's `//#VIEW` splices the structs only (it keeps its own bindings 0–6 and `MAX_*_LIGHTS` consts) |
| `shaders/common/output.wgsl` | `OUTPUT_WGSL` | `s3d_linear_to_srgb`, `s3d_srgb_to_linear`, `s3d_luma`, `s3d_tonemap` (None, Reinhard, AcesFitted, AgX, KhronosPbrNeutral: formulas in Appendix B, CPU mirrors in `output.rs`), `s3d_output` |
| `shaders/common/scene.wgsl` (+ 3 variants) | `scene_wgsl(&PassTarget)` | group-1 declarations; `s3d_scene_uv`, `s3d_scene_depth`, `s3d_view_depth_at`, `s3d_view_depth_of`, `s3d_depth_valid`, `s3d_soft_fade`, `s3d_scene_occluded`, `s3d_exposure_ev` |
| `shaders/common/fullscreen.wgsl` | `FULLSCREEN_WGSL` | `vs_fullscreen` (one triangle, uv (0,0) top left) |
| `shaders/common/hash.wgsl` | (internal, used by post + particles copies) | `s3d_pcg`, `s3d_hash01` (bit-identical to the Rust `rng` mirrors) |
| `shaders/frame/depth_resolve.wgsl` | internal | min-of-samples `frag_depth` pass (RODS + MSAA) |
| `shaders/post/distortion.wgsl` | `DISTORTION_WGSL` | `DISTORTION_RANGE`, `distortion_encode`, `distortion_depth_fade` |

Naming rule (critique P2-9): every **new** WGSL function, global and constant that can appear in a module next to
user code is prefixed `s3d_` / `S3D_`. Existing names (`sample_base`, `srgb_to_linear`, `linear_to_srgb`,
`quat_to_mat3`, `trs`, `brdf`, `fresnel_schlick`, `instance_*`, `view`, `material`, `base_tex`, `base_sampler`)
keep their meaning. `tests/custom_shader.rs` gains `custom_shaders_compile_against_every_permutation`: every
existing custom-shader fixture × every forward permutation × every override value.

### 3.3 `GpuView` (F; frozen after F3)

The existing prefix is unchanged (752 B). Appended fields (std140-compatible; WGSL `mat3x3<f32>` = 48 B):

```wgsl
struct View {
    view_proj: mat4x4<f32>, camera_pos: vec4<f32>, ambient: vec4<f32>, counts: vec4<u32>,
    dir_lights: array<DirLight, 4>, point_lights: array<PointLight, 16>,          // ← existing, 752 B
    view: mat4x4<f32>, proj: mat4x4<f32>, inv_view_proj: mat4x4<f32>, inv_proj: mat4x4<f32>,
    viewport: vec4<f32>,       // x, y, w, h in pixels of the texture this view renders into
    target_size: vec4<f32>,    // w, h, 1/w, 1/h of that texture (`target` is a WGSL reserved word)
    depth: vec4<f32>,          // near, far, orthographic (0/1), 0
    output: vec4<u32>,         // x encoding id (informative; pipelines use S3D_OUTPUT), y tonemapper id, z flags, w 0
    exposure: vec4<f32>,       // x linear exposure for Tonemapped (1 otherwise), y ev, z 0, w 0
    shadow_view_proj: mat4x4<f32>,
    shadow_params: vec4<f32>,  // depth bias (light NDC), normal bias (world), strength, edge fade start
    shadow_info: vec4<u32>,    // enabled, dir-light index (0), map size, taps
    env_params: vec4<f32>,     // diffuse, specular, specular max mip, enabled
    env_rotation: mat3x3<f32>,
    sky_rotation: mat3x3<f32>,
    sky_params: vec4<f32>,     // intensity, enabled, 0, 0
}   // 1312 B → stride 1536 at 256-B alignment (1344 at 64)
```

* It contains **no per-frame-varying data**. A still camera with static lights uploads 0 bytes (`TrackedBuffer`
  diff, unchanged).
* Depth views get their own slot (`view_proj` = the light matrix; other fields zero).
* Layout test: `pipeline::tests::gpu_view_matches_wgsl` parses the template with naga and compares every member
  offset.

### 3.4 Bind groups and limits (normative)

| pipeline kind | group 0 | group 1 | group 2 | group 3 |
|---|---|---|---|---|
| forward mesh (P-MAT) | view (0 `View`, 1–6 env/shadow) | material (0 uniform, 1–10 five maps + samplers) | — | — |
| depth-only (shadow) | view (shadow variant: dummy shadow map) | material (alpha-mask casters only) | — | — |
| skybox node (P-MAT) | view | sky cube + sampler | — | — |
| **user/particle nodes** | **view (`FrameLayouts::view`)** | **scene (`FrameLayouts::scene`) if it reads depth/E; otherwise free** | node | node |
| post passes (P-POST) | own (0 `GpuPost` dyn, 1 sampler) | own (S, B0, E, LUT, DB, L) | — | — |
| overlays | own (optionally `OverlayScene` groups) | own | own | own |

WebGL2 limits hold everywhere: ≤ 4 bind groups, ≤ 16 sampled textures per stage (forward uses 9; particles use
≤ 4 in their own groups plus up to 6 declared in group 0 layouts), uniforms ≤ 16 KiB per binding (`View` binds
1312 B), no storage buffers outside compute, ≤ 15 varyings (forward uses 8).

### 3.5 Pipeline keys and prewarm

```rust
pub(crate) struct PipelineKey {          // F, frozen after F3
    pub layout: u16,
    pub class: u8,                       // material class bits (P-MAT defines bits 2..=5; 0/1 unchanged)
    pub format: wgpu::TextureFormat,
    pub samples: u32,
    pub output: OutputEncoding,          // replaces `manual_srgb` (drives S3D_OUTPUT / MANUAL_SRGB)
    pub kinds: u8,                       // bit0 color maps layered, bit1 data maps layered (replaces `layered`)
    pub shader: Option<(ShaderHandle, u32)>,
    pub pass: PassKind,                  // Forward | Depth | DepthMask
    pub globals: u8,                     // frame-wide override set (shading model, normal maps, shadow taps)
}
type ModuleKey = (u16 /*layout*/, u8 /*kinds*/, bool /*ext*/, PassKind, Option<(ShaderHandle, u32)>);
```

Overrides move `MANUAL_SRGB` out of the module key, which halves forward modules.

Pipeline budget (per preset, all pipelines prewarmable):

| area | Low (WebGL2) | Medium/High |
|---|---|---|
| forward (layouts × classes × kinds × ext; 1 output format, 1 sample count) | ≤ 16 | ≤ 30 |
| depth/shadow | ≤ 4 | ≤ 6 |
| sky + bake | 1 + 4 (bake once) | 1 + 4 |
| post (composite per output format, bloom, exposure, FXAA quality) | ≤ 6 | ≤ 12 |
| particles (sprite/ribbon × format × samples × depth access × texture kind) | ≤ 8 | ≤ 16 |
| frame (clear, blit, depth resolve, fullscreen copies) | ≤ 4 | ≤ 8 |

`Renderer::prewarm(&PrewarmRequest)` builds forward pipelines for every current pass signature × requested
materials/layouts. It also builds post pipelines for the current post cameras (+ the capture format) and calls
`RenderNode::prewarm` for every reachable `(slot, PassTarget)`. Docs recommend calling it on loading screens:
WebGL2 program links take 50–150 ms each.

### 3.6 File layout and ownership (after F1/F2)

Legend: **F** = foundation builder (and the foundation maintainer during P). A file with two owners transfers at
the end of Phase F (`F→P-MAT`: F writes the skeleton, P-MAT owns it afterwards). A builder MUST NOT edit files
it does not own (§5.0).

```
Cargo.toml (workspace deps incl. bevy_mikktspace; members glob unchanged)          F
crates/space3d-core/src/data.rs (HdrImageData)                                     F
crates/space3d-render/
  Cargo.toml (feature `angle = ["wgpu/angle"]`)                                    F
  ARCHITECTURE.md                                                                  F (I merges area summaries)
  src/lib.rs, settings.rs, caps.rs, output.rs, node.rs, overlay.rs, timing.rs     F
  src/renderer.rs, stats.rs                                                        F
  src/quality.rs                                                                   F→I
  src/components.rs (Material fields, docs)                                        F
  src/material.rs (BlendMode, ShadowMode, MapMask, TextureTransform, MeshExtras)   F→P-MAT
  src/frame/mod.rs      render_frame orchestration, FrameState                     F
  src/frame/plan.rs     groups, modes, post groups, roles, PassTargets             F
  src/frame/views.rs    ViewData, lights (D19), culling jobs, GpuView writes        F
  src/frame/targets.rs  group textures, TexturePool                                F
  src/frame/passes.rs   scene/SceneRead/depth views/Distortion/overlays/post slot/display views/replay   F
  src/frame/extract.rs  MaterialTable/Key, ECS batching, sources, sort keys, transparent sort, source_casts   F→P-MAT
  src/frame/draws.rs    encode_draws for built-in batches (material bind groups, ext slot 2)                  F→P-MAT
  src/frame/tests.rs    existing frame tests (moved)                               F
  src/pipeline/mod.rs   Pipelines cache, PipelineKey, PassKind, overrides, clear/blit/resolve pipelines   F
  src/pipeline/view.rs  GpuView, GpuDirLight, GpuPointLight                        F
  src/pipeline/forward.rs  forward_shader composition, GpuMaterial, material BGLs, texture kinds          F→P-MAT
  src/pipeline/instance.rs instance_wgsl, instance_attributes (moved)              F→P-MAT
  src/arena.rs, assets.rs, texture.rs, handles.rs                                  F→P-MAT
  src/lighting/{mod.rs, api.rs, cubemap.rs, environment.rs, skybox.rs, shadow.rs}  F→P-MAT
  src/post/{mod.rs, api.rs, chain.rs}                                              F→P-POST
  src/post/{bloom.rs, exposure.rs, fxaa.rs, lut.rs, grading.rs, distortion.rs, distortion_mesh.rs, readback.rs, pipelines.rs}  P-POST
  src/shaders/{cull,compact,blit,clear}.wgsl, shaders/common/*, shaders/frame/*    F
  src/shaders/forward.wgsl                                                         F→P-MAT (F owns the //#VIEW and //#OUTPUT regions)
  src/shaders/{depth,sky,ibl_bake}.wgsl, brdf_lut.bin                              F→P-MAT (F writes minimal depth.wgsl)
  src/shaders/post/{composite,distortion}.wgsl                                     F→P-POST
  src/shaders/post/{bloom,exposure,exposure_dl,fxaa,distortion_mesh}.wgsl          P-POST
  tests/{frame_modes,nodes,overlay_stages,depth_views,capture_hdr,alloc_frame}.rs  F
  tests/{common/mod.rs (declares area modules), gl.rs, alloc.rs, headless.rs, custom_shader.rs}  F
  tests/common/{materials,post}.rs                                                 P-MAT / P-POST
  tests/{materials,environment,shadows,gl_materials,alloc_materials}.rs           P-MAT (glTF renders: space3d-assets/tests/gltf_materials.rs)
  tests/{post,post_distortion,gl_post,alloc_post}.rs                               P-POST
  benches/{instancing.rs, frame.rs}  F;  benches/materials.rs P-MAT;  benches/post.rs P-POST;  benches/graphics.rs I
  examples/frame_nodes.rs                                                          F
crates/space3d-assets/** (model.rs, loaders/{gltf,hdr,skybox}.rs, edit/mesh/tangents.rs, render.rs, Cargo.toml)  F (F2: `render` feature skeleton) → P-MAT
crates/space3d-particles/**                                                        F (skeleton) → P-PART
crates/space3d-stories/src/engine/{mod.rs, basics.rs, frame.rs}, Cargo.toml       F
crates/space3d-stories/src/engine/{materials.rs, lighting.rs} | post.rs | particles.rs | showcase.rs   P-MAT | P-POST | P-PART | I
crates/space3d-stories/tests/__screenshots__/Engine/{Frame | Materials, Environment, Lights/Directional_Shadow | Post | Particles | Showcase}/   F | P-MAT | P-POST | P-PART | I
crates/space3d/{Cargo.toml, src/lib.rs, tests/api_compat_249c874.rs, tests/prelude_policy.rs}   F
crates/space3d-editor/src/components.rs                                            F (F0 fix) → I (reprs)
examples/{pbr_showcase | post_processing | particles, particles_stress | graphics_showcase}/  P-MAT | P-POST | P-PART | I
docs/graphics/{api.md, frame.md} F; materials.md P-MAT; post.md P-POST; particles.md P-PART; migration.md I
docs/graphics.md, README.md, docs/{performance,platforms,storybook}.md             I
```

---

## 4. Downlevel, headless, determinism, performance

### 4.1 Downlevel / WebGL2 path per feature

Backend classes:
* **A:** native Vulkan/Metal/DX12 and browser WebGPU.
* **B:** WebGL2/GLES 3.0 with `EXT_color_buffer_float`. ANGLE on macOS offers this, and so does most desktop
  WebGL2.
* **C:** WebGL2 without float color buffers. wgpu-hal also treats WebGL2 that exposes only the unprefixed
  `EXT_color_buffer_half_float` as class C (documented; upstream patch candidate).
* **D:** GLES 3.1+ Android: compute, but no `BASE_VERTEX` below 3.2.

| feature | A | B | C | D |
|---|---|---|---|---|
| HDR scene (`Rgba16Float`) | ✓ | ✓ (viewport-rect clear colors > 1 clamp: GL blend constants; full-target clears exact) | Ldr fallback: `Rgba8UnormSrgb`, `Tonemapped` encoding, CPU-tonemapped clears | ✓ if float renderable |
| compact HDR (`Rg11b10Ufloat`) | with `RG11B10UFLOAT_RENDERABLE` | ✓ (float renderable) | — | as B |
| manual exposure + tonemap | composite | composite | per fragment | composite |
| auto exposure | compute histogram if `compute_histogram`, else render-pass chain | render-pass log-lum chain (7 passes) | off (warned once) | compute if `compute_histogram` |
| bloom | `Rg11b10`/`Rgba16Float` chain; level i sampled while writing i±1 (per-level views, like `generate_mips`) | ✓ | off (warned) | ✓ |
| FXAA, vignette, CA, grain, grading, LUT, dither | ✓ | ✓ | ✓ (composite reads the LDR `S`) | ✓ |
| distortion (DB `Rgba8Unorm`) | ✓, occluded by depth | ✓, occluded at 1×; unoccluded with MSAA | same as B | ✓ |
| soft particles / scene depth | §1.6 RODS rows | §1.6 GL rows (sampled, manual test; MSAA → fade off) | same as B | GL rows |
| particles simulation | GPU (compute + indirect) | CPU backend (8k wasm / 16k native budget) | CPU | GPU (no base vertex needed) |
| ribbons/beams | CPU-built segments (all classes) | ✓ | ✓ | ✓ |
| material maps, blend modes, alpha mask | ✓ (A2C with MSAA) | ✓ (9 of 16 texture units) | ✓ | ✓ |
| tangents / uv1 ext stream | `Snorm16x4`/`Float32x2`; rebinding slot 2 without `BASE_VERTEX` | ✓ | ✓ | ✓ |
| skybox | cube view | 6-layer square texture = `TEXTURE_CUBE_MAP` | ✓ | ✓ |
| IBL bake | fragment passes → `Rgba16Float` | ✓ | → `Rgba8UnormSrgb` (HDR sources CPU-Reinhard-tonemapped, warned) | ✓ |
| shadows | `Depth32Float`, compare sampler, depth view in GPU culling | `sampler2DShadow` + `textureLod`; CPU culling | ✓ | ✓ |
| GPU timings | `TIMESTAMP_QUERY` | — | — | if offered |
| storage buffers in vertex stages | **never used** by any feature | — | — | — |

Test coverage on the dev Mac:
* **A** = Metal (default).
* **B** = ANGLE (GLES 3.0 via Electron's `libEGL.dylib`, §5.0).
* **C** is forced with `FrameSettings::downlevel.no_float_targets` on Metal and on ANGLE.
* The **D** / GL paths are forced with `no_compute` + `no_read_only_depth` + `RenderSettings::draw_path =
  CpuDirect`.
* `DownlevelOverride::webgl2()` turns them all on at once. Every area's parity tests run under it.

### 4.2 Headless rules

1. Every feature renders headless with `RenderPlugin::headless` / `render_to_image` / `TestApp::screenshot`. No
   code path requires a surface.
2. Headless renderers finish all pending IBL bakes in the frame they are requested
   (`bake_passes_per_frame` is ignored). The first `render_to_image` is therefore final.
3. Particles replay pending ticks inside `RenderNode::prepare` (called from `render_frame`), so render-on-demand
   stories see exactly `n` ticks.
4. Auto exposure snaps on a camera's first post frame. A one-shot capture is correctly exposed.
5. Examples honour background mode (`SPACE3D_BACKGROUND`, `CI`, `CLAUDECODE`,
   `SPACE3D_EXIT_AFTER_FRAMES/_SECS`) through the platform runner, and `SPACE3D_HEADLESS=1` writes one PNG and
   exits. Tests never open windows. Builders run everything headless (AGENTS rule 7).

### 4.3 Determinism rules

* **Order:**
  * passes follow the sorted camera list, registration sequence numbers and settings-camera order (no hash-map
    iteration decides order);
  * pooled textures are handed out in request order;
  * no atomics decide positions (particles: prefix sums and masks; histogram: integer atomics with a fixed-order
    reduction).
* **Time:** render time is `(tick + alpha) × fixed_delta`. `FrameInfo::delta` is 0 when `Time::frame` did not
  advance since the renderer's previous frame. Consequences:
  * `render_to_image` twice gives identical images;
  * render-every-tick equals render-on-demand for particles.
* **Noise:**
  * dither: `pcg(0x9E3779B9 ^ FrameInfo::frame)`;
  * grain: `animated ? pcg(seed ^ frame) : seed`;
  * particles: PCG keyed by (emitter seed, spawn index);
  * IBL: fixed Hammersley sets.
  The renderer's internal `frame_index` (GC, readback) never feeds pixels (fixes frame.rs:918).
* **Replays** (`FrameKind::CaptureReplay`, `PostGroupInput::replay`) never advance temporal state: no exposure
  adaptation, no particle simulation, no grain advance.
* **Guarantee:** same machine + same inputs ⇒ identical `capture()` in every mode, draw path and backend. Across
  GPUs, goldens use per-backend variants (`SPACE3D_SNAPSHOT_VARIANT=backend`) or
  `CompareOptions::perceptual()` where documented.

### 4.4 Performance budgets

**Tiers.** All figures are at 1080p and 60 Hz, with a frame budget of 16.7 ms.
* **T1:** desktop dGPU (RTX 3060 / RX 6600), native.
* **T2:** Apple M1 (TBDR, 68 GB/s), Metal.
* **T3:** WebGL2 on Iris Xe / Adreno 6xx, at render scale 0.75 (1440×810), `GraphicsQuality::Low`.
* **Dev:** the build machine (Apple M4 Max, Metal). All CI gates are measured here. `SPACE3D_PERF_BUDGET_SCALE`
  relaxes them on slower runners.

**Reference scene "Battle"** (Phase I builds it as a bench and as the `Engine/Showcase/Battle` story at reduced
size):
* 300 ships: 3 procedural hull meshes, ORM + normal + emissive maps, instanced, about 40 draws;
* a nebula skybox with IBL; one shadowed sun; 20 point lights (explosion flashes);
* 100k particles (T3: 8k CPU backend), 64 ribbons, 16 beams, 2 distortion shockwaves;
* bloom; MSAA 4× on T1/T2 and FXAA Low on T3;
* two 256² RT previews; an egui HUD (windowed runs only).

**GPU budget per phase** (design targets per tier; `GpuTimings` labels in brackets):

| phase | T1 | T2 | T3 (Low) | owner |
|---|---|---|---|---|
| culling compute [`cull`] | 0.10 | 0.20 | — (CPU) | F |
| particle sim + compaction [`particles.sim`, `particles.compact`] | 0.20 | 0.40 | — (CPU) | P-PART |
| IBL bake (one-time, not per frame) [`ibl.bake`] | ≤ 3 | ≤ 8 | ≤ 50 total, ≤ 8 per frame | P-MAT |
| shadow map [`shadow`] (2048 / 2048 / 1024 PCF4) | 0.40 | 0.80 | 1.50 | P-MAT |
| scene Opaque, maps, IBL, PCF [`scene`] | 2.20 | 3.60 | 3.80 | P-MAT |
| Skybox (inside `scene`) | 0.10 | 0.20 | 0.30 | P-MAT |
| Transparent incl. particles/ribbons (hard) | 1.20 | 2.20 | 2.00 | P-MAT/P-PART |
| depth resolve + SceneRead pass [`depth_resolve`, `scene_read`] | 0.30 | 0.60 | 0.50 | F/P-PART |
| Distortion writers [`distortion`] | 0.15 | 0.30 | 0.40 | P-POST/P-PART |
| HdrScene overlays [`overlays.hdr`] | 0.20 | 0.40 | 0.30 | game |
| auto exposure [`post.exposure`] | 0.05 | 0.15 | off | P-POST |
| bloom [`post.bloom`] (6/540, 6/540, 4/270) | 0.35 | 0.90 | 0.90 | P-POST |
| composite [`post.composite`] | 0.12 | 0.40 | 0.50 | P-POST |
| FXAA [`post.fxaa`] | — | — | 0.40 | P-POST |
| AfterTonemap overlays / egui [`overlays`] | 0.30 | 0.50 | 0.80 | ui |
| 2 RT previews | 0.20 | 0.40 | 0.50 | — |
| **total** | **≈ 5.8** | **≈ 11.0** | **≈ 12.8 (≤ 14 ceiling)** | |

Frame-owned overhead (F): resolves, SceneRead load/store, composite plumbing and the extra pass switches. It is
≤ 0.35 ms on T1, ≤ 1.0 ms on T2 and ≤ 1.2 ms on T3. It is **zero in Direct mode with no new features**.

**Dev-machine gates** (CI, `benches/*` + `tests/perf_*.rs`, GPU timestamps, 1080p headless; they skip without
`TIMESTAMP_QUERY`). These are initial gates. A builder records the measured value in `docs/performance.md`, and a
gate may be relaxed only with the measurement and a reason. GPU figures are for the loaded GPU: frames back to back,
as the benches run them. Isolated frames let the GPU drop to its lowest clocks, where every pass costs about 3×
more. Record those figures for reference; they are not the gate.

| gate | budget (M4 Max) |
|---|---|
| Direct mode, existing `benches/instancing.rs` | CPU and GPU within ±5 % of the F0 baseline |
| Hdr minimal (composite only) over Direct | ≤ +0.10 ms GPU, ≤ +20 µs CPU |
| bloom 6 mips @1080p | ≤ 0.20 ms |
| all post on (AE compute, bloom, FXAA, CA, grain, grading, LUT, distortion) | ≤ 0.60 ms |
| particles: 100k alive, sim + compaction per tick | ≤ 0.06 ms |
| particles: draw 100k billboards (avg 12 px, overdraw 2) | ≤ 0.40 ms |
| particles CPU backend, 16k, single thread | ≤ 0.35 ms CPU |
| ECS 100k static PBR entities, extraction | ≤ 2.5 ms CPU (today 2.22) |
| shadow 2048², 100k instanced casters | ≤ 0.30 ms |
| forward full-screen lit pixels, all maps + IBL + PCF9, vs today's shader | ≤ +0.20 ms |
| Battle scene total GPU | ≤ 3.0 ms; CPU `render_frame` ≤ 1.5 ms |

*(Amended at the M2 review, measured on Dev, `docs/performance.md` "M2 review".)*
* **Direct mode, `benches/instancing.rs`**: met for explicit instance buffers; the ECS path of *moving*
  entities (`extract_upload/ecs/100k`) is 14–16 % above F0 (≈ 0.25 ms per 100k entities), bisected to the
  materials merge. The cause is the `Material` component this spec extends (§2.7: +12 fields): 64 → 160 bytes
  per entity, read in full every frame to intern it (`same_material` must compare every field), about 10 MB
  more memory traffic per frame at 100k. Relaxed for that path to "within +20 % of F0" with this measurement;
  static and explicit-buffer paths keep ±5 %. Games with 100k+ moving instances use `InstanceBuffers`.
* **CPU sub-budgets of the Battle** (below): the top-line `render_frame` ≤ 1.5 ms is met, but particles
  `prepare` (≈ 0.5 ms of the node, ≈ 1 ms with its uploads: CPU-built ribbon and beam strips, 2.3 MB a ticking
  frame) and post (≈ 0.38 ms: 11+ render passes of bloom) are over their 0.1 / 0.05 ms lines. Recorded as not
  met; the levers are a GPU strip builder with incremental uploads (particles) and the compute bloom chain
  (post), both deferred (§6.2).

**CPU budgets.**
* Native render stage for Battle ≤ 1.5 ms on Dev:
  * frame plan and passes ≤ 0.15 ms;
  * post ≤ 0.05 ms;
  * particles `prepare` (GPU backend) ≤ 0.1 ms;
  * lighting (no bake) ≤ 0.1 ms;
  * extraction ≤ 0.5 ms.
* Particles `tick_system`: ≤ 60 µs per tick at 500 layer instances.
* **wasm** (single thread, Chrome, mid laptop; measured manually with `examples/graphics_showcase --web` in
  Phase I, not a CI gate):
  * `render_frame` ≤ 4 ms;
  * particles tick + CPU sim at 8k ≤ 1.5 ms;
  * total frame CPU ≤ 10 ms.

**VRAM** (graphics-owned, 1080p):
* T1/T2 ≤ 220 MB:
  * `S` 16.6 + `S_msaa` 66 + depth 4× 33 + `depth_1x` 8.3;
  * bloom 2.8 + DB 2.1;
  * shadow 16.8;
  * IBL 4.3 + sky 1024² faces 33;
  * particles 262k pool 23.
* T3 ≤ 48 MB: `S` 9.3, depth 4.7, `L` 4.7, bloom 0.7, DB 0.6, shadow 4.2, IBL 128 ≈ 1, sky 512² ≈ 8.4, CPU
  particles < 1.
* `RenderStats::target_bytes` reports frame and post targets. `benches/graphics.rs` asserts the T1 figure on Dev.
  *(Amended at the M2 review.)* `target_bytes` now also counts the directional shadow map, and
  `RenderStats::lighting_bytes` the lighting area's cubemaps (sky, IBL source) and baked environments, every
  mip. The Battle at High, 1080p: 146 MB of targets and shadow + 84 MB of sky and IBL = 230 MB of textures
  (+ the 23 MB particle pool): **over the T1/T2 line by the sky's format** — the 1024² HDR sky is stored as
  `Rgba16Float` with mips (67 MB), where the line above budgets 33 MB (a 4-byte format). Lever: HDR cubemaps
  in `Rg11b10Ufloat` where renderable (deferred, §6.2). `battle_steady_state` pins both parts.
  *(Amended at the M2 review, round 1.)* The lever landed: HDR radiance cubes are `Rg11b10Ufloat` where it
  renders and filters (`GraphicsCaps::compact_hdr_format`), else `Rgba16Float`. The Battle at High, 1080p, on
  Dev: 146.0 MB of targets and shadow + 50.4 MB of sky (33.5 MB) and IBL = 196.4 MB of textures, 219.4 MB with
  the 23 MB particle pool: **met**. `battle_steady_state` asserts the line where the compact format exists and
  pins the parts elsewhere (GL: the sky stays `Rgba16Float`, 83.9 MB of sky and IBL).
* *(Amended at M1.1, measured on Dev: `docs/performance.md` "F7 / F8 and M1.1".)* The figures above are textures.
  With the half-rect prefilter the bloom chain's textures are 2.8 MB at 1080p (`Rg11b10Ufloat`), 11.1 MB at 4K
  and, for T3's Low chain at 1440 × 810, 1.9 MB (`Rg11b10Ufloat`) or 3.7 MB (`Rgba16Float`, the format ANGLE and
  WebGL2 pick) instead of 0.7. On top of textures, every render pass holds GPU driver memory while its frame is
  in flight — on Metal about 2.8 MB per pass per frame in flight, whatever its size: bloom's 11–12 passes add
  about 33 MB per frame in flight (+60 MB with one, +148 MB with three frames in flight at 1080p, against
  2.7 MB of `target_bytes`), the composite and a shadow map's pass about 2.8 MB each. Budgets count
  `target_bytes + render passes × 2.8 MB × frames in flight` on Apple GPUs (two to three frames in flight with
  a window); `tests/footprint.rs` checks the bloom share (≤ 80 MB paced, ≤ 200 MB unpaced). The T1/T2 line is
  therefore met for textures (≈ 220 MB) but not for process memory with bloom on Apple GPUs (+100–150 MB per
  post group) until the compute chain (one compute pass) lands.

**Passes and wgpu allocations.**
* Battle frame: about 21 render passes (shadow 1, scene 1, resolve 1, SceneRead 1, distortion 1, overlays-hdr 1,
  bloom 11, composite 1, overlays 1, RT 2) and ≤ 5 + 2/tick compute dispatches for particles.
* wgpu-hal/Metal allocates about 40 times per render pass (ARCHITECTURE "Allocation"). Each new mode gets its own
  measured bound (+10 %) in `tests/alloc*.rs`, with the same "no growth / < 256 KiB retained" checks. **Our code:
  0.**

**Steady-state invariants** (asserted in each area's tests):
* `cpu_reallocations == 0`, `textures_created == 0`, `ParticleStats::cpu_reallocations == 0` after warm-up;
* static scene uploads 0 B;
* animated post adds ≤ 16 B per post group;
* bind groups, views and pipelines are rebuilt only on key or generation change.

**`GraphicsQuality` presets** (Phase I implements; values normative):

| knob | Low (WebGL2 / C-class) | Medium (iGPU, Apple, mobile) | High (dGPU) |
|---|---|---|---|
| render scale (primary) | 0.75 | 1.0 | 1.0 |
| MSAA / AA | 1 + FXAA Low | 4 on TBDR, else 1 + FXAA Medium | 4 |
| bloom mips / max mip height | 4 / 270 | 6 / 540 | 7 / 720 |
| auto exposure | off | on | on |
| CA, grain | off | on if requested | on if requested |
| shadow map / filter | 1024 / Pcf4 | 2048 / Pcf9 | 2048 / Pcf16 |
| IBL specular / irradiance / samples / passes per frame | 128 / 16 / 32 / Some(6) | 256 / 32 / 64 / None | 512 / 32 / 128 / None |
| shading model | Classic | Standard | Standard |
| particles (`ParticleSettings::for_quality`) | CPU 8k (wasm) / 16k | GPU 65k (mobile) / 262k | GPU 262k |
| recommended sky face size (docs) | 512 | 1024 | 2048 |

*(Amended at the M2 review.)* Ultra (added in Phase I, opt-in) is High plus a 4096² Pcf16 shadow, a 256-sample
environment bake and bloom **8 / 720** (was 8 / 1080): a bloom level 0 is never larger than half the post
group's rect (`chain_size`; a bigger one was resampled up from the half-resolution prefilter for no detail),
so at 1080p High's 7 / 720 and Ultra run at 960 × 540 and Ultra is High plus one wider level everywhere.

---

## 5. Build plan (multi-agent, branch `graphics`)

### 5.0 Rules for every builder

**Branches and worktrees.**
* **Precondition for F0:** the render-fixes fix workflow has committed. `git -C
  /Users/rorychatt/git/spacecorps/Space3d-renderfix status --porcelain` must be empty, and the viewport-clear
  change (`clear.wgsl`, `Pipelines::viewport_clear`) must be in `HEAD`. Never copy uncommitted work. If the
  workflow is still running, poll (≤ 2 min per check) until it lands.
* `graphics` is created from that commit in a new worktree `/Users/rorychatt/git/spacecorps/Space3d-graphics`.
  P builders use branches `graphics-post`, `graphics-particles` and `graphics-materials`, each in its own worktree
  `Space3d-gfx-{post,particles,materials}`, created from the tag `graphics-F-core`.
* Commits end with the attribution lines given by the harness. Nothing is pushed unless the user asks.

**Ownership.**
* A builder edits only files it owns (§3.6 and the per-builder lists below).
* Everything else is read-only. Changes to F files, or to a frozen signature (§2, §2.6), go through the
  **foundation maintainer**. The F builder stays on during Phase P in that role: it applies change requests on
  `graphics`, additive only, and the requester merges `graphics` back.
* Requests are written to `scratchpad/graphics/change-requests.md`, one entry each with file, change and reason,
  or relayed by the orchestrator.

**Merging.**
* Each P milestone merges into `graphics` when green: merge `graphics` in, run the checks, then merge.
* Only `Cargo.lock` may conflict: regenerate it with `cargo update -w --offline`, or with plain `cargo` if a new
  dependency is needed.
* Golden directories are disjoint per area.

**Checks per milestone** (all must pass; keep each command under about 2 minutes; wrap long ones with
`perl -e 'alarm shift @ARGV; exec @ARGV' 110 <cmd>`, scope with `-p`/`--test`, and run full suites in the
background and poll):

```sh
cargo fmt --all -- --check
cargo clippy -p <crate> --all-targets -- -D warnings          # workspace-wide before each merge
cargo test -p <crate> [--test <name>]
cargo check -p <crate> --target wasm32-unknown-unknown         # every runtime crate touched
# WebGL2-class GL (ANGLE, GLES 3.0) on this Mac:
export ANGLE_LIB="/Applications/Antigravity IDE.app/Contents/Frameworks/Electron Framework.framework/Versions/A/Libraries"
DYLD_FALLBACK_LIBRARY_PATH="$ANGLE_LIB" cargo test -p space3d-render --features angle --test gl -- --test-threads=1
DYLD_FALLBACK_LIBRARY_PATH="$ANGLE_LIB" WGPU_BACKEND=gl cargo test -p <crate> --features space3d-render/angle --test <t> -- --test-threads=1
# goldens: first run writes missing goldens (not in CI). Inspect every new PNG (Read it) before committing.
cargo test -p space3d-stories --test storybook
SPACE3D_UPDATE_SNAPSHOTS=1 cargo test -p space3d-stories --test storybook            # re-bless, intentional only
SPACE3D_UPDATE_SNAPSHOTS=1 SPACE3D_SNAPSHOT_VARIANT=backend WGPU_BACKEND=gl ...      # GL variants
```

**Definition of done per milestone.**
* The acceptance tests listed below pass.
* There are no GPU errors (`gpu_error_count() == 0` in new tests).
* The steady-state invariants hold (§4.4).
* The docs section is written and the stories and goldens are committed.
* `DownlevelOverride::webgl2()` parity runs pass on Metal.
* Where GL applies, ANGLE runs pass.
* The wasm32 check passes.
* Nothing takes focus. Examples run with `SPACE3D_EXIT_AFTER_FRAMES=3` in background mode.

### 5.1 Phase F — foundation (single builder; then foundation maintainer)

F-core (F0–F6) gates Phase P. F7 is due before P-MAT PM5; F8 before Phase I.

**F0 — Baseline.**
* Create `graphics`, then merge `main` into it:
  * `overlay.rs` uses `SurfaceState::view_format`;
  * `encode_frame_overlays` runs after `encode_views`, and the capture overlays after the replay;
  * space3d-ui, test, stories, editor, CLI and facade come in.
* Fix the editor's exhaustive `Material` literal (`space3d-editor/src/components.rs:148`) to FRU.
* Run the engine storybook. `main`'s goldens were produced before render-fixes' changes. Where the merged
  renderer differs, re-bless on Dev after inspecting every diff (only render-fixes' known fixes may explain
  them). The result is the **new baseline** for every "bit-identical" claim. Commit it separately with the
  reason.
* Add `crates/space3d/tests/api_compat_249c874.rs` (§2.7).
* Record baseline numbers (`benches/instancing.rs`, alloc bounds, storybook timings) in `docs/performance.md`
  (F section).
* *Accept:*
  * the workspace test, clippy and fmt pass;
  * the compat fixture compiles;
  * `tests/overlay.rs` and the space3d-ui tests pass on the merged renderer;
  * wasm32 checks pass for render, ui and the facade.

**F1 — Mechanical split** (no behaviour change).
* `frame.rs` becomes `frame/{mod, plan, views, targets, passes, extract, draws, tests}.rs`. `plan` and `targets`
  start near-empty.
* `pipeline.rs` becomes `pipeline/{mod, view, forward, instance}.rs`.
* `space3d-stories/src/engine.rs` becomes `engine/{mod, basics, frame, materials, lighting, post, particles,
  showcase}.rs`, the new ones returning `vec![]`.
* *Accept:*
  * every existing test passes unchanged;
  * engine goldens are **exact** (tolerance 0 vs the F0 baseline);
  * alloc bounds are unchanged;
  * `benches/instancing.rs` is within noise.

**F2 — API freeze** (everything in §2, compiled).
* Every public type and method of §2.2–§2.5, with docs.
* Crate-internal contracts (§2.6) with stub bodies tagged `// STUB(P-XXX)`. Stubs return neutral values and never
  panic.
* Hubs and `lib.rs` exports; the prelude additions (§2.2.10).
* `space3d-core::HdrImageData`.
* The `space3d-particles` crate skeleton: Cargo.toml, lib.rs, a `ParticlesPlugin` that registers nothing yet, and
  a `prelude`.
* The facade `particles` feature.
* The stories `particles` feature and module wiring.
* Assets `render` feature: the empty module and the `space3d-render` dependency.
* `prelude_policy.rs` with the game-name fixture.
* `docs/graphics/api.md` (the semver policy of §2.7, and the ownership map of §3.6).
* *Accept:*
  * `cargo doc -p space3d-render -p space3d-particles --no-deps` is warning-free;
  * the compat and prelude tests pass;
  * every existing test and golden is exact;
  * wasm32 checks pass for render, particles, assets and the facade (with `--features particles`).

**F3 — Output tail, `GpuView`, keys.**
* `//#VIEW` and `//#OUTPUT` markers, `shaders/common/{view,output}.wgsl`, and the overrides of §3.1.
* `PipelineKey` and `ModuleKey` in their final shape (§3.5).
* The `GpuView` extension (§3.3), with F-owned fields filled: matrices, viewport, target_size, depth, output, exposure.
* `Tonemapper::apply` CPU mirrors.
* Point-light selection per view (D19).
* `TEXTURE_BINDING` on every depth texture.
* *Accept:*
  * engine goldens are **exact**;
  * `gpu_view_matches_wgsl`;
  * `tonemapper_cpu_mirror_matches_gpu` (unlit quads 0.05…64 through a Tonemapped-encoding test pipeline, within
    2/255, for each operator);
  * `custom_shaders_compile_against_every_permutation`;
  * `point_lights_over_16_pick_nearest_per_view`, and ≤ 16 exact;
  * the static-scene 0-byte upload test still passes.

**F4 — Frame plan, HDR, composite** (priority #2 foundation).
* `GraphicsCaps` (+ `DownlevelOverride` masking).
* Plan: target groups, modes, lead camera, post groups, scene/display roles (§1.3).
* Group textures and `TexturePool` (key change → reallocate at frame start only).
* Scene passes into `S` with the viewport-clear rules applied to `S`.
* The F composite: `composite.wgsl` with the final bind-group layout, dummies for B0/E/LUT/DB/L, and working
  manual exposure, tonemap, dither, alpha/transparent handling, rect mapping and upscale.
* The finish order of §1.4.
* The Ldr fallback: `Tonemapped` encoding and CPU-tonemapped clears.
* Passthrough render scale.
* HDR RTs (Direct into `Rgba16Float`) and RT MSAA (`RenderTargetDesc::samples`).
* Windowed-capture replay for intermediate groups (§1.10); `capture_scene_hdr`.
* `FrameSettings::post`, `CameraFrame`, and the new `RenderStats` counters.
* Stories:
  * `Engine/Frame/HDR Tonemapping`: emissive ramp 0.05…64; knobs tonemapper, exposure, `HdrMode`;
  * `Engine/Frame/Render Scale`;
  * `Engine/Frame/Multi Camera`: split-screen post, PiP, gizmo display view, RT preview with a transparent clear;
  * `Engine/Frame/LDR Fallback`: forced `no_float_targets`.
* *Accept* (`tests/frame_modes.rs`, `tests/capture_hdr.rs`):
  * **Defaults and exactness:**
    * `defaults_are_direct_and_exact` (goldens, tolerance 0);
    * `passthrough_postprocess_matches_direct` (`PostProcess::passthrough()`, within 1/255, dither off);
    * `hdr_values_survive_until_post` (`capture_scene_hdr` of an emissive 8.0 ≈ 8.0 ± 1 %).
  * **Tonemapping and exposure:**
    * `agx_and_aces_are_monotonic_below_one`;
    * `exposure_stops_are_linear`;
    * `ldr_fallback_matches_hdr_perceptually` (`CompareOptions::perceptual()`).
  * **Alpha:**
    * `rt_transparent_clear_keeps_alpha` (alpha 0 stays 0; dither weighted by alpha; the bloom half of this
      rule is tested in PO1);
    * `compact_hdr_only_on_opaque_primary`.
  * **Cameras and targets:**
    * `split_screen_post_groups_do_not_bleed`;
    * `gizmo_camera_is_display_view` (not tonemapped);
    * `pip_post_camera_overwrites_rect`;
    * `render_scale_half_upscales`;
    * `hdr_render_target_is_linear_float`;
    * `rt_msaa_resolves`.
  * **Capture and determinism:**
    * `render_to_image_twice_identical` (dither on);
    * `replay_path_equals_frame_output` (a `#[cfg(test)]` hook runs the intermediate replay into the offscreen
      target and compares with the frame);
    * `resize_is_lazy`.
  * **Steady state:** 0 `cpu_reallocations` and 0 `textures_created` after warm-up in Hdr, Ldr and passthrough;
    static scene uploads 0 B.
  * **Budgets and backends:**
    * perf gate "Hdr minimal" (§4.4);
    * the ANGLE `gl.rs` scenes in Hdr and in forced Ldr;
    * wasm32.

**F5 — Render nodes** (unblocks particles).
* `RenderNodes`, the snapshot cache and per-phase sorted slot lists (rebuilt only on generation change).
* Contexts, `SlotMask`, `prewarm` calls, `MeshRef`/`TextureRef`, `FrameInfo` (the delta rule, the renderer id,
  device lost).
* SceneRead variants (§1.6) including the depth resolve pipeline; the in-pass fallback routing.
* Depth store only when needed.
* `FrameLayouts` (group 0 = the forward view layout; group 1 scene) and the `scene_wgsl` variants.
* The Distortion phase plumbing: `DB` from `PostState::distortion_target` (the F stub creates and clears it), the
  writer pass after the settings camera's scene/SceneRead, the `distortion` input to `PostGroupInput`.
* Replays call `draw` with `CaptureReplay` and never `prepare`.
* `examples/frame_nodes.rs`.
* Stories:
  * `Engine/Frame/Node Phases`: a colored node per phase and order, around the built-ins;
  * `Engine/Frame/Soft Depth`: node fade vs an analytic plane; knobs MSAA 1/4 and `no_read_only_depth`.
* *Accept* (`tests/nodes.rs`):
  * **Ordering and scheduling:**
    * `slot_order_and_builtins_first`;
    * `slot_mask_skips_passes` (no SceneRead pass, no depth store, `scene_read_passes == 0`);
    * `prepare_once_per_frame_not_in_replay`;
    * `draw_context_reports_slot`.
  * **Scene depth, one test per §1.6 row:**
    * `scene_depth_matches_analytic_plane_rods_1x`, `_rods_msaa_single_view`, `_gl_manual` (forced), all within
      ±1 %;
    * `multi_view_msaa_uses_in_pass_fallback`.
  * **Distortion plumbing:** `distortion_phase_only_with_post_and_work`, verified with a test node writing a
    constant into DB and a stub composite readback of DB.
  * **Lifecycle:** `node_sees_new_renderer_id` (a second renderer), `poisoned_node_does_not_crash`.
  * **Allocation:** alloc case "nodes" in `tests/alloc_frame.rs`, 0 in our code.
  * **Backends:** ANGLE runs of the soft-depth scene (manual path); wasm32.

**F6 — Overlay stages.**
* `stage()`, `scene_reads()` and `prepare_scene()` wired for both modes (§1.5).
* `OverlayScene` values.
* `OverlayTarget::{shader_encodes_srgb, is_hdr}`.
* The HdrScene depth request keeps the last primary view's depth stored.
* Capture semantics.
* Story `Engine/Frame/Overlay Stages`: the same additive sprites via HdrScene vs AfterTonemap, with post on.
* *Accept* (`tests/overlay_stages.rs`):
  * `legacy_overlay_unchanged_direct_and_hdr` (a verbatim 249c874 overlay gives exact pixels after tonemap);
  * `hdr_scene_overlay_format_and_tonemap` (`Rgba16Float`; a 4.0 write equals `AgX(4.0)` ± 2/255);
  * `hdr_scene_overlay_in_direct_runs_before_after_tonemap`;
  * `hdr_scene_overlay_output_tonemapped_in_ldr`;
  * `no_primary_view_skips_hdr_scene`;
  * the space3d-ui egui tests and `tests/overlay.rs` unchanged.
* **Tag `graphics-F-core`.**

**F7 — Depth views (shadow plumbing).**
* `DepthViewRequest` from `Lighting::depth_views`.
* Culling integration: frustum slots and jobs share the dispatch windows, so there are no extra dispatches;
  CPU culling on `CpuDirect`.
* The depth-only pass (Clear 1.0, Store) before camera views.
* `PassKind::Depth`/`DepthMask` pipelines (vertex-only minimal `depth.wgsl` with `//#INSTANCE`).
* The `source_casts` filter.
* The `GpuView` slot for the light.
* `RenderStats::{depth_views, shadow_draw_calls, shadow_casters}`.
* Depth views are never replayed.
* *Accept* (`tests/depth_views.rs`, via a `#[cfg(test)]` request injector; *amended at F7:* integration
  tests cannot see `#[cfg(test)]` items, so the injector is the doc-hidden `space3d_render::testing`
  module, like `space3d_particles::testing`):
  * `depth_view_renders_caster_depth` on every draw path;
  * `depth_views_add_no_dispatches_on_gpu_paths`;
  * `depth_view_culls_offscreen_casters`;
  * alloc bound;
  * ANGLE.

**F8 — Timing, alloc, docs.**
* `GpuTimings`: one query set of 256, a 3-deep readback ring, labels per §4.4, and `Prepare/ViewPrepare
  *_timestamps` wired. Pass-level `timestamp_writes` only.
* Alloc bounds for Direct, Hdr, Ldr and nodes.
* `benches/frame.rs`.
* `docs/graphics/frame.md`, and the ARCHITECTURE "Frame" chapter (diagram §1.4, modes, depth matrix, overlays,
  budgets).
* *Accept:*
  * `gpu_timings_report_scopes` (skips without `TIMESTAMP_QUERY`);
  * no allocation from the timing ring after warm-up;
  * docs reviewed against the code.

### 5.2 Phase P — three parallel builders (disjoint files)

#### P-POST — post effects (owner of `src/post/**`, `src/shaders/post/**`, `tests/{post,post_distortion,gl_post,alloc_post}.rs`, `tests/common/post.rs`, `benches/post.rs`, `space3d-stories/src/engine/post.rs`, `__screenshots__/Engine/Post/`, `examples/post_processing/`, `docs/graphics/post.md`)

Implements design-post §3–§5 under §1.9 and §2.2.7 of this spec. Milestones, in priority order:

1. **PO1 Bloom** (priority #2):
   * mip sizing, the soft-knee prefilter in exposed units with Karis, 13-tap down and tent up with the `scatter`
     blend constant, energy-conserving mode;
   * pooled chain in `bloom_format`, per-mip views from `TexturePool`;
   * the composite adds bloom after exposure.
   * Story `Engine/Post/Bloom`.
   * *Accept:*
     * `bloom_only_above_threshold` (8.0 vs 0.8 quads);
     * `energy_conserving_preserves_mean` (±2 %);
     * `bloom_does_not_bleed_across_viewports`;
     * `hdr_scene_overlays_feed_bloom`;
     * `bloom_off_in_ldr_fallback`;
     * `transparent_rt_bloom_weighted_by_alpha` (no haze where alpha is 0);
     * alloc and 0-texture steady state;
     * gate "bloom 6 mips ≤ 0.20 ms";
     * ANGLE (EXT_color_buffer_float);
     * wasm.
2. **PO2 Auto exposure:**
   * compute histogram only when `GraphicsCaps::compute_histogram` (256 invocations, 1 KiB workgroup storage);
     otherwise the render-pass log-lum chain;
   * `E` per camera, the `snap` rules, `camera_exposure` readback, `reset_post_history`, and `dt` from
     `FrameInfo::delta`.
   * Story `Engine/Post/Auto Exposure` (180 ticks).
   * *Accept:*
     * `auto_exposure_converges_and_matches_reference` (±0.1 EV compute; ±0.5 EV downlevel via
       `no_compute`);
     * `render_to_image_is_idempotent_with_ae_and_grain`;
     * `instant_converges_frame_1`;
     * GL downlevel run.
3. **PO3 FXAA, vignette, CA, film grain:**
   * FXAA writes through `L`;
   * grain and dither are applied last (in FXAA when FXAA is on).
   * Stories `Engine/Post/FXAA`, `Engine/Post/Lens` (grain `animated: false`, fixed seed).
   * *Accept:*
     * `fxaa_softens_edges`;
     * `film_grain_is_seeded`;
     * `after_tonemap_overlays_untouched_by_grain_ca_vignette`;
     * `grain_after_fxaa`.
4. **PO4 Color grading, LUT, `.cube`, WGSL hooks** (async validation like custom shaders). Story
   `Engine/Post/Color Grading`.
   * *Accept:*
     * `lut_identity_is_neutral`, `lut_changes_colors`;
     * `cube_parser_*`;
     * `invalid_grading_hook_falls_back`.
5. **PO5 Distortion:**
   * composite apply: offset, chromatic split, bloom lookup offset, rect clamp;
   * `DISTORTION_WGSL` final;
   * `DistortionMaterial` node (`space3d/distortion-meshes`, public node API only: `MeshRef`, CPU frustum culling,
     staging uploads, `MESH_VERTEX_LAYOUT`, WebGL2 base-vertex rebinding).
   * Story `Engine/Post/Distortion`.
   * *Accept:*
     * `distortion_shifts_the_background` (± 1 px);
     * `distortion_occluded_by_closer_geometry` (RODS and forced GL);
     * `hdr_scene_overlays_are_distorted`;
     * `no_distortion_pass_without_writers`.
6. **PO6 Hardening:**
   * GPU timings under `post.*`, `benches/post.rs`, the gate "all post ≤ 0.60 ms";
   * `alloc_post.rs` (all effects);
   * `resize_reallocates_once_and_stays_bounded`;
   * device loss degrades;
   * `PostProcess::with_quality`;
   * `examples/post_processing` (keys toggle effects; headless PNG);
   * story `Engine/Post/Tonemappers` (4 viewports).
   * `docs/graphics/post.md`.

#### P-PART — particles (owner of `crates/space3d-particles/**`, `space3d-stories/src/engine/particles.rs`, `__screenshots__/Engine/Particles/`, `examples/particles/`, `examples/particles_stress/`, `docs/graphics/particles.md`)

Implements design-particles §3–§13 with the §2.5 deltas. Milestones:

0. **PA0 Early lane** (may start at F2; crate-internal only): data model plus RON (every built-in parses and
   round-trips), `ParticleEffects` registry, `tick_system`, runtime (ring slots, tick log, catch-up window `W`,
   finish/orphan logic), CPU simulation in Rust, RNG parity vectors.
   * *Accept:* the unit tests of design-particles §13 (spawn scheduling, capacity bounds with proptest, ring
     mapping, log trimming, finish on the exact tick, world matrix through `Parent`); 0 allocations in
     `tick_system` at steady state; wasm32.
1. **PA1 CPU backend rendering + prefabs** (priority #1):
   * the `ParticlesNode` (Transparent slot reading `DEPTH`), sprite pipelines per `(slot, PassTarget)` built in
     `prewarm`;
   * analytic shapes, the LUT, facings, blend modes;
   * soft fade per `DepthAccess`; `s3d_output` + `direct_soft_clip`;
   * `SpawnEffect` / `EffectSpawn`, and the built-ins `EXPLOSION`, `HIT_SPARK` and `ENGINE_EXHAUST` (sprite
     layers).
   * Stories `Engine/Particles/Builtin/{Explosion, Hit Spark, Engine Exhaust}` and
     `Engine/Particles/Features/{Blend Modes, Soft Particles, Shapes, Curves, Facing, CPU Backend}`.
   * *Accept:*
     * `render.rs` (blend modes on a solid background, soft fade near a plane in all three `DepthAccess` modes,
       facings, local vs world);
     * `determinism.rs` (runs identical; render-every-tick == render-on-demand == catch-up reset);
     * `headless.rs` (`render_to_image`, `capture()`);
     * `stories_simulate_without_renderer`;
     * the ANGLE run of the stories on the CPU backend within tolerance;
     * 0 allocations;
     * wasm32.
2. **PA2 GPU backend:**
   * kernels simulate/emit/mark/scan/scatter (`@workgroup_size(64)`, ≤ 4 storage buffers);
   * stats readback, pool growth, budgets and priorities.
   * *Accept:*
     * `parity.rs` (identical alive sets and slots; positions within 1e-3);
     * `budget.rs` (refused/shrunk; the `TEST_LIMITS` pattern);
     * gates "100k sim + compaction ≤ 0.06 ms" and "draw 100k ≤ 0.40 ms";
     * `Stress/100k` (perf tag);
     * alloc budget (≤ 5 dispatches + 2 per extra tick).
3. **PA3 Ribbons, beams, flipbooks, forces, lights, library:**
   * ribbon/beam segments (watertight), `ParticleBeam`, flipbooks from arrays with the ≥ 2-layer rule,
     procedural `Fireball16`/`Smoke16`;
   * light flashes (`PointLight`, subject to D19), attractors, vortex, turbulence;
   * the remaining built-ins `SHIELD_RIPPLE`, `WARP_IN`, `WARP_OUT`, `LASER_TRAIL`, `LASER_BEAM`;
   * the `.pfx.ron` asset loader with hot reload; scene registration by name.
   * `examples/particles` and `examples/particles_stress` (background mode, `SPACE3D_EXIT_AFTER_FRAMES`).
   * *Accept:*
     * ribbon watertightness;
     * beam endpoints;
     * flipbook frame selection;
     * `hot_reload_structure_change_drains`;
     * stories for every built-in and feature.
4. **PA4 Distortion layers, sorting, LOD, calibration** (after PO5):
   * `SpriteTarget::Distortion` into `DB`;
   * `ByDistance` GPU sort (deterministic ties);
   * LOD/quality;
   * `ParticleSettings::for_quality`;
   * HDR intensity calibration against `Bloom::glow()`.
   * Stories `Engine/Particles/Features/{Distortion, Sorting, HDR Bloom}`.
   * `docs/graphics/particles.md`, including the migration map (additive.rs `Kind` → `SpriteShape`, `fx::Effect`
     → `builtin::*`, egui lasers → `LASER_BEAM`).

#### P-MAT — materials, glTF, skybox, IBL, shadows (owner of `frame/{extract,draws}.rs`, `pipeline/{forward,instance}.rs`, `arena.rs`, `assets.rs`, `texture.rs`, `handles.rs`, `material.rs`, `lighting/**`, `shaders/forward.wgsl` except the F regions, `shaders/{depth,sky,ibl_bake}.wgsl`, `brdf_lut.bin`, `crates/space3d-assets/**`, render tests/benches listed in §3.6, `space3d-stories/src/engine/{materials,lighting}.rs`, their goldens, `examples/pbr_showcase/`, `docs/graphics/materials.md`)

Implements design-materials §3–§8 with the §2.2.8/§2.2.9/§2.4 deltas. Milestones, in the request's priority
order:

1. **PM1 Blend modes and material maps:**
   * `BlendMode` (Additive, Premultiplied, AlphaMask with A2C under MSAA), phases and sort key with
     `render_order`;
   * texture sets (4 material BGLs, 1×1 defaults, 2-layer white arrays), ORM detection, sRGB fixup flags;
   * per-pixel derivative TBN, `s3d_` helpers;
   * `ShadingModel` override, `LightingSettings` (`TransparentSort::None` default), `add_image_linear`,
     `TextureSource::from_image_linear`.
   * Stories `Engine/Materials/{Blend Modes, Alpha Mask, Normal Maps, PBR Maps}`.
   * *Accept:*
     * `no_map_materials_render_bit_identical` (goldens exact);
     * `blend_modes_composite_exactly` (CPU-computed pixels, all five modes, over opaque and transparent
       backgrounds);
     * `alpha_mask_discards`, `alpha_to_coverage_with_msaa`;
     * `normal_map_matches_tilted_geometry` (derivative path), `orm_packed_equals_separate_maps`,
       `occlusion_only_darkens_ambient`, `emissive_map_times_factor`;
     * `transparent_sort_opt_in_only`;
     * `custom_shaders_compile_against_every_permutation`;
     * gate "ECS 100k PBR ≤ 2.5 ms";
     * ANGLE (9 texture units);
     * wasm.
2. **PM2 glTF "just lights up" + tangents:**
   * the new `MaterialData` fields and `ModelMesh::extras`, the loader (every slot, `texCoord`, `KHR_texture_transform`,
     `KHR_materials_emissive_strength`, `KHR_materials_unlit`, `TANGENT`, `TEXCOORD_1`);
   * `compute_mikktspace_tangents`;
   * the ext vertex stream (arena slot 2, lockstep growth, WebGL2 rebinding), `MeshExtras`, `add_mesh_with`;
   * the `render` bridge, including `apply_material_maps`.
   * Story `Engine/Materials/glTF Import` (in-memory glTF).
   * *Accept:*
     * `gltf_reads_all_material_slots`;
     * `orm_packed_detection`;
     * `material_from_data_color_spaces`;
     * `apply_material_maps_keeps_custom_factors`;
     * `mikktspace_splits_mirrored_uvs`;
     * `normal_map_matches_tilted_geometry` (vertex-tangent path);
     * `ext_stream_markers`;
     * `ext_disabled_past_10_instance_locations`;
     * multi-draw path parity with the ext stream.
3. **PM3 Cubemaps and skybox:**
   * `CubemapHandle`/`CubemapSource`, 6-face and equirect conversion, `HdrImageLoader`, `SkyboxLoader`;
   * the `space3d/skybox` built-in node (public node API; Skybox phase; `LessEqual` at the far plane).
   * Story `Engine/Environment/Skybox`.
   * *Accept:* `skybox_orientation_six_colors`, `skybox_intensity_hdr`, `threejs_face_conversion`,
     `cube_on_gl_is_texture_cube_map` (ANGLE).
4. **PM4 IBL:**
   * fragment-pass bake (radiance mips, GGX prefilter, irradiance), the embedded BRDF LUT;
   * `EnvironmentLight`, `environment_state`, per-camera view bind groups via `Lighting::view_resources`;
   * the LDR bake fallback.
   * Story `Engine/Environment/IBL Spheres`.
   * *Accept:*
     * `ibl_white_furnace` (±3 %);
     * `ibl_sliced_equals_immediate`;
     * `ibl_ldr_fallback_close_to_hdr` (`no_float_targets`);
     * `environment_state_transitions`;
     * `brdf_lut_matches_reference`;
     * the bake gate.
5. **PM5 Shadows** (needs F7):
   * `DirectionalShadow`, fitting (`top_down`, texel snap), depth views via `Lighting::depth_views`, `source_casts`;
   * PCF 1/4/9/16 with `S3D_SHADOW_TAPS`, biases.
   * Story `Engine/Lights/Directional Shadow`.
   * *Accept:*
     * `shadow_same_on_every_path`;
     * `offscreen_caster_still_casts`;
     * `no_acne_at_grazing_angle`;
     * `unlit_and_transparent_do_not_cast_by_default`;
     * `explicit_instance_buffers_cast`;
     * `static_scene_uploads_nothing_with_shadows`;
     * the shadow gate;
     * ANGLE (`sampler2DShadow`).
6. **PM6 Hardening:**
   * forward `Renderer::prewarm` support, `benches/materials.rs`, `alloc_materials.rs` (all features);
   * `examples/pbr_showcase`;
   * `docs/graphics/materials.md` (fields, color spaces, glTF table, blend and sort rules, MikkTSpace, WebGL2
     notes, the IBL + Classic note).

### 5.3 Phase I — integration (single builder, on `graphics`; owner of `quality.rs`, `engine/showcase.rs`, `Engine/Showcase/` goldens, `examples/graphics_showcase/`, `benches/graphics.rs`, `docs/graphics.md`, `docs/graphics/migration.md`, README, `docs/{performance,platforms,storybook}.md`, editor reprs)

1. **I1 Merge and verify.**
   * All P branches are in `graphics`.
   * Workspace fmt, clippy and test.
   * The whole engine storybook passes on Metal. Every golden mismatch caused by a merge is diagnosed; re-bless
     only when intentional, with the reason in the commit.
   * GL variants are blessed for stories that differ on ANGLE (`SPACE3D_SNAPSHOT_VARIANT=backend`).
   * The wasm32 check of `space3d` with `--features particles` and every runtime crate passes.
2. **I2 Presets and budgets.**
   * `GraphicsQuality` (§4.4 table), `ParticleSettings::for_quality`.
   * The prewarm plan (`PrewarmRequest` helpers per preset).
   * The **Battle** scene in `benches/graphics.rs` and `tests/perf_graphics.rs`: the Dev gates of §4.4 and the
     VRAM figure.
3. **I3 Showcase stories:**
   * `Engine/Showcase/SpaceCorps Look`: ship with maps, nebula sky + IBL, shadow, engines, lasers, shockwave,
     bloom, FXAA; knob preset;
   * `Engine/Showcase/Battle` (small).
   * Both also run under `DownlevelOverride::webgl2()` (a GL-variant golden).
4. **I4 Examples.**
   * `examples/graphics_showcase`: windowed, headless PNG and web; all area examples are verified with
     `SPACE3D_EXIT_AFTER_FRAMES=3` in background mode.
   * The web build (`space3d build --web` or the existing web_demo flow) renders with the Low preset.
5. **I5 Docs.**
   * `docs/graphics.md`: overview, frame diagram, how-to per feature, overlay stages, WebGL2 table, budgets,
     presets.
   * `docs/graphics/migration.md`: the game's `additive.rs` → HdrScene or particles, `models.rs` →
     `apply_material_maps`, `render_scale.rs` → `CameraFrame::render_scale`, lasers → `LASER_BEAM`, sky quads →
     `Skybox`.
   * README feature list; performance, platforms and storybook docs; the ARCHITECTURE summary merge.
6. **I6 Tooling.**
   * The editor inspector and reprs for `CameraFrame`, `PostProcess`, `Skybox`, `EnvironmentLight` and
     `DirectionalShadow` (additive).
   * CLI `space3d stories` and the editor storybook viewer list the new stories.
   * A final full check.

### 5.4 Build-plan summary

| builder | starts | owns (short) | milestones (in order) | key acceptance |
|---|---|---|---|---|
| **F** (foundation → maintainer) | after the fix workflow commits | `frame/{mod,plan,views,targets,passes}`, `node`, `overlay`, `settings`, `caps`, `output`, `timing`, `renderer`, `stats`, `pipeline/{mod,view}`, `shaders/{common,frame}`, facade/stories wiring, compat tests | F0 baseline → F1 split → F2 API freeze → F3 output tail/GpuView → **F4 HDR + composite** → **F5 nodes** → F6 overlay stages ⟶ tag `graphics-F-core`; F7 depth views; F8 timings/docs | goldens exact at F1–F3; `frame_modes`, `nodes`, `overlay_stages`, `depth_views`; compat and prelude fixtures; alloc and 0-texture steady state; ANGLE; wasm |
| **P-POST** | `graphics-F-core` | `post/**`, `shaders/post/**`, post tests/stories/example/docs | **PO1 bloom** → PO2 auto exposure → PO3 FXAA/vignette/CA/grain → PO4 grading/LUT → PO5 distortion → PO6 hardening | bloom threshold/energy/no-bleed; AE ±0.1 EV (±0.5 downlevel); idempotent renders; distortion ±1 px; bloom ≤ 0.20 ms, all post ≤ 0.60 ms (Dev) |
| **P-PART** | PA0 at F2; PA1 at `graphics-F-core` | `crates/space3d-particles/**`, particle stories/examples/docs | PA0 CPU runtime → **PA1 CPU render + prefabs** → **PA2 GPU 100k** → PA3 ribbons/beams/flipbooks/library → PA4 distortion/sort/LOD | CPU/GPU parity; render-on-demand == every tick; soft fade in 3 depth modes; 100k sim ≤ 0.06 ms, draw ≤ 0.40 ms; ANGLE CPU backend; 0 allocations |
| **P-MAT** | `graphics-F-core` (PM5 after F7) | `frame/{extract,draws}`, `pipeline/{forward,instance}`, `arena/assets/texture/handles/material`, `lighting/**`, forward/depth/sky/bake shaders, `space3d-assets/**` | **PM1 blend modes + maps** → PM2 glTF + tangents → PM3 skybox → PM4 IBL → PM5 shadows → PM6 hardening | no-map materials bit-identical; exact blend math; glTF slots incl. ORM and texCoord 1; white furnace ±3 %; shadow parity on all paths; ECS 100k ≤ 2.5 ms |
| **I** | all P done | `quality.rs`, showcase, `graphics_showcase`, `benches/graphics.rs`, docs, README, editor reprs | I1 merge → I2 presets/Battle → I3 showcase goldens → I4 examples/web → I5 docs → I6 tooling | whole storybook green (Metal + GL variants); Battle ≤ 3.0 ms GPU / 1.5 ms CPU on Dev; VRAM; wasm with all features |

Critical path: F0→F5 → PA1/PA2 (particles) and PO1 (bloom) run in parallel; PM1–PM4 in parallel with those; F7
lands during PM3/PM4 → PM5; PA4 after PO5 → I.

---

## 6. Risks and deferred items

### 6.1 Risks

| # | risk | impact | mitigation |
|---|---|---|---|
| R1 | Phase F is large and on the critical path | P builders wait | F-core gate at F6 (F7/F8 overlap Phase P); the particles early lane at F2; API frozen at F2 so P can read it before F-core; F2 stubs compile so P branches build from day one |
| R2 | The render-fixes fix workflow is still changing `frame.rs`/`pipeline.rs` | F0 builds on a moving base | F0 precondition (clean worktree, viewport-clear commit present). If the camera or viewport semantics changed further, F0 re-checks §1.3/§1.4 against the new ARCHITECTURE "Cameras and viewports" and reports deltas before F1 |
| R3 | Golden baseline drift (main's goldens predate render-fixes) | false "bit-identical" failures | F0 re-blesses once, in a separate commit; F1–F3 must be exact against it; any later re-bless needs a stated reason |
| R4 | `frame.rs`/`pipeline.rs`/`forward.wgsl` hotspots | merge conflicts | F1 split + ownership (§3.6); F owns the output tail and the `View` regions of `forward.wgsl`; P-MAT owns the rest; internal contracts (§2.6) instead of shared edits |
| R5 | WebGL2 program-link hitches (50–150 ms per pipeline) | stutter | overrides halve module counts; uniform-driven post; `prewarm` for nodes, forward and post; per-preset pipeline budget (§3.5); docs: prewarm on loading screens |
| R6 | WebGL2 without `EXT_color_buffer_float`, or with only the unprefixed half-float extension | no HDR or bloom there | class-C Ldr fallback with a consistent look (per-fragment tonemap everywhere, CPU-tonemapped clears); `GraphicsCaps` lets the game choose a look; upstream wgpu patch candidate |
| R7 | HDR MSAA resolve aliases very bright edges (lasers) | jaggies despite MSAA | guidance: keep emissive ≤ ~30, use beams with analytic soft profiles and FXAA; tonemap-aware resolve deferred (§6.2) |
| R8 | Soft fade off in GL+MSAA and in multi-view MSAA groups | harder particle edges there | documented; MSAA is 1 by default on the web; hard occlusion is kept |
| R9 | Bloom chain reads level i while writing i±1 of one texture on GL/ANGLE | feedback loop or UB | per-level views as in `generate_mips` (proven on GL); if ANGLE shows artifacts, the pool hands out separate textures per level (behind a GL check), with no API change |
| R10 | `Material` grows about 80 → 176 B | ECS extraction slower at 100k–1M | `PartialEq` fast path before key building; gate ≤ 2.5 ms; `MaterialHandle` path deferred |
| R11 | CPU/GPU particle divergence (FMA, `sin`/`cos` ulps) | flaky goldens | parity is structural and exact (alive sets, slots), positions within tolerance; goldens per backend variant |
| R12 | Intermediate-mode captures do not call HdrScene overlays again | an overlay that expects a Capture-kind call in HDR is surprised | documented in the trait docs; the game's per-kind buffers still work; test `hdr_scene_overlay_not_replayed_in_hdr` |
| R13 | HDR clear colors now pass through exposure/tonemapping | the dark-space backdrop tint shifts slightly | documented; `Tonemapper::None` / `passthrough()` reproduces; the showcase story tunes the game's clear |
| R14 | Perf gates on a shared dev machine are noisy | false failures | gates are medians of N timestamped frames; `SPACE3D_PERF_BUDGET_SCALE`; measured values recorded; relaxing needs data |
| R15 | Standard-Z `Depth32Float` precision at the game's far plane (150 000) | soft-fade banding far away | error ≈ 5 mm at 400 units; fine for fades; reverse-Z deferred |
| R16 | P builders need F-owned changes mid-flight | blocked work | foundation-maintainer protocol (§5.0); additive changes only; requests serialized on `graphics` |
| R17 | The public API frozen at F2 is later found insufficient | rework | extensions are additive (hubs, `#[non_exhaustive]` enums and info structs, provided trait methods); renames and removals are forbidden after F2 without maintainer sign-off and a compat-fixture update |

### 6.2 Deferred (explicitly out of v1; each is additive later)

* **Public `PostEffect` trait** with pooled ping-pong targets and insertion points (D18). The pool exists
  internally already.
* **SMAA** (`AntiAliasing` is `#[non_exhaustive]`), and **TAA** with motion vectors.
* **Refraction / scene-color copy phase** (the frame design §6). Distortion covers heat haze and shockwaves.
* **OIT** (weighted blended) for interleaving meshes and particles. Two order spaces are documented (§1.7).
* **Render layers / camera masks** (critique P2-8), e.g. keeping particles and light flashes out of UI preview RTs.
  Partly mitigated: nearest-light selection per view, and preview RTs live away from battle positions.
* **Tonemap-aware MSAA resolve** (critique P2-10, R7).
* **`TRANSIENT_ATTACHMENT`** (memoryless) MSAA color/depth on Apple, and **unresolve blits** (which would enable
  soft fade in multi-view MSAA).
* **Shadows:** cascaded shadows, point/spot shadows; custom-shader vertex displacement in the shadow pass.
* **Particles:** lit particles, mesh particles, sub-emitters, depth collisions, an egui effect editor.
* **HDR display output** (scRGB/PQ; the composite is the single place to add it); reverse-Z.
* **Materials and IBL:** compute IBL-bake accelerator; `MaterialHandle` fast path; per-map texture transforms;
  KTX2/Basis.
* **Post:** local exposure, depth of field, motion blur, SSAO/SSR.
* **Upstream:** a wgpu-hal patch recognising the unprefixed `EXT_color_buffer_half_float`.
* *(Added at the M2 review.)*
  * **Auto exposure sky weight:** metering ignores only black; a skybox is metered like the scene (a nebula
    filling the frame sets the exposure). Down-weighting it needs scene depth in the post group's metering pass.
    Documented workaround: negative compensation and a low `max_ev` (post.md).
  * **HDR cubemaps in a 4-byte format** (`Rg11b10Ufloat` where renderable; the compute bake paths need a storage
    format, so the fragment path would build those mips): halves the sky and IBL memory (VRAM line above).
  * **GPU strip builder / incremental strip uploads** (particles) and the **compute bloom chain** (post): the
    Battle's CPU sub-budgets.
  * **wasm budgets** (§4.4, "measured manually … in Phase I"): the web build compiles and links for wasm32 and
    the Low look is covered by the `WebGL2 *` stories and the ANGLE runs, but no browser measurement was taken
    (no wasm-bindgen toolchain on the build machine).

---

## Appendix A — Critique resolution matrix

| finding | resolution | § |
|---|---|---|
| P0-1 two composite passes | one composite; post shader/params, frame slot; alpha preserved; bloom in exposed units; grain/dither last; no passthrough variant needed (D16); `OutputInputs` dropped | D1, 1.9 |
| P0-2 two HDR/tonemap/exposure APIs | `PostProcess` is the opt-in; one `Tonemapper`; `CameraFrame {hdr, render_scale}`; two switches with a defined rule | D2, 1.2–1.3, 2.2.1 |
| P0-3 multi-camera grouping | target groups (storage) + post groups `(target, rect)`; settings camera = highest-order post camera; display views LDR on top; the "all scenes, then posts" order and the PiP rule | D3, 1.3–1.4 |
| P0-4 three distortion systems | post's half-res `Rgba8Unorm` DB, fused apply; a frame-provided writer `PassTarget`; camera-state trigger; particles drop `Rg16Float`; `DistortionMaterial` = built-in node (P-POST) | D4, 1.8 |
| P0-5 forward tail / `GpuView` / `PipelineKey` owners | F owns `//#OUTPUT`, `S3D_OUTPUT` override, `GpuView` (incl. materials' fields) and `PipelineKey`, frozen at F3 | D5, 3.1–3.5 |
| P0-6 node API gaps | `&mut Resources`; `SlotMask` + `DrawContext::slot`; whole-slot routing to SceneRead; `SceneDepthRef` + generation; `RenderCapabilities` + `GraphicsCaps` in contexts; renderer id + `device_lost`; names `RenderNode`/`slots()` | D7, 2.2.4, 1.6 |
| P1-1 predates viewport clears | spec written on the clear semantics (first pass clears, viewport triangle, depth per camera); no transient MSAA/unresolve; multi-view MSAA → in-pass fallback; HDR viewport clears clamp on GL (documented; Ldr pre-tonemaps) | 1.4, 1.6, 5.0 (precondition) |
| P1-2 determinism holes | seeds from `Time::frame`; delta 0 rule; no time in `GpuView` | D9, 4.3 |
| P1-3 WebGL2 soft depth | sampled `Depth32Float`, no attachment, manual test; no linearize/R16F | D8, 1.6 |
| P1-4 too many view uniforms | one `GpuView`; groups 0 view / 1 scene / 2–3 node | D6, 3.3–3.4 |
| P1-5 transparent order | meshes → non-reading slots → reading slots; one particle node | D11, 1.7 |
| P1-6 shadows/IBL frame hooks | depth views (F7), env-keyed view bind groups, `Lighting` contract; skybox as a node; replay skips them | D17, 1.12, 2.6, F7 |
| P1-7 contradictory API rules | fields only on `Material`, `MaterialData`, `ModelMesh`, `RenderStats` (evidence-based; `ModelMesh` replaced `Primitive` at the M1 review); FRU config structs; `#[non_exhaustive]` enums/info; curated prelude + fixture test | D10, 2.7 |
| P1-8 duplicated infrastructure | `GraphicsCaps`, one `TexturePool`, `GpuTimings`, `RenderStats` counters, `HdrImageData` | D13, 2.2 |
| P1-9 budgets | one tier table, Battle budget per tier, WebGL2 preset, wasm CPU, VRAM, Dev gates | D12, 4.4 |
| P1-10 default transparent sort | `TransparentSort::None` default; `PerInstance` opt-in | 2.2.9, PM1 |
| P1-11 histogram workgroup size | `GraphicsCaps::compute_histogram` (≥ 256 invocations, ≥ 1 KiB) else render-pass path | 2.2.2, PO2 |
| P1-12 glTF maps in the game | `apply_material_maps` + migration note | 2.4, I5 |
| P1-13 inconsistent LDR colors | `Tonemapped` encoding shared by meshes, particles (`s3d_output`) and HdrScene overlays (`OverlayScene::output`); particles' soft clip only in Direct | 1.2, 1.5, 2.5 |
| P1-14 alpha / compact / HDR RT in post | alpha always preserved; transparent-group rules; compact only for opaque primaries; HDR RTs Direct | 1.9, D16 |
| P1-15 merge hotspots / baseline | F0 re-golden; F1 split; ownership map; exactness gates F1–F3; sequencing F-core → P | 3.6, 5.1 |
| P2-1 point-light cap | nearest 16 per view when > 16; unchanged ≤ 16 | D19 |
| P2-2 no prewarm plan | `RenderNode::prewarm`, `Renderer::prewarm(PrewarmRequest)`, pipeline budget per preset | 3.5 |
| P2-3 particle stories not wired | particle stories in `space3d-stories` (feature `particles` in `engine`) | 2.5, 2.8 |
| P2-4 `lenient()` | `CompareOptions::perceptual()` everywhere | 2.2.8, 4.3 |
| P2-5 `Fade` collision | renamed `FadeCurve`; particles never in the facade prelude | 2.5, 2.2.10 |
| P2-6 are HdrScene overlays distorted | yes (they are in `S`) | 1.8 |
| P2-7 frame design self-contradictions | `prepare` runs after planning and before culling (§1.4); the tonemap op lives in `GpuView.output.y`/`exposure.x`; the GL float claim is corrected (§4.1) | 1.4, 3.3, 4.1 |
| P2-8 no render layers | deferred, with mitigation | 6.2 |
| P2-9 helper name collisions | `s3d_`/`S3D_` prefix rule + permutation test | 3.2 |
| P2-10 tonemap-aware resolve | deferred (R7) | 6.2 |
| P2-11 Classic vs GGX IBL | documented; showcase/High use `Standard` | 2.2.9 |
| P2-12 examples and background mode | every example honours background mode and `SPACE3D_EXIT_AFTER_FRAMES`; checked in I4 | 4.2, 5.3 |

## Appendix B — WGSL reference (normative semantics; F3 writes these in `shaders/common/*`)

```wgsl
// output.wgsl ---------------------------------------------------------------------------------
const S3D_OUTPUT_LINEAR: u32 = 0u;  const S3D_OUTPUT_MANUAL_SRGB: u32 = 1u;  const S3D_OUTPUT_TONEMAPPED: u32 = 2u;
const S3D_TM_NONE: u32 = 0u; const S3D_TM_REINHARD: u32 = 1u; const S3D_TM_ACES: u32 = 2u;
const S3D_TM_AGX: u32 = 3u;  const S3D_TM_PBR_NEUTRAL: u32 = 4u;

fn s3d_luma(c: vec3<f32>) -> f32 { return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722)); }
// s3d_linear_to_srgb / s3d_srgb_to_linear: the same expressions as the template's existing helpers.

fn s3d_tonemap(op: u32, c_in: vec3<f32>) -> vec3<f32> {
    let c = max(c_in, vec3<f32>(0.0));
    switch op {
        case 1u: { return clamp(c / (1.0 + s3d_luma(c)), vec3(0.0), vec3(1.0)); }
        case 2u: { return s3d_aces_fitted(c); }          // design-post §5.4 (Hill fit, clamped)
        case 3u: { return s3d_agx(c); }                  // design-post §5.4 (minimal AgX, pow 2.2 out)
        case 4u: { return s3d_pbr_neutral(c); }
        default: { return clamp(c, vec3(0.0), vec3(1.0)); }
    }
}

// Khronos PBR Neutral (start compression 0.76, desaturation 0.15).
fn s3d_pbr_neutral(c_in: vec3<f32>) -> vec3<f32> {
    var c = c_in;
    let x = min(c.r, min(c.g, c.b));
    let offset = select(0.04, x - 6.25 * x * x, x < 0.08);
    c -= vec3<f32>(offset);
    let peak = max(c.r, max(c.g, c.b));
    if (peak < 0.76) { return c; }
    let d = 1.0 - 0.76;
    let new_peak = 1.0 - d * d / (peak + d - 0.76);
    c *= new_peak / peak;
    let g = 1.0 - 1.0 / (0.15 * (peak - new_peak) + 1.0);
    return mix(c, vec3<f32>(new_peak), g);
}

fn s3d_output(c: vec4<f32>, encoding: u32, exposure: f32, op: u32) -> vec4<f32> {
    if (encoding == S3D_OUTPUT_MANUAL_SRGB) {       // byte-identical to today's MANUAL_SRGB block
        return vec4<f32>(linear_to_srgb(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0))), c.a);
    }
    if (encoding == S3D_OUTPUT_TONEMAPPED) { return vec4<f32>(s3d_tonemap(op, c.rgb * exposure), c.a); }
    return c;                                       // Linear: unclamped
}

// scene.wgsl (variant with real depth; the no-depth variant binds a 1×1 R32Float = 1.0 and
// sets S3D_SCENE_DEPTH = false) -------------------------------------------------------------------
const S3D_SCENE_DEPTH: bool = true;
@group(1) @binding(0) var s3d_scene_depth_tex: texture_2d<f32>;   // Depth32Float as unfilterable float
@group(1) @binding(1) var s3d_exposure_tex: texture_2d<f32>;      // 1×1, EV in .r
fn s3d_depth_valid() -> bool { return S3D_SCENE_DEPTH; }
fn s3d_scene_uv(frag_xy: vec2<f32>) -> vec2<f32> { return (frag_xy - view.viewport.xy) / view.viewport.zw; }
fn s3d_scene_depth(px: vec2<i32>) -> f32 { return textureLoad(s3d_scene_depth_tex, px, 0).x; }
fn s3d_view_depth_at(px: vec2<i32>) -> f32 {          // positive view distance (perspective and ortho)
    let uv = s3d_scene_uv(vec2<f32>(px) + 0.5);
    let v = view.inv_proj * vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, s3d_scene_depth(px), 1.0);
    return -v.z / v.w;
}
fn s3d_view_depth_of(world: vec3<f32>) -> f32 { return -(view.view * vec4<f32>(world, 1.0)).z; }
fn s3d_soft_fade(px: vec2<i32>, frag_view_depth: f32, softness: f32) -> f32 {
    if (!s3d_depth_valid() || softness <= 0.0) { return 1.0; }
    return clamp((s3d_view_depth_at(px) - frag_view_depth) / softness, 0.0, 1.0);
}
fn s3d_scene_occluded(px: vec2<i32>, frag_device_depth: f32) -> bool {   // LessEqual test, manually
    return s3d_depth_valid() && frag_device_depth > s3d_scene_depth(px);
}
fn s3d_exposure_ev() -> f32 { return textureLoad(s3d_exposure_tex, vec2<i32>(0), 0).r; }

// depth_resolve.wgsl (RODS + MSAA single-view; depth-only pass into depth_1x, compare Always, write on)
@group(0) @binding(0) var depth_ms: texture_depth_multisampled_2d;
override S3D_SAMPLES: u32 = 4u;
@fragment fn fs_resolve(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {
    var d = 1.0;
    for (var s = 0u; s < S3D_SAMPLES; s++) { d = min(d, textureLoad(depth_ms, vec2<i32>(p.xy), i32(s))); }
    return d;
}

// distortion.wgsl (DISTORTION_WGSL) ---------------------------------------------------------------
const DISTORTION_RANGE: f32 = 0.0625;       // max |offset| per axis, in view heights
fn distortion_encode(offset: vec2<f32>, weight: f32) -> vec4<f32> {
    let o = clamp(offset * weight / DISTORTION_RANGE, vec2<f32>(-1.0), vec2<f32>(1.0));
    return vec4<f32>(max(o, vec2<f32>(0.0)), max(-o, vec2<f32>(0.0)));   // R=+x G=+y B=-x A=-y; blend One/One
}

// composite finish (dither in encoded space; seed = pcg(0x9E3779B9 ^ frame)) -----------------------
// n = hash01(px, seed) - hash01(px, seed ^ 0x68E31DA4)   (triangular in [-1, 1])
// sRGB-format D:   rgb = srgb_to_linear(saturate(linear_to_srgb(rgb) + n / 255))
// manual-sRGB D:   rgb = saturate(linear_to_srgb(rgb) + n / 255)            (then written as-is)
// float D:         no dither
```
