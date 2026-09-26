//! Post-processing.
//!
//! [`api`] is the public hub (re-exported from the crate root). The rest is the crate-internal
//! contract between the frame and the post area (graphics spec §2.6): the frame plans post
//! groups, hands each one's scene color, depth and distortion buffer to
//! [`PostState::encode_group`] and owns the pass slot (targets, formats, capture replay); post
//! owns the composite and its effects. The frame milestone F4 wrote the composite-only chain
//! (exposure, tonemapping, dither, alpha, rect mapping, upscale; `chain.rs`,
//! `shaders/post/composite.wgsl`); the post milestones add the effects: bloom (PO1,
//! `bloom.rs`), FXAA (`fxaa.rs`) and the vignette, chromatic aberration and film grain in the
//! composite (PO3); auto exposure, grading and the distortion apply are `STUB(P-POST)` until
//! PO2, PO4 and PO5. Distortion buffers are real (the frame's Distortion phase plumbing, F5,
//! clears and writes them).

pub(crate) mod api;
mod bloom;
mod chain;
mod distortion_mesh;
mod fxaa;

use crate::caps::GraphicsCaps;
use crate::frame::PoolDesc;
use crate::gpu::{TrackedBuffer, Uploader};
use crate::handles::FastMap;
use crate::node::{FULLSCREEN_WGSL, FrameInfo, FrameMode, RenderNodes};
use crate::output::{HASH_WGSL, OUTPUT_WGSL, OutputEncoding};
use crate::settings::FrameSettings;
use crate::stats::RenderStats;
use api::{AntiAliasing, BloomComposite, DISTORTION_FORMAT, DistortionResolution, FxaaQuality, PostProcess};
use bloom::{BloomPlan, BloomState};
use chain::{GpuPost, SeedKey};
use fxaa::{FxaaModules, LDR_FORMAT};
use space3d_core::{Entity, World};
use std::borrow::Cow;

/// Source of the composite pass (`shaders/post/composite.wgsl` after the output tail, the hash
/// functions and the full-screen triangle).
pub(crate) const COMPOSITE_WGSL: &str = include_str!("../shaders/post/composite.wgsl");

pub(crate) fn composite_source() -> String {
    format!("{OUTPUT_WGSL}\n{HASH_WGSL}\n{FULLSCREEN_WGSL}\n{COMPOSITE_WGSL}")
}

/// One post group of the frame plan (graphics spec §1.3): the settings camera, its rect and the
/// storage of its target group.
pub(crate) struct PostGroupPlan {
    /// The settings camera (highest-order post camera of the rect).
    #[allow(
        dead_code,
        reason = "graphics contract (spec §2.6): per-camera history of the post area"
    )]
    pub camera: Entity,
    pub settings: PostProcess,
    pub mode: FrameMode,
    /// The rect in the scene texture and in the output target, `[x, y, w, h]` pixels,
    /// unclipped (a camera viewport may reach past its target).
    pub scene_rect: [i32; 4],
    pub output_rect: [i32; 4],
    /// Size of the scene texture `S` and of the output target `D`.
    pub scene_size: (u32, u32),
    pub output_size: (u32, u32),
    /// `S` has an alpha channel (not `Rg11b10Ufloat`).
    pub scene_alpha: bool,
    /// Render-target group whose lead clear alpha is below 1: alpha is preserved.
    pub transparent: bool,
    /// The implicit group of an Ldr passthrough target (every effect off).
    pub passthrough: bool,
    /// The group's Distortion phase runs this frame (distortion enabled and a writer has work
    /// in the settings camera's view): its distortion buffer is written before the composite.
    pub distortion: bool,
}

/// What a post group needs this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PostGroupNeeds {
    pub bloom: bool,
    pub auto_exposure: bool,
    pub fxaa: bool,
    pub distortion: bool,
    /// Scene depth (distortion occlusion).
    pub depth: bool,
}

/// Encoder and device access of [`PostState::encode_group`].
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PostEncode<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: &'a mut wgpu::CommandEncoder,
    pub stats: &'a mut RenderStats,
    /// The frame's texture pool (bloom chain, LDR intermediate, log-luminance chain): textures
    /// acquired here are released after the group.
    #[allow(dead_code, reason = "the post area's effects (PO1–PO3) acquire pooled textures")]
    pub pool: &'a mut crate::frame::TexturePool,
}

