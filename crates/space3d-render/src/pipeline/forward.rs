//! Forward shader composition (`forward.wgsl` + instance layout + texture kind + custom
//! hooks), the material uniform and bind group layouts, forward pipelines, and custom-shader
//! validation.

use super::instance::{instance_attributes, instance_wgsl};
use super::{
    CLASS_BLEND, CLASS_DOUBLE_SIDED, DEPTH_FORMAT, KIND_COLOR_LAYERED, KIND_DATA_LAYERED, ModuleKey, PipelineKey,
    Pipelines, VIEW_STRUCTS, output_constants, parse_and_validate, sampled_view_dimension, texture_entry,
    uniform_entry,
};
use crate::arena::{VERTEX_ATTRIBUTES, VERTEX_SIZE};
use crate::instance::InstanceLayout;
use crate::output::{OUTPUT_WGSL, OutputEncoding};
use crate::shader::{CustomShader, hooks_wgsl};
use bytemuck::{Pod, Zeroable};
use std::borrow::Cow;
use std::sync::Arc;

/// Matches `MaterialParams` in forward.wgsl.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Pod, Zeroable)]
pub(crate) struct GpuMaterial {
    pub base_color: [f32; 4],
    pub emissive: [f32; 4],
    pub params: [f32; 4],
}

pub(super) const FORWARD_TEMPLATE: &str = include_str!("../shaders/forward.wgsl");

/// Material texture binding and its sampling function (`sample_base`).
pub(super) fn texture_wgsl(layered: bool) -> &'static str {
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

/// Full forward shader source for an instance layout, texture kinds (`PipelineKey::kinds`), the
/// extension vertex stream and a custom shader. The output encoding and the other pipeline
/// overrides are not part of the source (see `PipelineKey::constants`).
pub(crate) fn forward_shader(layout: &InstanceLayout, kinds: u8, ext: bool, custom: Option<&CustomShader>) -> String {
    let varyings = if custom.is_some() {
        "    @location(5) custom: vec4<f32>,"
    } else {
        ""
    };
    let color_layered = kinds & KIND_COLOR_LAYERED != 0;
    let data_layered = kinds & KIND_DATA_LAYERED != 0;
    let consts = format!(
        "const S3D_COLOR_LAYERED: bool = {color_layered};\n\
         const S3D_DATA_LAYERED: bool = {data_layered};\n\
         const S3D_EXT: bool = {ext};"
    );
    // The user's code is spliced in last, so markers inside it are left alone.
    FORWARD_TEMPLATE
        .replace("//#CONSTS", &consts)
        .replace("//#VIEW", VIEW_STRUCTS)
        .replace("//#OUTPUT", OUTPUT_WGSL)
        .replace("//#TEXTURE", texture_wgsl(color_layered))
        .replace("//#INSTANCE", &instance_wgsl(layout))
        .replace("//#VARYINGS", varyings)
        .replace("//#HOOKS", &hooks_wgsl(custom))
}

/// WGSL built-in functions GLSL ES 3.00 — WebGL2, and GLES 3.0 devices through wgpu's GL
/// backend — lacks: GLSL ES 3.10 brought them. naga's GLSL backend emits them regardless of the
/// version (it polyfills `frexp`, `countLeadingZeros`, `fma` and the fine/coarse derivatives,
/// and refuses `pack4x8snorm`/`pack4x8unorm` itself), so the GL driver fails to compile the
/// shader — only on those devices, and in the browser only once the pipeline is created.
pub(super) const GLES3_MISSING: [(naga::MathFunction, &str); 12] = {
    use naga::MathFunction as M;
    [
        (M::CountOneBits, "countOneBits"),
        (M::ReverseBits, "reverseBits"),
        (M::ExtractBits, "extractBits"),
        (M::InsertBits, "insertBits"),
        (M::FirstTrailingBit, "firstTrailingBit"),
        (M::FirstLeadingBit, "firstLeadingBit"),
        (M::CountTrailingZeros, "countTrailingZeros"),
        (M::Ldexp, "ldexp"),
        (M::Dot4I8Packed, "dot4I8Packed"),
        (M::Dot4U8Packed, "dot4U8Packed"),
        (M::Unpack4xI8, "unpack4xI8"),
        (M::Unpack4xU8, "unpack4xU8"),
    ]
};

