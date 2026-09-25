//! The particles render node, `"space3d/particles"` in `RenderNodes` (graphics spec §2.5).
//!
//! * Slots: `[Transparent @100 (reading DEPTH iff ParticleSettings::soft_particles),
//!   Distortion @100]`. Soft particles therefore draw in the view's SceneRead pass (§1.6), after
//!   the built-in blended meshes, as one slot, so their own back-to-front order is never split.
//! * `prepare` (once per frame, inside `render_frame`): replay the pending ticks of the tick log
//!   — with compute kernels recorded into the frame's encoder (GPU backend, `crate::gpu`) or in
//!   Rust (CPU backend, `crate::cpu`) — build the render instances (GPU compaction or CPU pack),
//!   cull and sort the draws of every camera view, and upload what changed through the
//!   renderer's staging ring. A frame without new ticks, local layers or extrapolation uploads
//!   nothing.
//! * `draw`: bind groups 0 (view) and 1 (scene depth) of the frame, group 2 (LUT, per-view
//!   flags, per-layer draw parameters), one instanced strip per draw item (an indirect draw on
//!   the GPU backend, its instance count written by the compaction).
//! * Pipelines per `PassTarget`, built in `prewarm` (WebGL2 links programs at creation). The
//!   shader reads scene depth according to `PassTarget::depth_access`: the hardware test plus
//!   `s3d_soft_fade` with a read-only attachment, a manual `s3d_scene_occluded` test plus the
//!   fade without one (GL), and the hardware test alone in the MSAA in-pass fallback.
//! * Colors go through `s3d_output` with the pass encoding, exposure and tonemapper, so particles
//!   match meshes in every frame mode; in `FrameMode::Direct` (no tonemapper) they soft-clip
//!   with `1 − e^−x` (`ParticleSettings::direct_soft_clip`).

mod layers;

pub(crate) use layers::RenderLayers;

use crate::cpu::{CpuSim, DIRECT, INSTANCE_SIZE, PackInput, PackItem, Particle, ParticleInstance, pack};
use crate::gpu::{GpuInput, GpuSim, PARTICLE_SIZE};
use crate::runtime::{ParticleRuntime, Runtime};
use crate::{ParticleBackend, ParticleEffects, ParticleSettings, ParticleStats, resolve_backend};
use layers::GpuDrawParams;
use space3d_core::glam::{Quat, Vec3};
use space3d_core::{Entity, GlobalTransform, Time};
use space3d_render::wgpu;
use space3d_render::{
    Aabb, DepthAccess, DrawContext, FrameMode, OUTPUT_WGSL, OutputEncoding, PassTarget, PhaseSlot, PrepareContext,
    RenderNode, RenderNodes, RenderPhase, SceneReads, SlotMask, VIEW_WGSL, ViewPrepareContext, scene_wgsl_for,
};

/// Name of the node in `RenderNodes`.
pub(crate) const NODE_NAME: &str = "space3d/particles";
/// Slot of color sprites.
pub(crate) const SLOT_COLOR: usize = 0;

const SPRITE_WGSL: &str = include_str!("../shaders/sprite.wgsl");

static SLOTS_SOFT: [PhaseSlot; 2] = [
    PhaseSlot::new(RenderPhase::Transparent, 100).reading(SceneReads::DEPTH),
    PhaseSlot::new(RenderPhase::Distortion, 100),
];
static SLOTS_HARD: [PhaseSlot; 2] = [
    PhaseSlot::new(RenderPhase::Transparent, 100),
    PhaseSlot::new(RenderPhase::Distortion, 100),
];

/// Premultiplied "over" for every particle blend (additive = alpha 0).
pub(crate) const PARTICLE_BLEND: wgpu::BlendState = wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING;

/// Per-view flags (`S3dpView`, 32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuParticleView {
    flags: [u32; 4],
    params: [f32; 4],
}

/// The draws of one camera view: `view_items[start..end]`, uniform slot `uniform`.
#[derive(Clone, Copy, Debug)]
struct ViewList {
    camera: Entity,
    start: u32,
    end: u32,
    uniform: u32,
}