/// Inputs of one post group's passes.
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PostGroupInput<'a> {
    pub camera: Entity,
    pub settings: PostProcess,
    pub mode: FrameMode,
    pub scene_output: OutputEncoding,
    pub scene: &'a wgpu::TextureView,
    pub scene_format: wgpu::TextureFormat,
    pub scene_size: (u32, u32),
    pub scene_rect: [u32; 4],
    /// The scene texture has alpha (not `Rg11b10Ufloat`).
    pub scene_alpha: bool,
    /// Render-target group whose lead clear alpha is below 1: alpha is preserved.
    pub transparent: bool,
    pub depth: Option<&'a wgpu::TextureView>,
    pub distortion: Option<&'a wgpu::TextureView>,
    pub output: &'a wgpu::TextureView,
    pub output_format: wgpu::TextureFormat,
    pub output_size: (u32, u32),
    pub output_rect: [u32; 4],
    pub output_manual_srgb: bool,
    /// First write into the output this frame: clear it (to black) outside the rect.
    pub output_clear: bool,
    /// The implicit group of an Ldr passthrough target (every effect off).
    pub passthrough: bool,
    /// Windowed-capture replay: no metering, no adaptation.
    pub replay: bool,
    pub frame: FrameInfo,
    /// Index of the group among this frame's [`PostState::plan_group`] calls.
    pub index: usize,
    /// Identity of `scene` (bind groups are cached by it).
    pub scene_generation: u64,
    /// The rect drawn in `output` (within the pass limits, see `ViewportRect::fitted`):
    /// viewport `[x, y, w, h]`.
    pub output_viewport: [f32; 4],
}

/// A post group's persistent distortion buffer.
struct DistortionTarget {
    size: (u32, u32),
    view: wgpu::TextureView,
    last_used: u64,
    /// Unique per creation (bind groups sampling it are cached by it).
    generation: u64,
}

/// Composite pipelines' output kind (`S3D_POST_OUT`): how the last pass writes `D`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum OutputKind {
    /// sRGB-format target: hardware encodes.
    Srgb,
    /// 8-bit target without an sRGB format or view: the shader encodes.
    ManualSrgb,
    /// Float target: linear, no dither.
    Linear,
}

impl OutputKind {
    fn of(format: wgpu::TextureFormat, manual_srgb: bool) -> Self {
        if manual_srgb {
            Self::ManualSrgb
        } else if format.is_srgb() {
            Self::Srgb
        } else if matches!(
            format,
            wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rgba32Float | wgpu::TextureFormat::Rg11b10Ufloat
        ) {
            Self::Linear
        } else {
            Self::ManualSrgb
        }
    }
    fn id(self) -> f64 {
        match self {
            Self::Srgb => 0.0,
            Self::ManualSrgb => 1.0,
            Self::Linear => 2.0,
        }
    }
}

/// Post state: pool users, per-camera history (exposure, distortion buffers), pipelines,
/// readback.
pub(crate) struct PostState {
    frame: u64,
    distortion: FastMap<Entity, DistortionTarget>,
    /// `GpuPost` of every post group planned this frame, one aligned slot each.
    params: TrackedBuffer,
    slot: usize,
    planned: usize,
    /// Group 0: params (dynamic offset) + linear-clamp sampler; group 1: `S`, bloom level 0,
    /// exposure, LUT, distortion buffer, LDR intermediate.
    params_bgl: wgpu::BindGroupLayout,
    inputs_bgl: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    /// 1×1 black stand-in for inputs a group does not have (bloom, exposure = 0 EV, LUT, …).
    dummy: wgpu::TextureView,
    /// The params bind group, with the params and seed buffers' generations it was made for.
    params_bg: Option<(wgpu::BindGroup, u32, u64)>,
    /// The post groups' per-frame seeds, a window of frames at a time.
    seeds: SeedWindow,
    /// Input bind groups by (scene-texture generation, distortion-buffer generation or 0,
    /// bloom-chain generation or 0), with the frame they were last used.
    inputs_bgs: FastMap<(u64, u64, u64), (wgpu::BindGroup, u64)>,
    /// Distortion buffers created (generation counter) and created since `end_frame`.
    distortion_generation: u64,
    created: u32,
    composite: FastMap<(wgpu::TextureFormat, OutputKind), wgpu::RenderPipeline>,
    fxaa_modules: FxaaModules,
    fxaa: FastMap<(FxaaQuality, wgpu::TextureFormat, OutputKind), wgpu::RenderPipeline>,
    /// FXAA input bind groups by LDR-intermediate generation, with the frame last used.
    fxaa_bgs: FastMap<u64, (wgpu::BindGroup, u64)>,
    /// Device capabilities, and this frame's (masked by `FrameSettings::downlevel`).
    caps: GraphicsCaps,
    frame_caps: GraphicsCaps,
    /// What each post group planned this frame needs (index = plan order = `PostGroupInput::index`).
    groups: Vec<GroupPost>,
    bloom: BloomState,
    /// Pooled textures whose memory `RenderStats::target_bytes` counts this frame.
    counted: Vec<u64>,
    /// Warned (once) that bloom is off in the Ldr fallback.
    warned_ldr_bloom: bool,
}