/// The first built-in function of [`GLES3_MISSING`] (or `textureGather*`, also GLSL ES 3.10)
/// any function of `module` calls, used or not.
pub(super) fn gles3_missing_builtin(module: &naga::Module) -> Option<&'static str> {
    let functions = module
        .functions
        .iter()
        .map(|(_, f)| f)
        .chain(module.entry_points.iter().map(|e| &e.function));
    for f in functions {
        for (_, e) in f.expressions.iter() {
            match *e {
                naga::Expression::Math { fun, .. } => {
                    if let Some((_, name)) = GLES3_MISSING.iter().find(|(m, _)| *m == fun) {
                        return Some(name);
                    }
                }
                naga::Expression::ImageSample { gather: Some(_), .. } => return Some("textureGather"),
                _ => {}
            }
        }
    }
    None
}

/// Translate the forward entry points (`vs_main`, `fs_main`) to GLSL ES 3.00 the way wgpu's GL
/// backend does on WebGL2 (overrides at their defaults); the error, if naga refuses (image
/// queries GLSL ES lacks such as `textureNumLevels`, `pack4x8snorm`, …).
pub(super) fn gles3_translation_error(module: &naga::Module, info: &naga::valid::ModuleInfo) -> Option<String> {
    use naga::back::glsl;
    let options = glsl::Options {
        version: glsl::Version::Embedded {
            version: 300,
            is_webgl: true,
        },
        writer_flags: glsl::WriterFlags::ADJUST_COORDINATE_SPACE | glsl::WriterFlags::FORCE_POINT_SIZE,
        binding_map: Default::default(),
        zero_initialize_workgroup_memory: true,
    };
    // Every output encoding: the fragment tail differs per `S3D_OUTPUT` value.
    let outputs = [
        OutputEncoding::Linear,
        OutputEncoding::ManualSrgb,
        OutputEncoding::Tonemapped,
    ];
    let stages = outputs.into_iter().flat_map(|output| {
        [
            (naga::ShaderStage::Vertex, "vs_main", output),
            (naga::ShaderStage::Fragment, "fs_main", output),
        ]
    });
    for (stage, entry_point, output) in stages {
        let key_constants = output_constants(output);
        let constants: naga::back::PipelineConstants = key_constants.0[..key_constants.1]
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect();
        let translated =
            naga::back::pipeline_constants::process_overrides(module, info, Some((stage, entry_point)), &constants)
                .map_err(|e| e.to_string())
                .and_then(|(module, info)| {
                    let pipeline = glsl::PipelineOptions {
                        shader_stage: stage,
                        entry_point: entry_point.to_string(),
                        multiview: None,
                    };
                    let mut out = String::new();
                    let policies = naga::proc::BoundsCheckPolicies::default();
                    glsl::Writer::new(&mut out, &module, &info, &options, &pipeline, policies)
                        .and_then(|mut w| w.write().map(drop))
                        .map_err(|e| e.to_string())
                });
        if let Err(e) = translated {
            return Some(format!("{stage:?} stage ({output:?} output): {e}"));
        }
    }
    None
}

/// Resources the forward template declares, `(group, binding, name)`: the only ones in the
/// forward pipeline layout, so the only ones a custom shader can use.
pub(super) const TEMPLATE_RESOURCES: [(u32, u32, &str); 4] = [
    (0, 0, "view"),
    (1, 0, "material"),
    (1, 1, "base_tex"),
    (1, 2, "base_sampler"),
];

