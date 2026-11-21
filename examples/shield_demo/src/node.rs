//! The shield render node, in the Transparent phase (order 50: scene pass, before the engine's
//! particles at 100). It draws:
//! * the hull shields (baked envelopes, front faces);
//! * the Aegis: a large bubble, its ground ring and gauge;
//! * shield fragments and sparks from one ring buffer of instanced quads;
//! * the bolts, as camera-facing streaks.
//!
//! No depth write (node contract outside Opaque); depth test against the ships.

use space3d::prelude::*;
use space3d::render::wgpu;
use space3d::render::{
    DrawContext, MESH_VERTEX_LAYOUT, MeshRef, OUTPUT_WGSL, PassTarget, PhaseSlot, PrepareContext, RenderNode,
    RenderPhase, SlotMask, VIEW_WGSL, ViewPrepareContext, scene_wgsl,
};
use std::sync::Arc;

/// Floats in the `Shield` uniform (WGSL): mat4 + 9 vec4 + 2 × 16 vec4.
pub const UNIFORM_FLOATS: usize = 16 + 9 * 4 + 2 * 16 * 4;
const UNIFORM_SIZE: u64 = (UNIFORM_FLOATS * 4) as u64;
/// Floats per particle: start xyz + birth, velocity xyz + life, colour rgb + size.
pub const PARTICLE_FLOATS: usize = 12;
/// Floats per streak: tail xyz + unused, direction xyz + length, colour rgb + half width.
pub const STREAK_FLOATS: usize = 12;
/// Particles in the ring buffer.
pub const PARTICLE_CAP: u32 = if cfg!(target_arch = "wasm32") { 16_384 } else { 32_768 };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawKind {
    Hull,
    Aegis,
    Ring,
}

#[derive(Clone)]
pub struct ShieldDraw {
    pub kind: DrawKind,
    pub mesh: MeshHandle,
    pub uniform: [f32; UNIFORM_FLOATS],
}

/// What the game hands to the node every frame.
pub struct ShieldFrame {
    /// Time and intensity for the particles and streaks (`a.w`, `color.w`).
    pub global: [f32; UNIFORM_FLOATS],
    pub draws: Vec<ShieldDraw>,
    /// New particles: (sequence number, first index, data). Kept for a few frames; the node
    /// applies those it has not seen.
    pub particle_writes: Vec<(u64, u32, Arc<Vec<f32>>)>,
    /// Particles to draw (ring-buffer high-water mark).
    pub particle_count: u32,
    pub streaks: Arc<Vec<f32>>,
}

impl Default for ShieldFrame {
    fn default() -> Self {
        Self {
            global: [0.0; UNIFORM_FLOATS],
            draws: Vec::new(),
            particle_writes: Vec::new(),
            particle_count: 0,
            streaks: Arc::new(Vec::new()),
        }
    }
}

const SHIELD_WGSL: &str = include_str!("shield.wgsl");
static SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Transparent, 50)];
const INSTANCE_ATTRS: [wgpu::VertexAttribute; 3] =
    wgpu::vertex_attr_array![4 => Float32x4, 5 => Float32x4, 6 => Float32x4];

struct Pipes {
    target: PassTarget,
    hull: wgpu::RenderPipeline,
    aegis: wgpu::RenderPipeline,
    ring: wgpu::RenderPipeline,
    particles: wgpu::RenderPipeline,
    streaks: wgpu::RenderPipeline,
}

#[derive(Default)]
pub struct ShieldNode {
    renderer: u64,
    layouts: Option<(wgpu::BindGroupLayout, wgpu::BindGroupLayout)>,
    uniform_layout: Option<wgpu::BindGroupLayout>,
    buffer: Option<(wgpu::Buffer, wgpu::BindGroup, u64)>,
    pipes: Vec<Pipes>,
    draws: Vec<(DrawKind, MeshRef, u32)>,
    bytes: Vec<u8>,
    particles: Option<wgpu::Buffer>,
    particle_count: u32,
    last_write: u64,
    streaks: Option<(wgpu::Buffer, u64)>,
    streak_count: u32,
}