/// The per-instance vertex layout of `ParticleInstance`.
const INSTANCE_ATTRIBUTES: [wgpu::VertexAttribute; 4] =
    wgpu::vertex_attr_array![0 => Float32x4, 1 => Float16x4, 2 => Float16x4, 3 => Uint32x2];

/// The sprite shader for a pass signature.
pub(crate) fn sprite_shader(target: &PassTarget) -> String {
    sprite_shader_for(target.output, target.depth_access)
}

/// The sprite shader for an output encoding and a scene depth access.
pub(crate) fn sprite_shader_for(output: OutputEncoding, access: DepthAccess) -> String {
    format!(
        "const S3DP_OUTPUT: u32 = {}u;\nconst S3DP_MANUAL_DEPTH: bool = {};\n{VIEW_WGSL}\n{}\n{OUTPUT_WGSL}\n{SPRITE_WGSL}",
        output.id(),
        access == DepthAccess::SampledOnly,
        scene_wgsl_for(access),
    )
}

/// GPU state of one renderer.
struct Gpu {
    renderer: u64,
    align: u64,
    bind_layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    sampler: wgpu::Sampler,
    lut: Option<(wgpu::Texture, wgpu::TextureView, u32)>,
    lut_version: Option<u64>,
    draws: Option<(wgpu::Buffer, u64)>,
    draws_version: Option<u64>,
    views: Option<(wgpu::Buffer, u64)>,
    view_mirror: Vec<GpuParticleView>,
    instances: Option<(wgpu::Buffer, u64)>,
    bind_group: Option<wgpu::BindGroup>,
    pipelines: Vec<(PassTarget, wgpu::RenderPipeline)>,
    /// Scratch for the draw-parameter upload.
    bytes: Vec<u8>,
}

