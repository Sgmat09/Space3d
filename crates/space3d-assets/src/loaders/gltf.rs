use super::{finish_mesh, missing_texture, percent_decode};
use crate::edit::texture::decode_image_with_hint;
use crate::error::AssetError;
use crate::loader::{AssetLoader, LoadContext};
use crate::model::{
    AlphaMode, MaterialData, MaterialTextureInfo, ModelData, ModelMesh, NodeData, Primitive, PrimitiveExtras,
    SamplerData, TextureInfo, UvTransformData, WrapMode,
};
use base64::Engine as _;
use glam::{Quat, Vec3};
use gltf::Gltf;
use gltf::mesh::Mode;
use space3d_core::{ImageData, MeshData, Transform};
use std::borrow::Cow;

/// Loads glTF 2.0 (`.gltf` with embedded or external buffers/images, and binary `.glb`)
/// into [`ModelData`]: triangle meshes (with `TANGENT` and `TEXCOORD_1` as
/// [`ModelMesh::extras`]), metallic-roughness materials with every texture slot (base color,
/// metallic-roughness, normal + scale, occlusion + strength, emissive), their `texCoord`
/// sets, samplers and `KHR_texture_transform`, `KHR_materials_emissive_strength` and
/// `KHR_materials_unlit`, and the node hierarchy of the default scene.
#[derive(Clone, Copy, Debug)]
pub struct GltfLoader {
    /// Decode images (disable when only geometry is needed).
    pub load_images: bool,
}

impl Default for GltfLoader {
    fn default() -> Self {
        Self { load_images: true }
    }
}

impl AssetLoader for GltfLoader {
    type Asset = ModelData;

    fn extensions(&self) -> &[&str] {
        &["gltf", "glb"]
    }

    fn dependencies(&self, bytes: &[u8], ctx: &LoadContext) -> Result<Vec<String>, AssetError> {
        let gltf = Gltf::from_slice(bytes).map_err(|e| AssetError::decode(ctx.path(), e))?;
        let mut deps = Vec::new();
        for b in gltf.buffers() {
            if let gltf::buffer::Source::Uri(uri) = b.source()
                && !uri.starts_with("data:")
            {
                deps.push(percent_decode(uri));
            }
        }
        if self.load_images {
            for img in gltf.images() {
                if let gltf::image::Source::Uri { uri, .. } = img.source()
                    && !uri.starts_with("data:")
                {
                    deps.push(percent_decode(uri));
                }
            }
        }
        Ok(deps)
    }

    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<ModelData, AssetError> {
        load_gltf(bytes, ctx, self.load_images)
    }
}

/// Encoded image bytes (if available), a format hint, and a name for diagnostics.
type ImageSource<'a> = (Option<Cow<'a, [u8]>>, Option<String>, String);

fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let (meta, data) = uri.strip_prefix("data:")?.split_once(',')?;
    if meta.ends_with(";base64") {
        base64::engine::general_purpose::STANDARD.decode(data).ok()
    } else {
        Some(percent_decode(data).into_bytes())
    }
}

fn read_uri<'a>(uri: &str, ctx: &'a LoadContext) -> Option<Cow<'a, [u8]>> {
    if uri.starts_with("data:") {
        decode_data_uri(uri).map(Cow::Owned)
    } else {
        ctx.dependency(&percent_decode(uri)).map(Cow::Borrowed)
    }
}

fn uri_extension(uri: &str) -> Option<String> {
    crate::path::AssetPath::new(uri).extension()
}

fn mime_extension(mime: &str) -> Option<String> {
    mime.strip_prefix("image/").map(|s| s.to_string())
}

fn strip_to_list(idx: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(idx.len().saturating_sub(2) * 3);
    for i in 0..idx.len().saturating_sub(2) {
        if i % 2 == 0 {
            out.extend([idx[i], idx[i + 1], idx[i + 2]]);
        } else {
            out.extend([idx[i + 1], idx[i], idx[i + 2]]);
        }
    }
    out
}

