//! Shader generation, GPU uniform layouts and the pipeline cache.

use crate::arena::{VERTEX_ATTRIBUTES, VERTEX_SIZE};
use crate::instance::{InstanceLayout, InstanceSemantic, wgsl_type};
use crate::handles::FastMap;
use bytemuck::{Pod, Zeroable};
use std::borrow::Cow;

/// Depth buffer format used by every pass.
pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Format of the offscreen/capture target and render-target textures.
pub(crate) const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub(crate) const MAX_DIR_LIGHTS: usize = 4;
pub(crate) const MAX_POINT_LIGHTS: usize = 16;

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(crate) struct GpuDirLight {
    pub direction: [f32; 4],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(crate) struct GpuPointLight {
    pub position_range: [f32; 4],
    pub color: [f32; 4],
}

/// Matches `View` in forward.wgsl (std140-compatible).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuView {
    pub view_proj: [[f32; 4]; 4],
    pub camera_pos: [f32; 4],
    pub ambient: [f32; 4],
    pub counts: [u32; 4],
    pub dir_lights: [GpuDirLight; MAX_DIR_LIGHTS],
    pub point_lights: [GpuPointLight; MAX_POINT_LIGHTS],
}

/// Matches `MaterialParams` in forward.wgsl.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Pod, Zeroable)]
pub(crate) struct GpuMaterial {
    pub base_color: [f32; 4],
    pub emissive: [f32; 4],
    pub params: [f32; 4],
}

/// Matches `CullParams` in cull.wgsl.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(crate) struct GpuCullParams {
    pub planes: [[f32; 4]; 6],
    pub job_start: u32,
    pub job_count: u32,
    pub total_workgroups: u32,
    pub workgroups_x: u32,
}

/// Matches `Job` in cull.wgsl.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(crate) struct GpuCullJob {
    pub sphere: [f32; 4],
    pub src_word: u32,
    pub count: u32,
    pub stride_words: u32,
    pub kind: u32,
    pub t_off: u32,
    pub s_off: u32,
    pub s_kind: u32,
    pub dst_word: u32,
    pub draw: u32,
    pub wg_start: u32,
    pub pad: [u32; 2],
}

/// Matches `CompactParams` in compact.wgsl.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(crate) struct GpuCompactParams {
    pub draw_start: u32,
    pub draw_count: u32,
    pub pad: [u32; 2],
}

/// Pipeline "class" bits that come from the material.
pub(crate) const CLASS_DOUBLE_SIDED: u8 = 1;
pub(crate) const CLASS_BLEND: u8 = 2;

/// Everything that selects a forward pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PipelineKey {
    pub layout: u16,
    pub class: u8,
    pub format: wgpu::TextureFormat,
    pub samples: u32,
    pub manual_srgb: bool,
}

const FORWARD_TEMPLATE: &str = include_str!("shaders/forward.wgsl");
const CULL_SHADER: &str = include_str!("shaders/cull.wgsl");
const COMPACT_SHADER: &str = include_str!("shaders/compact.wgsl");
const BLIT_SHADER: &str = include_str!("shaders/blit.wgsl");

fn to_vec4(expr: &str, ty: &str, fill: &str) -> String {
    match ty {
        "vec4<f32>" => expr.to_string(),
        "vec3<f32>" => format!("vec4<f32>({expr}, {fill})"),
        "vec2<f32>" => format!("vec4<f32>({expr}, {fill}, {fill})"),
        "f32" => format!("vec4<f32>({expr})"),
        t if t.starts_with("vec4<") => format!("vec4<f32>({expr})"),
        _ => format!("vec4<f32>(f32({expr}))"),
    }
}

fn to_vec3(expr: &str, ty: &str) -> String {
    match ty {
        "vec3<f32>" => expr.to_string(),
        "vec4<f32>" => format!("{expr}.xyz"),
        "f32" => format!("vec3<f32>({expr})"),
        "vec2<f32>" => format!("vec3<f32>({expr}, 0.0)"),
        _ => "vec3<f32>(0.0)".to_string(),
    }
}