impl Gpu {
    fn new(device: &wgpu::Device, renderer: u64, view: &wgpu::BindGroupLayout, scene: &wgpu::BindGroupLayout) -> Self {
        let align = device.limits().min_uniform_buffer_offset_alignment.max(16) as u64;
        let uniform = |binding, size: u64| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(size),
            },
            count: None,
        };
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d particles"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                uniform(2, std::mem::size_of::<GpuParticleView>() as u64),
                uniform(3, std::mem::size_of::<GpuDrawParams>() as u64),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("space3d particles"),
            bind_group_layouts: &[Some(view), Some(scene), Some(&bind_layout)],
            immediate_size: 0,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("space3d particles LUT"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            renderer,
            align,
            bind_layout,
            pipeline_layout,
            sampler,
            lut: None,
            lut_version: None,
            draws: None,
            draws_version: None,
            views: None,
            view_mirror: Vec::new(),
            instances: None,
            bind_group: None,
            pipelines: Vec::new(),
            bytes: Vec::new(),
        }
    }

    /// The pipeline for `target` (built on first use).
    fn pipeline(&mut self, device: &wgpu::Device, target: PassTarget) -> usize {
        if let Some(i) = self.pipelines.iter().position(|(t, _)| *t == target) {
            return i;
        }
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("space3d particles sprite"),
            source: wgpu::ShaderSource::Wgsl(sprite_shader(&target).into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("space3d particles sprite"),
            layout: Some(&self.pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_sprite"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: INSTANCE_SIZE,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &INSTANCE_ATTRIBUTES,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            // Blended: test against the scene depth, never write it.
            depth_stencil: target.depth_stencil(wgpu::CompareFunction::LessEqual, false),
            multisample: target.multisample(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_sprite"),
                compilation_options: Default::default(),
                targets: &[Some(target.color_target(Some(PARTICLE_BLEND)))],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipelines.push((target, pipeline));
        self.pipelines.len() - 1
    }
}

/// Grow-only GPU buffer: `Some(new buffer)` when `current` is missing or too small.
fn ensure_buffer(
    device: &wgpu::Device,
    current: &mut Option<(wgpu::Buffer, u64)>,
    size: u64,
    usage: wgpu::BufferUsages,
    label: &'static str,
) -> bool {
    if current.as_ref().is_some_and(|(_, s)| *s >= size) {
        return false;
    }
    let size = size.max(256).next_power_of_two();
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    *current = Some((buffer, size));
    true
}

/// Render pose (position, rotation) of every effect instance: its entity's `GlobalTransform`
/// while attached, else its last emitter pose.
fn update_poses(poses: &mut Vec<(Vec3, Quat)>, rt: &Runtime, world: &space3d_core::World) {
    poses.clear();
    for inst in &rt.instances {
        let pose = (inst.live && inst.attached && !inst.orphaned)
            .then(|| world.get::<&GlobalTransform>(inst.entity).ok())
            .flatten()
            .map(|g| {
                let (_, r, t) = g.0.to_scale_rotation_translation();
                (t, r)
            });
        poses.push(pose.unwrap_or((inst.pos, inst.rot)));
    }
}

/// The particles render node (registered by `ParticlesPlugin`).
pub(crate) struct ParticlesNode {
    soft: bool,
    enabled: bool,
    sim: CpuSim,
    /// The GPU backend's state (per renderer).
    gpu_sim: Option<GpuSim>,
    /// Backend of the last frame.
    backend: Option<ParticleBackend>,
    layers: RenderLayers,
    instances: Vec<ParticleInstance>,
    items: Vec<PackItem>,
    keys: Vec<(u32, u64, u32)>,
    poses: Vec<(Vec3, Quat)>,
    view_items: Vec<u32>,
    views: Vec<ViewList>,
    view_uniforms: Vec<GpuParticleView>,
    /// The packed instances changed since they were last uploaded.
    dirty: bool,
    packed_once: bool,
    last_extrapolation: f32,
    gpu: Option<Gpu>,
    warned_gpu_backend: bool,
    /// The frame bind groups of the last color draw (perf tests draw with them).
    #[cfg(test)]
    pub(crate) test_capture: Option<(wgpu::BindGroup, wgpu::BindGroup, u32, PassTarget)>,
}

/// What `simulate` reports to the stats.
struct Simulated {
    settings: ParticleSettings,
    reallocs: u32,
    ticks: u32,
    alive: u32,
    alive_frame: Option<u64>,
    dispatches: u32,
    bytes: u64,
}

impl ParticlesNode {
    pub(crate) fn new(settings: &ParticleSettings) -> Self {
        Self {
            soft: settings.soft_particles,
            enabled: settings.enabled,
            sim: CpuSim::default(),
            gpu_sim: None,
            backend: None,
            layers: RenderLayers::default(),
            instances: Vec::new(),
            items: Vec::new(),
            keys: Vec::new(),
            poses: Vec::new(),
            view_items: Vec::new(),
            views: Vec::new(),
            view_uniforms: Vec::new(),
            dirty: false,
            packed_once: false,
            last_extrapolation: 0.0,
            gpu: None,
            warned_gpu_backend: false,
            #[cfg(test)]
            test_capture: None,
        }
    }

    fn capacities(&self) -> [usize; 8] {
        [
            self.instances.capacity(),
            self.items.capacity(),
            self.keys.capacity(),
            self.poses.capacity(),
            self.view_items.capacity(),
            self.views.capacity(),
            self.view_uniforms.capacity(),
            self.gpu_sim.as_ref().map_or(0, GpuSim::capacities),
        ]
    }

    /// Replay the log on the frame's backend and build the draw items; publishes nothing.
    fn simulate(&mut self, ctx: &mut PrepareContext<'_>) -> Option<Simulated> {
        let settings = ctx.resources.get::<ParticleSettings>().cloned().unwrap_or_default();
        if !ctx.resources.contains::<ParticleRuntime>() {
            return None;
        }
        if settings.soft_particles != self.soft {
            self.soft = settings.soft_particles;
            if let Some(nodes) = ctx.resources.get_mut::<RenderNodes>() {
                nodes.invalidate();
            }
        }
        // Device lost: the CPU backend carries on until the renderer is recreated.
        let compute = ctx.graphics.compute && !ctx.frame.device_lost;
        let backend = resolve_backend(settings.backend, Some(compute));
        if settings.backend == ParticleBackend::Gpu && backend == ParticleBackend::Cpu && !self.warned_gpu_backend {
            self.warned_gpu_backend = true;
            tracing::warn!("ParticleBackend::Gpu needs compute shaders and indirect draws; simulating on the CPU");
        }
        let switched = self.backend.is_some_and(|b| b != backend);
        if switched && backend == ParticleBackend::Cpu {
            // The GPU pool cannot be read back without waiting: the live particles are dropped.
            self.sim.pool.fill(Particle::default());
        }
        self.backend = Some(backend);
        let rebuilt = ctx
            .resources
            .get::<ParticleEffects>()
            .is_some_and(|fx| self.layers.sync(fx));
        let dt = ctx
            .resources
            .get::<Time>()
            .map_or(0.0, |t| t.fixed_delta_secs() * settings.time_scale.max(0.0));
        let extrapolate = if settings.extrapolate {
            ctx.frame.alpha * dt
        } else {
            0.0
        };
        // The runtime moves out for the frame (no allocation), so the context stays free for
        // uploads and compute passes.
        let mut rt = std::mem::take(&mut ctx.resources.get_mut::<ParticleRuntime>()?.rt);
        let mut out = Simulated {
            settings,
            reallocs: 0,
            ticks: 0,
            alive: 0,
            alive_frame: None,
            dispatches: 0,
            bytes: 0,
        };
        let layers = &self.layers;
        // Distortion layers draw into the post distortion buffer (PA4), not color.
        let layer_of = |e: &crate::runtime::Entry| {
            layers
                .layer(e.effect, e.layer)
                .filter(|l| !l.distortion)
                .map(|l| l.pack)
        };
        match backend {
            ParticleBackend::Gpu => {
                if self.gpu_sim.as_ref().is_none_or(|g| g.renderer != ctx.frame.renderer) {
                    self.gpu_sim = Some(GpuSim::new(ctx.device, ctx.frame.renderer));
                }
                update_poses(&mut self.poses, &rt, ctx.world);
                let gpu = self.gpu_sim.as_mut().expect("created above");
                let carry = switched.then_some(&self.sim.pool[..]);
                gpu.prepare(
                    ctx,
                    &mut rt,
                    &layer_of,
                    &GpuInput {
                        poses: &self.poses,
                        extrapolate,
                        dt,
                        max_catchup: out.settings.max_catchup_ticks,
                        carry,
                    },
                    &mut self.items,
                );
                let s = gpu.stats;
                (out.ticks, out.dispatches, out.bytes, out.reallocs) = (s.ticks, s.dispatches, s.bytes, s.created);
                if let Some((frame, alive)) = gpu.readback.latest {
                    (out.alive, out.alive_frame) = (alive, Some(frame));
                }
                self.instances.clear();
            }
            _ => {
                out.reallocs += self.sim.replay(&mut rt, out.settings.max_catchup_ticks);
                out.ticks = self.sim.ticks;
                let has_local = rt.entries.iter().any(|e| e.live && e.sim.local);
                if switched
                    || !self.packed_once
                    || self.sim.ticks > 0
                    || rebuilt
                    || has_local
                    || extrapolate != self.last_extrapolation
                {
                    self.packed_once = true;
                    self.last_extrapolation = extrapolate;
                    update_poses(&mut self.poses, &rt, ctx.world);
                    pack(
                        &PackInput {
                            rt: &rt,
                            pool: &self.sim.pool,
                            poses: &self.poses,
                            extrapolate,
                        },
                        &layer_of,
                        &mut self.keys,
                        &mut self.instances,
                        &mut self.items,
                    );
                    self.dirty = true;
                }
                out.alive = self.instances.len() as u32;
                out.alive_frame = Some(ctx.frame.frame);
            }
        }
        out.reallocs += rt.reallocations;
        rt.reallocations = 0;
        if let Some(r) = ctx.resources.get_mut::<ParticleRuntime>() {
            r.rt = rt;
        }
        Some(out)
    }

    /// The backend of the last frame and a copy of its pool (tests; waits for the GPU).
    pub(crate) fn read_pool(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Option<(ParticleBackend, Vec<Particle>)> {
        match self.backend? {
            ParticleBackend::Gpu => {
                let pool = self.gpu_sim.as_ref()?.pool_buffer()?;
                let size = pool.size();
                let staging = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("space3d particles pool readback (tests)"),
                    size,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let mut encoder = device.create_command_encoder(&Default::default());
                encoder.copy_buffer_to_buffer(pool, 0, &staging, 0, size);
                queue.submit([encoder.finish()]);
                let slice = staging.slice(..);
                slice.map_async(wgpu::MapMode::Read, |_| {});
                device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
                let data = slice.get_mapped_range().ok()?;
                let pool: Vec<Particle> =
                    bytemuck::cast_slice(&data[..(size / PARTICLE_SIZE * PARTICLE_SIZE) as usize]).to_vec();
                Some((ParticleBackend::Gpu, pool))
            }
            b => Some((b, self.sim.pool.clone())),
        }
    }

    /// Cull and sort the draw items of every camera view; per-view flags.
    fn plan_views(&mut self, ctx: &PrepareContext<'_>, settings: &ParticleSettings) {
        self.view_items.clear();
        self.views.clear();
        self.view_uniforms.clear();
        for (vi, v) in ctx.views.iter().enumerate() {
            let start = self.view_items.len();
            if settings.enabled {
                for (i, it) in self.items.iter().enumerate() {
                    // Unbounded items (world attractors on the GPU backend) are never culled.
                    let unbounded = !(it.min.is_finite() && it.max.is_finite());
                    if unbounded
                        || v.frustum.intersects_aabb(&Aabb {
                            min: it.min,
                            max: it.max,
                        })
                    {
                        self.view_items.push(i as u32);
                    }
                }
                let (items, layers, view) = (&self.items, &self.layers.info, v.view);
                // Order, then alpha/premultiplied before additive, then back to front for sorted
                // layers; ties by layer and allocation serial (deterministic).
                let key = |i: u32| {
                    let it = &items[i as usize];
                    let l = &layers[it.layer as usize];
                    let depth = if l.pack.batched {
                        0.0
                    } else {
                        view.transform_point3(it.center).z
                    };
                    (l.order, l.additive, depth, it.layer, it.serial)
                };
                self.view_items[start..].sort_unstable_by(|&a, &b| {
                    let (ka, kb) = (key(a), key(b));
                    ka.0.cmp(&kb.0)
                        .then(ka.1.cmp(&kb.1))
                        .then(ka.2.total_cmp(&kb.2))
                        .then(ka.3.cmp(&kb.3))
                        .then(ka.4.cmp(&kb.4))
                });
            }
            self.views.push(ViewList {
                camera: v.camera,
                start: start as u32,
                end: self.view_items.len() as u32,
                uniform: vi as u32,
            });
            let soft_clip = settings.direct_soft_clip && v.mode == FrameMode::Direct;
            self.view_uniforms.push(GpuParticleView {
                flags: [soft_clip as u32, settings.soft_particles as u32, 0, 0],
                params: [settings.max_screen_size.max(0.0), 0.0, 0.0, 0.0],
            });
        }
    }

    /// Create or refresh the GPU resources and upload what changed; returns the bytes uploaded
    /// and the buffers or textures (re)created.
    fn upload(&mut self, ctx: &mut PrepareContext<'_>) -> (u64, u32) {
        let device = ctx.device;
        if self.gpu.as_ref().is_none_or(|g| g.renderer != ctx.frame.renderer) {
            self.gpu = Some(Gpu::new(
                device,
                ctx.frame.renderer,
                &ctx.layouts.view,
                &ctx.layouts.scene,
            ));
            self.dirty = true;
        }
        let Some(gpu) = self.gpu.as_mut() else {
            return (0, 0);
        };
        let (mut bytes, mut created) = (0u64, 0u32);
        let mut rebind = gpu.bind_group.is_none();

        // The curve LUT (only when the effects changed).
        if gpu.lut_version != self.layers.version {
            let rows = self.layers.rows.max(1);
            if gpu.lut.as_ref().is_none_or(|l| l.2 != rows) {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("space3d particles LUT"),
                    size: wgpu::Extent3d {
                        width: crate::effect::LUT_WIDTH,
                        height: rows,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba16Float,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                gpu.lut = Some((texture, view, rows));
                created += 1;
                rebind = true;
            }
            if let Some((texture, _, rows)) = &gpu.lut {
                let data: &[u8] = bytemuck::cast_slice(&self.layers.lut);
                ctx.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(crate::effect::LUT_WIDTH * 8),
                        rows_per_image: Some(*rows),
                    },
                    wgpu::Extent3d {
                        width: crate::effect::LUT_WIDTH,
                        height: *rows,
                        depth_or_array_layers: 1,
                    },
                );
                bytes += data.len() as u64;
            }
            gpu.lut_version = self.layers.version;
        }

        // Draw parameters, one aligned slot per render layer.
        let align = gpu.align;
        if gpu.draws_version != self.layers.version {
            let n = self.layers.params.len().max(1) as u64;
            if ensure_buffer(
                device,
                &mut gpu.draws,
                n * align,
                wgpu::BufferUsages::UNIFORM,
                "space3d particles draws",
            ) {
                created += 1;
                rebind = true;
            }
            gpu.bytes.clear();
            for p in &self.layers.params {
                gpu.bytes.extend_from_slice(bytemuck::bytes_of(p));
                gpu.bytes.resize(gpu.bytes.len().next_multiple_of(align as usize), 0);
            }
            if let (Some((buffer, _)), false) = (&gpu.draws, gpu.bytes.is_empty()) {
                ctx.upload(buffer, 0, &gpu.bytes);
                bytes += gpu.bytes.len() as u64;
            }
            gpu.draws_version = self.layers.version;
        }

        // Per-view flags (only the views whose flags changed).
        let n = self.view_uniforms.len().max(1) as u64;
        if ensure_buffer(
            device,
            &mut gpu.views,
            n * align,
            wgpu::BufferUsages::UNIFORM,
            "space3d particles views",
        ) {
            created += 1;
            rebind = true;
            gpu.view_mirror.clear();
        }
        if let Some((buffer, _)) = &gpu.views {
            for (i, u) in self.view_uniforms.iter().enumerate() {
                if gpu.view_mirror.get(i) != Some(u) {
                    ctx.upload(buffer, i as u64 * align, bytemuck::bytes_of(u));
                    bytes += std::mem::size_of::<GpuParticleView>() as u64;
                    if i < gpu.view_mirror.len() {
                        gpu.view_mirror[i] = *u;
                    } else {
                        gpu.view_mirror.push(*u);
                    }
                }
            }
        }

        // Instances (only after a repack).
        if self.dirty {
            let size = self.instances.len() as u64 * INSTANCE_SIZE;
            if ensure_buffer(
                device,
                &mut gpu.instances,
                size,
                wgpu::BufferUsages::VERTEX,
                "space3d particles instances",
            ) {
                created += 1;
            }
            if let (Some((buffer, _)), true) = (&gpu.instances, size > 0) {
                ctx.upload(buffer, 0, bytemuck::cast_slice(&self.instances));
                bytes += size;
            }
            self.dirty = false;
        }

        if rebind && let (Some((_, lut, _)), Some((draws, _)), Some((views, _))) = (&gpu.lut, &gpu.draws, &gpu.views) {
            gpu.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d particles"),
                layout: &gpu.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(lut),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: views,
                            offset: 0,
                            size: wgpu::BufferSize::new(std::mem::size_of::<GpuParticleView>() as u64),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: draws,
                            offset: 0,
                            size: wgpu::BufferSize::new(std::mem::size_of::<GpuDrawParams>() as u64),
                        }),
                    },
                ],
            }));
        }
        (bytes, created)
    }
}

