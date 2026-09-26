//! Auto exposure (graphics spec §5.2 PO2; design-post §5.2): metering a post group's rect in the
//! scene texture `S` into its camera's 1×1 exposure texture `E` (stops), which the bloom
//! prefilter and the composite read (`2^(E + compensation)`).
//!
//! Two paths, the same inputs and output:
//!
//! * **Compute** (`GraphicsCaps::compute_histogram` and an `R32Float` E): a 255-bin log2
//!   luminance histogram (black excluded), the percentile window, the weighted mean of the bin
//!   centers, the clamp and the adaptation in one compute pass of two dispatches
//!   (`shaders/post/exposure.wgsl`); the camera's 16-byte state buffer is then copied into E.
//! * **Render passes** (WebGL2, GLES 3.0, `DownlevelOverride::no_compute`): a quarter-size
//!   log-luminance level, reductions by 8 until it is at most 16 × 16, and an adapt pass that
//!   reads the previous E and writes the other of the camera's two E textures
//!   (`shaders/post/exposure_dl.wgsl`). No percentile filter: within ±0.5 EV of the compute path.
//!
//! Per camera the state persists (freed 60 renderer frames after its last use). The first
//! metered frame of a camera snaps to the target, as do frames after auto exposure was turned
//! on, after `Renderer::reset_post_history` and after a path change; otherwise the exposure
//! closes `1 − 2^(−dt · rate)` of the gap per frame, `dt` being `FrameInfo::delta` (0 when
//! `Time::frame` did not advance: `render_to_image` twice, capture replays), so the result is a
//! pure function of the frames rendered. Replays never meter.

use super::PostEncode;
use super::api::{AutoExposure, Metering};
use crate::caps::GraphicsCaps;
use crate::frame::PoolDesc;
use crate::gpu::{TrackedBuffer, Uploader};
use crate::handles::FastMap;
use crate::node::FULLSCREEN_WGSL;
use crate::output::OUTPUT_WGSL;
use bytemuck::{Pod, Zeroable};
use space3d_core::Entity;
use std::borrow::Cow;

/// What both metering paths share (`ExposureParams`, the metering weight).
pub(crate) const EXPOSURE_COMMON_WGSL: &str = include_str!("../shaders/post/exposure_common.wgsl");
/// The compute path.
pub(crate) const EXPOSURE_WGSL: &str = include_str!("../shaders/post/exposure.wgsl");
/// The render-pass path.
pub(crate) const EXPOSURE_DL_WGSL: &str = include_str!("../shaders/post/exposure_dl.wgsl");

pub(crate) fn compute_source() -> String {
    format!("{OUTPUT_WGSL}\n{EXPOSURE_COMMON_WGSL}\n{EXPOSURE_WGSL}")
}

pub(crate) fn downlevel_source() -> String {
    format!("{OUTPUT_WGSL}\n{FULLSCREEN_WGSL}\n{EXPOSURE_COMMON_WGSL}\n{EXPOSURE_DL_WGSL}")
}

/// log2 luminance range of the histogram (32 stops, 255 bins of 0.125 EV). Below it is black
/// (not metered); above it counts as its top.
pub(crate) const LOG2_MIN: f32 = -16.0;
pub(crate) const LOG2_MAX: f32 = 16.0;
/// Middle grey: the metered luminance is exposed to it (`S3D_EXPOSURE_KEY`).
#[cfg(test)]
pub(crate) const KEY: f32 = 0.18;
/// Samples metered per group at most: the stride grows (1, 2, 4) until the rect fits
/// (1080p: every 2nd pixel per axis, 518 k samples; 4K: every 4th).
const MAX_SAMPLES: u64 = 655_360;
/// Format of the render-pass path's log-luminance levels (R = Σ w·log2 L, G = Σ w, scaled).
const LEVEL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The render-pass path's first level: one texel per 4 × 4 pixels of the rect.
const LOGLUM_BLOCK: u32 = 4;
/// Each reduction: one texel per 8 × 8 texels.
const REDUCE: u32 = 8;
/// The adapt pass sums a level of at most this size per axis.
const ADAPT_MAX: u32 = 16;

/// Matches `ExposureParams` in exposure_common.wgsl: one aligned uniform slot per metered post
/// group.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub(crate) struct GpuExposure {
    /// The group's rect in `S`, clipped (x, y, w, h).
    pub rect: [u32; 4],
    /// Stride, metering (0 average, 1 center weighted), -, -.
    pub sampling: [u32; 4],
    /// log2 of the first bin, 1 / log2 range, low and high percentile.
    pub range: [f32; 4],
    /// Min and max EV, -, -.
    pub adapt: [f32; 4],
    /// Adaptation factor to bright and to dark, snap (1), -. The only per-frame-varying bytes
    /// (with a steady `dt` they do not change either).
    pub step: [f32; 4],
}

