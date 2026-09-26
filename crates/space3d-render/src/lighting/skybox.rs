//! The built-in skybox node (`"space3d/skybox"`, Skybox phase at order 0; graphics spec §2.2.4,
//! §5.2 PM3), written against the public node API: it draws the cubemap of every camera with a
//! [`Skybox`] whose cubemap is uploaded, after the opaque phase, at the far plane (`sky.wgsl`).
//!
//! * `prepare` (once per frame): the cameras with a skybox and a bind group per cubemap (group
//!   1: the radiance cube and a trilinear sampler), rebuilt only when a cubemap is re-uploaded.
//! * `prepare_view`: work iff the view's camera is one of them.
//! * `draw`: group 0 is the frame's view bind group (the `GpuView` carries `sky_rotation` and
//!   `sky_params`, filled by the lighting), one triangle. Pipelines per pass signature
//!   ([`PassTarget`]: format, samples, depth, output encoding), built in `prewarm`.
//!
//! Capture replays draw again from what `prepare` kept. A new renderer (device) rebuilds
//! everything.

use super::api::Skybox;
use crate::components::Camera3d;
use crate::handles::CubemapHandle;
use crate::node::{
    DrawContext, PassTarget, PhaseSlot, PrepareContext, RenderNode, RenderNodes, RenderPhase, SlotMask, VIEW_WGSL,
    ViewPrepareContext,
};
use crate::output::OUTPUT_WGSL;
use space3d_core::Entity;

/// Name of the built-in skybox node in [`RenderNodes`].
pub(crate) const NAME: &str = "space3d/skybox";

static SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Skybox, 0)];

/// Frames a cubemap's bind group is kept after its last use.
const IDLE_FRAMES: u64 = 120;

/// A cubemap's group-1 bind group.
struct SkyCube {
    cubemap: CubemapHandle,
    /// `GpuCubemap::id` it binds.
    gpu: u64,
    bind_group: wgpu::BindGroup,
    last_used: u64,
}

/// GPU objects of one renderer.
struct Gpu {
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
}

#[derive(Default)]
struct SkyboxNode {
    renderer: u64,
    frame: u64,
    gpu: Option<Gpu>,
    pipelines: Vec<(PassTarget, wgpu::RenderPipeline)>,
    cubes: Vec<SkyCube>,
    /// This frame's cameras with a skybox: (camera, `cubes` index).
    cameras: Vec<(Entity, usize)>,
}

impl SkyboxNode {
    /// The pipeline of a pass signature (`None` before the first `prepare`).
    fn pipeline(&mut self, device: &wgpu::Device, target: PassTarget) -> Option<&wgpu::RenderPipeline> {
        let gpu = self.gpu.as_ref()?;
        if let Some(i) = self.pipelines.iter().position(|(t, _)| *t == target) {
            return Some(&self.pipelines[i].1);
        }
        let constants = [("S3D_OUTPUT", target.output.id() as f64)];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("space3d skybox"),
            layout: Some(&gpu.pipeline_layout),
            vertex: wgpu::VertexState {
                module: &gpu.module,
                entry_point: Some("vs_sky"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            // At the far plane, behind everything opaque; never writes depth.
            depth_stencil: target.depth_stencil(wgpu::CompareFunction::LessEqual, false),
            multisample: target.multisample(),
            fragment: Some(wgpu::FragmentState {
                module: &gpu.module,
                entry_point: Some("fs_sky"),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &constants,
                    ..Default::default()
                },
                targets: &[Some(target.color_target(None))],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipelines.push((target, pipeline));
        self.pipelines.last().map(|(_, p)| p)
    }
}

impl RenderNode for SkyboxNode {
    fn slots(&self) -> &[PhaseSlot] {
        &SLOTS
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        if ctx.frame.renderer != self.renderer || self.gpu.is_none() {
            *self = Self {
                renderer: ctx.frame.renderer,
                ..Default::default()
            };
            let device = ctx.device;
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d skybox"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::Cube,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
            let view_layout = ctx.layouts.view.clone();
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space3d skybox"),
                bind_group_layouts: &[Some(&view_layout), Some(&layout)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d skybox"),
                source: wgpu::ShaderSource::Wgsl(
                    format!("{VIEW_WGSL}\n{OUTPUT_WGSL}\n{}", include_str!("../shaders/sky.wgsl")).into(),
                ),
            });
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("space3d skybox"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                ..Default::default()
            });
            self.gpu = Some(Gpu {
                layout,
                pipeline_layout,
                module,
                sampler,
            });
        }
        self.frame += 1;
        let frame = self.frame;
        // Drop idle bind groups before handing out indices.
        self.cubes.retain(|c| c.last_used + IDLE_FRAMES >= frame);
        self.cameras.clear();
        let Some(gpu) = &self.gpu else { return };
        for (camera, _, sky) in ctx.world.query::<(Entity, &Camera3d, &Skybox)>().iter() {
            let Some(cube) = ctx.assets.cubemap_gpu(sky.cubemap) else {
                continue;
            };
            let index = match self.cubes.iter().position(|c| c.cubemap == sky.cubemap) {
                Some(i) if self.cubes[i].gpu == cube.id => i,
                found => {
                    let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("space3d skybox"),
                        layout: &gpu.layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&cube.view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                            },
                        ],
                    });
                    let entry = SkyCube {
                        cubemap: sky.cubemap,
                        gpu: cube.id,
                        bind_group,
                        last_used: frame,
                    };
                    match found {
                        Some(i) => {
                            self.cubes[i] = entry;
                            i
                        }
                        None => {
                            self.cubes.push(entry);
                            self.cubes.len() - 1
                        }
                    }
                }
            };
            self.cubes[index].last_used = frame;
            self.cameras.push((camera, index));
        }
    }

    fn prepare_view(&mut self, ctx: &mut ViewPrepareContext<'_>) -> SlotMask {
        if self.cameras.iter().any(|(c, _)| *c == ctx.view.camera) {
            SlotMask::only(0)
        } else {
            SlotMask::NONE
        }
    }

    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        for &(_, target) in targets {
            self.pipeline(device, target);
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let Some(&(_, index)) = self.cameras.iter().find(|(c, _)| *c == ctx.view.camera) else {
            return;
        };
        let bind_group = self.cubes[index].bind_group.clone();
        let Some(pipeline) = self.pipeline(ctx.device, ctx.target) else {
            return;
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// Register the node (unless the app replaced it under the same name).
pub(super) fn register(nodes: &mut RenderNodes) {
    if !nodes.contains(NAME) {
        nodes.insert(NAME, SkyboxNode::default());
    }
}