fn fan_to_list(idx: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(idx.len().saturating_sub(2) * 3);
    for i in 1..idx.len().saturating_sub(1) {
        out.extend([idx[0], idx[i], idx[i + 1]]);
    }
    out
}

/// Decode a glTF/GLB document. External buffers/images must already be present in `ctx`
/// (the asset server takes care of that); missing images are replaced by a checker.
pub fn load_gltf(bytes: &[u8], ctx: &LoadContext, load_images: bool) -> Result<ModelData, AssetError> {
    let path = ctx.path();
    let gltf = Gltf::from_slice(bytes).map_err(|e| AssetError::decode(path, e))?;
    let doc = &gltf.document;

    let mut buffers: Vec<Cow<[u8]>> = Vec::new();
    for b in doc.buffers() {
        let data = match b.source() {
            gltf::buffer::Source::Bin => gltf
                .blob
                .as_deref()
                .map(Cow::Borrowed)
                .ok_or_else(|| AssetError::decode(path, "GLB binary chunk missing"))?,
            gltf::buffer::Source::Uri(uri) => {
                read_uri(uri, ctx).ok_or_else(|| AssetError::decode(path, format!("buffer '{uri}' is unavailable")))?
            }
        };
        if data.len() < b.length() {
            return Err(AssetError::decode(
                path,
                format!("buffer {} is {} bytes, expected {}", b.index(), data.len(), b.length()),
            ));
        }
        buffers.push(data);
    }

    let images: Vec<ImageData> = if load_images {
        let sources: Vec<ImageSource> = doc
            .images()
            .map(|img| match img.source() {
                gltf::image::Source::View { view, mime_type } => {
                    let data = buffers
                        .get(view.buffer().index())
                        .and_then(|b| b.get(view.offset()..view.offset() + view.length()))
                        .map(Cow::Borrowed);
                    (data, mime_extension(mime_type), format!("image {}", img.index()))
                }
                gltf::image::Source::Uri { uri, mime_type } => (
                    read_uri(uri, ctx),
                    mime_type.and_then(mime_extension).or_else(|| uri_extension(uri)),
                    if uri.starts_with("data:") {
                        format!("image {}", img.index())
                    } else {
                        uri.to_string()
                    },
                ),
            })
            .collect();
        crate::par::map(sources, |(data, hint, name)| match data {
            Some(d) => decode_image_with_hint(&d, hint.as_deref()).unwrap_or_else(|e| {
                tracing::warn!(asset = %path, image = %name, error = %e, "failed to decode glTF image");
                missing_texture()
            }),
            None => {
                tracing::warn!(asset = %path, image = %name, "glTF image data unavailable");
                missing_texture()
            }
        })
    } else {
        Vec::new()
    };

    let image_count = images.len();
    let tex = |t: gltf::Texture| -> Option<usize> {
        let i = t.source().index();
        (i < image_count).then_some(i)
    };
    let materials = doc.materials().map(|m| material(&m, &tex)).collect();

    let mut meshes = Vec::new();
    for mesh in doc.meshes() {
        let mut primitives = Vec::new();
        let mut extras = Vec::new();
        for prim in mesh.primitives() {
            let mode = prim.mode();
            if !matches!(mode, Mode::Triangles | Mode::TriangleStrip | Mode::TriangleFan) {
                tracing::warn!(asset = %path, ?mode, "skipping non-triangle glTF primitive");
                continue;
            }
            let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d[..]));
            let Some(positions) = reader.read_positions() else {
                tracing::warn!(asset = %path, "skipping glTF primitive without positions");
                continue;
            };
            let positions: Vec<[f32; 3]> = positions.collect();
            let n = positions.len();
            let raw: Vec<u32> = match reader.read_indices() {
                Some(i) => i.into_u32().collect(),
                None => (0..n as u32).collect(),
            };
            let indices = match mode {
                Mode::TriangleStrip => strip_to_list(&raw),
                Mode::TriangleFan => fan_to_list(&raw),
                _ => raw,
            };
            if indices.iter().any(|&i| i as usize >= n) {
                return Err(AssetError::decode(path, "primitive index out of range"));
            }
            let mut m = MeshData {
                positions,
                normals: reader.read_normals().map(|n| n.collect()).unwrap_or_default(),
                uvs: reader
                    .read_tex_coords(0)
                    .map(|t| t.into_f32().collect())
                    .unwrap_or_default(),
                colors: reader
                    .read_colors(0)
                    .map(|c| c.into_rgba_f32().collect())
                    .unwrap_or_default(),
                indices,
            };
            finish_mesh(&mut m);
            // Streams beyond `MeshData`; `ModelMesh::primitive_extras` drops any whose length no
            // longer matches (e.g. flat normals recomputed for a primitive without normals).
            extras.push(PrimitiveExtras {
                tangents: reader.read_tangents().map(|t| t.collect()).unwrap_or_default(),
                uvs1: reader
                    .read_tex_coords(1)
                    .map(|t| t.into_f32().collect())
                    .unwrap_or_default(),
            });
            primitives.push(Primitive {
                mesh: m,
                material: prim.material().index(),
            });
        }
        if extras.iter().all(|e| e.tangents.is_empty() && e.uvs1.is_empty()) {
            extras.clear();
        }
        meshes.push(ModelMesh {
            name: mesh.name().map(str::to_string),
            primitives,
            extras,
        });
    }

    let mut nodes: Vec<NodeData> = doc
        .nodes()
        .map(|n| {
            let (t, r, s) = n.transform().decomposed();
            NodeData {
                name: n.name().map(str::to_string),
                transform: Transform {
                    translation: Vec3::from(t),
                    rotation: Quat::from_array(r),
                    scale: Vec3::from(s),
                },
                mesh: n.mesh().map(|m| m.index()),
                children: n.children().map(|c| c.index()).collect(),
                parent: None,
            }
        })
        .collect();
    for i in 0..nodes.len() {
        for c in nodes[i].children.clone() {
            if let Some(child) = nodes.get_mut(c) {
                child.parent = Some(i);
            }
        }
    }
    let roots = match doc.default_scene().or_else(|| doc.scenes().next()) {
        Some(scene) => scene.nodes().map(|n| n.index()).collect(),
        None => (0..nodes.len()).filter(|&i| nodes[i].parent.is_none()).collect(),
    };

    Ok(ModelData {
        meshes,
        materials,
        images,
        nodes,
        roots,
    })
}

