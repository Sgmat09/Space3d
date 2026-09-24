//! Shader generation, GPU uniform layouts and the pipeline cache.

use crate::arena::{VERTEX_ATTRIBUTES, VERTEX_SIZE};
use crate::handles::{FastMap, ShaderHandle};
use crate::instance::{InstanceLayout, InstanceSemantic, wgsl_type};
use crate::shader::{CustomShader, hooks_wgsl};
use bytemuck::{Pod, Zeroable};
use std::borrow::Cow;
use std::sync::Arc;

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
    /// Custom shader and its source version (see [`Pipelines::sync_custom`]).
    pub shader: Option<(ShaderHandle, u32)>,
}

/// Shader module cache key: (layout, manual sRGB, layered texture, custom shader).
type ModuleKey = (u16, bool, bool, Option<(ShaderHandle, u32)>);

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

/// Zero-padded `vec4<f32>` of a custom attribute value of WGSL type `ty`.
fn custom_vec4(expr: &str, ty: &str) -> String {
    match ty {
        "vec4<f32>" => expr.to_string(),
        "vec3<f32>" => format!("vec4<f32>({expr}, 0.0)"),
        "vec2<f32>" => format!("vec4<f32>({expr}, 0.0, 0.0)"),
        "f32" => format!("vec4<f32>({expr}, 0.0, 0.0, 0.0)"),
        "u32" | "i32" => format!("vec4<f32>(f32({expr}), 0.0, 0.0, 0.0)"),
        t if t.starts_with("vec2<") => format!("vec4<f32>(vec2<f32>({expr}), 0.0, 0.0)"),
        t if t.starts_with("vec3<") => format!("vec4<f32>(vec3<f32>({expr}), 0.0)"),
        _ => format!("vec4<f32>({expr})"),
    }
}

/// Custom slots that always get an accessor (zero when the layout lacks them), so one custom
/// shader compiles for every layout.
const CUSTOM_ACCESSORS: u8 = 4;