/// Pixels between metered samples per axis for a rect of `w × h`: 1, 2 or 4.
pub(crate) fn stride(w: u32, h: u32) -> u32 {
    let mut s = 1u32;
    while s < 4 && w.div_ceil(s) as u64 * h.div_ceil(s) as u64 > MAX_SAMPLES {
        s *= 2;
    }
    s
}

/// Fraction of the gap to the target closed in a frame of `dt` seconds at `rate` (1/s):
/// `1 − 2^(−dt · rate)`; 1 for an infinite rate (`AutoExposure::instant`, even when `dt` is 0),
/// 0 when `dt` is 0 or the rate is not positive.
pub(crate) fn adapt_factor(dt: f32, rate: f32) -> f32 {
    if rate == f32::INFINITY {
        return 1.0;
    }
    if !(dt > 0.0 && rate > 0.0 && rate.is_finite()) {
        return 0.0;
    }
    1.0 - (-dt * rate).exp2()
}

/// `(min_ev, max_ev, low, high)` of `ae`, made usable: a finite, ordered EV range (defaults for
/// NaN) and a percentile window of at least 1 % inside 0..1.
pub(crate) fn sanitized(ae: &AutoExposure) -> (f32, f32, f32, f32) {
    let d = AutoExposure::default();
    let finite = |v: f32, or: f32| if v.is_finite() { v } else { or };
    let (a, b) = (finite(ae.min_ev, d.min_ev), finite(ae.max_ev, d.max_ev));
    let low = finite(ae.low_percentile, d.low_percentile).clamp(0.0, 0.99);
    let high = finite(ae.high_percentile, d.high_percentile).clamp(low + 0.01, 1.0);
    (a.min(b), a.max(b), low, high)
}

impl GpuExposure {
    /// Metering of the rect `rect` (clipped, in `S`) with `ae`; `snap`: no history this frame;
    /// `dt`: this frame's `FrameInfo::delta`.
    pub(crate) fn new(rect: [u32; 4], ae: &AutoExposure, snap: bool, dt: f32) -> Self {
        let (min_ev, max_ev, low, high) = sanitized(ae);
        let metering = match ae.metering {
            Metering::Average => 0,
            _ => 1,
        };
        Self {
            rect,
            sampling: [stride(rect[2], rect[3]), metering, 0, 0],
            range: [LOG2_MIN, 1.0 / (LOG2_MAX - LOG2_MIN), low, high],
            adapt: [min_ev, max_ev, 0.0, 0.0],
            step: [
                adapt_factor(dt, ae.adapt_to_bright),
                adapt_factor(dt, ae.adapt_to_dark),
                f32::from(u8::from(snap)),
                0.0,
            ],
        }
    }
}

/// How a camera is metered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MeterPath {
    Compute,
    RenderPasses,
}

impl MeterPath {
    /// The path on a device with `caps` (masked by `FrameSettings::downlevel`), with the exposure
    /// texture's format; `None` without a renderable float exposure format (auto exposure off).
    pub(crate) fn of(caps: &GraphicsCaps) -> Option<(Self, wgpu::TextureFormat)> {
        let format = caps.exposure_format?;
        caps.hdr_format?;
        if caps.compute_histogram && format == wgpu::TextureFormat::R32Float {
            Some((Self::Compute, format))
        } else {
            Some((Self::RenderPasses, format))
        }
    }
}

/// A camera's 1×1 exposure texture.
struct ExposureTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    generation: u64,
}

/// A camera's metering history.
struct CameraExposure {
    path: MeterPath,
    format: wgpu::TextureFormat,
    /// E (compute: `[0]` only; render passes: the ping-pong pair).
    textures: [Option<ExposureTexture>; 2],
    /// The E written last.
    current: usize,
    /// Compute path: the 16-byte state (ev, target, mean, valid) and its generation.
    state: Option<(wgpu::Buffer, u64)>,
    /// The next metering has no history.
    snap: bool,
    /// Auto exposure was on the last time this camera was planned.
    enabled: bool,
    /// Renderer frame of the last use (eviction).
    last_used: u64,
}

impl CameraExposure {
    fn current(&self) -> Option<&ExposureTexture> {
        self.textures[self.current].as_ref()
    }
}

/// A post group metered this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MeterPlan {
    pub path: MeterPath,
    /// Uniform slot of its `GpuExposure`.
    pub slot: u32,
}

/// Pipelines and layouts of the compute path.
struct ComputeMeter {
    bgl: wgpu::BindGroupLayout,
    histogram_pipeline: wgpu::ComputePipeline,
    average_pipeline: wgpu::ComputePipeline,
    /// 256 bins of weights and 256 of weighted positions, zero between groups (the average
    /// dispatch clears it).
    histogram: wgpu::Buffer,
}

