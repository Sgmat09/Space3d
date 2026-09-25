//! Draw commands of the built-in batches and their encoding into a view's pass (material bind
//! groups, pipelines, vertex buffers, direct / indirect / multi-draw calls).

use super::views::{ViewData, ViewTarget};
use super::{ARGS_SIZE, FrameState, uniform_binding};
use crate::arena::VERTEX_SIZE;
use crate::assets::RenderAssets;
use crate::handles::ShaderHandle;
use crate::pipeline::{GpuMaterial, PipelineKey, Pipelines};
use crate::renderer::Renderer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VertexSource {
    Ecs,
    Explicit(u32),
    CpuVisible,
    CullOut,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DrawKind {
    Direct { instances: u32 },
    Indirect { args: u32 },
    Multi { args_first: u32, count: u32 },
    MultiCount { args_first: u32, count: u32, run: u32 },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DrawCmd {
    pub(super) layout: u16,
    pub(super) class: u8,
    pub(super) material: u32,
    /// Custom shader of the material and its source version (resolved in `prepare`).
    pub(super) shader: Option<(ShaderHandle, u32)>,
    pub(super) first_index: u32,
    pub(super) index_count: u32,
    pub(super) base_vertex: i32,
    pub(super) vb: VertexSource,
    pub(super) vb_offset: u64,
    pub(super) kind: DrawKind,
}

/// What selects a draw's forward pipeline besides the pass's target format and samples:
/// (instance layout, class, layered texture, custom shader and its version).
pub(super) type PipelineState = (u16, u8, bool, Option<(ShaderHandle, u32)>);

/// Where vertex slot 0 starts in the mesh arena (in vertices) and the `base_vertex` to draw
/// with. Normally the whole arena is bound and draws select their mesh with `base_vertex`.
/// Without `DownlevelFlags::BASE_VERTEX` (WebGL2, GLES < 3.2) a non-zero base is not possible
/// in direct draws — wgpu does not reject it, and the GL backend then calls a missing entry
/// point (a panic on the web) — so the arena is bound from the mesh's first vertex instead
/// (`36 * n` bytes: always a valid 4-byte-aligned offset) and the draw uses base 0. Indirect
/// draws carry their base in the args; GPU paths are only chosen with `BASE_VERTEX`.
pub(super) fn direct_vertex_base(base_vertex: i32, direct: bool, supported: bool) -> (u32, i32) {
    if direct && !supported && base_vertex > 0 {
        (base_vertex as u32, 0)
    } else {
        (0, base_vertex)
    }
}

impl FrameState {
    /// Resolve the custom shaders of this frame's draws and (re)build the material bind group
    /// of every texture they sample.
    pub(super) fn prepare_material_bind_groups(
        &mut self,
        device: &wgpu::Device,
        assets: &RenderAssets,
        pipelines: &mut Pipelines,
        white: &wgpu::TextureView,
        white_sampler: &wgpu::Sampler,
    ) {
        let f = self;
        let mgen = f.materials_buf.gpu.generation;
        for d in 0..f.draws.len() {
            // Custom shaders: resolve the current source version (stale handles: built-in).
            if let Some(h) = f.materials.shader(f.draws[d].material) {
                f.draws[d].shader = assets.shaders.get(h.raw()).map(|e| {
                    pipelines.sync_custom(h, e.version, &e.shader);
                    (h, e.version)
                });
            }
            let tex = f.materials.texture(f.draws[d].material);
            let (view, sampler, version, layered) = match tex.and_then(|h| assets.textures.get(h.raw())) {
                Some(e) => match &e.gpu {
                    Some(g) => (&g.view, &g.sampler, e.version, g.layered),
                    None => (white, white_sampler, u32::MAX, false),
                },
                None => (white, white_sampler, u32::MAX, false),
            };
            let valid = f
                .material_bgs
                .get(&tex)
                .is_some_and(|(_, g, v, _)| *g == mgen && *v == version);
            if !valid {
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("space3d material"),
                    layout: pipelines.material_bgl(layered),
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform_binding(f.materials_buf.buffer(), std::mem::size_of::<GpuMaterial>()),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                    ],
                });
                f.material_bgs.insert(tex, (bg, mgen, version, layered));
            }
        }
    }
}