/// What a planned post group records this frame besides the composite.
#[derive(Clone, Copy, Debug, Default)]
struct GroupPost {
    bloom: Option<BloomPlan>,
    /// FXAA after the composite (which then writes the LDR intermediate).
    fxaa: Option<FxaaQuality>,
}

/// The part of `rect` (pixels, unclipped) inside a `size` texture, at least 1×1: `[x, y, w, h]`
/// (the frame's `ViewportRect::scissor`).
fn clip_rect(rect: [i32; 4], size: (u32, u32)) -> [u32; 4] {
    let axis = |start: i32, len: i32, extent: u32| {
        let extent = extent.max(1) as i64;
        let a = (start as i64).clamp(0, extent - 1);
        let b = (start as i64 + len.max(0) as i64).clamp(a + 1, extent);
        (a as u32, (b - a) as u32)
    };
    let (x, w) = axis(rect[0], rect[2], size.0);
    let (y, h) = axis(rect[1], rect[3], size.1);
    [x, y, w, h]
}

/// Frames of post seeds uploaded at once.
const SEED_WINDOW: u64 = 64;
/// Bytes of one post group's seeds (`post_seeds` in composite.wgsl).
const SEED_BYTES: u64 = 16;

/// The per-frame seeds (graphics spec §4.3: animated grain, dither) of every post group for a
/// window of [`SEED_WINDOW`] frames: slot `group × SEED_WINDOW + frame % SEED_WINDOW`, one
/// aligned uniform each, bound with a dynamic offset. Written when a window starts, the frame
/// leaves it, or the groups' [`SeedKey`]s change; a still scene uploads nothing in between. (It
/// uploaded each group's 16 bytes every frame: a staging copy costing about 4 µs of CPU and
/// 20–40 µs of GPU throughput in an otherwise idle frame.)
struct SeedWindow {
    align: u64,
    buffer: Option<wgpu::Buffer>,
    generation: u64,
    /// First frame of the uploaded window.
    start: Option<u64>,
    /// The keys the window was written for, and this frame's (plan order).
    keys: Vec<SeedKey>,
    current: Vec<SeedKey>,
    bytes: Vec<u8>,
    /// Collections that grew (`RenderStats::cpu_reallocations` at `end_frame`).
    reallocations: u32,
}

impl SeedWindow {
    fn new(align: u64) -> Self {
        Self {
            align: align.max(SEED_BYTES),
            buffer: None,
            generation: 0,
            start: None,
            keys: Vec::new(),
            current: Vec::new(),
            bytes: Vec::new(),
            reallocations: 0,
        }
    }

    /// Upload the window starting at `frame` unless the uploaded one covers it for the same
    /// keys.
    fn flush(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, up: &mut Uploader, frame: u64) {
        if self.current.is_empty() {
            return;
        }
        let need = self.current.len() as u64 * SEED_WINDOW * self.align;
        if self.buffer.as_ref().is_none_or(|b| b.size() < need) {
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("space3d post seeds"),
                size: need,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.generation += 1;
            self.start = None;
            up.reallocations += 1;
        }
        let covered = self.start.is_some_and(|s| (s..s + SEED_WINDOW).contains(&frame));
        if covered && self.keys == self.current {
            return;
        }
        let capacities = (self.bytes.capacity(), self.keys.capacity());
        self.bytes.clear();
        self.bytes.resize(need as usize, 0);
        for (g, key) in self.current.iter().enumerate() {
            for f in frame..frame + SEED_WINDOW {
                let at = self.offset(g, f) as usize;
                self.bytes[at..at + SEED_BYTES as usize].copy_from_slice(bytemuck::cast_slice(&key.seeds(f)));
            }
        }
        if let Some(buffer) = &self.buffer {
            up.write(encoder, buffer, 0, &self.bytes);
        }
        self.keys.clone_from(&self.current);
        self.start = Some(frame);
        self.reallocations += u32::from((self.bytes.capacity(), self.keys.capacity()) != capacities);
    }