/// Generate the `InstanceInput` struct + accessor functions for a layout. Instance attributes
/// start at shader location 4 (0..3 are the mesh vertex).
pub(crate) fn instance_wgsl(layout: &InstanceLayout) -> String {
    let mut s = String::from("struct InstanceInput {\n");
    let mut loc = 4;
    let mut model = None;
    let (mut t, mut r, mut sc, mut us, mut color, mut rect, mut layer) = (None, None, None, None, None, None, None);
    for (i, a) in layout.attributes.iter().enumerate() {
        if matches!(a.semantic, InstanceSemantic::Custom(_)) {
            continue;
        }
        let ty = wgsl_type(a.format).unwrap_or("vec4<f32>");
        if a.semantic == InstanceSemantic::ModelMatrix {
            for c in 0..4 {
                s += &format!("    @location({loc}) a{i}_{c}: vec4<f32>,\n");
                loc += 1;
            }
            model = Some(i);
            continue;
        }
        s += &format!("    @location({loc}) a{i}: {ty},\n");
        loc += 1;
        let field = (format!("inst.a{i}"), ty);
        match a.semantic {
            InstanceSemantic::Translation => t = Some(field),
            InstanceSemantic::Rotation => r = Some(field),
            InstanceSemantic::Scale => sc = Some(field),
            InstanceSemantic::UniformScale => us = Some(field),
            InstanceSemantic::Color => color = Some(field),
            InstanceSemantic::UvRect => rect = Some(field),
            InstanceSemantic::TextureLayer => layer = Some(field),
            InstanceSemantic::ModelMatrix | InstanceSemantic::Custom(_) => {}
        }
    }
    if loc == 4 {
        // Keep the struct non-empty for degenerate layouts.
        s += "    @location(4) unused: vec4<f32>,\n";
    }
    s += "}\n\n";

    s += "fn instance_model(inst: InstanceInput) -> mat4x4<f32> {\n";
    if let Some(i) = model {
        s += &format!("    return mat4x4<f32>(inst.a{i}_0, inst.a{i}_1, inst.a{i}_2, inst.a{i}_3);\n");
    } else {
        let te = t.map_or("vec3<f32>(0.0)".to_string(), |(e, ty)| to_vec3(&e, ty));
        let re = r.map_or("mat3x3<f32>(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 0.0, 1.0))".to_string(), |(e, ty)| {
            format!("quat_to_mat3({})", to_vec4(&e, ty, "1.0"))
        });
        let se = match (sc, us) {
            (Some((e, ty)), _) => to_vec3(&e, ty),
            (None, Some((e, ty))) => {
                if ty == "f32" {
                    format!("vec3<f32>({e})")
                } else {
                    format!("vec3<f32>({}.x)", e)
                }
            }
            (None, None) => "vec3<f32>(1.0)".to_string(),
        };
        s += &format!("    return trs({te}, {re}, {se});\n");
    }
    s += "}\n\n";

    s += "fn instance_color(inst: InstanceInput) -> vec4<f32> {\n";
    s += &format!(
        "    return {};\n}}\n\n",
        color.map_or("vec4<f32>(1.0)".to_string(), |(e, ty)| to_vec4(&e, ty, "1.0"))
    );
    s += "fn instance_uv_rect(inst: InstanceInput) -> vec4<f32> {\n";
    s += &format!(
        "    return {};\n}}\n\n",
        rect.map_or("vec4<f32>(0.0, 0.0, 1.0, 1.0)".to_string(), |(e, ty)| to_vec4(&e, ty, "1.0"))
    );
    s += "fn instance_layer(inst: InstanceInput) -> i32 {\n";
    s += &format!(
        "    return {};\n}}\n",
        layer.map_or("0".to_string(), |(e, ty)| match ty {
            "i32" => e,
            "u32" | "f32" => format!("i32({e})"),
            _ => format!("i32({e}.x)"),
        })
    );
    s
}

/// Full forward shader source for a layout.
pub(crate) fn forward_shader(layout: &InstanceLayout, manual_srgb: bool) -> String {
    FORWARD_TEMPLATE
        .replace("//#CONSTS", &format!("const MANUAL_SRGB: bool = {manual_srgb};"))
        .replace("//#INSTANCE", &instance_wgsl(layout))
}