/// A glTF sampler as [`SamplerData`] (the defaults — repeat, linear — without one).
fn sampler_data(s: gltf::texture::Sampler) -> SamplerData {
    use gltf::texture::{MagFilter, MinFilter, WrappingMode as W};
    let wrap = |w: W| match w {
        W::ClampToEdge => WrapMode::ClampToEdge,
        W::MirroredRepeat => WrapMode::MirroredRepeat,
        W::Repeat => WrapMode::Repeat,
    };
    SamplerData {
        wrap_s: wrap(s.wrap_s()),
        wrap_t: wrap(s.wrap_t()),
        nearest_mag: s.mag_filter() == Some(MagFilter::Nearest),
        nearest_min: matches!(
            s.min_filter(),
            Some(MinFilter::Nearest | MinFilter::NearestMipmapNearest | MinFilter::NearestMipmapLinear)
        ),
    }
}

/// `KHR_texture_transform` of a slot from its raw extension JSON (the `normalTexture` and
/// `occlusionTexture` slots have no typed accessor for it).
fn transform_from_json(v: &serde_json::Value) -> UvTransformData {
    let pair = |key: &str, default: [f32; 2]| {
        v.get(key)
            .and_then(|a| a.as_array())
            .filter(|a| a.len() == 2)
            .map_or(default, |a| {
                [0, 1].map(|i| a[i].as_f64().map_or(default[i], |x| x as f32))
            })
    };
    UvTransformData {
        offset: pair("offset", [0.0; 2]),
        rotation: v.get("rotation").and_then(|r| r.as_f64()).unwrap_or(0.0) as f32,
        scale: pair("scale", [1.0; 2]),
        tex_coord: v.get("texCoord").and_then(|t| t.as_u64()).map(|t| t as u32),
    }
}

