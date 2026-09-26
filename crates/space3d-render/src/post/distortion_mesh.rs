//! The built-in `"space3d/distortion-meshes"` node: draws entities with a
//! [`DistortionMaterial`] into the distortion buffer of every post camera that sees them
//! (graphics spec §1.8, §5.2 PO5), through the public node API only: meshes as [`MeshRef`]s in
//! [`MESH_VERTEX_LAYOUT`], CPU frustum culling per view, instance data through the staging ring,
//! and the vertex buffer bound at each mesh's first vertex (no `base_vertex`, no
//! `first_instance`: the WebGL2 path is the only path).
//!
//! * `prepare` (once per frame): extract `(DistortionMaterial, Mesh3d, GlobalTransform,
//!   Option<Visibility>)` entities, their meshes and textures, into draw order (texture, mesh,
//!   entity).
//! * `prepare_view` (per camera view): only scene views of cameras whose `PostProcess` enables
//!   distortion. Culls the items against the view frustum, appends the visible ones' instances
//!   to the frame's instance list (one run per view) and uploads that run when it differs from
//!   what the GPU buffer holds (a still scene uploads nothing).
//! * `draw`: the view's batches (one instanced draw per run of equal mesh and texture). Capture
//!   replays draw the same runs again.
//!
//! Nothing allocates in the steady state: every list keeps its capacity, and GPU objects are
//! created only when the renderer, a pass signature, a texture or the instance count changes.

use super::api::{DISTORTION_BLEND, DISTORTION_WGSL, DistortionMaterial, DistortionMode, PostProcess};
use crate::components::{Mesh3d, Visibility};
use crate::culling::Sphere;
use crate::handles::{FastMap, MeshHandle, TextureHandle};
use crate::node::{
    DepthAccess, DrawContext, FrameMode, MESH_VERTEX_LAYOUT, MeshRef, PassTarget, PhaseSlot, PrepareContext,
    RenderNode, RenderNodes, RenderPhase, SlotMask, VIEW_WGSL, ViewPrepareContext, ViewRole, scene_wgsl_for,
};
use crate::output::HASH_WGSL;
use bytemuck::{Pod, Zeroable};
use space3d_core::{Entity, GlobalTransform};
use std::ops::Range;

/// Name of the built-in node in [`RenderNodes`].
pub(crate) const NAME: &str = "space3d/distortion-meshes";

static SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Distortion, 0)];

/// The writer's own WGSL (after `VIEW_WGSL`, `scene_wgsl`, `DISTORTION_WGSL` and the hashes).
const MESH_WGSL: &str = include_str!("../shaders/post/distortion_mesh.wgsl");

/// Mode ids of `GpuInstance::params[1]` (distortion_mesh.wgsl).
const MODE_NOISE: f32 = 0.0;
const MODE_TEXTURE: f32 = 1.0;
const MODE_RADIAL: f32 = 2.0;

/// Bind groups of textures unused for this many frames are dropped.
const TEXTURE_FRAMES: u64 = 60;

/// One mesh's instance (vertex slot 1, locations 4–9): model matrix, material.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub(crate) struct GpuInstance {
    /// Model matrix columns.
    pub model: [[f32; 4]; 4],
    /// Strength (view heights), mode id, inner and outer UV radius.
    pub params: [f32; 4],
    /// UV scroll offset (x, y), soft depth (world units), 1 = sRGB texture.
    pub extra: [f32; 4],
}

const INSTANCE_ATTRIBUTES: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    4 => Float32x4,
    5 => Float32x4,
    6 => Float32x4,
    7 => Float32x4,
    8 => Float32x4,
    9 => Float32x4,
];

const INSTANCE_SIZE: u64 = std::mem::size_of::<GpuInstance>() as u64;

/// A distortion mesh of this frame.
#[derive(Clone, Copy, Debug)]
struct Item {
    /// Draw order: texture (0 = none, else first use + 1), mesh (first use), entity.
    key: (u32, u32, u64),
    /// Index into `meshes`.
    mesh: u32,
    texture: Option<TextureHandle>,
    /// World bounding sphere.
    sphere: Sphere,
    instance: GpuInstance,
}

/// One instanced draw: a run of instances with the same mesh and texture.
#[derive(Clone, Debug)]
struct Batch {
    mesh: u32,
    texture: Option<TextureHandle>,
    instances: Range<u32>,
}

/// The batches of one camera view.
#[derive(Clone, Debug)]
struct ViewDraw {
    camera: Entity,
    batches: Range<u32>,
}

/// A texture's bind group (group 2), with the texture version it was made for.
struct TextureBind {
    version: u32,
    bind_group: wgpu::BindGroup,
    srgb: bool,
    used: u64,
}

