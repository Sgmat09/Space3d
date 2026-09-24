# space3d-render architecture

A wgpu 30 forward renderer designed around two ideas: **everything is instanced** and **the CPU
touches as few bytes as possible per frame**. It runs windowed (surface provided by
`space3d-platform`) and headless (offscreen target + readback for tests, storybook and CI).

## Module map

| module | role |
|---|---|
| `renderer.rs` | `Renderer`: instance/adapter/device/queue, capability detection, surfaces, offscreen target, asset upload, `capture()` |
| `frame.rs` | per-frame extract → batch → cull → upload → encode; `RenderSettings`; material interning |
| `instance.rs` | `InstanceData` trait, `InstanceLayout`/semantics, built-in formats, `InstanceBuffer<T>`, `InstanceBuffers`, `InstancedMesh` |
| `gpu.rs` | `Uploader` (StagingBelt), `GrowBuffer`, `TrackedBuffer` (shadow-diffed uploads), parallel staging copies |
| `arena.rs` | mesh arena: shared vertex/index buffers, first-fit range allocator, GPU-side growth |
| `assets.rs` | `RenderAssets`: handle allocation without a GPU, pending uploads, textures/render targets |
| `texture.rs` | `write_texture` uploads (block-aware pitch), GPU mip generation, sampler cache |
| `pipeline.rs` | GPU uniform layouts, WGSL generation per instance layout, pipeline cache |
| `culling.rs` | `Frustum`, `Aabb`, `Sphere` (CPU side; mirrored in `shaders/cull.wgsl`) |
| `dirty.rs` | `DirtyRanges`: sorted, coalescing range set |
| `stats.rs` | `RenderStats`, `DrawPath` |
| `shaders/` | `forward.wgsl` (template), `cull.wgsl`, `compact.wgsl`, `blit.wgsl` |

## Frame data flow

```
 Stage::Render  (render_system → Renderer::render_frame)
 ───────────────────────────────────────────────────────────────────────────────────────────
 RenderAssets.pending ──► upload_assets_into ──► mesh arena (belt / mapped_at_creation)
                                              └► textures (write_texture + GPU mip blit)

 World ──┬─ lights (DirectionalLight ≤4, PointLight ≤16) ─────────────► GpuView (per camera)
         ├─ cameras (Camera3d + GlobalTransform, sorted by order) ─────► views + frustums
         ├─ ECS: (GlobalTransform, Mesh3d, Material, Visibility?)
         │      intern Material → id ; key = class|material|mesh ; sort
         │      write matrices into `ecs` TrackedBuffer (diff vs shadow) ─► ECS sources
         └─ InstancedMesh ─► InstanceBuffers[handle]
                sync dirty ranges ─► per-buffer GrowBuffer ───────────────► explicit sources

 sources sorted by (blend, pipeline class, layout, material, mesh, buffer)

 for each view:
   CpuDirect:  ECS → per-instance sphere test; all visible → draw from `ecs`
                     partial → visible matrices into `cpu_visible` (diffed)
               explicit → buffer-level AABB test → draw whole buffer or skip
   Gpu*:       each cullable source → CullJob + output region in `cull_out`
                                    + DrawIndexedIndirectArgs template
               jobs grouped per source buffer → one mark + one scatter dispatch each

 flush TrackedBuffers (only changed bytes) ─► StagingBelt ─► copy_buffer_to_buffer
 encoder: [uploads] → copy templates→args → clear counts
          → compute(mark → scan → scatter [+compact])
          → per view: [copy own render target → feedback texture] → render pass
          → submit → present → belt.recall → poll(Poll)
```

Everything above happens in **one command encoder and one submit per frame**.

## Instancing

### Path 1 — automatic ECS batching
Entities with `Mesh3d + Material + GlobalTransform` are gathered into `EcsItem`s (key + matrix),
sorted by `(pipeline class, material id, mesh)` and written contiguously into the persistent
`ecs` instance buffer (`InstanceTransform`, 64 B). Each run of equal keys becomes one *source*
→ one instanced draw. Materials are plain values; `MaterialTable` interns equal materials
(hash of their bit patterns) into a stable id with a slot in the material uniform buffer
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
| `Color` | linear tint × material color |
| `UvRect` | atlas rect (`uv * zw + xy`) |
| `TextureLayer` | texture-array layer (atlas page / animation frame) |
| `Custom(n)` | reserved for custom pipelines (ignored by the built-in shader) |