impl RenderNode for ParticlesNode {
    fn slots(&self) -> &[PhaseSlot] {
        if self.soft { &SLOTS_SOFT } else { &SLOTS_HARD }
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        let before = self.capacities();
        let Some(sim) = self.simulate(ctx) else {
            return;
        };
        self.enabled = sim.settings.enabled;
        self.plan_views(ctx, &sim.settings);
        let (bytes, created) = if ctx.frame.device_lost {
            (0, 0)
        } else {
            self.upload(ctx)
        };
        let after = self.capacities();
        let node_reallocs = before.iter().zip(&after).filter(|(a, b)| a != b).count() as u32;
        let draw_calls = self.view_items.len() as u32;
        let backend = self.backend;
        let resets = self.sim.resets + self.gpu_sim.as_ref().map_or(0, |g| g.resets);
        if let Some(runtime) = ctx.resources.get_mut::<ParticleRuntime>() {
            let rt = &mut runtime.rt;
            let (effects, layer_instances) = rt.counts();
            let stats = ParticleStats {
                backend,
                effects,
                layer_instances,
                pool_capacity: rt.pool.total(),
                pool_reserved: rt.pool.reserved(),
                alive: sim.alive,
                alive_frame: sim.alive_frame,
                spawned: rt.spawned,
                refused: rt.refused,
                shrunk: rt.shrunk,
                ticks_simulated: sim.ticks,
                catchup_resets: resets,
                dispatches: sim.dispatches,
                draw_calls,
                ribbon_segments: 0,
                bytes_uploaded: bytes + sim.bytes,
                cpu_reallocations: sim.reallocs + node_reallocs + created,
            };
            rt.refused = 0;
            rt.shrunk = 0;
            if let Some(s) = ctx.resources.get_mut::<ParticleStats>() {
                *s = stats;
            }
        }
    }