/// GPU state of one renderer.
struct Gpu {
    renderer: u64,
    layout: wgpu::PipelineLayout,
    texture_layout: wgpu::BindGroupLayout,
    /// Group 2 with a 1×1 stand-in (noise and radial meshes).
    plain: wgpu::BindGroup,
    /// Modules without and with scene depth.
    modules: [Option<wgpu::ShaderModule>; 2],
    pipelines: Vec<(PassTarget, wgpu::RenderPipeline)>,
    /// Instances of every view this frame (vertex slot 1) and its capacity in instances.
    instances: Option<wgpu::Buffer>,
    capacity: usize,
}

impl Gpu {
    fn new(device: &wgpu::Device, layouts: &crate::node::FrameLayouts, renderer: u64) -> Self {
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d distortion mesh texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("space3d distortion meshes"),
            bind_group_layouts: &[Some(&layouts.view), Some(&layouts.scene), Some(&texture_layout)],
            immediate_size: 0,
        });
        let stand_in = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("space3d distortion mesh stand-in"),
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("space3d distortion mesh stand-in"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let plain = texture_bind_group(device, &texture_layout, &stand_in, &sampler);
        Self {
            renderer,
            layout,
            texture_layout,
            plain,
            modules: [None, None],
            pipelines: Vec::new(),
            instances: None,
            capacity: 0,
        }
    }

    /// The pipeline for a pass signature (built on first use).
    fn pipeline(&mut self, device: &wgpu::Device, target: PassTarget) -> &wgpu::RenderPipeline {
        if let Some(i) = self.pipelines.iter().position(|(t, _)| *t == target) {
            return &self.pipelines[i].1;
        }
        let depth = target.depth_access != DepthAccess::None;
        let module = self.modules[usize::from(depth)].get_or_insert_with(|| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d distortion meshes"),
                source: wgpu::ShaderSource::Wgsl(shader_source(target.depth_access).into()),
            })
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("space3d distortion meshes"),
            layout: Some(&self.layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs_distortion"),
                compilation_options: Default::default(),
                buffers: &[
                    Some(MESH_VERTEX_LAYOUT),
                    Some(wgpu::VertexBufferLayout {
                        array_stride: INSTANCE_SIZE,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &INSTANCE_ATTRIBUTES,
                    }),
                ],
            },
            // Both faces: a writer is a region of the screen, not a surface.
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: target.depth_stencil(wgpu::CompareFunction::Always, false),
            multisample: target.multisample(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs_distortion"),
                compilation_options: Default::default(),
                targets: &[Some(target.color_target(Some(DISTORTION_BLEND)))],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipelines.push((target, pipeline));
        &self.pipelines.last().expect("pushed").1
    }
}

/// The writer's full source for a depth access.
pub(crate) fn shader_source(access: DepthAccess) -> String {
    format!(
        "{VIEW_WGSL}\n{}\n{DISTORTION_WGSL}\n{HASH_WGSL}\n{MESH_WGSL}",
        scene_wgsl_for(access)
    )
}

