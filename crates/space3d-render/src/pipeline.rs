//! Shader generation, GPU uniform layouts and the pipeline cache.

use crate::arena::{VERTEX_ATTRIBUTES, VERTEX_SIZE};
use crate::handles::FastMap;
use crate::instance::{InstanceLayout, InstanceSemantic, wgsl_type};
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
    /// First word of the per-workgroup compaction records in the args buffer.
    pub wg_word: u32,
    pub pad: [u32; 3],
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
    pub run_start: u32,
    pub run_count: u32,
    pub workgroups_x: u32,
    pub pad: u32,
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
    /// The material texture is a multi-layer array (`texture_2d_array`); otherwise it is bound
    /// as `texture_2d` (see [`Pipelines::material_bgl`]).
    pub layered: bool,
}

const FORWARD_TEMPLATE: &str = include_str!("shaders/forward.wgsl");
const CULL_SHADER: &str = include_str!("shaders/cull.wgsl");
const COMPACT_SHADER: &str = include_str!("shaders/compact.wgsl");
const BLIT_TEMPLATE: &str = include_str!("shaders/blit.wgsl");

/// Mip-generation blit shader sampling a `texture_2d` or a `texture_2d_array` source.
pub(crate) fn blit_shader(layered: bool) -> String {
    let source = if layered {
        "@group(0) @binding(0) var src_tex: texture_2d_array<f32>;\n\
         @group(0) @binding(1) var src_sampler: sampler;\n\n\
         @fragment\n\
         fn fs_main(in: VsOut) -> @location(0) vec4<f32> {\n    \
         return textureSample(src_tex, src_sampler, in.uv, in.layer);\n}\n"
    } else {
        "@group(0) @binding(0) var src_tex: texture_2d<f32>;\n\
         @group(0) @binding(1) var src_sampler: sampler;\n\n\
         @fragment\n\
         fn fs_main(in: VsOut) -> @location(0) vec4<f32> {\n    \
         return textureSample(src_tex, src_sampler, in.uv);\n}\n"
    };
    BLIT_TEMPLATE.replace("//#SOURCE", source)
}

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
        let (_, swizzle) = attribute_format(a.format);
        let field = (format!("inst.a{i}{swizzle}"), ty);
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
        let re = r.map_or(
            "mat3x3<f32>(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 0.0, 1.0))".to_string(),
            |(e, ty)| format!("quat_to_mat3({})", to_vec4(&e, ty, "1.0")),
        );
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
        rect.map_or("vec4<f32>(0.0, 0.0, 1.0, 1.0)".to_string(), |(e, ty)| to_vec4(
            &e, ty, "1.0"
        ))
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

/// Material texture binding and its sampling function (`sample_base`).
fn texture_wgsl(layered: bool) -> &'static str {
    if layered {
        "@group(1) @binding(1) var base_tex: texture_2d_array<f32>;\n\n\
         fn sample_base(uv: vec2<f32>, layer: i32) -> vec4<f32> {\n    \
         return textureSample(base_tex, base_sampler, uv, layer);\n}\n"
    } else {
        // Single-layer textures: the instance layer is irrelevant (an array lookup would clamp
        // it to layer 0 as well).
        "@group(1) @binding(1) var base_tex: texture_2d<f32>;\n\n\
         fn sample_base(uv: vec2<f32>, layer: i32) -> vec4<f32> {\n    \
         return textureSample(base_tex, base_sampler, uv);\n}\n"
    }
}

/// Full forward shader source for a layout and material texture kind.
pub(crate) fn forward_shader(layout: &InstanceLayout, manual_srgb: bool, layered: bool) -> String {
    FORWARD_TEMPLATE
        .replace("//#CONSTS", &format!("const MANUAL_SRGB: bool = {manual_srgb};"))
        .replace("//#TEXTURE", texture_wgsl(layered))
        .replace("//#INSTANCE", &instance_wgsl(layout))
}