    fn prepare_view(&mut self, ctx: &mut ViewPrepareContext<'_>) -> SlotMask {
        if !self.enabled || self.gpu.as_ref().is_none_or(|g| g.bind_group.is_none()) {
            return SlotMask::NONE;
        }
        match self.views.iter().find(|l| l.camera == ctx.view.camera) {
            Some(l) if l.end > l.start => SlotMask::only(SLOT_COLOR),
            _ => SlotMask::NONE,
        }
    }

    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        for &(slot, target) in targets {
            if slot == SLOT_COLOR {
                gpu.pipeline(device, target);
            }
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        if ctx.slot != SLOT_COLOR {
            return;
        }
        let Some(list) = self.views.iter().find(|l| l.camera == ctx.view.camera).copied() else {
            return;
        };
        #[cfg(test)]
        {
            self.test_capture = Some((
                ctx.view_bind_group.clone(),
                ctx.scene_bind_group.clone(),
                ctx.view.view_offset,
                ctx.target,
            ));
        }
        self.record(
            pass,
            ctx.device,
            ctx.target,
            (ctx.view_bind_group, ctx.scene_bind_group, ctx.view.view_offset),
            list,
        );
    }
}

impl ParticlesNode {
    /// Record the draws of `list` with the frame's view and scene bind groups.
    fn record(
        &mut self,
        pass: &mut wgpu::RenderPass<'static>,
        device: &wgpu::Device,
        target: PassTarget,
        (view_bind_group, scene_bind_group, view_offset): (&wgpu::BindGroup, &wgpu::BindGroup, u32),
        list: ViewList,
    ) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        if list.end == list.start || gpu.bind_group.is_none() {
            return;
        }
        let p = gpu.pipeline(device, target);
        let Some(bind_group) = &gpu.bind_group else {
            return;
        };
        let direct = gpu.instances.as_ref().map(|(b, _)| b);
        let indirect = self.gpu_sim.as_ref().and_then(GpuSim::draw_buffers);
        pass.set_pipeline(&gpu.pipelines[p].1);
        pass.set_bind_group(0, view_bind_group, &[view_offset]);
        pass.set_bind_group(1, scene_bind_group, &[]);
        let view_uniform = (list.uniform as u64 * gpu.align) as u32;
        for &i in &self.view_items[list.start as usize..list.end as usize] {
            let item = &self.items[i as usize];
            let from = item.first as u64 * INSTANCE_SIZE;
            let to = from + item.count as u64 * INSTANCE_SIZE;
            pass.set_bind_group(2, bind_group, &[view_uniform, (item.layer as u64 * gpu.align) as u32]);
            if item.args == DIRECT {
                let Some(instances) = direct else { continue };
                pass.set_vertex_buffer(0, instances.slice(from..to));
                pass.draw(0..4, 0..item.count);
            } else {
                let (Some((work, instances)), Some(sim)) = (indirect, self.gpu_sim.as_ref()) else {
                    continue;
                };
                pass.set_vertex_buffer(0, instances.slice(from..to));
                pass.draw_indirect(work, sim.args_offset(item.args));
            }
        }
    }

    /// The GPU backend's state (tests).
    pub(crate) fn gpu_sim(&mut self) -> Option<&mut GpuSim> {
        self.gpu_sim.as_mut()
    }

    /// Record the first view's draws again, into a fresh `size` target of the last color draw's
    /// pass signature (depth cleared to far), inside a render pass writing timestamps `index`
    /// and `index + 1` (the perf tests time the particle draws alone). Returns the draw calls.
    #[cfg(test)]
    pub(crate) fn record_timed_draws(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        queries: &wgpu::QuerySet,
        index: u32,
        size: (u32, u32),
    ) -> Option<u32> {
        let (view_bg, scene_bg, view_offset, target) = self.test_capture.clone()?;
        let list = *self.views.first()?;
        let texture = |format, usage| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("space3d particles timed draws"),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: target.samples,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let color = texture(target.color, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let depth = target.depth.map(|f| texture(f, wgpu::TextureUsages::RENDER_ATTACHMENT));
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("particles.draw (perf test)"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: depth.as_ref().map(|view| wgpu::RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: (!target.depth_read_only).then_some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: Some(wgpu::RenderPassTimestampWrites {
                    query_set: queries,
                    beginning_of_pass_write_index: Some(index),
                    end_of_pass_write_index: Some(index + 1),
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        self.record(&mut pass, device, target, (&view_bg, &scene_bg, view_offset), list);
        Some(list.end - list.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use naga::back::glsl;

    /// The sprite shader validates for every depth access and output encoding, and translates
    /// to GLSL ES 3.00 (WebGL2) for both stages, the way wgpu's GL backend does.
    #[test]
    fn sprite_shader_validates_and_translates_to_webgl2() {
        let options = glsl::Options {
            version: glsl::Version::Embedded {
                version: 300,
                is_webgl: true,
            },
            writer_flags: glsl::WriterFlags::ADJUST_COORDINATE_SPACE | glsl::WriterFlags::FORCE_POINT_SIZE,
            binding_map: Default::default(),
            zero_initialize_workgroup_memory: true,
        };
        for access in [
            DepthAccess::None,
            DepthAccess::ReadOnlyAttached,
            DepthAccess::SampledOnly,
        ] {
            for output in [
                OutputEncoding::Linear,
                OutputEncoding::ManualSrgb,
                OutputEncoding::Tonemapped,
            ] {
                let src = sprite_shader_for(output, access);
                let module =
                    naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
                let info = naga::valid::Validator::new(
                    naga::valid::ValidationFlags::all(),
                    naga::valid::Capabilities::empty(),
                )
                .validate(&module)
                .unwrap_or_else(|e| panic!("{access:?} {output:?}: {e:?}"));
                for (stage, entry) in [
                    (naga::ShaderStage::Vertex, "vs_sprite"),
                    (naga::ShaderStage::Fragment, "fs_sprite"),
                ] {
                    let pipeline = glsl::PipelineOptions {
                        shader_stage: stage,
                        entry_point: entry.to_string(),
                        multiview: None,
                    };
                    let mut out = String::new();
                    glsl::Writer::new(&mut out, &module, &info, &options, &pipeline, Default::default())
                        .and_then(|mut w| w.write().map(drop))
                        .unwrap_or_else(|e| panic!("{access:?} {output:?} {stage:?}: {e}"));
                    // No storage buffers in any stage (WebGL2), and the vertex shader fetches
                    // curves through `textureLod`.
                    assert!(!out.contains("buffer "), "{stage:?} uses a storage buffer");
                    if stage == naga::ShaderStage::Vertex {
                        assert!(out.contains("textureLod"), "LUT fetch");
                    }
                }
            }
        }
    }

    /// The instance layout matches the vertex attributes: 40 bytes, WebGL2-safe formats.
    #[test]
    fn instance_layout() {
        assert_eq!(INSTANCE_SIZE, 40);
        let offsets: Vec<u64> = INSTANCE_ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, [0, 16, 24, 32]);
        assert_eq!(std::mem::size_of::<GpuParticleView>(), 32);
        assert_eq!(std::mem::size_of::<GpuDrawParams>(), 80);
        assert!(SLOTS_SOFT[0].reads.contains(SceneReads::DEPTH));
        assert!(!SLOTS_HARD[0].reads.contains(SceneReads::DEPTH));
        assert_eq!(SLOTS_SOFT[1].phase, RenderPhase::Distortion);
    }
}