/// naga validation ([`parse_and_validate`]) of a forward shader with a custom shader spliced in,
/// plus what naga accepts but the forward pipeline cannot run the same way on every backend.
///
/// Rejected on every backend (`Err`):
///
/// * resources of its own: the pipeline layout has no bindings for them, so the pipeline
///   fails validation;
/// * workgroup memory (`var<workgroup>`, used or not): WGSL allows it in compute shaders only,
///   naga in any stage, and backends disagree about it in the vertex and fragment stages the
///   hooks run in (Metal accepts it, GL and browsers reject it);
/// * any other address space but `function` and `private` (immediates, mesh and ray-tracing
///   payloads: optional capabilities the forward pipeline does not use);
/// * pipeline constants (`override`) without a default: the forward pipeline sets none, so
///   creating it fails.
///
/// What GLSL ES 3.00 (WebGL2, GLES 3.0) cannot run — the built-in functions of
/// [`GLES3_MISSING`] and `textureGather*`, and anything else naga cannot translate to GLSL
/// ES 3.00 (`textureNumLevels`, `textureNumSamples`, `pack4x8snorm`, `pack4x8unorm`, …) — is
/// rejected only when `glsl_es_300` (the device is such a GL device: the driver would fail to
/// compile it, in the browser only once the pipeline exists). Elsewhere Metal, Vulkan, DX12,
/// WebGPU and GLES 3.1+ run it, so the shader builds and draws exactly as before; the
/// portability problem comes back as `Ok(Some(warning))` (reported through
/// [`crate::Renderer::shader_warning`]), so it still shows up on the machine the shader is
/// written on.
///
/// Detected here synchronously, because on browser WebGPU the error scope around pipeline
/// creation only answers later (see [`Pipelines::poll_pending`]): until then every frame
/// drawing with the invalid pipeline is lost.
pub(crate) fn validate_custom_wgsl(src: &str, glsl_es_300: bool) -> Result<Option<String>, String> {
    use naga::AddressSpace as A;
    let (module, info) = parse_and_validate(src)?;
    for (_, g) in module.global_variables.iter() {
        let name = g.name.as_deref().unwrap_or("<unnamed>");
        match g.space {
            A::Function | A::Private => {}
            A::WorkGroup => {
                return Err(format!(
                    "custom shaders cannot use workgroup memory: `var<workgroup> {name}` exists \
                     only in compute shaders, and the hooks run in the vertex and fragment stages \
                     (use `var<private>` for per-invocation state)"
                ));
            }
            A::Uniform | A::Storage { .. } | A::Handle => {
                let template = g
                    .binding
                    .as_ref()
                    .is_some_and(|b| TEMPLATE_RESOURCES.contains(&(b.group, b.binding, name)));
                if !template {
                    let at = g.binding.as_ref().map_or(String::new(), |b| {
                        format!(" at @group({}) @binding({})", b.group, b.binding)
                    });
                    return Err(format!(
                        "custom shaders cannot declare resources: `{name}` ({:?}){at} is not bound \
                         by the forward pipeline (only `view`, `material`, `base_tex` and \
                         `base_sampler` are)",
                        g.space
                    ));
                }
            }
            space => {
                return Err(format!(
                    "custom shaders cannot declare `{name}` in the {space:?} address space: \
                     only `var<private>` globals (and the template's resources) are portable to \
                     every backend"
                ));
            }
        }
    }
    for (_, o) in module.overrides.iter() {
        if o.init.is_none() {
            let name = o.name.as_deref().unwrap_or("<unnamed>");
            return Err(format!(
                "custom shaders cannot declare pipeline constants without a default: \
                 `override {name}` needs one (`override {name}: T = value;`), since the forward \
                 pipeline sets no constants"
            ));
        }
    }
    let gles3 = if let Some(name) = gles3_missing_builtin(&module) {
        format!("it uses `{name}`, which GLSL ES 3.00 lacks")
    } else if let Some(e) = gles3_translation_error(&module, &info) {
        format!("it does not translate to GLSL ES 3.00: {e}")
    } else {
        return Ok(None);
    };
    if glsl_es_300 {
        Err(format!(
            "custom shader cannot build on this device, which compiles GLSL ES 3.00 like WebGL2 \
             and GLES 3.0: {gles3}"
        ))
    } else {
        Ok(Some(format!(
            "custom shader builds on this device but not on WebGL2 / GLES 3.0 (GLSL ES 3.00), \
             where its draws use the built-in shader: {gles3}"
        )))
    }
}