fn to_bytes(data: &[f32]) -> Vec<u8> {
    data.iter().flat_map(|f| f.to_le_bytes()).collect()
}

impl ShieldNode {
    fn ensure_uniform_layout(&mut self, device: &wgpu::Device) {
        if self.uniform_layout.is_none() {
            self.uniform_layout = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("shield uniform"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE),
                    },
                    count: None,
                }],
            }));
        }
    }

    fn ensure_pipes(&mut self, device: &wgpu::Device, target: PassTarget) -> Option<usize> {
        if let Some(i) = self.pipes.iter().position(|p| p.target == target) {
            return Some(i);
        }
        self.ensure_uniform_layout(device);
        let (view, scene) = self.layouts.as_ref()?;
        let own = self.uniform_layout.as_ref()?;
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shield"),
            bind_group_layouts: &[Some(view), Some(scene), Some(own)],
            immediate_size: 0,
        });
        let src = format!(
            "{VIEW_WGSL}\n{}\n{OUTPUT_WGSL}\nconst SH_OUTPUT: u32 = {}u;\n{SHIELD_WGSL}",
            scene_wgsl(&target),
            target.output.id()
        );
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shield"),
            source: wgpu::ShaderSource::Wgsl(src.into()),
        });
        // Alpha 0 with premultiplied blending: purely additive light.
        let blend = wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING;
        let instance = [Some(wgpu::VertexBufferLayout {
            array_stride: (PARTICLE_FLOATS * 4) as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &INSTANCE_ATTRS,
        })];
        let mesh = [Some(MESH_VERTEX_LAYOUT)];
        let pipe = |vs: &str, fs: &str, buffers: &[Option<wgpu::VertexBufferLayout<'_>>], cull| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("shield"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState {
                    cull_mode: cull,
                    ..Default::default()
                },
                depth_stencil: target.depth_stencil(wgpu::CompareFunction::LessEqual, false),
                multisample: target.multisample(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(target.color_target(Some(blend)))],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipes = Pipes {
            target,
            // Front faces only: the nearest layer of the envelope, without a depth pre-pass
            // (nodes outside the Opaque phase must not write depth).
            hull: pipe("vs_hull", "fs_shield", &mesh, Some(wgpu::Face::Back)),
            aegis: pipe("vs_bubble", "fs_shield", &mesh, None),
            ring: pipe("vs_ring", "fs_ring", &mesh, None),
            particles: pipe("vs_particle", "fs_particle", &instance, None),
            streaks: pipe("vs_streak", "fs_streak", &instance, None),
        };
        self.pipes.push(pipes);
        Some(self.pipes.len() - 1)
    }
}