/// Instance vertex attributes (shader locations from 4).
pub(crate) fn instance_attributes(layout: &InstanceLayout) -> Vec<wgpu::VertexAttribute> {
    let mut out = Vec::new();
    let mut loc = 4;
    for a in layout.attributes {
        match a.semantic {
            InstanceSemantic::Custom(_) => {}
            InstanceSemantic::ModelMatrix => {
                for c in 0..4 {
                    out.push(wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: (a.offset + c * 16) as u64,
                        shader_location: loc,
                    });
                    loc += 1;
                }
            }
            _ => {
                out.push(wgpu::VertexAttribute {
                    format: a.format,
                    offset: a.offset as u64,
                    shader_location: loc,
                });
                loc += 1;
            }
        }
    }
    if out.is_empty() {
        out.push(wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x4,
            offset: 0,
            shader_location: 4,
        });
    }
    out
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages, dynamic: bool, size: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: wgpu::BufferSize::new(size),
        },
        count: None,
    }
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Compute pipelines for GPU culling (only built when compute is available).
pub(crate) struct CullPipelines {
    pub bgl: wgpu::BindGroupLayout,
    pub pipeline: wgpu::ComputePipeline,
    pub compact_bgl: wgpu::BindGroupLayout,
    pub compact: wgpu::ComputePipeline,
}

/// Bind group layouts, shaders and cached pipelines.
pub(crate) struct Pipelines {
    pub view_bgl: wgpu::BindGroupLayout,
    pub material_bgl: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    shaders: FastMap<(u16, bool), wgpu::ShaderModule>,
    forward: FastMap<PipelineKey, wgpu::RenderPipeline>,
    /// Registered instance layouts; index = layout id.
    pub layouts: Vec<&'static InstanceLayout>,
    pub cull: Option<CullPipelines>,
    blit_bgl: wgpu::BindGroupLayout,
    blit_module: wgpu::ShaderModule,
    blit_layout: wgpu::PipelineLayout,
    blit: FastMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    pub blit_sampler: wgpu::Sampler,
}