/// Material bind group layouts: `[single-layer (texture_2d), array (texture_2d_array)]`.
pub(super) fn material_bgls(device: &wgpu::Device) -> [wgpu::BindGroupLayout; 2] {
    [false, true].map(|layered| {
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
    })
}

impl Pipelines {
    /// Build a forward pipeline. Custom shaders are validated with naga first (including that
    /// they declare no resources the layout lacks; see [`validate_custom_wgsl`] for what is
    /// rejected and what only warned about) and built inside validation and internal
    /// error scopes, so their errors never reach the uncaptured-error handler. Where the
    /// scopes do not answer immediately (browser WebGPU), the pipeline is returned with its
    /// pending result (for [`Self::poll_pending`]).
    #[allow(clippy::type_complexity)]
    pub(super) fn build_forward(
        &mut self,
        device: &wgpu::Device,
        key: PipelineKey,
        custom: Option<&Arc<CustomShader>>,
    ) -> Result<
        (
            wgpu::RenderPipeline,
            Option<(ModuleKey, space3d_tasks::Task<Option<String>>)>,
        ),
        String,
    > {
        let layout = self.layouts[key.layout as usize];
        let module_key = key.module_key();
        let cached = self.shaders.get(&module_key).cloned();
        let source = match cached {
            Some(_) => None,
            None => {
                let (_, kinds, ext, _, _) = module_key;
                let source = forward_shader(layout, kinds, ext, custom.map(|c| &**c));
                if custom.is_some()
                    && let Some(warning) = validate_custom_wgsl(&source, self.glsl_es_300)?
                {
                    self.custom_warned(key, warning);
                }
                Some(source)
            }
        };
        // Backend shader translation failures are internal errors, not validation errors.
        // (Pushed after every early return: the scopes must be popped in reverse order.)
        let scopes = custom.map(|_| {
            (
                device.push_error_scope(wgpu::ErrorFilter::Validation),
                device.push_error_scope(wgpu::ErrorFilter::Internal),
            )
        });
        let module = match cached {
            Some(m) => m,
            None => {
                let source = source.expect("generated when not cached");
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
        let (constants, n) = key.constants();
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(layout.name),
            layout: Some(&self.layouts_by_texture[(key.kinds & KIND_COLOR_LAYERED) as usize]),
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
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &constants[..n],
                    ..Default::default()
                },
                targets: &[Some(wgpu::ColorTargetState {
                    format: key.format,
                    blend: blend.then_some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let Some((validation, internal)) = scopes else {
            return Ok((pipeline, None));
        };
        // Scopes pop innermost first.
        let (internal, validation) = (internal.pop(), validation.pop());
        let mut error = Box::pin(async move {
            match internal.await {
                Some(e) => Some(e.to_string()),
                None => validation.await.map(|e| e.to_string()),
            }
        });
        // Native backends resolve the scopes immediately; in the browser the result arrives
        // later: the pipeline is used meanwhile and replaced if it turns out to be invalid.
        match futures::FutureExt::now_or_never(&mut error) {
            Some(Some(e)) => {
                self.shaders.remove(&module_key);
                Err(e)
            }
            Some(None) => Ok((pipeline, None)),
            None => Ok((pipeline, Some((module_key, space3d_tasks::spawn(error))))),
        }
    }
}
