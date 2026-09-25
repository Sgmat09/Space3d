//! Post-processing.
//!
//! [`api`] is the public hub (re-exported from the crate root). The rest is the crate-internal
//! contract between the frame and the post area (graphics spec §2.6): the frame plans post
//! groups, hands each one's scene color, depth and distortion buffer to
//! [`PostState::encode_group`] and owns the pass slot (targets, formats, capture replay); post
//! owns the composite and its effects. The frame milestone F4 wrote the composite-only chain
//! (exposure, tonemapping, dither, alpha, rect mapping, upscale; `chain.rs`,
//! `shaders/post/composite.wgsl`); effects are the post area's (`STUB(P-POST)`). Distortion
//! buffers are real (the frame's Distortion phase plumbing, F5, clears and writes them).

pub(crate) mod api;
mod chain;
mod distortion_mesh;

use crate::caps::GraphicsCaps;
use crate::gpu::{TrackedBuffer, Uploader};
use crate::handles::FastMap;
use crate::node::{FULLSCREEN_WGSL, FrameInfo, FrameMode, RenderNodes};
use crate::output::{HASH_WGSL, OUTPUT_WGSL, OutputEncoding};
use crate::settings::FrameSettings;
use crate::stats::RenderStats;
use api::{DISTORTION_FORMAT, DistortionResolution, PostProcess};
use chain::GpuPost;
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
    params_bg: Option<(wgpu::BindGroup, u32)>,
    /// Input bind groups by (scene-texture generation, distortion-buffer generation or 0),
    /// with the frame they were last used.
    inputs_bgs: FastMap<(u64, u64), (wgpu::BindGroup, u64)>,
    /// Distortion buffers created (generation counter) and created since `end_frame`.
    distortion_generation: u64,
    created: u32,
    composite: FastMap<(wgpu::TextureFormat, OutputKind), wgpu::RenderPipeline>,
}

impl PostState {
    pub fn new(device: &wgpu::Device, caps: &GraphicsCaps) -> Self {
        let _ = caps; // STUB(P-POST): bloom / exposure formats and paths
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
            inputs_bgs: FastMap::default(),
            composite: FastMap::default(),
            distortion_generation: 0,
            created: 0,
        }
    }

    pub fn begin_frame(&mut self, frame: &FrameInfo, settings: &FrameSettings) {
        let _ = settings; // STUB(P-POST): read_exposure, gpu_timing, downlevel paths
        self.frame = frame.frame;
        self.planned = 0;
        self.params.begin();
    }

    /// Step 6: what a post group needs (bloom, auto exposure, FXAA, distortion, depth). Writes
    /// the group's composite parameters (slot = call order this frame).
    pub fn plan_group(&mut self, world: &World, group: &PostGroupPlan, reallocs: &mut u32) -> PostGroupNeeds {
        let _ = world; // STUB(P-POST): distortion writers, auto exposure history
        let params = GpuPost::new(group, self.frame);
        // Two writes: only the per-frame seeds (the last 16 bytes) change in a still scene,
        // and a write marks all of its bytes when any of them changed.
        let bytes = bytemuck::bytes_of(&params);
        let (at, split) = (self.planned * self.slot, std::mem::offset_of!(GpuPost, frame));
        self.params.write_at(at, &bytes[..split], reallocs);
        self.params.write_at(at + split, &bytes[split..], reallocs);
        self.planned += 1;
        PostGroupNeeds {
            distortion: !group.passthrough && group.settings.enabled && group.settings.distortion.enabled,
            ..Default::default()
        }
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
        let generation = self.params.gpu.generation;
        if self.params_bg.as_ref().is_none_or(|(_, g)| *g != generation) {
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
                ],
            });
            self.params_bg = Some((bg, generation));
        }
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
    /// encodes sRGB) ahead of use (`Renderer::prewarm`). STUB(P-POST): the effects' pipelines.
    pub fn prewarm(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat, manual_srgb: bool) {
        let kind = OutputKind::of(format, manual_srgb);
        self.composite_pipeline(device, format, kind);
    }

    /// The composite pipeline for `format` and `kind` (created on first use).
    fn composite_pipeline(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        kind: OutputKind,
    ) -> &wgpu::RenderPipeline {
        let (module, layout) = (&self.module, &self.layout);
        self.composite.entry((format, kind)).or_insert_with(|| {
            let constants = [("S3D_POST_OUT", kind.id())];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("space3d composite"),
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
                    entry_point: Some("fs_composite"),
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants: &constants,
                        ..Default::default()
                    },
                    targets: &[Some(format.into())],
                }),
                multiview_mask: None,
                cache: None,
            })
        })
    }

    /// Record meter → bloom → composite (→ FXAA) for one post group into the frame encoder.
    /// F4: the composite (STUB(P-POST): metering, bloom, FXAA and the other effects).
    pub fn encode_group(&mut self, cx: &mut PostEncode<'_>, input: &PostGroupInput<'_>) {
        if self.params_bg.is_none() {
            return;
        }
        let kind = OutputKind::of(input.output_format, input.output_manual_srgb);
        self.composite_pipeline(cx.device, input.output_format, kind);
        let Some((params_bg, _)) = &self.params_bg else {
            return;
        };
        let pipeline = &self.composite[&(input.output_format, kind)];
        let frame = self.frame;
        let (inputs_bgl, dummy) = (&self.inputs_bgl, &self.dummy);
        let distortion = input
            .distortion
            .and_then(|_| self.distortion.get(&input.camera))
            .map_or(0, |t| t.generation);
        let key = (input.scene_generation, distortion);
        let inputs = &mut self.inputs_bgs.entry(key).or_insert_with(|| {
            let entry = |binding, view| wgpu::BindGroupEntry {
                binding,
                resource: wgpu::BindingResource::TextureView(view),
            };
            let bg = cx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d post inputs"),
                layout: inputs_bgl,
                entries: &[
                    entry(0, input.scene),
                    entry(1, dummy),
                    entry(2, dummy),
                    entry(3, dummy),
                    entry(4, input.distortion.unwrap_or(dummy)),
                    entry(5, dummy),
                ],
            });
            (bg, frame)
        });
        inputs.1 = frame;
        let load = if input.output_clear {
            wgpu::LoadOp::Clear(wgpu::Color::BLACK)
        } else {
            wgpu::LoadOp::Load
        };
        let mut pass = cx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("space3d composite"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: input.output,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        let [x, y, w, h] = input.output_viewport;
        pass.set_viewport(x, y, w, h, 0.0, 1.0);
        let [sx, sy, sw, sh] = input.output_rect;
        pass.set_scissor_rect(sx, sy, sw, sh);
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, params_bg, &[(input.index * self.slot) as u32]);
        pass.set_bind_group(1, &inputs.0, &[]);
        pass.draw(0..3, 0..1);
        cx.stats.render_passes += 1;
        cx.stats.fullscreen_passes += 1;
        cx.stats.post_passes += 1;
    }

    pub fn end_frame(&mut self, stats: &mut RenderStats) {
        stats.textures_created += std::mem::take(&mut self.created);
        let frame = self.frame;
        self.distortion.retain(|_, t| t.last_used + 60 > frame);
        // Bind groups of scene textures no longer composited (replaced or freed).
        self.inputs_bgs.retain(|_, (_, used)| *used + 3 > frame);
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