impl Pipelines {
    pub fn new(device: &wgpu::Device, compute: bool) -> Self {
        let view_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d view"),
            entries: &[uniform_entry(
                0,
                wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                true,
                std::mem::size_of::<GpuView>() as u64,
            )],
        });
        let material_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d material"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::FRAGMENT, true, std::mem::size_of::<GpuMaterial>() as u64),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("space3d forward"),
            bind_group_layouts: &[Some(&view_bgl), Some(&material_bgl)],
            immediate_size: 0,
        });

        let cull = compute.then(|| {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d cull"),
                entries: &[
                    uniform_entry(0, wgpu::ShaderStages::COMPUTE, true, std::mem::size_of::<GpuCullParams>() as u64),
                    storage_entry(1, true),
                    storage_entry(2, true),
                    storage_entry(3, false),
                    storage_entry(4, false),
                ],
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d cull"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(CULL_SHADER)),
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space3d cull"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("space3d cull"),
                layout: Some(&pl),
                module: &module,
                entry_point: Some("cull"),
                compilation_options: Default::default(),
                cache: None,
            });
            let compact_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d compact"),
                entries: &[
                    uniform_entry(0, wgpu::ShaderStages::COMPUTE, true, std::mem::size_of::<GpuCompactParams>() as u64),
                    storage_entry(1, true),
                    storage_entry(2, true),
                    storage_entry(3, false),
                    storage_entry(4, false),
                ],
            });
            let cmodule = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d compact"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(COMPACT_SHADER)),
            });
            let cpl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space3d compact"),
                bind_group_layouts: &[Some(&compact_bgl)],
                immediate_size: 0,
            });
            let compact = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("space3d compact"),
                layout: Some(&cpl),
                module: &cmodule,
                entry_point: Some("compact"),
                compilation_options: Default::default(),
                cache: None,
            });
            CullPipelines {
                bgl,
                pipeline,
                compact_bgl,
                compact,
            }
        });

        let blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d blit"),
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
        let blit_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("space3d blit"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(BLIT_SHADER)),
        });
        let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("space3d blit"),
            bind_group_layouts: &[Some(&blit_bgl)],
            immediate_size: 0,
        });
        let blit_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("space3d blit"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            view_bgl,
            material_bgl,
            layout,
            shaders: FastMap::default(),
            forward: FastMap::default(),
            layouts: vec![<crate::instance::InstanceTransform as crate::instance::InstanceData>::LAYOUT],
            cull,
            blit_bgl,
            blit_module,
            blit_layout,
            blit: FastMap::default(),
            blit_sampler,
        }
    }

    /// Stable id for an instance layout (registers it on first use).
    pub fn layout_id(&mut self, layout: &'static InstanceLayout) -> u16 {
        if let Some(i) = self.layouts.iter().position(|l| std::ptr::eq(*l, layout) || **l == *layout) {
            return i as u16;
        }
        self.layouts.push(layout);
        (self.layouts.len() - 1) as u16
    }

    /// Get or build the forward pipeline for `key`.
    pub fn forward(&mut self, device: &wgpu::Device, key: PipelineKey) -> &wgpu::RenderPipeline {
        if !self.forward.contains_key(&key) {
            let layout = self.layouts[key.layout as usize];
            let module = self.shaders.entry((key.layout, key.manual_srgb)).or_insert_with(|| {
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(layout.name),
                    source: wgpu::ShaderSource::Wgsl(Cow::Owned(forward_shader(layout, key.manual_srgb))),
                })
            });
            let inst_attrs = instance_attributes(layout);
            let blend = key.class & CLASS_BLEND != 0;
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(layout.name),
                layout: Some(&self.layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_main"),
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
                    depth_write_enabled: Some(!blend),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: key.samples,
                    ..Default::default()
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: key.format,
                        blend: blend.then_some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.forward.insert(key, pipeline);
        }
        &self.forward[&key]
    }

    /// Mip-generation pipeline for a color format.
    pub fn blit(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> (&wgpu::RenderPipeline, &wgpu::BindGroupLayout) {
        let (module, layout) = (&self.blit_module, &self.blit_layout);
        let p = self.blit.entry(format).or_insert_with(|| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("space3d mipgen"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(format.into())],
                }),
                multiview_mask: None,
                cache: None,
            })
        });
        (p, &self.blit_bgl)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::*;

    fn validate(src: &str) {
        let module = wgpu::naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{}\n{src}", e.emit_to_string(src)));
        let mut v = wgpu::naga::valid::Validator::new(
            wgpu::naga::valid::ValidationFlags::all(),
            wgpu::naga::valid::Capabilities::empty(),
        );
        v.validate(&module).unwrap_or_else(|e| panic!("{e:?}\n{src}"));
    }

    #[test]
    fn generated_shaders_validate() {
        for l in [
            InstanceTransform::LAYOUT,
            InstanceTransformColor::LAYOUT,
            InstanceCompact::LAYOUT,
            InstancePacked::LAYOUT,
        ] {
            validate(&forward_shader(l, false));
            validate(&forward_shader(l, true));
        }
        validate(CULL_SHADER);
        validate(COMPACT_SHADER);
        validate(BLIT_SHADER);
    }

    #[test]
    fn uniform_sizes_match_wgsl() {
        assert_eq!(std::mem::size_of::<GpuView>(), 64 + 16 * 3 + 32 * (MAX_DIR_LIGHTS + MAX_POINT_LIGHTS));
        assert_eq!(std::mem::size_of::<GpuMaterial>(), 48);
        assert_eq!(std::mem::size_of::<GpuCullParams>(), 112);
        assert_eq!(std::mem::size_of::<GpuCullJob>(), 64);
    }

    #[test]
    fn instance_attributes_cover_locations() {
        let a = instance_attributes(InstancePacked::LAYOUT);
        assert_eq!(a.len(), 5);
        assert_eq!(a.last().unwrap().shader_location, 8);
        assert_eq!(instance_attributes(InstanceTransformColor::LAYOUT).len(), 5);
    }
}