fn texture_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("space3d distortion mesh texture"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// The node (see the module docs).
#[derive(Default)]
struct DistortionMeshes {
    gpu: Option<Gpu>,
    /// This frame's meshes (first use order) and items (draw order).
    meshes: Vec<(MeshHandle, MeshRef)>,
    textures_used: Vec<TextureHandle>,
    items: Vec<Item>,
    /// Texture bind groups by texture.
    textures: FastMap<TextureHandle, TextureBind>,
    /// This frame's instances (every view's run, in view order), the GPU buffer's contents,
    /// batches and views.
    staged: Vec<GpuInstance>,
    uploaded: Vec<GpuInstance>,
    batches: Vec<Batch>,
    views: Vec<ViewDraw>,
    /// Renderer frames (texture bind-group eviction).
    tick: u64,
    warned_layered: bool,
}

impl DistortionMeshes {
    /// Extract this frame's distortion meshes.
    fn extract(&mut self, ctx: &PrepareContext<'_>) {
        let time = ctx.frame.time;
        let mut query = ctx.world.query::<(
            Entity,
            &DistortionMaterial,
            &Mesh3d,
            &GlobalTransform,
            Option<&Visibility>,
        )>();
        for (entity, m, mesh, global, visibility) in query.iter() {
            if visibility == Some(&Visibility::Hidden) || !m.strength.is_finite() || m.strength == 0.0 {
                continue;
            }
            let mesh_index = match self.meshes.iter().position(|(h, _)| *h == mesh.0) {
                Some(i) => i,
                None => {
                    let Some(r) = ctx.mesh(mesh.0) else { continue };
                    self.meshes.push((mesh.0, r));
                    self.meshes.len() - 1
                }
            };
            let (mode, inner, outer) = match m.mode {
                DistortionMode::Radial { inner, outer } => (MODE_RADIAL, inner, outer),
                _ if m.texture.is_some() => (MODE_TEXTURE, 0.0, 0.0),
                _ => (MODE_NOISE, 0.0, 0.0),
            };
            let mut texture = None;
            let mut srgb = false;
            if mode == MODE_TEXTURE
                && let Some(h) = m.texture
            {
                match self.bind_texture(ctx, h) {
                    Some(s) => {
                        texture = Some(h);
                        srgb = s;
                    }
                    // Not uploaded yet (or unusable): nothing to draw.
                    None => continue,
                }
            }
            let tex_key = texture.map_or(0, |h| {
                let i = match self.textures_used.iter().position(|t| *t == h) {
                    Some(i) => i,
                    None => {
                        self.textures_used.push(h);
                        self.textures_used.len() - 1
                    }
                };
                i as u32 + 1
            });
            // The scroll as a UV offset in 0..1 (textures should repeat; the noise does).
            let scroll = if mode == MODE_RADIAL {
                [0.0; 2]
            } else {
                let at = |s: f32| {
                    let v = s as f64 * time;
                    (v - v.floor()) as f32
                };
                [at(m.scroll.x), at(m.scroll.y)]
            };
            let model = global.0;
            let bounds = self.meshes[mesh_index].1.bounds;
            self.items.push(Item {
                key: (tex_key, mesh_index as u32, entity.to_bits().get()),
                mesh: mesh_index as u32,
                texture,
                sphere: bounds.transformed(&model),
                instance: GpuInstance {
                    model: model.to_cols_array_2d(),
                    params: [m.strength, mode, inner, outer],
                    extra: [scroll[0], scroll[1], m.soft_depth.max(0.0), f32::from(u8::from(srgb))],
                },
            });
        }
        self.items.sort_unstable_by_key(|i| i.key);
    }

    /// The bind group of texture `h` is current; returns whether the texture is sRGB-encoded.
    /// `None` until it is uploaded, and for texture arrays (warned once).
    fn bind_texture(&mut self, ctx: &PrepareContext<'_>, h: TextureHandle) -> Option<bool> {
        let t = ctx.texture(h)?;
        if t.layered {
            if !self.warned_layered {
                self.warned_layered = true;
                tracing::warn!(
                    "DistortionMaterial::texture must be a single-layer 2D texture; texture arrays are skipped"
                );
            }
            return None;
        }
        let gpu = self.gpu.as_ref()?;
        let tick = self.tick;
        match self.textures.get_mut(&h) {
            Some(b) if b.version == t.version => b.used = tick,
            _ => {
                let bind_group = texture_bind_group(ctx.device, &gpu.texture_layout, &t.view, &t.sampler);
                self.textures.insert(
                    h,
                    TextureBind {
                        version: t.version,
                        bind_group,
                        srgb: t.format.is_srgb(),
                        used: tick,
                    },
                );
            }
        }
        self.textures.get(&h).map(|b| b.srgb)
    }
}

impl RenderNode for DistortionMeshes {
    fn slots(&self) -> &[PhaseSlot] {
        &SLOTS
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        self.tick += 1;
        self.meshes.clear();
        self.textures_used.clear();
        self.items.clear();
        self.staged.clear();
        self.batches.clear();
        self.views.clear();
        if ctx.frame.device_lost {
            return;
        }
        // Cheap check first: most apps have no distortion meshes (no GPU state is created).
        if ctx.world.query::<&DistortionMaterial>().iter().next().is_none() {
            return;
        }
        if self.gpu.as_ref().is_none_or(|g| g.renderer != ctx.frame.renderer) {
            self.gpu = Some(Gpu::new(ctx.device, ctx.layouts, ctx.frame.renderer));
            self.textures.clear();
            self.uploaded.clear();
        }
        self.extract(ctx);
        let tick = self.tick;
        self.textures.retain(|_, b| b.used + TEXTURE_FRAMES > tick);
    }

    fn prepare_view(&mut self, ctx: &mut ViewPrepareContext<'_>) -> SlotMask {
        let view = ctx.view;
        // Distortion slots draw only in the scene views of post cameras with distortion on.
        if self.items.is_empty() || view.mode == FrameMode::Direct || view.role != ViewRole::Scene {
            return SlotMask::NONE;
        }
        let distorts = ctx
            .world
            .get::<&PostProcess>(view.camera)
            .is_ok_and(|p| p.enabled && p.distortion.enabled);
        let Some(gpu) = self.gpu.as_mut() else {
            return SlotMask::NONE;
        };
        if !distorts {
            return SlotMask::NONE;
        }
        let start = self.staged.len();
        let first_batch = self.batches.len();
        for item in &self.items {
            if !view.frustum.intersects_sphere(&item.sphere) {
                continue;
            }
            let i = self.staged.len() as u32;
            self.staged.push(item.instance);
            let in_view = self.batches.len() > first_batch;
            match self.batches.last_mut() {
                Some(b) if in_view && b.mesh == item.mesh && b.texture == item.texture => {
                    b.instances.end = i + 1;
                }
                _ => self.batches.push(Batch {
                    mesh: item.mesh,
                    texture: item.texture,
                    instances: i..i + 1,
                }),
            }
        }
        let end = self.staged.len();
        if end == start {
            return SlotMask::NONE;
        }
        self.views.push(ViewDraw {
            camera: view.camera,
            batches: first_batch as u32..self.batches.len() as u32,
        });
        // The instance buffer: grown (and refilled) when too small, else this view's run
        // uploaded when it differs from the buffer's contents.
        let from = if gpu.capacity < end {
            let capacity = end.next_power_of_two().max(16);
            gpu.instances = Some(ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("space3d distortion mesh instances"),
                size: capacity as u64 * INSTANCE_SIZE,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            gpu.capacity = capacity;
            self.uploaded.clear();
            0
        } else if self.uploaded.len() >= end && self.uploaded[start..end] == self.staged[start..end] {
            end
        } else {
            start
        };
        if from < end
            && let Some(buffer) = &gpu.instances
        {
            ctx.upload(
                buffer,
                from as u64 * INSTANCE_SIZE,
                bytemuck::cast_slice(&self.staged[from..end]),
            );
            if self.uploaded.len() < end {
                self.uploaded.resize(end, GpuInstance::zeroed());
            }
            self.uploaded[from..end].copy_from_slice(&self.staged[from..end]);
        }
        SlotMask::only(0)
    }

    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        for &(_, target) in targets {
            gpu.pipeline(device, target);
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let Self {
            gpu,
            meshes,
            textures,
            batches,
            views,
            ..
        } = self;
        let Some(gpu) = gpu.as_mut() else { return };
        let Some(view) = views.iter().find(|v| v.camera == ctx.view.camera) else {
            return;
        };
        let pipeline = gpu.pipeline(ctx.device, ctx.target).clone();
        let Some(instances) = &gpu.instances else { return };
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        let mut bound = None;
        for b in &batches[view.batches.start as usize..view.batches.end as usize] {
            if bound != Some(b.texture) {
                let group = b
                    .texture
                    .and_then(|h| textures.get(&h))
                    .map_or(&gpu.plain, |t| &t.bind_group);
                pass.set_bind_group(2, group, &[]);
                bound = Some(b.texture);
            }
            let mesh = &meshes[b.mesh as usize].1;
            // The arena bound at the mesh's first vertex, the run of instances at its first
            // instance: base vertex and first instance 0 (WebGL2 has neither).
            let first_vertex = mesh.base_vertex.max(0) as u64 * crate::arena::VERTEX_SIZE;
            pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(first_vertex..));
            pass.set_vertex_buffer(1, instances.slice(b.instances.start as u64 * INSTANCE_SIZE..));
            pass.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(
                mesh.first_index..mesh.first_index + mesh.index_count,
                0,
                0..b.instances.len() as u32,
            );
        }
    }
}

/// Register the node (unless the app replaced it under the same name).
pub(super) fn register(nodes: &mut RenderNodes) {
    if !nodes.contains(NAME) {
        nodes.insert(NAME, DistortionMeshes::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_layout() {
        assert_eq!(INSTANCE_SIZE, 96);
        assert_eq!(std::mem::offset_of!(GpuInstance, params), 64);
        assert_eq!(std::mem::offset_of!(GpuInstance, extra), 80);
    }

    /// The writer validates with and without scene depth, and its vertex inputs match the
    /// mesh arena and instance layouts.
    #[test]
    fn writer_shader_validates() {
        for access in [DepthAccess::None, DepthAccess::SampledOnly] {
            let src = shader_source(access);
            crate::pipeline::validate_wgsl(&src).unwrap_or_else(|e| panic!("{e}\n{src}"));
        }
        assert_eq!(MESH_VERTEX_LAYOUT.attributes.len(), 4);
        assert!(INSTANCE_ATTRIBUTES.iter().all(|a| a.shader_location >= 4));
    }
}