impl RenderNode for ShieldNode {
    fn slots(&self) -> &[PhaseSlot] {
        &SLOTS
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        if ctx.frame.renderer != self.renderer || self.layouts.is_none() {
            self.renderer = ctx.frame.renderer;
            self.layouts = Some((ctx.layouts.view.clone(), ctx.layouts.scene.clone()));
            self.uniform_layout = None;
            self.buffer = None;
            self.pipes.clear();
            self.particles = None;
            self.streaks = None;
            self.last_write = 0;
        }
        let align = u64::from(ctx.device.limits().min_uniform_buffer_offset_alignment);
        let stride = UNIFORM_SIZE.div_ceil(align) * align;
        self.draws.clear();
        self.bytes.clear();
        self.streak_count = 0;
        self.particle_count = 0;
        let Some(frame) = ctx.resources.get::<ShieldFrame>() else {
            return;
        };
        let (global, draws, writes, particle_count, streaks) = (
            frame.global,
            frame.draws.clone(),
            frame.particle_writes.clone(),
            frame.particle_count,
            frame.streaks.clone(),
        );
        // offset 0: the global block of the particles and streaks
        for f in &global {
            self.bytes.extend_from_slice(&f.to_le_bytes());
        }
        self.bytes.resize(stride as usize, 0);
        for d in &draws {
            let Some(mesh) = ctx.mesh(d.mesh) else { continue };
            let offset = self.bytes.len() as u32;
            for f in &d.uniform {
                self.bytes.extend_from_slice(&f.to_le_bytes());
            }
            self.bytes.resize(offset as usize + stride as usize, 0);
            self.draws.push((d.kind, mesh, offset));
        }
        // particles: one ring buffer, only the new ones are uploaded
        if self.particles.is_none() {
            self.particles = Some(ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("shield particles"),
                size: u64::from(PARTICLE_CAP) * (PARTICLE_FLOATS * 4) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if let Some(buffer) = self.particles.clone() {
            for (seq, first, data) in &writes {
                if *seq > self.last_write {
                    self.last_write = *seq;
                    ctx.upload(
                        &buffer,
                        u64::from(*first) * (PARTICLE_FLOATS * 4) as u64,
                        &to_bytes(data),
                    );
                }
            }
        }
        self.particle_count = particle_count.min(PARTICLE_CAP);
        // streaks: rewritten every frame
        if !streaks.is_empty() {
            let bytes = to_bytes(&streaks);
            let needed = bytes.len() as u64;
            if self.streaks.as_ref().is_none_or(|b| b.1 < needed) {
                let size = needed.next_power_of_two().max(4096);
                let buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("shield streaks"),
                    size,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.streaks = Some((buffer, size));
            }
            if let Some((buffer, _)) = &self.streaks {
                let buffer = buffer.clone();
                ctx.upload(&buffer, 0, &bytes);
            }
            self.streak_count = (streaks.len() / STREAK_FLOATS) as u32;
        }
        self.ensure_uniform_layout(ctx.device);
        let needed = self.bytes.len() as u64;
        if self.buffer.as_ref().is_none_or(|b| b.2 < needed) {
            let size = needed.next_power_of_two().max(stride * 8);
            let buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("shield uniforms"),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let Some(layout) = self.uniform_layout.as_ref() else {
                return;
            };
            let bind = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("shield uniforms"),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: wgpu::BufferSize::new(UNIFORM_SIZE),
                    }),
                }],
            });
            self.buffer = Some((buffer, bind, size));
        }
        if let Some((buffer, _, _)) = &self.buffer {
            let buffer = buffer.clone();
            ctx.upload(&buffer, 0, &self.bytes);
        }
    }

    fn prepare_view(&mut self, _ctx: &mut ViewPrepareContext<'_>) -> SlotMask {
        if self.buffer.is_none() {
            SlotMask::NONE
        } else {
            SlotMask::only(0)
        }
    }

    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        for &(_, target) in targets {
            self.ensure_pipes(device, target);
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let Some(index) = self.ensure_pipes(ctx.device, ctx.target) else {
            return;
        };
        let Some((_, bind, _)) = self.buffer.as_ref() else {
            return;
        };
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        let stride = MESH_VERTEX_LAYOUT.array_stride;
        let pipes = &self.pipes[index];
        for (kind, mesh, offset) in &self.draws {
            pass.set_pipeline(match kind {
                DrawKind::Hull => &pipes.hull,
                DrawKind::Aegis => &pipes.aegis,
                DrawKind::Ring => &pipes.ring,
            });
            pass.set_bind_group(2, bind, &[*offset]);
            // WebGL2 has no base_vertex: bind the buffer at the mesh's first vertex
            let first = mesh.base_vertex.max(0) as u64 * stride;
            pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(first..));
            pass.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(mesh.first_index..mesh.first_index + mesh.index_count, 0, 0..1);
        }
        pass.set_bind_group(2, bind, &[0]);
        if self.particle_count > 0
            && let Some(buffer) = &self.particles
        {
            pass.set_pipeline(&pipes.particles);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..6, 0..self.particle_count);
        }
        if self.streak_count > 0
            && let Some((buffer, _)) = &self.streaks
        {
            pass.set_pipeline(&pipes.streaks);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..6, 0..self.streak_count);
        }
    }
}
