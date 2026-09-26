//! Depth-only pipelines of depth views (shadow maps; graphics spec §3.4–§3.5, milestone F7):
//! `depth.wgsl` composed with the instance layout and the material's texture kind, one pipeline
//! per (`PipelineKey`, depth bias).

use super::forward::texture_wgsl;
use super::instance::{instance_attributes, instance_wgsl};
use super::{
    CLASS_DOUBLE_SIDED, DEPTH_FORMAT, KIND_COLOR_LAYERED, KIND_DATA_LAYERED, PassKind, PipelineKey, Pipelines,
    VIEW_STRUCTS,
};
use crate::arena::{VERTEX_ATTRIBUTES, VERTEX_SIZE};
use crate::instance::InstanceLayout;
use std::borrow::Cow;

const DEPTH_TEMPLATE: &str = include_str!("../shaders/depth.wgsl");

/// The depth shader for an instance layout and texture kinds (`PipelineKey::kinds`).
pub(crate) fn depth_shader(layout: &InstanceLayout, kinds: u8) -> String {
    DEPTH_TEMPLATE
        .replace("//#VIEW", VIEW_STRUCTS)
        .replace(
            "//#TEXTURE",
            &texture_wgsl(kinds & KIND_COLOR_LAYERED != 0, kinds & KIND_DATA_LAYERED != 0),
        )
        .replace("//#INSTANCE", &instance_wgsl(layout))
}

/// `DepthBiasState` as a hashable cache key (the floats by their bits).
fn bias_key(bias: wgpu::DepthBiasState) -> [u32; 3] {
    [bias.constant as u32, bias.slope_scale.to_bits(), bias.clamp.to_bits()]
}

impl Pipelines {
    /// The depth-only pipeline of `key` (`key.pass` is `Depth` or `DepthMask`; the format,
    /// sample count, output and custom shader are ignored) with depth `bias`: `Depth32Float`,
    /// depth test `Less` with writes, back faces culled unless the class is double-sided.
    /// `Depth` pipelines have no fragment stage and bind group 0 only; `DepthMask` ones also the
    /// material (group 1) and discard masked texels.
    pub fn depth(
        &mut self,
        device: &wgpu::Device,
        key: PipelineKey,
        bias: wgpu::DepthBiasState,
    ) -> &wgpu::RenderPipeline {
        let mask = key.pass == PassKind::DepthMask;
        let key = PipelineKey {
            format: DEPTH_FORMAT,
            samples: 1,
            output: crate::output::OutputEncoding::Linear,
            shader: None,
            globals: 0,
            pass: if mask { PassKind::DepthMask } else { PassKind::Depth },
            // The material's bind group (group 1) fixes both texture kinds.
            kinds: if mask {
                key.kinds & (KIND_COLOR_LAYERED | KIND_DATA_LAYERED)
            } else {
                0
            },
            ..key
        };
        let bias = wgpu::DepthBiasState {
            clamp: if self.depth_bias_clamp { bias.clamp } else { 0.0 },
            ..bias
        };
        let cache_key = (key, bias_key(bias));
        if !self.depth.contains_key(&cache_key) {
            let layout = self.layouts[key.layout as usize];
            let module = self
                .shaders
                .entry(key.module_key())
                .or_insert_with(|| {
                    device.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("space3d depth"),
                        source: wgpu::ShaderSource::Wgsl(Cow::Owned(depth_shader(layout, key.kinds))),
                    })
                })
                .clone();
            let inst_attrs = instance_attributes(layout);
            let pipeline_layout = if mask {
                &self.layouts_by_texture[key.kinds as usize]
            } else {
                &self.depth_layout
            };
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("space3d depth"),
                layout: Some(pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(if mask { "vs_mask" } else { "vs_depth" }),
                    compilation_options: Default::default(),
                    buffers: &[
                        Some(wgpu::VertexBufferLayout {
                            array_stride: VERTEX_SIZE,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &VERTEX_ATTRIBUTES,
                        }),
                        Some(wgpu::VertexBufferLayout {
                            array_stride: layout.stride as u64,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &inst_attrs,
                        }),
                    ],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: if key.class & CLASS_DOUBLE_SIDED != 0 {
                        None
                    } else {
                        Some(wgpu::Face::Back)
                    },
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: Default::default(),
                    bias,
                }),
                multisample: Default::default(),
                fragment: mask.then_some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_mask"),
                    compilation_options: Default::default(),
                    targets: &[],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.depth.insert(cache_key, pipeline);
        }
        &self.depth[&cache_key]
    }

    /// Depth pipelines built so far (tests).
    #[cfg(test)]
    pub fn depth_pipeline_count(&self) -> usize {
        self.depth.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{InstanceCompact, InstanceData, InstanceHalf, InstancePacked, InstanceTransform};

    #[test]
    fn depth_shaders_validate_for_every_layout_and_kind() {
        for l in [
            InstanceTransform::LAYOUT,
            InstanceCompact::LAYOUT,
            InstancePacked::LAYOUT,
            InstanceHalf::LAYOUT,
        ] {
            for kinds in [0, KIND_COLOR_LAYERED] {
                let src = depth_shader(l, kinds);
                let (module, info) = super::super::parse_and_validate(&src).unwrap_or_else(|e| panic!("{e}\n{src}"));
                // WebGL2 / GLES 3.0 translate it too.
                assert_eq!(super::super::forward::gles3_missing_builtin(&module), None);
                let _ = info;
            }
        }
    }

    #[test]
    fn depth_pipelines_build_and_are_cached_per_bias() {
        let Ok(mut r) = pollster::block_on(crate::Renderer::new_headless(4, 4)) else {
            eprintln!("skipping GPU test: no adapter available");
            return;
        };
        let key = PipelineKey {
            layout: 0,
            class: 0,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            samples: 4,
            output: crate::output::OutputEncoding::Linear,
            kinds: 0,
            shader: None,
            pass: PassKind::Depth,
            globals: 0,
        };
        let bias = wgpu::DepthBiasState {
            constant: 2,
            slope_scale: 1.5,
            clamp: 0.01,
        };
        let device = r.device.clone();
        r.pipelines.depth(&device, key, bias);
        // Format, samples and output do not select a depth pipeline.
        r.pipelines.depth(
            &device,
            PipelineKey {
                format: wgpu::TextureFormat::Rgba16Float,
                samples: 1,
                ..key
            },
            bias,
        );
        assert_eq!(r.pipelines.depth_pipeline_count(), 1);
        r.pipelines.depth(&device, key, Default::default());
        for (pass, kinds, class) in [
            (PassKind::DepthMask, 0, 0),
            (PassKind::DepthMask, KIND_COLOR_LAYERED, CLASS_DOUBLE_SIDED),
        ] {
            r.pipelines.depth(
                &device,
                PipelineKey {
                    pass,
                    kinds,
                    class,
                    ..key
                },
                bias,
            );
        }
        assert_eq!(r.pipelines.depth_pipeline_count(), 4);
        assert_eq!(r.gpu_error_count(), 0);
    }
}