`pipeline.rs` generates the `InstanceInput` struct and accessor functions for each layout and
splices them into `forward.wgsl` (`instance_model`, `instance_color`, `instance_uv_rect`,
`instance_layer`). Pipelines are cached by `(layout, class, format, samples, manual_srgb)`.
Built-ins: `InstanceTransform` (64 B), `InstanceTransformColor` (80 B), `InstanceCompact`
(32 B: pos + uniform scale + f32 quat), `InstancePacked` (32 B: pos + scale + snorm16 quat +
unorm8 color + layer — 2.5× smaller than matrix + color).

`InstanceBuffer<T>` is user-owned CPU data registered in the `InstanceBuffers` resource. Every
mutating accessor (`set`, `get_mut`, `range_mut`, `push`, `swap_remove`, `as_mut_slice`, …)
records a dirty range. Usage hints:

* `Static` — GPU buffer sized exactly; after the first upload nothing is sent unless edited.
* `Dynamic` — dirty ranges only; GPU buffer grows 1.5×.
* `Stream` — rewritten each frame; uploaded whole every frame without range bookkeeping.

Bounds for CPU buffer-level culling are grown incrementally over dirty ranges and recomputed
exactly only when >50 % changed, after shrinking, or after a `set_bounds` override is lifted
(edits made while overridden are not tracked).

### Draw paths and GPU-driven culling

Chosen at startup from adapter capabilities (`RenderCapabilities::best_draw_path`), optionally
lowered at runtime via `RenderSettings::draw_path`; reported in `RenderStats::draw_path`.

| path | requirements | culling | draw submission |
|---|---|---|---|
| `GpuMultiDrawCount` | compute + `INDIRECT_EXECUTION` + `INDIRECT_FIRST_INSTANCE` + `MULTI_DRAW_INDIRECT_COUNT` (Vulkan, DX12) | compute | cull → compact non-empty args per run → `multi_draw_indexed_indirect_count` |
| `GpuMultiDraw` | compute + indirect + `INDIRECT_FIRST_INSTANCE` (Metal, WebGPU w/ feature) | compute | `multi_draw_indexed_indirect` per run |
| `GpuIndirect` | compute + indirect | compute | `draw_indexed_indirect` per batch; vertex buffer rebound at the batch's region (no `first_instance` needed) |
| `CpuDirect` | none (WebGL2, GLES, downlevel) | CPU spheres/AABBs | `draw_indexed` per batch |

GPU culling (`shaders/cull.wgsl`):

* A **job** per (view, source): source word offset, count, stride, how to decode position/scale
  (`CullSource::Matrix` or `TranslationScale`, f32 only), mesh bounding sphere, output region,
  indirect-args index. Jobs of the same source buffer share one dispatch; each workgroup finds
  its job by binary search over `wg_start`, so a dispatch covers any number of batches.
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
* `GpuMultiDrawCount` adds `compact.wgsl`: one workgroup per run moves the run's non-empty
  args to its front **in their original order** (block counts + prefix sum) and stores the
  count, so `multi_draw_indexed_indirect_count` skips fully culled batches.
