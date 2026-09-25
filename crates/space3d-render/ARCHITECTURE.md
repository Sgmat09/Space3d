# space3d-render architecture

A wgpu 30 forward renderer designed around two ideas: **everything is instanced** and **the CPU
touches as few bytes as possible per frame**. It runs windowed (surface provided by
`space3d-platform`) and headless (offscreen target + readback for tests, storybook and CI).

## Module map

| module | role |
|---|---|
| `renderer.rs` | `Renderer`: instance/adapter/device/queue, capability detection, surfaces, offscreen target, asset upload, `capture()` |
| `frame/` | per-frame work: `mod.rs` (`render_frame` orchestration, `FrameState`, `RenderSettings`, uploads, frame facts), `plan.rs` (cameras, viewport rects, the frame plan: target groups, modes, post groups, view roles), `extract.rs` (ECS/explicit extraction, batching, material interning), `views.rs` (lights, `GpuView`s, draw commands, CPU culling, GPU culling jobs/dispatches), `draws.rs` (material bind groups, `encode_draws`), `passes.rs` (view passes with the node phases, depth resolve, SceneRead and Distortion passes, HdrScene overlays, group finish: composites and display views, capture replay), `nodes.rs` (the render-node snapshot and schedule, node `prepare`/`prepare_view`, pass targets and depth-read modes, scene-input bind groups), `targets.rs` (scene textures of intermediate groups, resolved depths, render-target feedback copies, the texture pool), `tests.rs` |
| `instance.rs` | `InstanceData` trait, `InstanceLayout`/semantics, built-in formats, `InstanceBuffer<T>`, `InstanceBuffers`, `InstancedMesh` |
| `gpu.rs` | `Uploader` (staging ring), `GrowBuffer`, `TrackedBuffer` (shadow-diffed uploads), parallel staging copies |
| `arena.rs` | mesh arena: shared vertex/index buffers, first-fit range allocator, GPU-side growth |
| `assets.rs` | `RenderAssets`: handle allocation without a GPU, pending uploads, textures/render targets |
| `texture.rs` | `write_texture` uploads (block-aware pitch), GPU mip generation, sampler cache |
| `pipeline/` | `mod.rs` (pipeline cache, `PipelineKey`, culling/blit/clear pipelines, culling uniforms), `view.rs` (`GpuView`, lights), `forward.rs` (forward shader composition, `GpuMaterial`, material layouts, forward pipelines, custom-shader validation), `instance.rs` (instance attributes, generated `InstanceInput` WGSL) |
| `shader.rs` | `CustomShader`: user WGSL hooks spliced into the forward shader |
| `settings.rs`, `caps.rs`, `output.rs`, `node.rs`, `timing.rs`, `quality.rs` | graphics API (frame): `FrameSettings`/`CameraFrame`, `GraphicsCaps`, `OutputEncoding`/`Tonemapper`/`OUTPUT_WGSL`, the render-node API, `GpuTimings`, `GraphicsQuality` (see `docs/graphics/api.md`) |
| `material.rs`, `lighting/`, `post/` | graphics API hubs of the materials and post areas (`BlendMode`…, cubemaps/`Skybox`/shadows/`LightingSettings`, `PostProcess`…) and their crate-internal frame contracts (`Lighting`, `PostState`) |
| `readback.rs` | non-blocking readback of GPU-culled visible counts for `RenderStats` |
| `culling.rs` | `Frustum`, `Aabb`, `Sphere` (CPU side; mirrored in `shaders/cull.wgsl`) |
| `dirty.rs` | `DirtyRanges`: sorted, coalescing range set |
| `stats.rs` | `RenderStats`, `DrawPath` |
| `shaders/` | `forward.wgsl` (template), `cull.wgsl`, `compact.wgsl`, `blit.wgsl`, `clear.wgsl` (viewport clears); `common/` (`view`, `output`, `scene`, `fullscreen`, `hash`: the WGSL modules nodes and overlays compose), `frame/depth_resolve.wgsl` (minimum of a multisampled depth's samples), `post/composite.wgsl` (scene texture → display target), `post/distortion.wgsl` |

## Frame data flow

```
 Stage::Render  (render_system → Renderer::render_frame)
 ───────────────────────────────────────────────────────────────────────────────────────────
 RenderAssets.pending ──► upload_assets_into ──► mesh arena (staging ring)
                                              └► textures (write_texture + GPU mip blit)

 World ──┬─ lights (DirectionalLight ≤4, PointLight: 16 nearest per view) ─► GpuView (per camera)
         ├─ cameras (Camera3d + GlobalTransform, sorted by order) ─────► views + frustums
         ├─ ECS: (GlobalTransform, Mesh3d, Material, Visibility?) — archetype columns
         │      intern Material → id (cached) ; key = class|material|mesh   (serial)
         │      gather matrices of kept rows                    (rayon ≥ 32k per archetype)
         │      bucket by key (order reused while the key sequence is unchanged)
         │      diff matrices into `ecs` TrackedBuffer shadow   (rayon ≥ 32k) ─► ECS sources
         └─ InstancedMesh ─► InstanceBuffers[handle]
                sync dirty ranges ─► per-buffer GrowBuffer ───────────────► explicit sources

 sources sorted by (blend, class, layout, shader + texture kind, material, mesh, buffer)

 for each view:
   CpuDirect:  ECS → per-instance sphere test (one pass, rayon for large batches);
                     all visible → draw from `ecs`
                     partial → visible matrices into `cpu_visible` (diffed)
               explicit → per-block AABB test (4096 instances, bounds refreshed lazily
                          for edited blocks) → one draw per visible run of blocks
   Gpu*:       each cullable source → CullJob + output region in `cull_out`
                                    + DrawIndexedIndirectArgs template
 Gpu*: the jobs of all views grouped per source buffer (views in windows of 64 frustums)
       → one mark + one scatter dispatch each, one scan, [one compaction]

 flush TrackedBuffers (only changed bytes) ─► staging ring ─► copy_buffer_to_buffer
   (CpuDirect: `ecs` uploads only ranges drawn from it; the rest stays pending)
 encoder: [uploads] → copy templates→args → clear counts
          → [render nodes: prepare (compute), prepare_view per view]
          → compute(mark → scan → scatter [+compact]) → copy args → stats readback ring
          → per view: [copy own render target → feedback texture] → render pass
                      (first pass into a target clears it; viewport: scissor [+ clear triangle];
                       node slots around the built-in opaque and blended batches)
                      [→ depth resolve → SceneRead pass] [→ Distortion pass]
          → [HdrScene overlays] → post groups' composites → display views
          → AfterTonemap overlays
          → submit → present → staging recall → poll(Poll)
 next frames: read the mapped args copy (visible counts) into RenderStats
```

Everything above happens in **one command encoder and one submit per frame**.

## Instancing

### Path 1 — automatic ECS batching
Entities with `Mesh3d + Material + GlobalTransform` are extracted archetype by archetype (the
order hecs queries use), reading the component columns as slices: a serial pass interns each
material and records the batch key, mesh and row of every visible entity; the matrices of the
kept rows are then gathered into a compact array, split across rayon workers for archetypes
of 32 768 entities or more. The entities are grouped by key `(pipeline class, material id,
mesh)` — a counting sort that keeps query order within a key, so the result equals a stable
sort — and their matrices written contiguously into the persistent `ecs` instance buffer
(`InstanceTransform`, 64 B). That write diffs against the CPU shadow; at 32 768 entities or
more it runs on all workers in chunks of 2048, each reporting up to four changed runs (more
are merged into the last), so sparse changes still upload exactly the changed matrices.
The grouping depends only on the key sequence, so it is reused as long as that sequence is
unchanged (static scenes, or only transforms moving); hecs has no change detection, so the
per-entity extraction and matrix diff still run every frame (see Performance: about 7 ms of
render-stage CPU for a static 1M-entity scene, 8.4 ms when all of them move). Each run of equal keys becomes one *source*
→ one instanced draw. Materials are plain values; `MaterialTable` interns equal materials
(hash of their bit patterns; the previous entity's material and a 64-slot direct-mapped cache
answer almost every lookup with one key comparison, so the map is only probed on misses) into
a stable id with a slot in the material uniform buffer
(`size_of::<GpuMaterial>()` rounded up to the device's `min_uniform_buffer_offset_alignment`:
256 B on desktop/WebGPU, 64 B on iOS and many mobile GPUs), so 1000 entities with
equal-but-separate `Material` values still produce one draw.
Unused materials are garbage-collected after 600 frames.

### Path 2 — explicit `InstancedMesh` + `InstanceBuffer<T>`
For 100k–1M+ instances with no per-entity ECS cost. `T: InstanceData` is any `Pod` struct with
a `const LAYOUT: &'static InstanceLayout` listing attributes and their *semantics*:

| semantic | used by the built-in shader as |
|---|---|
| `ModelMatrix` | 4×`Float32x4` columns |
| `Translation`, `Rotation` (quat, any float/snorm format), `Scale`, `UniformScale` | TRS → model matrix |
| `TranslationScale` | `xyz` translation + `w` uniform scale in one 4-component float attribute |
| `Color` | linear tint × material color |
| `UvRect` | atlas rect (`uv * zw + xy`) |
| `TextureLayer` | texture-array layer (atlas page / animation frame) |
| `Custom(n)` | bound as `inst.customN`; read by [custom shaders](#custom-shaders) via `instance_customN(inst)`, ignored by the built-in shader |

`pipeline/instance.rs` generates the `InstanceInput` struct and accessor functions for each layout and
splices them into `forward.wgsl` (`instance_model`, `instance_color`, `instance_uv_rect`,
`instance_layer`, `instance_custom0..3` plus one per declared `Custom(n)`). `validate()`
enforces WebGPU's rules (offset aligned to `min(4, format size)`, each semantic once) and at
most 12 instance shader locations (16 vertex attributes, 4 used by the mesh). Pipelines are
cached by `PipelineKey` `(layout, class, format, samples, output encoding, texture kinds,
custom shader + version, pass kind, frame-wide overrides)` (`kinds` bit 0: the material texture
is an array, see Textures) and shader modules by `(layout, kinds, ext stream, pass kind, custom
shader)`: the output encoding (`S3D_OUTPUT`, and the legacy `MANUAL_SRGB` flag custom shaders
read) and the other `S3D_*` switches are WGSL `override` constants specialized per pipeline,
so one module serves every output. `Unorm8x4Bgra`
attributes are fetched as `Unorm8x4` and swizzled in the generated shader: wgpu's GL backend
passes `GL_BGRA` as the component count, which WebGL2/GLES reject.
Built-ins: `InstanceTransform` (64 B), `InstanceTransformColor` (80 B), `InstanceCompact`
(32 B: pos + uniform scale + f32 quat), `InstancePacked` (32 B: pos + scale + snorm16 quat +
unorm8 color + layer — 2.5× smaller than matrix + color), `InstanceHalf` (16 B: f16 pos +
uniform scale as one `Float16x4` `TranslationScale`, snorm16 quat). f16 has an 11-bit
significand — steps of 1/64 at 16–32 units, 0.5 at 512–1024, 32 near the 65 504 maximum — so
`InstanceHalf` suits bounded or camera-relative scenes, not large worlds. `f32_to_f16` /
`f16_to_f32` convert (round to nearest even).

`InstanceBuffer<T>` is user-owned CPU data registered in the `InstanceBuffers` resource. Every
mutating accessor (`set`, `get_mut`, `range_mut`, `push`, `swap_remove`, `as_mut_slice`, …)
records a dirty range. Usage hints:

* `Static` — GPU buffer sized exactly; after the first upload nothing is sent unless edited.
* `Dynamic` — dirty ranges only; GPU buffer grows 1.5×.
* `Stream` — rewritten each frame; uploaded whole every frame without range bookkeeping.

CPU culling bounds are kept per block of 4096 instances (position AABB + max instance scale).
Edits only flag their block; the CPU path recomputes the flagged blocks exactly, in parallel on
native, when it needs them — GPU culling paths never compute bounds at all. Removals
(`swap_remove`, `truncate`) keep the remaining boxes, which stay conservative, so removing one
instance costs one block, not a rescan. The built-in formats read position/scale fields
directly; other layouts decode them at offsets resolved at compile time
(`InstanceLayout::cull_source` is a `const fn`): a model matrix, or a translation in
`Float32x3/x4` or `Float16x4` with an optional `f32`/`f16` uniform or vector scale (the GPU
culler decodes f16 by hand, so no `SHADER_F16` feature is needed). Positions in other formats
(snorm, integer) cannot be decoded: such layouts render but are never culled per instance —
the renderer logs a warning the first time it sees one — and the CPU path culls them per
block only if the type overrides `InstanceData::bounds` (otherwise they are always drawn).
`set_bounds` overrides the automatic bounds with one box for the whole buffer; edits made
meanwhile are still tracked.

### Draw paths and GPU-driven culling

Chosen at startup from adapter capabilities (`RenderCapabilities::best_draw_path`), optionally
lowered at runtime via `RenderSettings::draw_path`; reported in `RenderStats::draw_path`.

| path | requirements | culling | draw submission |
|---|---|---|---|
| `GpuMultiDrawCount` | compute + `INDIRECT_EXECUTION` + `INDIRECT_FIRST_INSTANCE` + `MULTI_DRAW_INDIRECT_COUNT` (Vulkan, DX12) | compute | cull → compact non-empty args per run → `multi_draw_indexed_indirect_count` |
| `GpuMultiDraw` | compute + indirect + `INDIRECT_FIRST_INSTANCE` (Metal, WebGPU w/ feature) | compute | `multi_draw_indexed_indirect` per run |
| `GpuIndirect` | compute + indirect | compute | `draw_indexed_indirect` per batch; vertex buffer rebound at the batch's region (no `first_instance` needed) |
| `CpuDirect` | none (WebGL2, GLES, downlevel) | CPU spheres (ECS) / per-block AABBs (explicit) | `draw_indexed` per batch (explicit: per visible run of blocks) |

GPU paths additionally require `DownlevelFlags::BASE_VERTEX`; devices without it (WebGL2,
GLES 3.0/3.1) always use `CpuDirect` (see Mesh arena).

GPU culling (`shaders/cull.wgsl`):

* A **job** per (view, source): source word offset, count, stride, how to decode position/scale
  (`CullSource::Matrix`, or `TranslationScale` in f32 or f16), mesh bounding sphere, output
  region, indirect-args index and its view's frustum. The jobs of *all views* that read the
  same source buffer share one dispatch: each workgroup finds its job by binary search over
  `wg_start`, and the job selects its view's six planes in a uniform array of 64 frustums
  (views past 64 go to further windows: one more mark + scatter dispatch per buffer and 64
  views, each window bound at its own fixed offset). The frustums sit at the start of the
  cull-params uniform buffer, so culling binds no extra buffer and no extra dynamic offset
  (either costs wgpu an allocation per frame). The dispatch count therefore depends on the
  number of source buffers, not on buffers × views: 120 explicit buffers seen by 13 views
  take 241 dispatches (it was 3121 with a mark + scatter dispatch per view and buffer —
  `frame::tests::culling_dispatches_scale_with_buffers_not_views`, which also checks that each
  of its 70 viewports shows exactly its own camera's objects and clear color on every path).
* **Order-preserving compaction** in three dispatches, so GPU paths draw visible instances in
  exactly the CPU path's order (alpha blending and equal-depth surfaces are deterministic):
  1. `cull_mark`: 64 instances per workgroup; the frustum test result becomes a 64-bit mask
     (workgroup `atomicOr`, order-independent) stored in the workgroup's record.
  2. `cull_scan`: one workgroup per job computes the exclusive prefix sum of its workgroups'
     visible counts (each lane sums a block of records, then a 64-lane Hillis–Steele scan) and
     writes the total as `instance_count`.
  3. `cull_scatter`: each visible instance's slot is its record's base + the visible lanes
     below it (`countOneBits`); its raw words are copied into the view's region of `cull_out`.
  Records (16 B per 64 instances) live after the indirect args in the `args` buffer, so the
  passes need only four storage bindings. No atomics decide output positions.
* Bounding-sphere radii scale by an upper bound of the matrix's largest stretch (Gershgorin on
  `AᵀA`), not the longest column, so sheared hierarchy transforms (non-uniform parent scale ×
  rotated child) are never culled while visible. CPU (`max_axis_scale`) and GPU agree.
* `DrawIndexedIndirectArgs` are reset each frame by a GPU copy from a template buffer (the
  templates themselves are diffed on the CPU, so a static scene uploads nothing).
* **Runs**: consecutive batches sharing pipeline, material and layout are one run; their output
  regions are contiguous (run base aligned to 256 B) and `first_instance` is relative to the run
  base, so a run is a single multi-draw. Meshes differ only in `first_index/base_vertex` thanks to
  the mesh arena — no rebinding inside a run.
* `GpuMultiDrawCount` adds `compact.wgsl`: one workgroup per run (one dispatch for the runs of
  every view) moves the run's non-empty args to its front **in their original order** (block
  counts + prefix sum) and stores the count, so `multi_draw_indexed_indirect_count` skips
  fully culled batches.
* Explicit buffers larger than `max_storage_buffer_binding_size`, ECS batches that end past
  it in the `ecs` buffer (bound from offset 0, so the batches before them keep GPU culling:
  `frame::tests::ecs_batches_inside_the_storage_binding_keep_gpu_culling`), and output regions
  that would end past that binding in `cull_out` (which accumulates every view's regions) fall
  back to CPU culling + direct draws for that view, even on GPU paths. So do layouts whose
  position cannot be decoded (see above), which on the CPU path are culled per block only with
  user bounds.

Batches are drawn in the order `(blend, class, instance layout, custom shader + texture kind,
material, mesh)`: transparent materials (`alpha_blend`) after opaque ones (no per-instance
depth sort), and within each, every pipeline is set once per view and every material once per
pipeline, whatever the spawn order; runs of one material's meshes are contiguous, so
multi-draw paths merge them. `RenderStats::pipeline_switches` / `material_switches` count the
binds (`tests/instancing.rs::batches_are_ordered_by_pipeline_then_material_then_mesh`).

## Cameras and viewports
Cameras render in ascending `order` (ties: entity id), each in its own render pass. Cameras
sharing a target (split screen, picture-in-picture, minimaps, overlays) draw into the same
attachments, so per target and frame:

* The first pass into a target clears it: with the camera's clear color if the camera covers
  the whole target, otherwise black (never last frame's content — undefined for swapchain
  images anyway — and never a viewport's clear color outside the viewport). Later passes load
  what the earlier ones drew; a later camera covering the whole target with a clear color
  clears it all.
* A camera's viewport (`[x, y, w, h]`, normalized, top left origin) is rounded to whole
  pixels — each edge to the nearest pixel boundary; a viewport thinner than that gets the
  pixel holding its center, within the target if it overlaps it (growing the far edge would
  push one in the last half pixel just outside) — for its projection's aspect ratio, the
  pass's viewport and its scissor rect (its part within the attachment). GL
  takes integer viewports (wgpu's GL backend truncates position and size separately, which
  shifted fractional viewports by a pixel and opened gaps between neighbours), so every backend
  draws the same pixels and neighbouring viewports share their edges exactly. With a clear
  color the camera then clears just that rectangle by drawing one triangle over the viewport
  (`Pipelines::viewport_clear`, `shaders/clear.wgsl`): the color is the pass's *blend
  constant* (blend factors `Constant`/`Zero`), so the pipeline needs no buffer or bind group
  and one pipeline per (target format, sample count) serves every color — nothing is written
  or allocated per frame. It uses the pass's color format and sample count and neither tests
  nor writes depth. The color goes through the same sRGB handling as a clear load op
  (hardware encoding on sRGB targets, `manual_srgb` encoding on the CPU otherwise), and
  `Rgba16Float` targets get unclamped HDR colors (checked on Metal and ANGLE).
* Before a pass with a viewport ends, an MSAA pass resets its scissor rect to the whole
  target: wgpu's GL backend resolves MSAA with a framebuffer blit at the end of the pass, and
  GL clips blits to the scissor rect, so only the viewport was resolved and the rest of the
  target kept the previous frame (caught on ANGLE by
  `tests/headless.rs::the_first_view_of_a_frame_clears_its_target`).
* Depth is per camera: every pass clears the whole depth attachment and discards it, so a
  camera never tests against another's geometry.
* Viewports are not limited to the target: a rect within wgpu's viewport limits (at most
  `max_texture_dimension_2d` wide and high, within twice that of the origin) is drawn as is;
  any other — a 101× zoom viewport, a far-off or non-finite one — is drawn through its part
  within the target with the projection remapped in clip space so each pixel shows what the
  whole viewport would (`ViewportRect::fitted`), and one entirely outside draws nothing.
  Passing such a rect to `set_viewport` failed validation and lost the frame
  (`frame::tests::out_of_range_viewports_draw_their_visible_part`).
* A pass's attachments have the size its views were prepared for on every path. When the
  window's surface is lost after a frame's views were prepared (`SurfaceAcquire::Recreate`),
  the frame's primary views are drawn offscreen, like the frames until a new surface is
  attached, so the offscreen target — which a windowed renderer otherwise sizes only for
  captures — is sized for them first (`Renderer::surface_lost`); a scissor rect outside the
  attachment would fail validation and lose the frame's commands, uploads included. Scissor
  rects are also clamped to the attachment's own size
  (`frame::tests::a_surface_lost_mid_frame_renders_offscreen_at_the_prepared_size`,
  `scissor_rects_stay_within_the_attachment`).

This is identical on every draw path (`tests/headless.rs`: split screen with and without MSAA,
picture-in-picture over a full-screen camera, an overlay without a clear color, the first-pass
rule across frames in the window and in a render target, two viewports sharing a render
target, viewports thinner than a pixel at the target's edges; `frame::tests::
viewport_clear_writes_every_target_format` clears sRGB, linear 8/10-bit and `Rgba16Float`
targets at 1 and 4 samples; the `viewports` portability scene matches the default backend
on GL in `tests/gl.rs`).

## Frame modes, scene textures and the composite
Cameras are grouped by render target (a *target group*); each group renders in one mode per
frame, chosen from components and settings only (graphics spec §1.2–§1.3, `frame/plan.rs`):

| mode | when | scene color | fragments write | display target `D` written by |
|---|---|---|---|---|
| Direct (default) | no post camera, render scale 1 | none | `Linear` (or `ManualSrgb`) into `D` | the camera passes (everything above) |
| Hdr | a post camera (`PostProcess` enabled, `FrameSettings::post`), float targets, lead's `HdrMode` not `Ldr` | `Rgba16Float` (`Rg11b10Ufloat` for `HdrMode::Compact` on an opaque-cleared, single-sample primary target) | `Linear`, unclamped | the composite, then display views |
| Ldr | post camera without float targets (or `HdrMode::Ldr`): *fallback*; no post camera and render scale < 1: *passthrough* | `Rgba8UnormSrgb` | fallback: `Tonemapped` (exposure × tonemapper per fragment, clears tonemapped on the CPU with `Tonemapper::apply`); passthrough: `Linear` | the composite, then display views |

A group's *lead camera* (its highest-order post camera, else its first) sets its `HdrMode` and
render scale (`CameraFrame`, else `FrameSettings::render_scale` for the primary target; clamped
to 0.25–1; HDR render targets always render Direct at scale 1 and ignore post). Every distinct
viewport rect with a post camera is a *post group*: its settings come from the rect's
highest-order post camera, and the views on that rect up to it are its *scene views*, which
render into the group's scene texture `S` (at the scaled size, with the group's samples and a
group depth; the viewport-clear rules above apply to `S`). Every other view of the group (a
gizmo camera after a post camera on its rect, a minimap without post) is a *display view*.
After the group's last scene view (`passes.rs` `finish_group`), each post group is
composited into `D` in settings-camera order — the first composite into `D` clears it, drawing
only its rect — and then the display views draw into `D` at 1 sample with the group's display
depth, LDR, in camera order. A passthrough group has one implicit full-target post group.

`S`, its multisampled twin, the group depth and the display depth are created while planning a
frame whose key (scene size, format, samples, display size) changed, and freed 3 frames after
their group last rendered intermediately (`targets.rs` `GroupTextures`); `RenderStats::
{textures_created, target_bytes, intermediate_groups, post_groups, post_passes}` report them.
`S` persists until the next frame's plan, so `Renderer::capture_scene_hdr` reads the last frame's
scene color (as float RGBA; `Err` in Direct mode) and a windowed capture replays an intermediate
primary group by compositing again from `S` (no scene pass) into the offscreen target, then its
display views, then the overlays.

The composite (`shaders/post/composite.wgsl`, `post/mod.rs` `PostState::encode_group`) is one
full-screen triangle per post group, viewport and scissor on the group's rect in `D`. It reads
`S` at the rect (a texel load when the rects have the same size — render scale 1 is exact —
otherwise bilinear with every tap clamped inside the rect, so split screens never bleed),
un-premultiplies transparent render-target groups, multiplies by the exposure (`2^stops` in
Hdr, 1 in Ldr), applies the tonemapper (Hdr; `None` otherwise), dithers (±1 LSB triangular in
the encoded space, seed `pcg(0x9E3779B9 ^ frame)` with `frame = Time::frame`), encodes for `D`
(hardware sRGB, manual sRGB for 8-bit non-sRGB surfaces, linear for float) and re-premultiplies
(alpha is always kept). Its parameters (`post/chain.rs` `GpuPost`, one 256-byte slot per post
group) are shadow-diffed like every renderer buffer: a still scene uploads nothing, and only the
16-byte seed vector when dither reseeds each frame. Pipelines are per (output format, output
kind) (`S3D_POST_OUT` override); the input bind group per scene texture generation. Effects
(bloom, auto exposure, FXAA, lens, grading, distortion) are the post area's slots in the same
shader and chain.

Render targets take a `RenderTargetDesc` (`RenderAssets::add_render_target_with`): `Hdr` creates
them in `GraphicsCaps::hdr_format` (linear radiance; sampling materials read it unclamped and
their camera's post tonemaps it), `samples > 1` gives them a multisampled color and depth
(resolved into the texture) for Direct passes. Depth textures are sampleable (`TEXTURE_BINDING`)
at 1 sample, and multisampled only where multisampled depth can be resolved
(`GraphicsCaps::msaa_depth_resolve`: not on GL, where a sampled multisampled texture needs GLES
3.1).

Tests: `tests/frame_modes.rs` (modes, exactness of passthrough, tonemapper monotonicity,
exposure linearity, Ldr-fallback parity, alpha, compact HDR, split screen, gizmo display view,
PiP, render scale, HDR and MSAA render targets, determinism, lazy resize, steady state, WebGL2
override parity), `tests/capture_hdr.rs`, `frame::tests::replay_path_equals_frame_output`,
`tests/alloc_frame.rs` (allocations per mode), `tests/gl.rs` (the portability scenes in Hdr and
Ldr on ANGLE); `benches/frame.rs` (Direct vs Hdr minimal at 1080p).

## Render nodes, scene depth and overlay stages
Render nodes (`node.rs`, `frame/nodes.rs`, graphics spec §1.4–§1.8) draw into frame-owned
passes. The renderer keeps a snapshot of the `RenderNodes` resource (re-read only when its
generation changes, like the overlay snapshot) with one sorted slot list per scheduling list —
Opaque, Skybox, Transparent, depth-reading Transparent, Distortion — ordered by `(order,
registration, slot)`, and per node the slot bits of each list and of its negative-order slots.
Each frame (`prepare_nodes`, after the views are prepared and before GPU culling): every node's
`prepare` (with `&mut Resources`, the frame's `ViewInfo`s, the staging uploader and the encoder),
then per camera view every node's `prepare_view` with the `PassTarget` each slot will draw with
there. The returned masks (views × nodes, reused) are cut to the slots that can draw in the view
(Distortion slots only in a distortion-enabled post group's settings camera) and reduced to the
view's *work bits* (`nodes::work`); `prewarm` runs for each `(slot, PassTarget)` not seen before
(tracked per node, kept across snapshots). Views with no node work record exactly what they did
before nodes existed.

In a view's scene pass (`passes.rs` `encode_view`): Opaque slots of negative order, the built-in
opaque batches, the other Opaque slots, the Skybox slots, Transparent slots of negative order,
the built-in blended batches (`ViewData::opaque_end` splits the view's draws; without node work
between them the batches are one `encode_draws` call as before), the other non-reading
Transparent slots, and the depth-reading ones when they draw in-pass. Node draws get the view's
viewport and scissor, which are restored before built-in draws resume. The pass stores depth only
when a reader of this view's depth has work: its SceneRead pass, the Distortion writers of its
post group, or HdrScene overlays reading depth (after the last primary scene view). After the
pass, by the depth matrix of `docs/graphics/frame.md`: a depth resolve
(`shaders/frame/depth_resolve.wgsl`, the minimum of the samples into the group's
`ResolvedDepth`, created at frame start by `ensure_resolved_depth`) for multisampled groups on
read-only-depth devices, then the SceneRead pass — single sample, `Load` on the resolved color,
the depth attached read-only (`depth_ops: None`) or not at all (GL) and bound in group 1 — or,
for GL with MSAA or several views sharing a multisampled target (a later view would load
multisampled content without the single-sample pass's draws), the in-pass fallback with the far
dummy bound. Scene views of a post group's settings camera then run the Distortion pass into the
group's persistent distortion buffer (`PostState::distortion_target`, created while preparing).

Group 1 bind groups (`SceneInputs`): the far dummy (1×1 `R32Float` = 1.0) with the 0 EV
exposure dummy, and one per depth texture read (created on first use, keyed by the view, dropped
3 frames after the last use; their creation order gives `SceneDepthRef::generation`). Depth is
sampled through a nearest sampler (binding 2): GLES 3.0 depth textures are incomplete unless
filtered nearest, and `textureLoad` uses the texture's own (linear) filter state on GL.

Overlays (`overlay.rs`) are split by `stage()`, read with `scene_reads()` at the start of every
frame (`OverlayCache::sync`). HdrScene overlays run after the primary target's last scene view
(`FrameState::hdr_scene_view`): in Direct mode right after that view into `D`; otherwise at the
start of `finish_group`, into `S` before the composites; one pass, `prepare_scene` → `prepare` →
`render` per overlay, the target at `OverlayTarget` size = scene size, `scale_factor ×
render_scale`. AfterTonemap overlays run where all overlays ran before. Capture replays call
node `draw`s with `FrameKind::CaptureReplay` (Direct groups: SceneRead passes included) and
HdrScene overlays only for Direct primary groups (kind `Capture`); intermediate groups
composite again from `S`, which already holds them.

`Renderer::prewarm` walks the last frame's views (and each requested extra format on primary
views): forward pipelines for the requested materials × instance layouts, every node's `prewarm`
for its slots' targets, and the composite pipelines of the intermediate groups' targets.

Tests: `tests/nodes.rs` (order around the built-ins, masks, lifecycle, every depth row against an
analytic plane, in-pass fallback, Distortion plumbing, renderer id, poisoning, prewarm),
`tests/overlay_stages.rs`, `frame::tests::{prepare_once_per_frame_not_in_replay,
distortion_buffer_holds_the_writers_output, hdr_scene_overlay_not_replayed_in_hdr}`,
`tests/alloc_frame.rs` ("nodes"), `tests/gl.rs::gl_backend_reads_scene_depth` (ANGLE).

## Custom shaders
A `CustomShader` is WGSL spliced into the built-in forward shader, registered with
`RenderAssets::add_shader` and selected per material with `Material::with_shader`. It defines
`vertex_hook(v: VertexInput, inst: InstanceInput, out: VsOut) -> VsOut` (end of the vertex
stage) and/or `fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32>` (end of the fragment
stage, `color` = lit linear color); a missing hook is the identity. Everything the template
declares is in scope: the generated `InstanceInput` and its accessors — including
`instance_customN(inst)` (the `Custom(N)` attribute as a zero-padded `vec4<f32>`, swizzled to
RGBA for `Unorm8x4Bgra`; slots 0–3 always exist and are zero when the layout lacks them, so a
shader using them works with every layout) and the raw `inst.customN` (as fetched: memory
order, B G R A, for `Unorm8x4Bgra`) — the
`view`/`material` uniforms, `sample_base`, the color helpers, and in custom pipelines an extra
interpolated varying `VsOut.custom: vec4<f32>` to pass data from the vertex to the fragment
hook. This is how per-instance custom data is consumed: `tests/custom_shader.rs` renders a
user `InstanceData` whose `Custom(0)`/`Custom(3)` attributes set each instance's color, on
every draw path (GPU culling copies whole instances, custom bytes included).

Each (instance layout, sRGB mode, texture kind, shader version) gets its own module and
pipelines, cached like the built-in ones; the material's shader is part of its identity and
of the batch order. `update_shader` (hot reload) bumps a version that drops the old
pipelines; `remove_shader` reverts its materials to the built-in shader, and the pipelines of
removed shaders are released at the next 256-frame sweep. A custom shader is validated with
naga first (the portable capability set), plus what naga accepts but the forward pipeline
cannot run alike on every backend: resources of its own (the forward pipeline layout has no
bindings for them, so their pipeline would be invalid), workgroup memory (`var<workgroup>`,
used or not: WGSL allows it in compute shaders only, and backends disagree about it in the
vertex and fragment stages the hooks run in — Metal accepts it, GL and browsers reject it),
any other address space but `function`/`private`, `override` constants without a default
(the pipeline sets none), and built-ins WebGL2 / GLES 3.0 cannot compile — GLSL ES 3.00 lacks
`countOneBits`, `reverseBits`, `extractBits`, `insertBits`, `firstLeadingBit`,
`firstTrailingBit`, `countTrailingZeros`, `ldexp`, `dot4I8Packed`/`dot4U8Packed`,
`unpack4xI8`/`unpack4xU8` and `textureGather*` (naga's GLSL backend emits them anyway), and the
forward entry points are translated to GLSL ES 3.00 once to catch what naga refuses there
(`textureNumLevels`, `pack4x8unorm`, …). Rejecting those on every backend keeps a shader that
works on the machine it is written on working on the web (`CustomShader` has the per-backend
table; `pipeline::tests::custom_shaders_cannot_use_builtins_webgl2_lacks`, and
`tests/custom_shader.rs::custom_shaders_are_limited_to_what_webgl2_compiles`, which also runs
on ANGLE). Its pipeline is then created inside validation and internal error
scopes, so no error reaches the uncaptured-error handler: a failure is logged once, reported
by `Renderer::shader_error`, and the draws of that pipeline key fall back to the built-in
shader. Native backends answer the scopes immediately. In the browser the answer
arrives asynchronously: the pipeline is used meanwhile, the answer is polled every frame
(`Pipelines::poll_pending`), and an invalid pipeline is then replaced by the built-in one —
so an invalid custom pipeline costs at most the few frames until the browser answers
(`frame::tests::custom_pipelines_rejected_asynchronously_fall_back` injects such an answer;
`tests/custom_shader.rs::custom_shaders_declaring_their_own_bindings_fall_back`,
`custom_shaders_using_workgroup_memory_fall_back`).
Culling still uses the instance's position and scale (a vertex hook that moves geometry far
outside the mesh bounds can be culled while visible).

## CPU → GPU transfer

### Staging: `Uploader` (staging ring)
All buffer uploads go through a ring of equally sized 4 MiB `MAP_WRITE | COPY_SRC` chunks: data
is copied **directly into mapped staging memory** and a `copy_buffer_to_buffer` is recorded in
the frame encoder. Chunks are unmapped before submit (`finish`), re-mapped after it (`recall`)
and reused only once the map callback reports the GPU is done with them, so it is a
frame-in-flight-aware ring: no write-after-read hazards, and no staging allocation in the
steady state. Writes larger than a chunk are split into chunk-sized pieces (recorded in order,
so ordering against other commands is unchanged). Every chunk is therefore interchangeable,
and idle chunks beyond twice the busiest frame of the last 128–256 recalls are released: the
ring follows the actual upload volume. (`wgpu::util::StagingBelt` was replaced because it never
frees a chunk and creates one of exactly the request size whenever a write is larger than every
chunk it has, so a growing upload leaked one mapped chunk per frame. `queue.write_buffer_with`
was rejected: it allocates a fresh staging buffer per call in wgpu-core and runs before the
encoder's copies.) Copies ≥ 1 MiB are split across rayon workers on native
(`copy_to_staging`, disjoint `WriteOnly` sub-slices; checked byte for byte without a GPU in
`gpu::tests::parallel_staging_copies_are_exact` and pixel for pixel — one instance per pixel
of a 3.2 MB buffer — in `tests/instancing.rs::parallel_staging_copies_deliver_every_instance`).

### Change detection
* **User buffers** — explicit dirty ranges (`DirtyRanges`: sorted, merged when adjacent or within
  16 instances; at most 64 ranges — once saturated, a new range within the set's average
  spacing of a neighbour is folded into it in O(1), otherwise the closest pair is merged).
  >50 % dirty → one full copy
  (`tests/instancing.rs::more_than_half_dirty_uploads_the_whole_buffer`).
* **Renderer-owned buffers** (`TrackedBuffer`: ECS instances, per-view visible instances, view
  uniforms, materials, cull jobs/params, args templates, run meta) keep a CPU shadow; writes are
  compared with it and only differing byte ranges are uploaded. Result: an unchanged scene with a
  still camera uploads **0 bytes** per frame; moving one ECS entity uploads one 64-byte matrix.
  On `CpuDirect` the `ecs` buffer is only read by fully visible batches, so only its dirty
  ranges inside those batches are uploaded; the rest stays pending (the shadow is always
  current) until a batch is drawn from it again. Culled and partially visible batches (drawn
  from `cpu_visible`) upload nothing to `ecs`.

### Persistent buffers and growth
`GrowBuffer` never reallocates per frame: capacity grows 1.5× (256-byte aligned) or exactly for
`Static` data (pushing one instance per frame for 1000 frames reallocates a `Dynamic` buffer
at most 20 times: `tests/instancing.rs::growing_dynamic_buffers_reallocate_logarithmically`); reallocation bumps a generation so dependent bind groups are rebuilt, and tracked
buffers re-upload their shadow. wgpu zero-initializes new buffers, which the shadow diff relies on.

### Mesh arena
All meshes live in one vertex buffer (36-byte interleaved vertex: position, normal, uv, sRGB
unorm8 color) and one `u32` index buffer, sub-allocated first-fit with free-range coalescing.
Each buffer grows (2×, at least by what is missing) only when *it* is short, and never beyond
the device's `max_buffer_size`; a mesh that cannot fit is logged, keeps its CPU data and is not
drawn. Old contents are copied GPU→GPU into the regrown buffer, then mesh data is written
through the staging ring like any other upload. (The regrown buffers are deliberately not
`mapped_at_creation`: without `MAP_WRITE`, wgpu backs such a mapping with a hidden staging
buffer of the full new size, zero-filled on the CPU and copied whole, and on WebGPU copies the
whole mapped range through wasm memory.) Updates of same-sized meshes are written in place;
removals free their ranges for reuse.

Draws select their mesh with `first_index` + `base_vertex` (indices are stored unrebased).
WebGL2 and GLES < 3.2 lack `DownlevelFlags::BASE_VERTEX`; wgpu does not validate it and its GL
backend then calls `glDrawElementsInstancedBaseVertex`, which panics on the web (and on ANGLE
ES 3.0 draws every mesh but the first wrong). Without the flag (`RenderCapabilities::
base_vertex`), direct draws bind the arena's vertex buffer from the mesh's first vertex
(`36·n` bytes, always 4-aligned; rebound only when the mesh changes) and pass `base_vertex = 0`.
Indirect args carry the base themselves and are only used where the flag exists.

### Textures
`queue.write_texture` per layer/mip with the row pitch computed from the format's block size
(works for BC7/ASTC/ETC2 when `TEXTURE_COMPRESSION_*` is enabled — check
`Renderer::supports_texture_format`). Materials sample through a filterable-float binding, so
only formats that are filterable float on the device are accepted (`FLOAT32_FILTERABLE` is
requested when available); integer, depth and unsupported formats log an error and sample
white, and so do block-compressed textures whose size is not a whole number of blocks (wgpu
would reject them with a validation panic; smaller mips may be partial blocks and are
uploaded with their block-aligned physical extent). Single-layer textures (images, render targets, the white fallback) are bound as `D2`
views and sampled as `texture_2d`; multi-layer textures as `D2Array` / `texture_2d_array`
with the layer chosen per instance. The two kinds use separate material bind group layouts
and pipeline variants: wgpu's GL backend binds a texture with the target implied by its layer
count (a 1-layer texture is `TEXTURE_2D`, which a `sampler2DArray` reads as black), and it
makes square textures with a multiple of 6 layers cube maps — on GL those arrays get one
unused extra layer. Missing mips of formats the device can render to are generated on the GPU
with a linear blit per level and layer (sRGB-correct): the source is a view of the previous
level of the texture's own kind (all layers for arrays, layer picked by the draw's vertex
range), since GL cannot sample one layer of an array through a `D2` view. Otherwise only the
provided mips are uploaded. `tests/textures.rs` checks generated mips (a minified one-texel
red/blue pattern averages to the sRGB-correct purple), full BC1/BC7/ETC2/ASTC mip chains at
power-of-two and odd sizes (each format the adapter supports), the unaligned-size refusal,
and atlas sampling through `UvRect`. (Forced onto ANGLE's GL backend on macOS — GLES 3.0,
which must report ETC2 — ETC2 textures upload without error but sample black: a driver
limitation outside what wgpu or the renderer can detect, so the test skips ETC2 on ANGLE. The
other checks pass there.)

Render-target textures (`RenderAssets::add_render_target`, resized with
`resize_render_target`) get their own depth buffer and can be sampled by materials. A texture
cannot be a color attachment and a sampled resource in the same pass, so when a view draws a
material that samples the view's own target (a monitor seen by its camera — on GPU paths, any
monitor in the world, since draws are recorded for every source), the target is copied to a
per-target feedback texture just before the pass and those draws sample the copy: the previous
frame's content, like a video feedback loop.

## Buffer lifetimes

| buffer | usage | lifetime | written by |
|---|---|---|---|
| mesh arena vertex/index | VERTEX / INDEX | persistent, grows 2× | staging ring on upload |
| `ecs` instances | VERTEX (+STORAGE) | persistent, 1.5× | shadow diff each frame |
| explicit instance buffers | VERTEX (+STORAGE) | per `InstanceBufferHandle`; freed on `remove` | dirty ranges |
| `cpu_visible` | VERTEX | persistent | shadow diff (CPU path, partially visible batches) |
| view uniforms | UNIFORM, dynamic offset per camera | persistent | shadow diff |
| material uniforms | UNIFORM, dynamic offset per material | persistent | on intern only |
| cull params (+ view frustums) / jobs / templates | UNIFORM / STORAGE / COPY_SRC | persistent | shadow diff |
| `cull_out`, args (+ compaction records), compacted args, counts | STORAGE+VERTEX / STORAGE+INDIRECT (args also COPY_SRC for the stats readback) | persistent, 1.5× | GPU only (compute, copy, clear) |
| run meta (multi-draw-count runs) | STORAGE | persistent | shadow diff |
| render-target feedback copies | TEXTURE_BINDING+COPY_DST | while a view samples its own target (+60 frames) | GPU copy before the pass |
| staging chunks (4 MiB) | MAP_WRITE+COPY_SRC | recycled ring; idle chunks beyond 2× the recent per-frame peak are freed | CPU |
| capture readback | MAP_READ | per capture | GPU copy |
| culling-stats readback (3) | MAP_READ | persistent ring, 1.5× (all idle slots grow together) | GPU copy of the indirect args |

Every `GrowBuffer` (1.5× growth) is clamped to the device's `max_buffer_size`. A larger
buffer fails validation, which loses the whole frame, so the buffers whose size the scene
drives keep the requested size itself within the limit: explicit instance buffers over it are
logged and not drawn; ECS matrices past it (more than `max_buffer_size / 64` entities) are cut
from the largest batches — every batch keeps up to the same count, so the small batches of
other materials or meshes are drawn whole whatever their sort order — and the overflow is
logged once; a partially visible ECS batch that no longer fits `cpu_visible` (which
accumulates every view's) is drawn whole from `ecs`, unculled; `cull_out` and the indirect
args (with their records) are bounded by the storage-binding checks of GPU culling
(`frame::tests::ecs_instances_past_max_buffer_size_are_cut_not_fatal`,
`partially_visible_ecs_past_max_buffer_size_is_drawn_unculled`; both run on a device created
with those limits, so wgpu validates them). The other renderer buffers (materials, view
uniforms, cull jobs and params, draw templates, run meta) grow with the number of distinct
materials, views and draws — 20–256 B each, so reaching `max_buffer_size` would take millions
of them — and are not checked: past the limit they would fail validation (and trip
`GrowBuffer`'s debug assertion first).

Textures are bounded the same way by `max_texture_dimension_2d`: image textures over it are
refused (sampled as white), render targets over it are not created — logged; cameras
targeting them are skipped and materials sample white — until `resize_render_target` brings
them within it, and the primary target (window surface or headless target) is clamped to it
per axis (`Renderer::size` reports the clamped size;
`tests/headless.rs::targets_past_the_texture_size_limit_do_not_lose_frames`).

## Allocation in the steady state
**Renderer code allocates nothing per frame.** All per-frame scratch (ECS keys/rows/matrices/
order, `sources`, `cameras`, `views`, `draws`, `jobs`, `dispatches`, culling masks, the
parallel diff's per-chunk runs) lives in `FrameState` or its buffers and is cleared, not
dropped. Asset and instance registries are moved out of the resource map with `mem::take`, and
`render_system` borrows the `Renderer` with `Resources::scope`, which moves its existing Box
out and back (no re-boxing). Large staging copies, ECS gathers and diffs, block-bounds
refreshes and large CPU culling batches use rayon parallel iterators, whose `join`-based jobs
live on the stack (`scope.spawn` would box every task). `RenderStats::cpu_reallocations`
counts scratch growth and is asserted to be 0 on steady frames
(`tests/instancing.rs::static_buffer_and_static_scene_upload_nothing_after_first_frame`,
`tests/alloc.rs`).

**wgpu's per-frame bookkeeping does allocate**, and it is the whole remainder: command
encoder, passes (their command lists grow from empty each frame), `set_*`/`dispatch` calls in
wgpu-hal's Metal backend, indirect-draw validation, submission, and the boxed callbacks the
renderer hands to wgpu (staging-ring recall, culling-stats readback). Measured with a
counting global allocator on Apple M4 Max / Metal, wgpu 30.0.1, release build, 100k
instances at 256×256 (30 warm-up + 120 measured frames; min–max per frame):

| scene | `CpuDirect` | `GpuIndirect` / `GpuMultiDraw` |
|---|---|---|
| explicit buffer, full rewrite / 1000 sparse edits / `Stream` | 56–57 | 104–105 |
| ECS, all 100k moving | 56–57 | 107–111 |
| ECS, static | 46–47 | 98–99 |

Backtraces of one traced frame per case put every allocation inside a wgpu, wgpu-hal or naga
call, except rayon's injector queue (crossbeam-deque), which allocates one block per 63 jobs
submitted from outside the pool — about once every 15–60 frames. The culling-stats readback
accounts for 7–8 of the GPU paths' allocations. No bytes stay allocated across frames.
wgpu-hal's Metal backend allocates in every compute dispatch and bind, so the GPU paths'
count grows with the number of culling dispatches — two per source buffer (and window of 64
views), whatever the number of views: 40 explicit buffers seen by 4 views allocate 298 times
per frame on `GpuMultiDraw` (544 when every view had its own dispatches; `CpuDirect`: 124).

`tests/alloc_frame.rs` pins the graphics frame modes the same way: Direct 76, Hdr / Ldr /
passthrough 106 (one composite pass more) and render nodes 119 (Direct plus one SceneRead pass
and three node draws) per frame on Metal; the renderer's own work allocates nothing in any of
them.

`tests/alloc.rs` pins this: a scene with 40k moving ECS entities (two materials, one
textured), a 100k-instance dynamic buffer rewritten every frame, an f16 buffer and a custom
shader allocates 82–86 times per frame on `CpuDirect` and 191–195 on the GPU paths on Metal
(test profile; 4 batches, 3 pipeline binds and, on GPU paths, 7 compute dispatches —
wgpu-hal's Metal backend allocates in every dispatch and pipeline bind). The test fails above 95 / 215 (Metal;
twice that on other backends, whose wgpu-hal code differs), if the second half of 120 frames
allocates more on average than the first, or if 256 KiB or more stays allocated. It then adds
two viewport cameras and checks that giving them clear colors (a viewport-clear draw each, see
[Cameras and viewports](#cameras-and-viewports)) allocates nothing more: 314.6 per frame with
and without on `GpuMultiDraw` / Metal, 133.7 on `CpuDirect` / ANGLE (each extra render pass
itself costs wgpu about 40 on Metal).

## RenderStats
`draw_calls`, `indirect_draw_calls`, `multi_draw_calls`, `batches`, `instances`,
`instances_drawn`, `culled`, `triangles`, `gpu_counts_frame`, `bytes_uploaded`,
`instance_bytes_uploaded`, `buffer_reallocations`, `cpu_reallocations`, `compute_dispatches`,
`pipeline_switches`, `material_switches`, `draw_path`, `gpu_culling`, `meshes_uploaded`,
`textures_uploaded`, `views`, `frame`.

Visible counts of GPU-culled batches exist only in the indirect args. Every GPU-culling frame
copies its args (20 B per culled batch) into one of three `MAP_READ` buffers, mapped when the
frame's submission completes (`map_buffer_on_submit`); a later frame sums a mapped copy
(`readback.rs`). Nothing waits for the GPU, so `instances_drawn`, `culled` and `triangles` of
GPU-culled batches are those of an earlier frame, named by `gpu_counts_frame`: normally the
previous one (1 frame of latency; `tests/instancing.rs::gpu_culling_stats_are_read_back`), 2–3
when the GPU runs behind, and a frame is not read back at all while all three buffers are in
flight. Until the first copy is read (e.g. the first GPU-culling frame) those batches count as
fully drawn and `gpu_counts_frame` is `None`; CPU-culled batches always report the current
frame.

## Headless capture and determinism
Headless renderers draw primary cameras into an offscreen `Rgba8UnormSrgb` target.
`Renderer::capture()` copies it into a `MAP_READ` buffer with rows padded to
`COPY_BYTES_PER_ROW_ALIGNMENT`, maps it via `map_buffer_on_submit` and returns a
`space3d_tasks::Task` that strips the padding into `ImageData` (native: resolved by a background
task that polls the device without blocking, every millisecond, and by each frame's own poll;
web: by the browser/WebGL poll). It never waits inside `device.poll`: a blocking poll holds
wgpu's device-wide snatch lock for the whole GPU wait, which stalled `queue.present` and buffer
destruction on the main thread (`capture_blocking` waits on the calling thread for its own
submission only). Windowed renderers re-record the last frame's primary
views into the offscreen target before copying (surface textures are not readable), at the
size those views were prepared for, then run the overlays (`OverlayTargetKind::Capture`) on top
of them — views first, as in a frame (`Renderer::encode_capture_replay`;
`frame::tests::capture_replay_draws_overlays_after_the_views` forces the replay headless and
checks it equals the frame). Surface overlays get the frame's view format (the sRGB view of a
browser canvas), not the surface texture's. `resize`/`set_msaa_samples` reallocate the headless target
lazily at the next frame, so a capture in between still returns the last rendered frame.
`render_to_image(&mut App)` propagates transforms, renders one frame without advancing time and
blocks for the capture (native only). Output is deterministic on one machine on every draw
path: fixed-key hashing, stable sorts with sequence tie-breaks, order-preserving GPU compaction
(no atomics decide positions), no time-dependent state in the renderer
(`tests/instancing.rs::blended_instances_render_deterministically_on_every_path` renders
overlapping alpha-blended instances repeatedly and against CpuDirect).

## GPU errors and surfaces
The renderer installs a wgpu uncaptured-error handler: errors are logged through `tracing`
(the first 16, then every 1000th) and counted (`Renderer::gpu_error_count`). Debug builds and
tests still panic on them so bugs surface; release builds keep running instead of taking wgpu's
default panic. A device-lost callback logs as well and records the loss: every resource
created after it is invalid and every use of one is an error, which from then on is only
logged, in debug builds too. Captures run inside a validation error scope, so their errors
become the capture's `Err` (and `render_to_image`'s `None`) rather than uncaptured errors —
this also covers the time between `Device::destroy` and the callback, which wgpu only fires
once in-flight work has finished. A lost device thus degrades (captures fail, frames are
dropped) instead of crashing (`tests/headless.rs::a_lost_device_degrades_instead_of_panicking`,
`renderer::tests::errors_after_device_loss_do_not_panic_in_debug`). On `CurrentSurfaceTexture::Outdated` the
surface is reconfigured; on `Lost` it is dropped (`has_surface()` turns false, frames render
offscreen — that frame included, at the size its views were prepared for: see [Cameras and
viewports](#cameras-and-viewports)) because a lost surface must be recreated from the window
with `set_surface`, and reconfiguring it fails validation.

Surfaces prefer an sRGB format. Browser WebGPU canvases have none (`Bgra8Unorm`/
`Rgba8Unorm`), so the canvas is configured with its sRGB twin as a view format and frames
render through that view (MSAA attachment in the same format): blending and MSAA resolves run
in linear space as on native. Only without `SURFACE_VIEW_FORMATS` or an sRGB twin (e.g.
`Rgba16Float`) does the shader encode sRGB itself (`manual_srgb`, blending on encoded values).
wgpu reports a canvas configuration the browser rejects as `Lost`; a canvas that is lost before
its first frame with a view format is reconfigured without it (in-shader encoding) rather than
recreated with the same configuration. `Renderer::surface_format` is the view format, for
overlays drawing into the same view.

## Platform fallbacks

| platform | backend | draw path (typical) | notes |
|---|---|---|---|
| Windows | DX12 / Vulkan | `GpuMultiDrawCount` | |
| Linux | Vulkan (GLES fallback) | `GpuMultiDrawCount` / `CpuDirect` on GLES | GLES gets the winit display handle via `new_with_surface_and_display` |
| macOS / iOS | Metal | `GpuMultiDraw` | multi-draw is emulated by wgpu-hal as a loop |
| Android | Vulkan / GLES | `GpuMultiDraw*` / `CpuDirect` | surface dropped on suspend, recreated on resume |
| Web (WebGPU) | browser WebGPU | `GpuIndirect` or `GpuMultiDraw` (`indirect-first-instance`) | renderer created asynchronously; renders through an sRGB view of the canvas |
| Web (WebGL2) | wgpu GL | `CpuDirect` | no storage buffers/compute, no `BASE_VERTEX` (arena rebound per mesh); surfaces without sRGB format use in-shader sRGB encoding |

The GL-specific behaviour (no `BASE_VERTEX`, texture targets fixed by layer count, no BGRA
vertex formats) is covered by `tests/gl.rs`, which renders the `tests/common` portability
scenes on wgpu's GL backend and compares them with the default backend; it skips without a GL
adapter (on macOS, use ANGLE — ES 3.0, WebGL2's level — as described in the file). Unit tests
also force `caps.base_vertex = false` on the default backend.

## Performance
Latest measurements (2026-09-24, working tree of the `render-fixes` branch; Apple M4 Max,
Metal; release builds). The machine was shared with other builds throughout (load average
20–80), so absolute times vary by up to 2× between runs; A/B comparisons were run
interleaved, and the numbers below are medians of the quieter runs.

Criterion (`cargo bench -p space3d-render --bench instancing`, `CpuDirect`, camera looking
away, each iteration waits for the GPU; load ≈ 20):

| bench | median | throughput |
|---|---|---|
| `extract_upload/explicit/100k` (rewrite all, 3.2 MB) | 0.67 ms | 150 M instances/s |
| `extract_upload/explicit/1000k` (rewrite all, 32 MB) | 1.96 ms | 510 M instances/s |
| `extract_upload/ecs/100k` (all moving, incl. transform propagation) | 2.27 ms | 44 M entities/s |
| `extract_upload/ecs_static_mixed/100k` (4 meshes × 8 materials interleaved) | 2.22 ms | 45 M entities/s |
| `mesh_arena/upload_1000` (145k vertices, 728k indices) | 1.92 ms | 520 k meshes/s |

Render-stage CPU time at 1M instances, 1280×720 (median of 40 frames, counting-allocator
probe; "before" is the same probe built from the previous commit, run interleaved):

| scene | path | before | now |
|---|---|---|---|
| ECS, 1M moving entities | `GpuMultiDraw` | 10.6–10.7 ms | 8.4–8.6 ms |
| ECS, 1M moving entities | `CpuDirect` | 13.4–15.1 ms | 11.3–11.5 ms |
| ECS, 1M static entities | `GpuMultiDraw` | 8.7–8.8 ms | 7.1 ms |
| explicit `InstanceCompact`, 1M rewritten (32 MB) | `GpuMultiDraw` / `CpuDirect` | — | 0.8 / 1.2 ms (+ 5 ms GPU wait) |

The explicit path meets a 60 Hz budget at 1M instances with room to spare. In a profile of 1M
moving ECS entities (`GpuMultiDraw`, taken under load) the render stage's main thread spent
about 35 % in the 64 MB staging copy, 27 % in the serial per-entity key pass (material
interning, visibility and mesh checks) and 38 % in the parallel matrix gather and diff, so 1M+
dynamic objects belong in `InstanceBuffer`s. Allocation counts are in [Allocation in the steady state](#allocation-in-the-steady-state).

Many small GPU-culled buffers seen by many views (120 explicit buffers of 50 instances, 12
render-target views + the primary, 128×96; per frame, CPU submit / CPU + GPU wall; three
interleaved runs, load ≈ 20–27): culling dispatches per source buffer for all views cut
`GpuMultiDraw` from 1.2–1.6 / 9.7–10.6 ms (3121 dispatches) to 0.45–0.6 / 1.9–2.6 ms (241);
the pre-`render-fixes` renderer, with one unordered dispatch per view and buffer, took
0.8–1.0 / 6.9–7.3 ms (1560), `CpuDirect` 0.25 / 0.8 ms. Typical scenes (4 buffers × 100k
instances, 2 views) are unchanged within noise (25 → 9 dispatches).

## Known limitations
* No shadows, IBL or post-processing; lighting is metallic/roughness Blinn-Phong with Schlick
  Fresnel, 4 directional + 16 point lights per view (with more, the 16 nearest to the view's
  camera, in query order; with at most 16, every light in query order).
* After a `Lost` surface the platform runner attaches a new one on a later redraw (throttled
  to once per second), so the window can stay blank briefly.
* Layer indices past a texture array's last layer sample the unused padding layer on GL for
  square arrays of 6k layers (other backends clamp to the last layer).
* GPU-culled visible counts in `RenderStats` lag by 1–3 frames (read back without blocking).
* Transparent instances are not depth-sorted within a batch.
* ECS extraction interns materials and checks meshes serially (the matrix gather and diff are
  parallel); instance layouts whose position is not f32/f16 are not culled per instance.
* ECS entities past `max_buffer_size / 64` are not all drawn (the largest batches are cut).
  The renderer requests the adapter's own limits, so this is 4 194 304 entities only where the
  adapter reports WebGPU's 256 MiB default; native adapters and browsers usually report more,
  and wgpu's GL backend reports `i32::MAX` (33.5 M entities). That many objects belong in
  `InstanceBuffer`s anyway.
* Render targets larger than `max_texture_dimension_2d` are not created, and the primary
  target is clamped to it (see Buffer lifetimes).
* Custom shaders are hooks inside the forward shader: they cannot declare bindings, workgroup
  memory or unset pipeline constants, or replace the lighting model's inputs, and culling
  ignores vertex displacement.
* Viewport clear colors above 1 (HDR) rely on unclamped blend constants (verified on Metal
  and ANGLE); a GL driver that clamps them, as OpenGL ES 2.0 did, clamps such viewport clears
  to 1 (whole-target clears are load ops and unaffected).