/// Generate the `InstanceInput` struct + accessor functions for a layout. Instance attributes
/// start at shader location 4 (0..3 are the mesh vertex), in attribute order (see
/// [`instance_attributes`]).
pub(crate) fn instance_wgsl(layout: &InstanceLayout) -> String {
    let mut s = String::from("struct InstanceInput {\n");
    let mut loc = 4;
    let mut model = None;
    let (mut t, mut r, mut sc, mut us, mut color, mut rect, mut layer) = (None, None, None, None, None, None, None);
    let mut custom = Vec::new();
    for (i, a) in layout.attributes.iter().enumerate() {
        let ty = wgsl_type(a.format).unwrap_or("vec4<f32>");
        if a.semantic == InstanceSemantic::ModelMatrix {
            for c in 0..4 {
                s += &format!("    @location({loc}) a{i}_{c}: vec4<f32>,\n");
                loc += 1;
            }
            model = Some(i);
            continue;
        }
        if let InstanceSemantic::Custom(n) = a.semantic {
            s += &format!("    @location({loc}) custom{n}: {ty},\n");
            loc += 1;
            custom.push((n, ty));
            continue;
        }
        s += &format!("    @location({loc}) a{i}: {ty},\n");
        loc += 1;
        let (_, swizzle) = attribute_format(a.format);
        let e = format!("inst.a{i}{swizzle}");
        match a.semantic {
            InstanceSemantic::Translation => t = Some((e, ty)),
            InstanceSemantic::TranslationScale => {
                t = Some((format!("{e}.xyz"), "vec3<f32>"));
                us = Some((format!("{e}.w"), "f32"));
            }
            InstanceSemantic::Rotation => r = Some((e, ty)),
            InstanceSemantic::Scale => sc = Some((e, ty)),
            InstanceSemantic::UniformScale => us = Some((e, ty)),
            InstanceSemantic::Color => color = Some((e, ty)),
            InstanceSemantic::UvRect => rect = Some((e, ty)),
            InstanceSemantic::TextureLayer => layer = Some((e, ty)),
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
    // Custom attributes for custom shaders: declared slots, and slots 0..4 as zero otherwise.
    for n in 0..CUSTOM_ACCESSORS {
        if !custom.iter().any(|(c, _)| *c == n) {
            s += &format!(
                "\nfn instance_custom{n}(inst: InstanceInput) -> vec4<f32> {{\n    return vec4<f32>(0.0);\n}}\n"
            );
        }
    }
    for (n, ty) in custom {
        let e = custom_vec4(&format!("inst.custom{n}"), ty);
        s += &format!("\nfn instance_custom{n}(inst: InstanceInput) -> vec4<f32> {{\n    return {e};\n}}\n");
    }
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

/// Full forward shader source for a layout, material texture kind and custom shader.
pub(crate) fn forward_shader(
    layout: &InstanceLayout,
    manual_srgb: bool,
    layered: bool,
    custom: Option<&CustomShader>,
) -> String {
    let varyings = if custom.is_some() {
        "    @location(5) custom: vec4<f32>,"
    } else {
        ""
    };
    // The user's code is spliced in last, so markers inside it are left alone.
    FORWARD_TEMPLATE
        .replace("//#CONSTS", &format!("const MANUAL_SRGB: bool = {manual_srgb};"))
        .replace("//#TEXTURE", texture_wgsl(layered))
        .replace("//#INSTANCE", &instance_wgsl(layout))
        .replace("//#VARYINGS", varyings)
        .replace("//#HOOKS", &hooks_wgsl(custom))
}

/// Parse and validate WGSL with naga (the portable subset: no optional capabilities), with a
/// readable error message.
pub(crate) fn validate_wgsl(src: &str) -> Result<(), String> {
    use wgpu::naga;
    let module = naga::front::wgsl::parse_str(src).map_err(|e| e.emit_to_string(src))?;
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
        .validate(&module)
        .map_err(|e| e.emit_to_string(src))?;
    Ok(())
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

/// Instance vertex attributes (shader locations from 4, in attribute order; `Custom` ones
/// included so custom shaders can read them).
pub(crate) fn instance_attributes(layout: &InstanceLayout) -> Vec<wgpu::VertexAttribute> {
    let mut out = Vec::new();
    let mut loc = 4;
    for a in layout.attributes {
        match a.semantic {
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
    shaders: FastMap<ModuleKey, wgpu::ShaderModule>,
    forward: FastMap<PipelineKey, wgpu::RenderPipeline>,
    /// Sources of the custom shaders drawn so far: (version, source).
    custom: FastMap<ShaderHandle, (u32, Arc<CustomShader>)>,
    /// First build error of a custom shader version.
    custom_errors: FastMap<ShaderHandle, (u32, String)>,
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
            custom: FastMap::default(),
            custom_errors: FastMap::default(),
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
        if layout.cull_source() == crate::instance::CullSource::None {
            tracing::warn!(
                layout = layout.name,
                "instance positions are not in a float format the culler decodes (f32/f16): \
                 these instances are never culled on the GPU, and on the CPU only per block if \
                 `InstanceData::bounds` is overridden or `InstanceBuffer::set_bounds` is used"
            );
        }
        self.layouts.push(layout);
        (self.layouts.len() - 1) as u16
    }

    /// Make `source` (version `version`) the current code of custom shader `handle`. A new
    /// version drops the pipelines, modules and error of the old one.
    pub fn sync_custom(&mut self, handle: ShaderHandle, version: u32, source: &Arc<CustomShader>) {
        match self.custom.get(&handle) {
            Some((v, _)) if *v == version => return,
            Some(_) => {
                self.forward.retain(|k, _| k.shader.is_none_or(|(h, _)| h != handle));
                self.shaders.retain(|k, _| k.3.is_none_or(|(h, _)| h != handle));
                self.custom_errors.remove(&handle);
            }
            None => {}
        }
        self.custom.insert(handle, (version, source.clone()));
    }

    /// Forget custom shaders for which `live` is false (removed from `RenderAssets`): their
    /// sources, errors, modules and pipelines.
    pub fn retain_custom(&mut self, live: impl Fn(ShaderHandle) -> bool) {
        if self.custom.keys().all(|&h| live(h)) {
            return;
        }
        self.custom.retain(|&h, _| live(h));
        self.custom_errors.retain(|&h, _| live(h));
        self.forward.retain(|k, _| k.shader.is_none_or(|(h, _)| live(h)));
        self.shaders.retain(|k, _| k.3.is_none_or(|(h, _)| live(h)));
    }

    /// Custom shaders with cached state, and forward pipelines built for custom shaders.
    #[cfg(test)]
    pub fn custom_counts(&self) -> (usize, usize) {
        (
            self.custom.len(),
            self.forward.keys().filter(|k| k.shader.is_some()).count(),
        )
    }

    /// Error of the current version of a custom shader, if it failed to build.
    pub fn custom_error(&self, handle: ShaderHandle) -> Option<&str> {
        let (version, e) = self.custom_errors.get(&handle)?;
        (self.custom.get(&handle).map(|(v, _)| *v) == Some(*version)).then_some(e.as_str())
    }

    /// Get or build the forward pipeline for `key`. A custom shader that fails to build (or is
    /// unknown) is reported once and replaced by the built-in pipeline for that key.
    pub fn forward(&mut self, device: &wgpu::Device, key: PipelineKey) -> &wgpu::RenderPipeline {
        if !self.forward.contains_key(&key) {
            let pipeline = match key.shader {
                None => self.build_forward(device, key, None).expect("built-in shader builds"),
                Some((handle, version)) => {
                    let source = self
                        .custom
                        .get(&handle)
                        .filter(|(v, _)| *v == version)
                        .map(|(_, s)| s.clone());
                    let built = match &source {
                        Some(s) => self.build_forward(device, key, Some(s)),
                        None => Err("shader not registered".to_string()),
                    };
                    match built {
                        Ok(p) => p,
                        Err(e) => {
                            let layout = self.layouts[key.layout as usize].name;
                            let name = source.as_ref().map_or("?", |s| s.name.as_str());
                            tracing::error!(
                                shader = name,
                                layout,
                                "custom shader failed to build; drawing with the built-in shader:\n{e}"
                            );
                            self.custom_errors.entry(handle).or_insert((version, e));
                            self.forward(device, PipelineKey { shader: None, ..key }).clone()
                        }
                    }
                }
            };
            self.forward.insert(key, pipeline);
        }
        &self.forward[&key]
    }

    /// Build a forward pipeline. Custom shaders are validated with naga first and built inside
    /// a validation error scope, so their errors never reach the uncaptured-error handler.
    fn build_forward(
        &mut self,
        device: &wgpu::Device,
        key: PipelineKey,
        custom: Option<&Arc<CustomShader>>,
    ) -> Result<wgpu::RenderPipeline, String> {
        let layout = self.layouts[key.layout as usize];
        let module_key = (key.layout, key.manual_srgb, key.layered, key.shader);
        let scope = custom.map(|_| device.push_error_scope(wgpu::ErrorFilter::Validation));
        let module = match self.shaders.get(&module_key) {
            Some(m) => m.clone(),
            None => {
                let source = forward_shader(layout, key.manual_srgb, key.layered, custom.map(|c| &**c));
                if custom.is_some() {
                    validate_wgsl(&source)?;
                }
                let label = custom.map_or(layout.name, |c| c.name.as_str());
                let m = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(label),
                    source: wgpu::ShaderSource::Wgsl(Cow::Owned(source)),
                });
                self.shaders.insert(module_key, m.clone());
                m
            }
        };
        let inst_attrs = instance_attributes(layout);
        let blend = key.class & CLASS_BLEND != 0;
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(layout.name),
            layout: Some(&self.layouts_by_texture[key.layered as usize]),
            vertex: wgpu::VertexState {
                module: &module,
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
                module: &module,
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
        // Native backends resolve the scope immediately; on the web the result arrives later
        // (and naga has already validated the code).
        if let Some(scope) = scope
            && let Some(Some(e)) = futures::FutureExt::now_or_never(scope.pop())
        {
            self.shaders.remove(&module_key);
            return Err(e.to_string());
        }
        Ok(pipeline)
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
    use wgpu::VertexFormat as F;

    fn validate(src: &str) {
        validate_wgsl(src).unwrap_or_else(|e| panic!("{e}\n{src}"));
    }

    /// Atlas + texture layer + custom data: every semantic the built-in layouts leave out.
    const ATLAS: &InstanceLayout = &InstanceLayout {
        name: "Atlas",
        stride: 64,
        attributes: &[
            InstanceAttribute::new(InstanceSemantic::Translation, F::Float32x3, 0),
            InstanceAttribute::new(InstanceSemantic::UniformScale, F::Float32, 12),
            InstanceAttribute::new(InstanceSemantic::UvRect, F::Float32x4, 16),
            InstanceAttribute::new(InstanceSemantic::Custom(0), F::Float32x4, 32),
            InstanceAttribute::new(InstanceSemantic::TextureLayer, F::Uint32, 48),
            InstanceAttribute::new(InstanceSemantic::Custom(7), F::Uint32x2, 52),
            InstanceAttribute::new(InstanceSemantic::Scale, F::Float16x2, 60),
        ],
    };

    /// f16 position with a separate f16 uniform scale in the upper half of a word.
    const HALF_SPLIT: &InstanceLayout = &InstanceLayout {
        name: "HalfSplit",
        stride: 16,
        attributes: &[
            InstanceAttribute::new(InstanceSemantic::Translation, F::Float16x4, 0),
            InstanceAttribute::new(InstanceSemantic::UniformScale, F::Float16, 6),
            InstanceAttribute::new(InstanceSemantic::Rotation, F::Snorm16x4, 8),
        ],
    };

    #[test]
    fn generated_shaders_validate() {
        let custom = CustomShader::new(
            "tint",
            "fn vertex_hook(v: VertexInput, inst: InstanceInput, out: VsOut) -> VsOut {\n\
             \x20   var o = out;\n    o.custom = instance_custom0(inst) + instance_custom3(inst);\n    return o;\n}\n\
             fn fragment_hook(in: VsOut, color: vec4<f32>) -> vec4<f32> {\n    return in.custom * color;\n}\n",
        );
        let only_fragment = CustomShader::new(
            "f",
            "fn fragment_hook(in: VsOut, c: vec4<f32>) -> vec4<f32> { return c.bgra; }",
        );
        for l in [
            InstanceTransform::LAYOUT,
            InstanceTransformColor::LAYOUT,
            InstanceCompact::LAYOUT,
            InstancePacked::LAYOUT,
            InstanceHalf::LAYOUT,
            ATLAS,
            HALF_SPLIT,
        ] {
            l.validate().unwrap();
            for (manual_srgb, layered) in [(false, false), (true, false), (false, true), (true, true)] {
                for c in [None, Some(&custom), Some(&only_fragment)] {
                    validate(&forward_shader(l, manual_srgb, layered, c));
                }
            }
        }
        validate(CULL_SHADER);
        validate(COMPACT_SHADER);
        validate(&blit_shader(false));
        validate(&blit_shader(true));
        // A broken custom shader is rejected with a message, not a panic.
        let broken = CustomShader::new(
            "broken",
            "fn fragment_hook(in: VsOut) -> f32 { return undefined_thing; }",
        );
        let e = validate_wgsl(&forward_shader(ATLAS, false, false, Some(&broken))).unwrap_err();
        assert!(e.contains("undefined_thing") || e.contains("fragment_hook"), "{e}");
    }

    /// The generated accessors read the attributes the layout declares.
    #[test]
    fn generated_accessors_use_the_declared_attributes() {
        let src = instance_wgsl(ATLAS);
        for line in [
            "@location(4) a0: vec3<f32>",
            "@location(5) a1: f32",
            "@location(6) a2: vec4<f32>",
            "@location(7) custom0: vec4<f32>",
            "@location(8) a4: u32",
            "@location(9) custom7: vec2<u32>",
            "@location(10) a6: vec2<f32>",
            "return inst.a2;",
            "return i32(inst.a4);",
            "fn instance_custom0(inst: InstanceInput) -> vec4<f32> {\n    return inst.custom0;",
            "return vec4<f32>(vec2<f32>(inst.custom7), 0.0, 0.0);",
            "fn instance_custom3(inst: InstanceInput) -> vec4<f32> {\n    return vec4<f32>(0.0);",
            "trs(inst.a0, mat3x3<f32>",
            "vec3<f32>(inst.a6, 0.0))",
        ] {
            assert!(src.contains(line), "missing `{line}` in\n{src}");
        }
        let half = instance_wgsl(InstanceHalf::LAYOUT);
        assert!(
            half.contains("return trs(inst.a0.xyz, quat_to_mat3(inst.a1), vec3<f32>(inst.a0.w));"),
            "{half}"
        );
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
                InstanceAttribute::new(InstanceSemantic::Translation, F::Float32x3, 0),
                InstanceAttribute::new(InstanceSemantic::Color, F::Unorm8x4Bgra, 12),
            ],
        };
        LAYOUT.validate().expect("valid layout");
        let attrs = instance_attributes(LAYOUT);
        assert!(attrs.iter().all(|a| a.format != F::Unorm8x4Bgra));
        assert_eq!(attrs[1].format, F::Unorm8x4);
        let src = instance_wgsl(LAYOUT);
        assert!(src.contains("return inst.a1.zyxw;"), "{src}");
        validate(&forward_shader(LAYOUT, false, false, None));
    }

    /// Exact `(format, offset, location)` of every built-in layout: a wrong offset or format
    /// (e.g. a tint read from the matrix) would still validate and render something.
    #[test]
    fn instance_attributes_match_the_structs() {
        let tuples = |l: &InstanceLayout| {
            instance_attributes(l)
                .iter()
                .map(|a| (a.format, a.offset, a.shader_location))
                .collect::<Vec<_>>()
        };
        let matrix = |extra: &[(F, u64, u32)]| {
            let mut v: Vec<_> = (0..4).map(|c| (F::Float32x4, c * 16, 4 + c as u32)).collect();
            v.extend_from_slice(extra);
            v
        };
        assert_eq!(tuples(InstanceTransform::LAYOUT), matrix(&[]));
        assert_eq!(tuples(InstanceTransformColor::LAYOUT), matrix(&[(F::Float32x4, 64, 8)]));
        assert_eq!(
            tuples(InstanceCompact::LAYOUT),
            [(F::Float32x3, 0, 4), (F::Float32, 12, 5), (F::Float32x4, 16, 6)]
        );
        assert_eq!(
            tuples(InstancePacked::LAYOUT),
            [
                (F::Float32x3, 0, 4),
                (F::Float32, 12, 5),
                (F::Snorm16x4, 16, 6),
                (F::Unorm8x4, 24, 7),
                (F::Uint32, 28, 8)
            ]
        );
        assert_eq!(
            tuples(InstanceHalf::LAYOUT),
            [(F::Float16x4, 0, 4), (F::Snorm16x4, 8, 5)]
        );
        // Custom attributes get locations too (custom shaders read them).
        assert_eq!(tuples(ATLAS)[3], (F::Float32x4, 32, 7));
        assert_eq!(tuples(ATLAS).len(), 7);
        // Byte offsets inside the Rust structs.
        assert_eq!(std::mem::offset_of!(InstanceTransformColor, color), 64);
        assert_eq!(std::mem::offset_of!(InstanceCompact, rotation), 16);
        assert_eq!(std::mem::offset_of!(InstancePacked, rotation), 16);
        assert_eq!(std::mem::offset_of!(InstancePacked, color), 24);
        assert_eq!(std::mem::offset_of!(InstancePacked, layer), 28);
        assert_eq!(std::mem::offset_of!(InstanceHalf, rotation), 8);
    }
}