/// Vertex format handed to wgpu for an instance attribute format, and the swizzle the shader
/// applies to it. `Unorm8x4Bgra` is fetched as `Unorm8x4` and swizzled back to RGBA: wgpu's GL
/// backend passes `GL_BGRA` as the component count, which WebGL2/GLES reject (the attribute
/// then reads garbage, or the draw crashes the driver), and browsers' WebGPU may not offer it.
/// The swizzle is free, so every backend takes the same path.
pub(crate) fn attribute_format(format: wgpu::VertexFormat) -> (wgpu::VertexFormat, &'static str) {
    match format {
        wgpu::VertexFormat::Unorm8x4Bgra => (wgpu::VertexFormat::Unorm8x4, ".zyxw"),
        f => (f, ""),
    }
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
                    format: attribute_format(a.format).0,
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

/// Compute pipelines for GPU culling (only built when compute is available). The three cull
/// passes share `bgl`; see `shaders/cull.wgsl`.
pub(crate) struct CullPipelines {
    pub bgl: wgpu::BindGroupLayout,
    pub mark: wgpu::ComputePipeline,
    pub scan: wgpu::ComputePipeline,
    pub scatter: wgpu::ComputePipeline,
    pub compact_bgl: wgpu::BindGroupLayout,
    pub compact: wgpu::ComputePipeline,
}

/// Mip-generation blit for one source view dimension (`[single-layer, array]`).
struct Blit {
    bgl: wgpu::BindGroupLayout,
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
}

/// Bind group layouts, shaders and cached pipelines.
pub(crate) struct Pipelines {
    pub view_bgl: wgpu::BindGroupLayout,
    /// Material layouts: `[single-layer (texture_2d), array (texture_2d_array)]`.
    material_bgls: [wgpu::BindGroupLayout; 2],
    layouts_by_texture: [wgpu::PipelineLayout; 2],
    shaders: FastMap<(u16, bool, bool), wgpu::ShaderModule>,
    forward: FastMap<PipelineKey, wgpu::RenderPipeline>,
    /// Registered instance layouts; index = layout id.
    pub layouts: Vec<&'static InstanceLayout>,
    pub cull: Option<CullPipelines>,
    blits: [Blit; 2],
    blit: FastMap<(wgpu::TextureFormat, bool), wgpu::RenderPipeline>,
    pub blit_sampler: wgpu::Sampler,
}

/// Bind group layout entry of a filterable float texture of the given view dimension.
fn texture_entry(binding: u32, view_dimension: wgpu::TextureViewDimension) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension,
            multisampled: false,
        },
        count: None,
    }
}