/// Pipelines and layouts of the render-pass path.
struct PassMeter {
    params_bgl: wgpu::BindGroupLayout,
    source_bgl: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    loglum: wgpu::RenderPipeline,
    reduce: wgpu::RenderPipeline,
    /// The adapt pass per E format.
    adapt: FastMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
}

/// Auto-exposure state of the renderer: per-camera histories, the metering parameters of this
/// frame's groups, pipelines (created on first use) and cached bind groups.
pub(crate) struct Meters {
    /// `GpuExposure` of every group metered this frame, one aligned slot each.
    params: TrackedBuffer,
    slot: usize,
    slots: u32,
    cameras: FastMap<Entity, CameraExposure>,
    compute: Option<ComputeMeter>,
    passes: Option<PassMeter>,
    /// Compute bind groups by (params generation, scene generation, state generation), and the
    /// render-pass path's params bind group by params generation; source bind groups by
    /// (kind, source generation, previous-E generation). With the renderer frame last used.
    compute_bgs: FastMap<(u32, u64, u64), (wgpu::BindGroup, u64)>,
    params_bg: Option<(wgpu::BindGroup, u32)>,
    source_bgs: FastMap<(u8, u64, u64), (wgpu::BindGroup, u64)>,
    generation: u64,
    /// Textures created since `take_created`.
    created: u32,
    /// Renderer frames (eviction clock; never feeds pixels).
    tick: u64,
    /// The pooled levels (generation, bytes) of the last `encode` on the render-pass path (for
    /// `RenderStats::target_bytes`): the first `pooled_len`.
    pooled: [(u64, u64); 8],
    pooled_len: usize,
    /// Collections that grew (`RenderStats::cpu_reallocations`).
    pub reallocations: u32,
}

impl Meters {
    pub(crate) fn new(align: usize) -> Self {
        Self {
            params: TrackedBuffer::new("space3d exposure params", wgpu::BufferUsages::UNIFORM),
            slot: std::mem::size_of::<GpuExposure>().next_multiple_of(align),
            slots: 0,
            cameras: FastMap::default(),
            compute: None,
            passes: None,
            compute_bgs: FastMap::default(),
            params_bg: None,
            source_bgs: FastMap::default(),
            generation: 0,
            created: 0,
            tick: 0,
            pooled: [(0, 0); 8],
            pooled_len: 0,
            reallocations: 0,
        }
    }

    pub(crate) fn begin_frame(&mut self) {
        self.tick += 1;
        self.params.begin();
        self.slots = 0;
    }

    /// Plan metering `camera`'s rect (`rect`: clipped, in `S`) with `ae` on `path`; `dt`:
    /// `FrameInfo::delta`. Writes the group's parameters.
    pub(crate) fn plan(
        &mut self,
        camera: Entity,
        rect: [u32; 4],
        ae: &AutoExposure,
        (path, format): (MeterPath, wgpu::TextureFormat),
        dt: f32,
        reallocs: &mut u32,
    ) -> MeterPlan {
        let tick = self.tick;
        let fresh = || CameraExposure {
            path,
            format,
            textures: [None, None],
            current: 0,
            state: None,
            snap: true,
            enabled: true,
            last_used: tick,
        };
        if !self.cameras.contains_key(&camera) {
            let capacity = self.cameras.capacity();
            self.cameras.insert(camera, fresh());
            *reallocs += u32::from(self.cameras.capacity() != capacity);
        }
        let state = self.cameras.get_mut(&camera).expect("inserted");
        if state.path != path || state.format != format {
            // Another path (a downlevel override toggled): new textures, no history.
            *state = fresh();
        }
        if !state.enabled {
            state.snap = true;
            state.enabled = true;
        }
        state.last_used = tick;
        let params = GpuExposure::new(rect, ae, state.snap, dt);
        let slot = self.slots;
        self.params
            .write_at(slot as usize * self.slot, bytemuck::bytes_of(&params), reallocs);
        self.slots += 1;
        MeterPlan { path, slot }
    }

    /// `camera` is planned without auto exposure this frame: its next metering snaps.
    pub(crate) fn disable(&mut self, camera: Entity) {
        if let Some(s) = self.cameras.get_mut(&camera) {
            s.enabled = false;
        }
    }

    /// Forget `camera`'s history: its next metering snaps.
    pub(crate) fn reset(&mut self, camera: Entity) {
        if let Some(s) = self.cameras.get_mut(&camera) {
            s.snap = true;
        }
    }

    /// Upload this frame's parameters.
    pub(crate) fn flush(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, up: &mut Uploader) {
        if self.slots == 0 {
            return;
        }
        let mut reallocs = 0;
        self.params
            .ensure_len(std::mem::size_of::<GpuExposure>(), &mut reallocs);
        self.params.flush(device, encoder, up);
        self.reallocations += reallocs;
    }