impl Renderer {
    /// Record a view's draws into its pass.
    pub(super) fn encode_draws(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        view: &ViewData,
        format: wgpu::TextureFormat,
        samples: u32,
        manual_srgb: bool,
    ) {
        let f = &self.frame;
        let (Some((vb, ib)), Some((view_bg, _))) = (&f.arena_buffers, &f.view_bg) else {
            return;
        };
        if view.draws.is_empty() {
            return;
        }
        pass.set_bind_group(0, view_bg, &[view.view_offset]);
        pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
        let material_stride = f.material_stride() as u32;
        let base_vertex_supported = self.caps.base_vertex;
        // The render target this pass draws into must not be sampled by it.
        let own_target = match &view.target {
            ViewTarget::Texture { handle, .. } if view.feedback => Some(*handle),
            _ => None,
        };
        let mut last_pipeline: Option<PipelineState> = None;
        let mut last_material = u32::MAX;
        let mut layered = false;
        let mut last_vb: Option<(VertexSource, u64)> = None;
        // First arena vertex bound to slot 0 (see `direct_vertex_base`).
        let mut slot0: Option<u32> = None;
        #[cfg(test)]
        let mut max_direct_base_vertex = self.frame.max_direct_base_vertex;
        for cmd in &f.draws[view.draws.clone()] {
            // (Indirect draws take their counts from the args buffer.)
            let direct = matches!(cmd.kind, DrawKind::Direct { .. });
            if direct && cmd.index_count == 0 {
                continue;
            }
            if cmd.material != last_material {
                let tex = f.materials.texture(cmd.material);
                let bg = match tex {
                    // Render targets and their feedback copies are single-layer.
                    Some(t) if Some(t) == own_target => f
                        .feedback
                        .get(&t)
                        .and_then(|fb| fb.bind_group.as_ref())
                        .map(|(bg, _)| (bg, false)),
                    _ => f.material_bgs.get(&tex).map(|(bg, _, _, l)| (bg, *l)),
                };
                let Some((bg, l)) = bg else {
                    continue;
                };
                pass.set_bind_group(1, bg, &[cmd.material * material_stride]);
                self.stats.material_switches += 1;
                last_material = cmd.material;
                layered = l;
            }
            if last_pipeline != Some((cmd.layout, cmd.class, layered, cmd.shader)) {
                let key = PipelineKey {
                    layout: cmd.layout,
                    class: cmd.class,
                    format,
                    samples,
                    manual_srgb,
                    layered,
                    shader: cmd.shader,
                };
                pass.set_pipeline(self.pipelines.forward(&self.device, key));
                self.stats.pipeline_switches += 1;
                last_pipeline = Some((cmd.layout, cmd.class, layered, cmd.shader));
            }
            let (first_vertex, base_vertex) = direct_vertex_base(cmd.base_vertex, direct, base_vertex_supported);
            if slot0 != Some(first_vertex) {
                pass.set_vertex_buffer(0, vb.slice(first_vertex as u64 * VERTEX_SIZE..));
                slot0 = Some(first_vertex);
            }
            if last_vb != Some((cmd.vb, cmd.vb_offset)) {
                let buffer = match cmd.vb {
                    VertexSource::Ecs => f.ecs.buffer(),
                    VertexSource::CpuVisible => f.cpu_visible.buffer(),
                    VertexSource::CullOut => f.cull_out.get(),
                    VertexSource::Explicit(slot) => match f.explicit.get(slot as usize) {
                        Some(Some(g)) => g.buf.get(),
                        _ => continue,
                    },
                };
                pass.set_vertex_buffer(1, buffer.slice(cmd.vb_offset..));
                last_vb = Some((cmd.vb, cmd.vb_offset));
            }
            let indices = cmd.first_index..cmd.first_index + cmd.index_count;
            match cmd.kind {
                DrawKind::Direct { instances } => {
                    #[cfg(test)]
                    {
                        max_direct_base_vertex = max_direct_base_vertex.max(base_vertex);
                    }
                    pass.draw_indexed(indices, base_vertex, 0..instances);
                }
                DrawKind::Indirect { args } => {
                    pass.draw_indexed_indirect(f.args.get(), args as u64 * ARGS_SIZE);
                    self.stats.indirect_draw_calls += 1;
                }
                DrawKind::Multi { args_first, count } => {
                    pass.multi_draw_indexed_indirect(f.args.get(), args_first as u64 * ARGS_SIZE, count);
                    self.stats.indirect_draw_calls += 1;
                    self.stats.multi_draw_calls += 1;
                }
                DrawKind::MultiCount { args_first, count, run } => {
                    pass.multi_draw_indexed_indirect_count(
                        f.compact_args.get(),
                        args_first as u64 * ARGS_SIZE,
                        f.counts.get(),
                        run as u64 * 4,
                        count,
                    );
                    self.stats.indirect_draw_calls += 1;
                    self.stats.multi_draw_calls += 1;
                }
            }
            self.stats.draw_calls += 1;
        }
        #[cfg(test)]
        {
            self.frame.max_direct_base_vertex = max_direct_base_vertex;
        }
    }
}