    /// Dynamic offset of post group `group`'s seeds at `frame`.
    fn offset(&self, group: usize, frame: u64) -> u32 {
        ((group as u64 * SEED_WINDOW + frame % SEED_WINDOW) * self.align) as u32
    }
}

impl PostState {
    pub fn new(device: &wgpu::Device, caps: &GraphicsCaps) -> Self {
        let params_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d post params"),
            entries: &[
                crate::pipeline::uniform_entry(
                    0,
                    wgpu::ShaderStages::FRAGMENT,
                    true,
                    std::mem::size_of::<GpuPost>() as u64,
                ),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                crate::pipeline::uniform_entry(2, wgpu::ShaderStages::FRAGMENT, true, SEED_BYTES),
            ],
        });
        let texture = |binding, filterable| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let inputs_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d post inputs"),
            entries: &[
                texture(0, true),
                texture(1, true),
                texture(2, false),
                texture(3, true),
                texture(4, true),
                texture(5, true),
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("space3d composite"),
            bind_group_layouts: &[Some(&params_bgl), Some(&inputs_bgl)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("space3d composite"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(composite_source())),
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("space3d post linear clamp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let dummy = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("space3d post dummy"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let align = device.limits().min_uniform_buffer_offset_alignment as usize;
        let bloom = BloomState::new(device, align);
        Self {
            frame: 0,
            distortion: FastMap::default(),
            params: TrackedBuffer::new("space3d post params", wgpu::BufferUsages::UNIFORM),
            slot: std::mem::size_of::<GpuPost>().next_multiple_of(align),
            planned: 0,
            params_bgl,
            inputs_bgl,
            layout,
            module,
            sampler,
            dummy,
            params_bg: None,
            seeds: SeedWindow::new(align as u64),
            inputs_bgs: FastMap::default(),
            composite: FastMap::default(),
            fxaa_modules: FxaaModules::default(),
            fxaa: FastMap::default(),
            fxaa_bgs: FastMap::default(),
            distortion_generation: 0,
            created: 0,
            caps: caps.clone(),
            frame_caps: caps.clone(),
            groups: Vec::new(),
            bloom,
            counted: Vec::new(),
            warned_ldr_bloom: false,
        }
    }

    pub fn begin_frame(&mut self, frame: &FrameInfo, settings: &FrameSettings) {
        // STUB(P-POST): read_exposure, gpu_timing (PO2, PO6).
        self.frame_caps = self.caps.masked(settings.downlevel);
        self.frame = frame.frame;
        self.planned = 0;
        self.params.begin();
        self.seeds.current.clear();
        self.groups.clear();
        self.counted.clear();
        self.bloom.params.begin();
        self.bloom.slots = 0;
    }

    /// Step 6: what a post group needs (bloom, auto exposure, FXAA, distortion, depth). Writes
    /// the group's composite parameters (slot = call order this frame).
    pub fn plan_group(&mut self, world: &World, group: &PostGroupPlan, reallocs: &mut u32) -> PostGroupNeeds {
        let _ = world; // STUB(P-POST): distortion writers, auto exposure history
        let bloom = self.plan_bloom(group, reallocs);
        // FXAA runs in every mode (display space), never in the passthrough group.
        let fxaa = match group.settings.anti_aliasing {
            AntiAliasing::Fxaa(q) if !group.passthrough && group.settings.enabled => Some(q),
            _ => None,
        };
        let capacity = self.groups.capacity();
        self.groups.push(GroupPost { bloom, fxaa });
        *reallocs += u32::from(self.groups.capacity() != capacity);
        let params = GpuPost::new(group, bloom.is_some(), fxaa.is_some());
        // Nothing in it changes per frame: a still scene uploads nothing.
        self.params
            .write_at(self.planned * self.slot, bytemuck::bytes_of(&params), reallocs);
        let capacity = self.seeds.current.capacity();
        self.seeds
            .current
            .push(SeedKey::of(params.flags[0], &group.settings.film_grain));
        *reallocs += u32::from(self.seeds.current.capacity() != capacity);
        self.planned += 1;
        PostGroupNeeds {
            bloom: bloom.is_some(),
            fxaa: fxaa.is_some(),
            distortion: !group.passthrough && group.settings.enabled && group.settings.distortion.enabled,
            ..Default::default()
        }
    }

    /// The bloom chain of a post group this frame, its pass uniforms written: bloom enabled
    /// with a non-zero intensity, in Hdr mode (off in the Ldr fallback, warned once; never in
    /// the passthrough group) where `GraphicsCaps::bloom_format` exists.
    fn plan_bloom(&mut self, group: &PostGroupPlan, reallocs: &mut u32) -> Option<BloomPlan> {
        let s = &group.settings;
        let b = &s.bloom;
        if group.passthrough || !s.enabled || !b.enabled || b.intensity <= 0.0 {
            return None;
        }
        if group.mode != FrameMode::Hdr {
            if !self.warned_ldr_bloom {
                self.warned_ldr_bloom = true;
                tracing::warn!("bloom is off in the Ldr fallback (no float render targets or HdrMode::Ldr)");
            }
            return None;
        }
        // The energy-conserving composite promises the scene's mean: its chain is
        // `Rgba16Float`. `Rg11b10Ufloat` (5–6 mantissa bits) converts by truncation on some GPUs
        // (Apple), which over the chain's ~9 writes loses about 5 % of the energy (and more of
        // blue) — invisible in an additive glow, whose chain keeps the compact format.
        let format = match b.composite {
            BloomComposite::EnergyConserving => self.frame_caps.hdr_format,
            _ => self.frame_caps.bloom_format,
        }?;
        let rect = clip_rect(group.scene_rect, group.scene_size);
        let chain = bloom::chain_size((rect[2], rect[3]), b);
        let first_slot = self.bloom.slots;
        let (params, slot) = (&mut self.bloom.params, self.bloom.slot);
        let mut n = first_slot as usize;
        // STUB(P-POST) PO2: with auto exposure the prefilter also multiplies by 2^E.
        let exposure = (s.exposure.exp2(), false);
        bloom::passes(group.scene_size, rect, &chain, b, exposure, |p| {
            params.write_at(n * slot, bytemuck::bytes_of(&p), reallocs);
            n += 1;
        });
        self.bloom.slots = n as u32;
        Some(BloomPlan {
            format,
            size: chain.size,
            mips: chain.mips,
            merged: chain.merged,
            half: chain.half,
            first_slot,
            scatter: b.scatter.clamp(0.0, 1.0),
        })
    }

    /// Upload this frame's parameters (after every [`Self::plan_group`], before any
    /// [`Self::encode_group`]).
    pub fn flush(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, up: &mut Uploader) {
        if self.planned == 0 {
            return;
        }
        let mut reallocs = 0;
        self.params.ensure_len(std::mem::size_of::<GpuPost>(), &mut reallocs);
        self.params.flush(device, encoder, up);
        self.seeds.flush(device, encoder, up, self.frame);
        let generation = self.params.gpu.generation;
        let seeds_generation = self.seeds.generation;
        let Some(seeds) = &self.seeds.buffer else {
            return;
        };
        if self
            .params_bg
            .as_ref()
            .is_none_or(|(_, g, sg)| *g != generation || *sg != seeds_generation)
        {
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d post params"),
                layout: &self.params_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: self.params.buffer(),
                            offset: 0,
                            size: wgpu::BufferSize::new(std::mem::size_of::<GpuPost>() as u64),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: seeds,
                            offset: 0,
                            size: wgpu::BufferSize::new(SEED_BYTES),
                        }),
                    },
                ],
            });
            self.params_bg = Some((bg, generation, seeds_generation));
        }
        self.bloom.flush(device, encoder, up, &self.sampler);
    }

    /// The persistent distortion buffer of a post group (created on demand; freed 60 frames
    /// after its last use), sized from `rect` and `res`. The caller's pass clears it.
    pub fn distortion_target(
        &mut self,
        device: &wgpu::Device,
        camera: Entity,
        rect: [u32; 4],
        res: DistortionResolution,
    ) -> (&wgpu::TextureView, (u32, u32)) {
        let size = match res {
            DistortionResolution::Half => (rect[2].div_ceil(2).max(1), rect[3].div_ceil(2).max(1)),
            _ => (rect[2].max(1), rect[3].max(1)),
        };
        let frame = self.frame;
        let fresh = self.distortion.get(&camera).is_none_or(|t| t.size != size);
        if fresh {
            self.distortion_generation += 1;
            self.created += 1;
            let t = create_distortion_target(device, size, frame, self.distortion_generation);
            self.distortion.insert(camera, t);
        }
        let t = self.distortion.get_mut(&camera).expect("inserted");
        t.last_used = frame;
        (&t.view, t.size)
    }

    /// A post group's existing distortion buffer: its view (a cheap clone), size and
    /// generation.
    pub fn distortion_view(&self, camera: Entity) -> Option<(wgpu::TextureView, (u32, u32), u64)> {
        self.distortion
            .get(&camera)
            .map(|t| (t.view.clone(), t.size, t.generation))
    }

    /// Build the composite pipeline for an output target of `format` (`manual_srgb`: the shader
    /// encodes sRGB) ahead of use (`Renderer::prewarm`), with FXAA (Medium) and the bloom chains.
    /// STUB(P-POST): auto exposure (PO2).
    pub fn prewarm(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat, manual_srgb: bool) {
        let kind = OutputKind::of(format, manual_srgb);
        self.composite_pipeline(device, format, kind);
        // FXAA (Medium, the default quality) and the composite into its LDR intermediate.
        self.composite_pipeline(device, LDR_FORMAT, OutputKind::Srgb);
        self.fxaa_pipeline(device, FxaaQuality::Medium, format, kind);
        // The additive chain's format and the energy-conserving one's (see `plan_bloom`).
        for format in [self.caps.bloom_format, self.caps.hdr_format].into_iter().flatten() {
            self.bloom.pipelines(device, format);
        }
    }

    /// The composite pipeline for `format` and `kind` (created on first use).
    fn composite_pipeline(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        kind: OutputKind,
    ) -> &wgpu::RenderPipeline {
        let (module, layout) = (&self.module, &self.layout);
        self.composite
            .entry((format, kind))
            .or_insert_with(|| fullscreen_pipeline(device, layout, module, "fs_composite", format, kind))
    }

    /// The FXAA pipeline for a quality, `format` and `kind` (created on first use).
    fn fxaa_pipeline(
        &mut self,
        device: &wgpu::Device,
        q: FxaaQuality,
        format: wgpu::TextureFormat,
        kind: OutputKind,
    ) -> &wgpu::RenderPipeline {
        let (modules, layout) = (&mut self.fxaa_modules, &self.layout);
        self.fxaa.entry((q, format, kind)).or_insert_with(|| {
            let module = modules.get(device, q);
            fullscreen_pipeline(device, layout, module, "fs_fxaa", format, kind)
        })
    }

    /// Record meter → bloom → composite (→ FXAA) for one post group into the frame encoder.
    /// Bloom (PO1), the composite, FXAA and the lens effects (PO3); STUB(P-POST): metering (PO2).
    pub fn encode_group(&mut self, cx: &mut PostEncode<'_>, input: &PostGroupInput<'_>) {
        if self.params_bg.is_none() {
            return;
        }
        let group = self.groups.get(input.index).copied().unwrap_or_default();
        let bloom = group.bloom.and_then(|plan| {
            let scene = (input.scene, input.scene_generation);
            let (view, generation) = self.bloom.encode(cx, scene, &plan, &self.dummy, self.frame, None)?;
            self.count_target(cx.stats, input.replay, generation, plan.bytes());
            Some((view, generation))
        });
        // With FXAA the composite writes the LDR intermediate L (pooled, the output's size, the
        // same rect) and the FXAA pass writes the output.
        let ldr = group.fxaa.map(|_| {
            let desc = PoolDesc {
                size: input.output_size,
                format: LDR_FORMAT,
                mips: 1,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            };
            let index = cx.pool.acquire(cx.device, desc);
            let texture = cx.pool.get(index);
            let bytes = 4 * input.output_size.0 as u64 * input.output_size.1 as u64;
            let (view, generation) = (texture.view.clone(), texture.generation);
            self.count_target(cx.stats, input.replay, generation, bytes);
            (view, generation)
        });
        let kind = OutputKind::of(input.output_format, input.output_manual_srgb);
        let (composite_format, composite_kind) = match ldr {
            Some(_) => (LDR_FORMAT, OutputKind::Srgb),
            None => (input.output_format, kind),
        };
        self.composite_pipeline(cx.device, composite_format, composite_kind);
        if let Some(q) = group.fxaa {
            self.fxaa_pipeline(cx.device, q, input.output_format, kind);
        }
        let Some((params_bg, _, _)) = &self.params_bg else {
            return;
        };
        let frame = self.frame;
        let (inputs_bgl, dummy) = (&self.inputs_bgl, &self.dummy);
        let distortion = input
            .distortion
            .and_then(|_| self.distortion.get(&input.camera))
            .map_or(0, |t| t.generation);
        let key = (
            input.scene_generation,
            distortion,
            bloom.as_ref().map_or(0, |(_, g)| *g),
        );
        let entry = |binding, view| wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::TextureView(view),
        };
        let inputs = &mut self.inputs_bgs.entry(key).or_insert_with(|| {
            let bg = cx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d post inputs"),
                layout: inputs_bgl,
                entries: &[
                    entry(0, input.scene),
                    entry(1, bloom.as_ref().map_or(dummy, |(view, _)| view)),
                    entry(2, dummy),
                    entry(3, dummy),
                    entry(4, input.distortion.unwrap_or(dummy)),
                    // Never L: the composite renders into it.
                    entry(5, dummy),
                ],
            });
            (bg, frame)
        });
        inputs.1 = frame;
        let output_load = if input.output_clear {
            wgpu::LoadOp::Clear(wgpu::Color::BLACK)
        } else {
            wgpu::LoadOp::Load
        };
        let offsets = [(input.index * self.slot) as u32, self.seeds.offset(input.index, frame)];
        let (composite_target, composite_load) = match &ldr {
            Some((view, _)) => (view, wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)),
            None => (input.output, output_load),
        };
        let pipeline = &self.composite[&(composite_format, composite_kind)];
        let pass = FullscreenPass {
            label: "space3d composite",
            target: composite_target,
            load: composite_load,
            viewport: input.output_viewport,
            scissor: input.output_rect,
        };
        pass.draw(cx, pipeline, params_bg, offsets, &inputs.0);
        let (Some(q), Some((view, generation))) = (group.fxaa, &ldr) else {
            return;
        };
        let fxaa_bg = &mut self.fxaa_bgs.entry(*generation).or_insert_with(|| {
            let bg = cx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d fxaa inputs"),
                layout: inputs_bgl,
                entries: &[
                    entry(0, dummy),
                    entry(1, dummy),
                    entry(2, dummy),
                    entry(3, dummy),
                    entry(4, dummy),
                    entry(5, view),
                ],
            });
            (bg, frame)
        });
        fxaa_bg.1 = frame;
        let pipeline = &self.fxaa[&(q, input.output_format, kind)];
        let pass = FullscreenPass {
            label: "space3d fxaa",
            target: input.output,
            load: output_load,
            viewport: input.output_viewport,
            scissor: input.output_rect,
        };
        pass.draw(cx, pipeline, params_bg, offsets, &fxaa_bg.0);
    }

    /// Count a pooled texture's memory in `RenderStats::target_bytes` once per frame (post
    /// groups of one size share it; replays count nothing).
    fn count_target(&mut self, stats: &mut RenderStats, replay: bool, generation: u64, bytes: u64) {
        if replay || self.counted.contains(&generation) {
            return;
        }
        let capacity = self.counted.capacity();
        self.counted.push(generation);
        stats.cpu_reallocations += u32::from(self.counted.capacity() != capacity);
        stats.target_bytes += bytes;
    }

    pub fn end_frame(&mut self, stats: &mut RenderStats) {
        stats.textures_created += std::mem::take(&mut self.created);
        stats.cpu_reallocations += std::mem::take(&mut self.seeds.reallocations);
        let frame = self.frame;
        self.distortion.retain(|_, t| t.last_used + 60 > frame);
        // Bind groups of scene textures no longer composited (replaced or freed).
        self.inputs_bgs.retain(|_, (_, used)| *used + 3 > frame);
        self.bloom.sources.retain(|_, (_, used)| *used + 3 > frame);
        self.fxaa_bgs.retain(|_, (_, used)| *used + 3 > frame);
    }

    /// Register the built-in post nodes (`"space3d/distortion-meshes"`).
    pub fn register_nodes(nodes: &mut RenderNodes) {
        distortion_mesh::register(nodes);
    }

    /// Last read-back exposure (stops) of a post camera.
    pub fn camera_exposure(&self, camera: Entity) -> Option<f32> {
        let _ = camera; // STUB(P-POST)
        None
    }

    /// Forget a camera's temporal history (auto exposure snaps next frame).
    pub fn reset_history(&mut self, camera: Entity) {
        self.distortion.remove(&camera);
    }
}