    /// Build the pipelines of `path` (prewarm; otherwise on first use).
    pub(crate) fn prewarm(&mut self, device: &wgpu::Device, path: MeterPath, format: wgpu::TextureFormat) {
        match path {
            MeterPath::Compute => {
                self.compute_meter(device);
            }
            MeterPath::RenderPasses => {
                self.pass_meter(device, format);
            }
        }
    }

    fn compute_meter(&mut self, device: &wgpu::Device) -> &ComputeMeter {
        self.compute.get_or_insert_with(|| {
            let storage = |binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            };
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d exposure (compute)"),
                entries: &[
                    crate::pipeline::uniform_entry(
                        0,
                        wgpu::ShaderStages::COMPUTE,
                        true,
                        std::mem::size_of::<GpuExposure>() as u64,
                    ),
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    storage(2),
                    storage(3),
                ],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space3d exposure (compute)"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d exposure (compute)"),
                source: wgpu::ShaderSource::Wgsl(Cow::Owned(compute_source())),
            });
            let pipeline = |entry| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&layout),
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                })
            };
            let histogram = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("space3d exposure histogram"),
                size: 512 * 4,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            });
            ComputeMeter {
                histogram_pipeline: pipeline("cs_histogram"),
                average_pipeline: pipeline("cs_average"),
                bgl,
                histogram,
            }
        })
    }

    fn pass_meter(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> &PassMeter {
        let m = self.passes.get_or_insert_with(|| {
            let params_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d exposure params"),
                entries: &[crate::pipeline::uniform_entry(
                    0,
                    wgpu::ShaderStages::FRAGMENT,
                    true,
                    std::mem::size_of::<GpuExposure>() as u64,
                )],
            });
            let texture = |binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            };
            let source_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d exposure source"),
                entries: &[texture(0), texture(1)],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space3d exposure (render passes)"),
                bind_group_layouts: &[Some(&params_bgl), Some(&source_bgl)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d exposure (render passes)"),
                source: wgpu::ShaderSource::Wgsl(Cow::Owned(downlevel_source())),
            });
            PassMeter {
                loglum: pass_pipeline(device, &layout, &module, "fs_loglum", LEVEL_FORMAT),
                reduce: pass_pipeline(device, &layout, &module, "fs_reduce", LEVEL_FORMAT),
                adapt: FastMap::default(),
                params_bgl,
                source_bgl,
                layout,
                module,
            }
        });
        let PassMeter {
            layout, module, adapt, ..
        } = m;
        adapt
            .entry(format)
            .or_insert_with(|| pass_pipeline(device, layout, module, "fs_adapt", format));
        m
    }

    /// Record the metering of a planned group (`camera`, whose rect in the scene texture
    /// `scene` is `rect`) into its E. Returns the camera's E (view, generation) for the bloom
    /// prefilter and the composite: the one just written, or on a replay (`replay`: no metering)
    /// the last one written; `None` before the first metering.
    pub(crate) fn encode(
        &mut self,
        cx: &mut PostEncode<'_>,
        camera: Entity,
        plan: MeterPlan,
        (scene, scene_generation): (&wgpu::TextureView, u64),
        rect: [u32; 4],
        replay: bool,
    ) -> Option<(wgpu::TextureView, u64)> {
        let tick = self.tick;
        self.pooled_len = 0;
        if replay {
            let s = self.cameras.get(&camera)?;
            return s.current().map(|t| (t.view.clone(), t.generation));
        }
        let format = self.cameras.get(&camera)?.format;
        match plan.path {
            MeterPath::Compute => {
                self.compute_meter(cx.device);
            }
            MeterPath::RenderPasses => {
                self.pass_meter(cx.device, format);
            }
        }
        let Self {
            params,
            slot,
            cameras,
            compute,
            passes,
            compute_bgs,
            params_bg,
            source_bgs,
            generation,
            created,
            pooled,
            pooled_len,
            ..
        } = self;
        let state = cameras.get_mut(&camera)?;
        let offset = (plan.slot as usize * *slot) as u32;
        match plan.path {
            MeterPath::Compute => {
                let meter = compute.as_ref()?;
                if state.textures[0].is_none() {
                    state.textures[0] = Some(exposure_texture(cx.device, format, generation, created));
                }
                if state.state.is_none() {
                    *generation += 1;
                    let buffer = cx.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("space3d exposure state"),
                        size: 16,
                        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    });
                    state.state = Some((buffer, *generation));
                }
                let (state_buffer, state_generation) = state.state.as_ref()?;
                let key = (params.gpu.generation, scene_generation, *state_generation);
                let bg = compute_bgs.entry(key).or_insert_with(|| {
                    let bg = cx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("space3d exposure (compute)"),
                        layout: &meter.bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: params.buffer(),
                                    offset: 0,
                                    size: wgpu::BufferSize::new(std::mem::size_of::<GpuExposure>() as u64),
                                }),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(scene),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: meter.histogram.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: state_buffer.as_entire_binding(),
                            },
                        ],
                    });
                    (bg, tick)
                });
                bg.1 = tick;
                let s = stride(rect[2], rect[3]);
                let groups = (rect[2].div_ceil(s).div_ceil(16), rect[3].div_ceil(s).div_ceil(16));
                {
                    let mut pass = cx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("space3d exposure"),
                        timestamp_writes: None,
                    });
                    pass.set_bind_group(0, &bg.0, &[offset]);
                    pass.set_pipeline(&meter.histogram_pipeline);
                    pass.dispatch_workgroups(groups.0.max(1), groups.1.max(1), 1);
                    pass.set_pipeline(&meter.average_pipeline);
                    pass.dispatch_workgroups(1, 1, 1);
                }
                cx.stats.compute_passes += 1;
                cx.stats.compute_dispatches += 2;
                let target = state.textures[0].as_ref()?;
                cx.encoder.copy_buffer_to_texture(
                    wgpu::TexelCopyBufferInfo {
                        buffer: state_buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: None,
                            rows_per_image: None,
                        },
                    },
                    target.texture.as_image_copy(),
                    wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
                state.current = 0;
            }
            MeterPath::RenderPasses => {
                let meter = passes.as_ref()?;
                for t in &mut state.textures {
                    if t.is_none() {
                        *t = Some(exposure_texture(cx.device, format, generation, created));
                    }
                }
                if params_bg.as_ref().is_none_or(|(_, g)| *g != params.gpu.generation) {
                    let bg = cx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("space3d exposure params"),
                        layout: &meter.params_bgl,
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: params.buffer(),
                                offset: 0,
                                size: wgpu::BufferSize::new(std::mem::size_of::<GpuExposure>() as u64),
                            }),
                        }],
                    });
                    *params_bg = Some((bg, params.gpu.generation));
                }
                let pbg = &params_bg.as_ref()?.0;
                // Levels: a quarter of the rect, then an eighth of the previous one while larger
                // than 16 × 16.
                let mut sizes = [(0u32, 0u32); 8];
                let mut n = 0;
                let mut size = (
                    rect[2].div_ceil(LOGLUM_BLOCK).max(1),
                    rect[3].div_ceil(LOGLUM_BLOCK).max(1),
                );
                sizes[0] = size;
                n += 1;
                while (size.0 > ADAPT_MAX || size.1 > ADAPT_MAX) && n < sizes.len() {
                    size = (size.0.div_ceil(REDUCE), size.1.div_ceil(REDUCE));
                    sizes[n] = size;
                    n += 1;
                }
                let mut levels = [0usize; 8];
                for (i, &size) in sizes[..n].iter().enumerate() {
                    levels[i] = cx.pool.acquire(
                        cx.device,
                        PoolDesc {
                            size,
                            format: LEVEL_FORMAT,
                            mips: 1,
                            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                        },
                    );
                    // Rgba16Float: 8 bytes per texel (`RenderStats::target_bytes`).
                    let bytes = 8 * size.0 as u64 * size.1 as u64;
                    pooled[i] = (cx.pool.get(levels[i]).generation, bytes);
                }
                *pooled_len = n;
                let previous = state.current;
                let next = 1 - previous;
                let prev_view = &state.textures[previous].as_ref()?.view;
                let prev_generation = state.textures[previous].as_ref()?.generation;
                for pass_index in 0..=n {
                    // 0: S → level 0; 1..n: level i − 1 → level i; n: last level + E → E.
                    let (kind, source, source_generation) = if pass_index == 0 {
                        (0u8, scene, scene_generation)
                    } else {
                        let t = cx.pool.get(levels[pass_index - 1]);
                        (1u8, &t.view, t.generation)
                    };
                    let adapt = pass_index == n;
                    let key = (
                        if adapt { 2 } else { kind },
                        source_generation,
                        if adapt { prev_generation } else { 0 },
                    );
                    let bg = source_bgs.entry(key).or_insert_with(|| {
                        let bg = cx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("space3d exposure source"),
                            layout: &meter.source_bgl,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(source),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::TextureView(prev_view),
                                },
                            ],
                        });
                        (bg, tick)
                    });
                    bg.1 = tick;
                    let (target, pipeline) = if adapt {
                        (&state.textures[next].as_ref()?.view, meter.adapt.get(&format)?)
                    } else if pass_index == 0 {
                        (&cx.pool.get(levels[0]).view, &meter.loglum)
                    } else {
                        (&cx.pool.get(levels[pass_index]).view, &meter.reduce)
                    };
                    let mut pass = cx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("space3d exposure"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: target,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        ..Default::default()
                    });
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(0, pbg, &[offset]);
                    pass.set_bind_group(1, &bg.0, &[]);
                    pass.draw(0..3, 0..1);
                    drop(pass);
                    cx.stats.render_passes += 1;
                    cx.stats.fullscreen_passes += 1;
                    cx.stats.post_passes += 1;
                }
                state.current = next;
            }
        }
        state.snap = false;
        state.current().map(|t| (t.view.clone(), t.generation))
    }

    /// `camera`'s current E: its texture and format (readback).
    pub(crate) fn texture(&self, camera: Entity) -> Option<(&wgpu::Texture, wgpu::TextureFormat)> {
        let s = self.cameras.get(&camera)?;
        s.current().map(|t| (&t.texture, s.format))
    }

    /// The pooled textures (generation, bytes) the last `encode` used.
    pub(crate) fn pooled(&self) -> &[(u64, u64)] {
        &self.pooled[..self.pooled_len]
    }

    /// Textures created since the last call.
    pub(crate) fn take_created(&mut self) -> u32 {
        std::mem::take(&mut self.created)
    }

    /// Drop histories unused for 60 renderer frames and bind groups unused for 3.
    pub(crate) fn end_frame(&mut self) {
        let tick = self.tick;
        self.cameras.retain(|_, s| s.last_used + 60 > tick);
        self.compute_bgs.retain(|_, (_, used)| *used + 3 > tick);
        self.source_bgs.retain(|_, (_, used)| *used + 3 > tick);
    }
}