* Layouts whose position is not f32, explicit buffers or an ECS batch larger than
  `max_storage_buffer_binding_size`, and output regions that would end past that binding in
  `cull_out` (which accumulates every view's regions) fall back to CPU culling + direct draws
  for that view, even on GPU paths.

Transparent materials (`alpha_blend`) sort after opaque batches (no per-instance depth sort).

## CPU → GPU transfer

### Staging: `Uploader` = `wgpu::util::StagingBelt`
All buffer uploads go through a StagingBelt (4 MiB chunks): data is copied **directly into
mapped staging memory** and a `copy_buffer_to_buffer` is recorded in the frame encoder. Chunks
are recycled only after the GPU finished with them (`finish` before submit, `recall` after), so
it is a frame-in-flight-aware ring: no write-after-read hazards, and no staging allocation in the
steady state. (`queue.write_buffer_with` was rejected: it allocates a fresh staging buffer per
call in wgpu-core.) Copies ≥ 1 MiB are split across rayon workers on native
(`copy_to_staging`, disjoint `WriteOnly` sub-slices).

### Change detection
* **User buffers** — explicit dirty ranges (`DirtyRanges`: sorted, merged when adjacent or within
  16 instances; above 64 ranges the closest pair is merged). >50 % dirty → one full copy.
* **Renderer-owned buffers** (`TrackedBuffer`: ECS instances, per-view visible instances, view
  uniforms, materials, cull jobs/params, args templates, draw meta) keep a CPU shadow; writes are
  compared with it and only differing byte ranges are uploaded. Result: an unchanged scene with a
  still camera uploads **0 bytes** per frame; moving one ECS entity uploads one 64-byte matrix.

### Persistent buffers and growth
`GrowBuffer` never reallocates per frame: capacity grows 1.5× (256-byte aligned) or exactly for
`Static` data; reallocation bumps a generation so dependent bind groups are rebuilt, and tracked
buffers re-upload their shadow. wgpu zero-initializes new buffers, which the shadow diff relies on.

### Mesh arena
All meshes live in one vertex buffer (36-byte interleaved vertex: position, normal, uv, sRGB
unorm8 color) and one `u32` index buffer, sub-allocated first-fit with free-range coalescing.
Each buffer grows (2×, at least by what is missing) only when *it* is short, and never beyond
the device's `max_buffer_size`; a mesh that cannot fit is logged, keeps its CPU data and is not
drawn. The regrown buffer is created `mapped_at_creation`: meshes landing in the fresh region
are written straight into the mapping, old contents are copied GPU→GPU. Updates of same-sized
meshes are written in place; removals free their ranges for reuse.

### Textures
`queue.write_texture` per layer/mip with the row pitch computed from the format's block size
(works for BC7/ASTC/ETC2 when `TEXTURE_COMPRESSION_*` is enabled — check
`Renderer::supports_texture_format`). Materials sample through a filterable-float binding, so
only formats that are filterable float on the device are accepted (`FLOAT32_FILTERABLE` is
requested when available); integer, depth and unsupported formats log an error and sample
white. All material textures are 2D arrays (layer chosen per instance). Missing mips of
formats the device can render to are generated on the GPU with a linear blit per level
(sRGB-correct); otherwise only the provided mips are uploaded.

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
| mesh arena vertex/index | VERTEX / INDEX | persistent, grows 2× | belt or mapped-at-creation on upload |
| `ecs` instances | VERTEX (+STORAGE) | persistent, 1.5× | shadow diff each frame |
| explicit instance buffers | VERTEX (+STORAGE) | per `InstanceBufferHandle`; freed on `remove` | dirty ranges |
| `cpu_visible` | VERTEX | persistent | shadow diff (CPU path, partially visible batches) |
| view uniforms | UNIFORM, dynamic offset per camera | persistent | shadow diff |
| material uniforms | UNIFORM, dynamic offset per material | persistent | on intern only |
| cull params / jobs / templates / draw meta | UNIFORM / STORAGE / COPY_SRC | persistent | shadow diff |
| `cull_out`, args (+ compaction records), compacted args, counts | STORAGE+VERTEX / STORAGE+INDIRECT | persistent, 1.5× | GPU only (compute, copy, clear) |
| run meta (multi-draw-count runs) | STORAGE | persistent | shadow diff |
| render-target feedback copies | TEXTURE_BINDING+COPY_DST | while a view samples its own target (+60 frames) | GPU copy before the pass |
| staging chunks | MAP_WRITE+COPY_SRC | recycled ring (StagingBelt) | CPU |
| capture readback | MAP_READ | per capture | GPU copy |

Every `GrowBuffer` (1.5× growth) is clamped to the device's `max_buffer_size`; callers keep the
requested size itself within it (explicit instance buffers over the limit are logged and not
drawn).

## No allocation in our code in the steady state
All per-frame scratch (`items`, `sources`, `cameras`, `views`, `draws`, `jobs`, `dispatches`)
lives in `FrameState` and is cleared, not dropped. Asset and instance registries are moved out
of the resource map with `mem::take`, and `render_system` borrows the `Renderer` with
`Resources::scope`, which moves its existing Box out and back (no re-boxing). Large staging
copies are split with a rayon parallel iterator, whose `join`-based jobs live on the stack
(`scope.spawn` would box every task). `RenderStats::cpu_reallocations` counts scratch growth
and is asserted to be 0 on steady frames in
`tests/instancing.rs::static_buffer_and_static_scene_upload_nothing_after_first_frame`.
wgpu's own per-frame bookkeeping (encoders, passes, submission, staging-belt recall) allocates
and is outside this guarantee.

## RenderStats
`draw_calls`, `indirect_draw_calls`, `multi_draw_calls`, `batches`, `instances`,
`instances_drawn`, `culled` (CPU-visible count; GPU culling results stay on the GPU),
`triangles`, `bytes_uploaded`, `instance_bytes_uploaded`, `buffer_reallocations`,
`cpu_reallocations`, `compute_dispatches`, `draw_path`, `gpu_culling`, `meshes_uploaded`,
`textures_uploaded`, `views`, `frame`.

## Headless capture and determinism
Headless renderers draw primary cameras into an offscreen `Rgba8UnormSrgb` target.
`Renderer::capture()` copies it into a `MAP_READ` buffer with rows padded to
`COPY_BYTES_PER_ROW_ALIGNMENT`, maps it via `map_buffer_on_submit` and returns a
`space3d_tasks::Task` (native: waits on a rayon worker; web: resolved by the browser/WebGL poll)
that strips the padding into `ImageData`. Windowed renderers re-record the last frame's primary
views into the offscreen target before copying (surface textures are not readable), at the
size those views were prepared for. `resize`/`set_msaa_samples` reallocate the headless target
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
default panic. A device-lost callback logs as well. On `CurrentSurfaceTexture::Outdated` the
surface is reconfigured; on `Lost` it is dropped (`has_surface()` turns false, frames render
offscreen) because a lost surface must be recreated from the window with `set_surface`, and
reconfiguring it fails validation.

## Platform fallbacks

| platform | backend | draw path (typical) | notes |
|---|---|---|---|
| Windows | DX12 / Vulkan | `GpuMultiDrawCount` | |
| Linux | Vulkan (GLES fallback) | `GpuMultiDrawCount` / `CpuDirect` on GLES | GLES gets the winit display handle via `new_with_surface_and_display` |
| macOS / iOS | Metal | `GpuMultiDraw` | multi-draw is emulated by wgpu-hal as a loop |
| Android | Vulkan / GLES | `GpuMultiDraw*` / `CpuDirect` | surface dropped on suspend, recreated on resume |
| Web (WebGPU) | browser WebGPU | `GpuIndirect` or `GpuMultiDraw` (`indirect-first-instance`) | renderer created asynchronously |
| Web (WebGL2) | wgpu GL | `CpuDirect` | no storage buffers/compute; surfaces without sRGB format use in-shader sRGB encoding |

## Known limitations
* No shadows, IBL or post-processing; lighting is metallic/roughness Blinn-Phong with Schlick
  Fresnel, 4 directional + 16 point lights.
* Material textures (including single-layer ones and render targets) are bound as `D2Array`
  views. wgpu's GL backend binds a single-layer texture with its `TEXTURE_2D` target, so on
  WebGL2/GLES textured materials can sample black (not verified on a GL device).
* The platform runner recreates a surface only on resume; after a `Lost` surface the window
  stays blank until then.
* GPU-culled visible counts are not read back (stats report pre-cull counts on GPU paths).
* Transparent instances are not depth-sorted within a batch.
* `InstanceSemantic::Custom` attributes need a custom pipeline, which is not exposed yet.