/// A full-screen post pass: its target, load op and the post group's rect.
struct FullscreenPass<'a> {
    label: &'static str,
    target: &'a wgpu::TextureView,
    load: wgpu::LoadOp<wgpu::Color>,
    viewport: [f32; 4],
    scissor: [u32; 4],
}

impl FullscreenPass<'_> {
    /// Record the pass: one full-screen triangle with the group's parameters and seeds
    /// (`offsets`) and `inputs`.
    fn draw(
        &self,
        cx: &mut PostEncode<'_>,
        pipeline: &wgpu::RenderPipeline,
        params: &wgpu::BindGroup,
        offsets: [u32; 2],
        inputs: &wgpu::BindGroup,
    ) {
        let mut pass = cx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(self.label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: self.target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: self.load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        let [x, y, w, h] = self.viewport;
        pass.set_viewport(x, y, w, h, 0.0, 1.0);
        let [sx, sy, sw, sh] = self.scissor;
        pass.set_scissor_rect(sx, sy, sw, sh);
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, params, &offsets);
        pass.set_bind_group(1, inputs, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        cx.stats.render_passes += 1;
        cx.stats.fullscreen_passes += 1;
        cx.stats.post_passes += 1;
    }
}

/// A post pipeline: the full-screen triangle and fragment `entry` of `module`, writing `format`
/// with the output kind `kind` (`S3D_POST_OUT`).
fn fullscreen_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    entry: &str,
    format: wgpu::TextureFormat,
    kind: OutputKind,
) -> wgpu::RenderPipeline {
    let constants = [("S3D_POST_OUT", kind.id())];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(entry),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_fullscreen"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(entry),
            compilation_options: wgpu::PipelineCompilationOptions {
                constants: &constants,
                ..Default::default()
            },
            targets: &[Some(format.into())],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_distortion_target(
    device: &wgpu::Device,
    (w, h): (u32, u32),
    frame: u64,
    generation: u64,
) -> DistortionTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d distortion buffer"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DISTORTION_FORMAT,
        // COPY_SRC: tests read the buffer back.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    DistortionTarget {
        size: (w, h),
        view: texture.create_view(&Default::default()),
        last_used: frame,
        generation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distortion_targets_persist_per_camera() {
        let Ok(r) = pollster::block_on(crate::Renderer::new_headless(4, 4)) else {
            eprintln!("skipping GPU test: no adapter available");
            return;
        };
        let mut post = PostState::new(r.device(), r.graphics_caps());
        let mut world = World::new();
        let (a, b) = (world.spawn(()), world.spawn(()));
        let (_, size) = post.distortion_target(r.device(), a, [0, 0, 101, 60], DistortionResolution::Half);
        assert_eq!(size, (51, 30));
        let (_, size) = post.distortion_target(r.device(), b, [0, 0, 101, 60], DistortionResolution::Full);
        assert_eq!(size, (101, 60));
        assert_eq!(post.distortion.len(), 2);
        let plan = PostGroupPlan {
            camera: a,
            settings: PostProcess::default(),
            mode: FrameMode::Hdr,
            scene_rect: [0, 0, 4, 4],
            output_rect: [0, 0, 4, 4],
            scene_size: (4, 4),
            output_size: (4, 4),
            scene_alpha: true,
            transparent: false,
            passthrough: false,
            distortion: false,
        };
        let mut reallocs = 0;
        assert!(post.plan_group(&world, &plan, &mut reallocs).distortion);
        let passthrough = PostGroupPlan {
            passthrough: true,
            ..plan
        };
        assert!(!post.plan_group(&world, &passthrough, &mut reallocs).distortion);
        post.reset_history(a);
        let mut stats = RenderStats::default();
        post.end_frame(&mut stats);
        assert_eq!(post.distortion.len(), 1);
        assert_eq!(post.camera_exposure(b), None);
        let mut nodes = RenderNodes::default();
        PostState::register_nodes(&mut nodes);
        assert!(nodes.contains("space3d/distortion-meshes"));
        assert_eq!(r.gpu_error_count(), 0);
    }
}