/// View dimension of a sampled 2D texture: single-layer textures are `D2`, arrays `D2Array`.
/// GL binds a texture with a target chosen from its layer count alone, so the view (and the
/// shader's texture type) must match that count.
pub(crate) fn sampled_view_dimension(layered: bool) -> wgpu::TextureViewDimension {
    if layered {
        wgpu::TextureViewDimension::D2Array
    } else {
        wgpu::TextureViewDimension::D2
    }
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
        let material_bgls = [false, true].map(|layered| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(if layered {
                    "space3d material (texture array)"
                } else {
                    "space3d material"
                }),
                entries: &[
                    uniform_entry(
                        0,
                        wgpu::ShaderStages::FRAGMENT,
                        true,
                        std::mem::size_of::<GpuMaterial>() as u64,
                    ),
                    texture_entry(1, sampled_view_dimension(layered)),
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            })
        });
        let layouts_by_texture = [0, 1].map(|i| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space3d forward"),
                bind_group_layouts: &[Some(&view_bgl), Some(&material_bgls[i])],
                immediate_size: 0,
            })
        });

        let cull = compute.then(|| {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d cull"),
                entries: &[
                    uniform_entry(
                        0,
                        wgpu::ShaderStages::COMPUTE,
                        true,
                        std::mem::size_of::<GpuCullParams>() as u64,
                    ),
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
            let entry = |name: &'static str| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(name),
                    layout: Some(&pl),
                    module: &module,
                    entry_point: Some(name),
                    compilation_options: Default::default(),
                    cache: None,
                })
            };
            let (mark, scan, scatter) = (entry("cull_mark"), entry("cull_scan"), entry("cull_scatter"));
            let compact_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d compact"),
                entries: &[
                    uniform_entry(
                        0,
                        wgpu::ShaderStages::COMPUTE,
                        true,
                        std::mem::size_of::<GpuCompactParams>() as u64,
                    ),
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
                mark,
                scan,
                scatter,
                compact_bgl,
                compact,
            }
        });

        let blits = [false, true].map(|layered| {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space3d blit"),
                entries: &[
                    texture_entry(0, sampled_view_dimension(layered)),
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d blit"),
                source: wgpu::ShaderSource::Wgsl(Cow::Owned(blit_shader(layered))),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space3d blit"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            Blit { bgl, module, layout }
        });
        let blit_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("space3d blit"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            view_bgl,
            material_bgls,
            layouts_by_texture,
            shaders: FastMap::default(),
            forward: FastMap::default(),
            layouts: vec![<crate::instance::InstanceTransform as crate::instance::InstanceData>::LAYOUT],
            cull,
            blits,
            blit: FastMap::default(),
            blit_sampler,
        }
    }

    /// Material bind group layout for a single-layer (`texture_2d`) or array texture.
    pub fn material_bgl(&self, layered: bool) -> &wgpu::BindGroupLayout {
        &self.material_bgls[layered as usize]
    }

    /// Stable id for an instance layout (registers it on first use).
    pub fn layout_id(&mut self, layout: &'static InstanceLayout) -> u16 {
        if let Some(i) = self
            .layouts
            .iter()
            .position(|l| std::ptr::eq(*l, layout) || **l == *layout)
        {
            return i as u16;
        }
        self.layouts.push(layout);
        (self.layouts.len() - 1) as u16
    }

    /// Get or build the forward pipeline for `key`.
    pub fn forward(&mut self, device: &wgpu::Device, key: PipelineKey) -> &wgpu::RenderPipeline {
        if !self.forward.contains_key(&key) {
            let layout = self.layouts[key.layout as usize];
            let module = self
                .shaders
                .entry((key.layout, key.manual_srgb, key.layered))
                .or_insert_with(|| {
                    device.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some(layout.name),
                        source: wgpu::ShaderSource::Wgsl(Cow::Owned(forward_shader(
                            layout,
                            key.manual_srgb,
                            key.layered,
                        ))),
                    })
                });
            let inst_attrs = instance_attributes(layout);
            let blend = key.class & CLASS_BLEND != 0;
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(layout.name),
                layout: Some(&self.layouts_by_texture[key.layered as usize]),
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

    /// Mip-generation pipeline for a color format, sampling a single-layer (`D2`) or an array
    /// (`D2Array`, layer chosen by the draw's vertex range) source view.
    pub fn blit(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        layered: bool,
    ) -> (&wgpu::RenderPipeline, &wgpu::BindGroupLayout) {
        let blit = &self.blits[layered as usize];
        let (module, layout) = (&blit.module, &blit.layout);
        let p = self.blit.entry((format, layered)).or_insert_with(|| {
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
        (p, &blit.bgl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::*;

    fn validate(src: &str) {
        let module =
            wgpu::naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{}\n{src}", e.emit_to_string(src)));
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
            for (manual_srgb, layered) in [(false, false), (true, false), (false, true), (true, true)] {
                validate(&forward_shader(l, manual_srgb, layered));
            }
        }
        validate(CULL_SHADER);
        validate(COMPACT_SHADER);
        validate(&blit_shader(false));
        validate(&blit_shader(true));
    }

    #[test]
    fn uniform_sizes_match_wgsl() {
        assert_eq!(
            std::mem::size_of::<GpuView>(),
            64 + 16 * 3 + 32 * (MAX_DIR_LIGHTS + MAX_POINT_LIGHTS)
        );
        assert_eq!(std::mem::size_of::<GpuMaterial>(), 48);
        assert_eq!(std::mem::size_of::<GpuCullParams>(), 128);
        assert_eq!(std::mem::size_of::<GpuCompactParams>(), 16);
        assert_eq!(std::mem::size_of::<GpuCullJob>(), 64);
    }

    /// Regression: `Unorm8x4Bgra` reached wgpu's GL backend, which cannot express it (WebGL2 /
    /// GLES: INVALID_VALUE and garbage colors; ANGLE: a crash). It is now fetched as `Unorm8x4`
    /// and swizzled in the generated shader.
    #[test]
    fn bgra_instance_colors_are_emulated() {
        const LAYOUT: &InstanceLayout = &InstanceLayout {
            name: "Bgra",
            stride: 16,
            attributes: &[
                InstanceAttribute::new(InstanceSemantic::Translation, wgpu::VertexFormat::Float32x3, 0),
                InstanceAttribute::new(InstanceSemantic::Color, wgpu::VertexFormat::Unorm8x4Bgra, 12),
            ],
        };
        LAYOUT.validate().expect("valid layout");
        let attrs = instance_attributes(LAYOUT);
        assert!(attrs.iter().all(|a| a.format != wgpu::VertexFormat::Unorm8x4Bgra));
        assert_eq!(attrs[1].format, wgpu::VertexFormat::Unorm8x4);
        let src = instance_wgsl(LAYOUT);
        assert!(src.contains("return inst.a1.zyxw;"), "{src}");
        validate(&forward_shader(LAYOUT, false, false));
    }

    #[test]
    fn instance_attributes_cover_locations() {
        let a = instance_attributes(InstancePacked::LAYOUT);
        assert_eq!(a.len(), 5);
        assert_eq!(a.last().unwrap().shader_location, 8);
        assert_eq!(instance_attributes(InstanceTransformColor::LAYOUT).len(), 5);
    }
}
