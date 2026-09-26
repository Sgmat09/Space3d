//! Instance attributes of a layout: the vertex attributes (from shader location 4) and the
//! generated WGSL `InstanceInput` struct with its accessors.

use crate::instance::{InstanceLayout, InstanceSemantic, wgsl_type};

pub(super) fn to_vec4(expr: &str, ty: &str, fill: &str) -> String {
    match ty {
        "vec4<f32>" => expr.to_string(),
        "vec3<f32>" => format!("vec4<f32>({expr}, {fill})"),
        "vec2<f32>" => format!("vec4<f32>({expr}, {fill}, {fill})"),
        "f32" => format!("vec4<f32>({expr})"),
        t if t.starts_with("vec4<") => format!("vec4<f32>({expr})"),
        _ => format!("vec4<f32>(f32({expr}))"),
    }
}

pub(super) fn to_vec3(expr: &str, ty: &str) -> String {
    match ty {
        "vec3<f32>" => expr.to_string(),
        "vec4<f32>" => format!("{expr}.xyz"),
        "f32" => format!("vec3<f32>({expr})"),
        "vec2<f32>" => format!("vec3<f32>({expr}, 0.0)"),
        _ => "vec3<f32>(0.0)".to_string(),
    }
}

/// Zero-padded `vec4<f32>` of a custom attribute value of WGSL type `ty`.
pub(super) fn custom_vec4(expr: &str, ty: &str) -> String {
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
pub(super) const CUSTOM_ACCESSORS: u8 = 4;

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
        let (_, swizzle) = attribute_format(a.format);
        if let InstanceSemantic::Custom(n) = a.semantic {
            // The raw field is in fetch order (memory order for BGRA formats); the accessor
            // applies the swizzle like the built-in semantics.
            s += &format!("    @location({loc}) custom{n}: {ty},\n");
            loc += 1;
            custom.push((n, ty, swizzle));
            continue;
        }
        s += &format!("    @location({loc}) a{i}: {ty},\n");
        loc += 1;
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
        if !custom.iter().any(|(c, ..)| *c == n) {
            s += &format!(
                "\nfn instance_custom{n}(inst: InstanceInput) -> vec4<f32> {{\n    return vec4<f32>(0.0);\n}}\n"
            );
        }
    }
    for (n, ty, swizzle) in custom {
        let e = custom_vec4(&format!("inst.custom{n}{swizzle}"), ty);
        s += &format!("\nfn instance_custom{n}(inst: InstanceInput) -> vec4<f32> {{\n    return {e};\n}}\n");
    }
    s
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

/// Shader locations the instance attributes of `layout` take (from location 4; a model matrix
/// takes 4, a degenerate layout its placeholder).
pub(crate) fn instance_locations(layout: &InstanceLayout) -> u32 {
    let n: u32 = layout
        .attributes
        .iter()
        .map(|a| match a.semantic {
            InstanceSemantic::ModelMatrix => 4,
            _ => 1,
        })
        .sum();
    n.max(1)
}

/// Most instance locations that leave room for the extension stream's two attributes: WebGPU
/// and WebGL2 guarantee 16 vertex attributes, and the mesh vertex takes 4.
pub(crate) const EXT_MAX_INSTANCE_LOCATIONS: u32 = 10;

/// Whether pipelines of `layout` can read the extension vertex stream (tangents, second UV set)
/// after its instance attributes. Larger layouts draw ext-less: per-pixel tangent frames, and
/// UV0 for the second UV set's maps.
pub(crate) fn ext_fits(layout: &InstanceLayout) -> bool {
    instance_locations(layout) <= EXT_MAX_INSTANCE_LOCATIONS
}