/// A new 1×1 exposure texture of `format` (a new `generation`, counted in `created`).
fn exposure_texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    generation: &mut u64,
    created: &mut u32,
) -> ExposureTexture {
    *generation += 1;
    *created += 1;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d exposure"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    ExposureTexture {
        view: texture.create_view(&Default::default()),
        texture,
        generation: *generation,
    }
}

/// A render-pass metering pipeline: the full-screen triangle and `entry`, writing `format`.
fn pass_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    entry: &str,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
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
            compilation_options: Default::default(),
            targets: &[Some(format.into())],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// CPU mirror of the compute path's metering (tests): the target exposure of samples
/// `(luminance, metering weight)` with `ae`'s clamp and percentile window, through the same
/// 255-bin histogram (with the samples' mean position in each bin). `None` when nothing is
/// metered (all black).
#[cfg(test)]
pub(crate) fn histogram_target(samples: impl IntoIterator<Item = (f32, f32)>, ae: &AutoExposure) -> Option<f32> {
    let (min_ev, max_ev, low, high) = sanitized(ae);
    let mut bins = [0u64; 256];
    let mut positions = [0u64; 256];
    for (lum, w) in samples {
        if lum > LOG2_MIN.exp2() {
            let x = ((lum.log2() - LOG2_MIN) / (LOG2_MAX - LOG2_MIN)).clamp(0.0, 1.0) * 255.0;
            let bin = (x as usize).min(254);
            let at = (((x - bin as f32) * 64.0) as u64).min(63);
            let w = (w * 16.0 + 0.5) as u64;
            bins[1 + bin] += w;
            positions[1 + bin] += w * at;
        }
    }
    let total: u64 = bins.iter().sum();
    let (lo, hi) = (total as f64 * low as f64, total as f64 * high as f64);
    let (mut sw, mut swl, mut incl) = (0.0f64, 0.0f64, 0u64);
    for (b, &count) in bins.iter().enumerate().skip(1) {
        let excl = incl;
        incl += count;
        let portion = ((incl as f64).min(hi) - (excl as f64).max(lo)).max(0.0);
        let inside = (positions[b] as f64 / count.max(1) as f64 + 0.5) / 64.0;
        let center = LOG2_MIN as f64 + (b as f64 - 1.0 + inside) * (LOG2_MAX - LOG2_MIN) as f64 / 255.0;
        sw += portion;
        swl += portion * center;
    }
    (sw > 0.0).then(|| ((KEY.log2() as f64 - swl / sw) as f32).clamp(min_ev, max_ev))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GPU milliseconds of metering a 1920 × 1080 rect on `path`, the GPU loaded: `rounds`
    /// command buffers of `runs` meterings between two empty timestamped passes, best round.
    /// `None` without an adapter, `TIMESTAMP_QUERY` or the path.
    fn gpu_ms_per_metering(path: MeterPath, runs: u32, rounds: u32) -> Option<f64> {
        let r = pollster::block_on(crate::Renderer::new_headless(64, 64)).ok()?;
        let caps = r.graphics_caps().clone();
        if !caps.timestamps {
            return None;
        }
        let format = caps.exposure_format?;
        if path == MeterPath::Compute && !(caps.compute_histogram && format == wgpu::TextureFormat::R32Float) {
            return None;
        }
        let (device, queue) = (r.device(), r.queue());
        let size = (1920u32, 1080u32);
        let scene_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("exposure gate scene"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // A gradient over 12 stops: every bin range is used.
        let mut data = Vec::with_capacity((size.0 * size.1 * 4) as usize);
        for y in 0..size.1 {
            for x in 0..size.0 {
                let v = crate::instance::f32_to_f16((x as f32 / size.0 as f32 * 12.0 - 8.0 + y as f32 * 1e-3).exp2());
                data.extend_from_slice(&[v, v, v, crate::instance::f32_to_f16(1.0)]);
            }
        }
        queue.write_texture(
            scene_texture.as_image_copy(),
            bytemuck::cast_slice(&data),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size.0 * 8),
                rows_per_image: None,
            },
            scene_texture.size(),
        );
        let scene = scene_texture.create_view(&Default::default());
        let align = device.limits().min_uniform_buffer_offset_alignment as usize;
        let mut meters = Meters::new(align);
        let camera = space3d_core::World::new().spawn(());
        let rect = [0, 0, size.0, size.1];
        let mut reallocs = 0;
        meters.begin_frame();
        let plan = meters.plan(
            camera,
            rect,
            &AutoExposure::on(),
            (path, format),
            1.0 / 60.0,
            &mut reallocs,
        );
        let mut up = Uploader::new(device);
        let mut encoder = device.create_command_encoder(&Default::default());
        meters.flush(device, &mut encoder, &mut up);
        up.finish();
        queue.submit([encoder.finish()]);
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("exposure gate"),
            ty: wgpu::QueryType::Timestamp,
            count: 2 * rounds,
        });
        let bytes = 16 * rounds as u64;
        let buffer = |usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("exposure gate"),
                size: bytes,
                usage,
                mapped_at_creation: false,
            })
        };
        let resolve = buffer(wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC);
        let read = buffer(wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST);
        let marker = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("exposure gate marker"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let mut pool = crate::frame::TexturePool::default();
        let mut stats = crate::RenderStats::default();
        let mut record = |encoder: &mut wgpu::CommandEncoder, timed: Option<u32>| {
            let mark = |encoder: &mut wgpu::CommandEncoder, begin: Option<u32>, end: Option<u32>| {
                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &marker,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    timestamp_writes: Some(wgpu::RenderPassTimestampWrites {
                        query_set: &queries,
                        beginning_of_pass_write_index: begin,
                        end_of_pass_write_index: end,
                    }),
                    ..Default::default()
                });
            };
            if let Some(round) = timed {
                mark(encoder, None, Some(2 * round));
            }
            for _ in 0..runs {
                pool.begin_frame(0);
                let mut cx = PostEncode {
                    device,
                    queue,
                    encoder,
                    stats: &mut stats,
                    pool: &mut pool,
                };
                meters
                    .encode(&mut cx, camera, plan, (&scene, 1), rect, false)
                    .expect("metered");
                pool.release_all();
            }
            if let Some(round) = timed {
                mark(encoder, Some(2 * round + 1), None);
            }
        };
        for _ in 0..30 {
            let mut encoder = device.create_command_encoder(&Default::default());
            record(&mut encoder, None);
            queue.submit([encoder.finish()]);
        }
        for round in 0..rounds {
            let mut encoder = device.create_command_encoder(&Default::default());
            record(&mut encoder, Some(round));
            queue.submit([encoder.finish()]);
        }
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.resolve_query_set(&queries, 0..2 * rounds, &resolve, 0);
        encoder.copy_buffer_to_buffer(&resolve, 0, &read, 0, bytes);
        queue.submit([encoder.finish()]);
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let t: Vec<u64> = bytemuck::pod_collect_to_vec(&read.slice(..).get_mapped_range().expect("mapped"));
        let period = queue.get_timestamp_period() as f64 * 1e-6;
        let best = t
            .chunks(2)
            .map(|p| p[1].saturating_sub(p[0]) as f64 * period / runs as f64)
            .filter(|ms| *ms > 0.0)
            .fold(f64::MAX, f64::min);
        assert_eq!(r.gpu_error_count(), 0);
        Some(best)
    }

    /// Measurement (spec §4.4: auto exposure 0.05 ms on T1, 0.15 ms on T2): metering a 1080p
    /// rect on each path, the GPU loaded. Run alone: `cargo test -p space3d-render --lib
    /// exposure_gpu_time -- --ignored --nocapture`.
    #[test]
    #[ignore = "perf measurement: run alone with --ignored"]
    fn exposure_gpu_time() {
        for path in [MeterPath::Compute, MeterPath::RenderPasses] {
            match gpu_ms_per_metering(path, 10, 30) {
                Some(ms) => eprintln!("{path:?}: {ms:.4} ms GPU per 1080p metering (loaded)"),
                None => eprintln!("{path:?}: not measurable here"),
            }
        }
    }

    #[test]
    fn gpu_exposure_matches_wgsl() {
        for src in [compute_source(), downlevel_source()] {
            let module = naga::front::wgsl::parse_str(&src).unwrap();
            let (_, params) = module
                .types
                .iter()
                .find(|(_, t)| t.name.as_deref() == Some("ExposureParams"))
                .unwrap();
            let naga::TypeInner::Struct { members, span } = &params.inner else {
                panic!("ExposureParams is a struct")
            };
            assert_eq!(*span as usize, std::mem::size_of::<GpuExposure>());
            let offset = |name: &str| members.iter().find(|m| m.name.as_deref() == Some(name)).unwrap().offset as usize;
            assert_eq!(offset("sampling"), std::mem::offset_of!(GpuExposure, sampling));
            assert_eq!(offset("range"), std::mem::offset_of!(GpuExposure, range));
            assert_eq!(offset("adapt"), std::mem::offset_of!(GpuExposure, adapt));
            assert_eq!(offset("step"), std::mem::offset_of!(GpuExposure, step));
            crate::pipeline::validate_wgsl(&src).unwrap_or_else(|e| panic!("{e}"));
        }
    }

    #[test]
    fn strides_bound_the_samples() {
        assert_eq!(stride(320, 180), 1);
        assert_eq!(stride(1280, 720), 2);
        assert_eq!(stride(1920, 1080), 2);
        assert_eq!(stride(2560, 1440), 4);
        assert_eq!(stride(3840, 2160), 4);
        assert_eq!(stride(7680, 4320), 4, "capped: the render-pass path's blocks are 4 px");
    }

    #[test]
    fn adaptation_factors() {
        assert_eq!(adapt_factor(0.0, 3.0), 0.0, "time did not advance");
        assert_eq!(adapt_factor(1.0 / 60.0, f32::INFINITY), 1.0);
        assert_eq!(
            adapt_factor(0.0, f32::INFINITY),
            1.0,
            "instant converges without time too"
        );
        assert_eq!(adapt_factor(1.0 / 60.0, 0.0), 0.0);
        assert_eq!(adapt_factor(1.0 / 60.0, f32::NAN), 0.0);
        // One second at rate 1 closes half the gap; at rate 3, 7/8 of it.
        let frames = |rate: f32| {
            let k = adapt_factor(1.0 / 60.0, rate);
            1.0 - (1.0 - k).powi(60)
        };
        assert!((frames(1.0) - 0.5).abs() < 1e-4);
        assert!((frames(3.0) - 0.875).abs() < 1e-4);
    }

    #[test]
    fn settings_are_sanitized() {
        let ae = AutoExposure {
            min_ev: 4.0,
            max_ev: -2.0,
            low_percentile: 0.95,
            high_percentile: 0.5,
            ..AutoExposure::on()
        };
        assert_eq!(sanitized(&ae), (-2.0, 4.0, 0.95, 0.96));
        let nan = AutoExposure {
            min_ev: f32::NAN,
            low_percentile: f32::NAN,
            ..AutoExposure::on()
        };
        assert_eq!(sanitized(&nan), (-8.0, 8.0, 0.1, 0.9));
        let p = GpuExposure::new([0, 0, 1920, 1080], &AutoExposure::on(), true, 1.0 / 60.0);
        assert_eq!(p.sampling, [2, 1, 0, 0]);
        assert_eq!(p.step[2], 1.0);
        assert_eq!(p.range, [-16.0, 1.0 / 32.0, 0.1, 0.9]);
    }

    /// The histogram costs at most 1/128 of a bin (0.001 EV) against the exact trimmed mean of a
    /// uniform scene, a fraction of a bin otherwise; black is not metered; the clamp applies.
    #[test]
    fn histogram_mirror_matches_exact_metering() {
        let ae = AutoExposure {
            metering: Metering::Average,
            ..AutoExposure::on()
        };
        // A uniform scene of luminance L: target log2(0.18 / L).
        for l in [0.01f32, 0.18, 0.5, 1.0, 3.7, 20.0] {
            let t = histogram_target((0..1000).map(|_| (l, 1.0)), &ae).unwrap();
            assert!((t - (0.18 / l).log2()).abs() <= 0.002, "{l}: {t}");
        }
        // 90 % black: the rest decides.
        let samples = (0..1000).map(|i| (if i < 900 { 0.0 } else { 0.5 }, 1.0));
        let t = histogram_target(samples, &ae).unwrap();
        assert!((t - (0.18f32 / 0.5).log2()).abs() < 0.07, "{t}");
        assert_eq!(histogram_target((0..10).map(|_| (0.0, 1.0)), &ae), None);
        // The brightest 10 % (a light source) is cut by the percentile window.
        let samples = (0..1000).map(|i| (if i < 50 { 500.0 } else { 0.18 }, 1.0));
        let t = histogram_target(samples, &ae).unwrap();
        assert!(t.abs() < 0.07, "{t}");
        // Clamped.
        let t = histogram_target((0..10).map(|_| (1e-4, 1.0)), &ae).unwrap();
        assert_eq!(t, 8.0);
    }
}