/// A texture slot's details: `texCoord` (as overridden by its transform), transform, sampler.
fn texture_info(tex_coord: u32, texture: &gltf::Texture, transform: Option<UvTransformData>) -> TextureInfo {
    TextureInfo {
        tex_coord: transform.and_then(|t| t.tex_coord).unwrap_or(tex_coord),
        transform,
        sampler: sampler_data(texture.sampler()),
    }
}

/// One glTF material as [`MaterialData`]: every slot (image indices through `tex`), factors,
/// alpha mode, the per-slot texture details and the supported extensions.
fn material(m: &gltf::Material, tex: &dyn Fn(gltf::Texture) -> Option<usize>) -> MaterialData {
    let pbr = m.pbr_metallic_roughness();
    let info = |i: &gltf::texture::Info| {
        let t = i.texture_transform().map(|t| UvTransformData {
            offset: t.offset(),
            rotation: t.rotation(),
            scale: t.scale(),
            tex_coord: t.tex_coord(),
        });
        texture_info(i.tex_coord(), &i.texture(), t)
    };
    let raw_transform = |v: Option<&serde_json::Value>| v.map(transform_from_json);
    let base = pbr.base_color_texture();
    let mr = pbr.metallic_roughness_texture();
    let normal = m.normal_texture();
    let occlusion = m.occlusion_texture();
    let emissive = m.emissive_texture();
    let texture_info = MaterialTextureInfo {
        base_color: base.as_ref().map(info).unwrap_or_default(),
        metallic_roughness: mr.as_ref().map(info).unwrap_or_default(),
        normal: normal.as_ref().map_or_else(TextureInfo::default, |n| {
            let t = raw_transform(n.extension_value("KHR_texture_transform"));
            texture_info(n.tex_coord(), &n.texture(), t)
        }),
        occlusion: occlusion.as_ref().map_or_else(TextureInfo::default, |o| {
            let t = raw_transform(o.extension_value("KHR_texture_transform"));
            texture_info(o.tex_coord(), &o.texture(), t)
        }),
        emissive: emissive.as_ref().map(info).unwrap_or_default(),
    };
    MaterialData {
        name: m.name().map(str::to_string),
        base_color: pbr.base_color_factor(),
        base_color_texture: base.and_then(|i| tex(i.texture())),
        metallic: pbr.metallic_factor(),
        roughness: pbr.roughness_factor(),
        emissive: m.emissive_factor(),
        normal_texture: normal.as_ref().and_then(|n| tex(n.texture())),
        alpha_mode: match m.alpha_mode() {
            gltf::material::AlphaMode::Opaque => AlphaMode::Opaque,
            gltf::material::AlphaMode::Mask => AlphaMode::Mask(m.alpha_cutoff().unwrap_or(0.5)),
            gltf::material::AlphaMode::Blend => AlphaMode::Blend,
        },
        double_sided: m.double_sided(),
        metallic_roughness_texture: mr.and_then(|i| tex(i.texture())),
        occlusion_texture: occlusion.as_ref().and_then(|o| tex(o.texture())),
        emissive_texture: emissive.and_then(|i| tex(i.texture())),
        normal_scale: normal.as_ref().map_or(1.0, |n| n.scale()),
        occlusion_strength: occlusion.as_ref().map_or(1.0, |o| o.strength()),
        emissive_strength: m.emissive_strength().unwrap_or(1.0),
        unlit: m.unlit(),
        texture_info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_and_fan_conversion() {
        assert_eq!(strip_to_list(&[0, 1, 2, 3]), vec![0, 1, 2, 2, 1, 3]);
        assert_eq!(fan_to_list(&[0, 1, 2, 3]), vec![0, 1, 2, 0, 2, 3]);
        assert!(strip_to_list(&[0, 1]).is_empty());
    }

    #[test]
    fn data_uris() {
        assert_eq!(
            decode_data_uri("data:application/octet-stream;base64,AQID").unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(decode_data_uri("data:text/plain,a%20b").unwrap(), b"a b");
    }
}
