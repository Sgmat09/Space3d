use super::{finish_mesh, missing_texture, percent_decode};
use crate::edit::texture::decode_image_with_hint;
use crate::error::AssetError;
use crate::loader::{AssetLoader, LoadContext};
use crate::model::{AlphaMode, MaterialData, ModelData, ModelMesh, NodeData, Primitive};
use base64::Engine as _;
use glam::{Quat, Vec3};
use gltf::Gltf;
use gltf::mesh::Mode;
use space3d_core::{ImageData, MeshData, Transform};
use std::borrow::Cow;

/// Loads glTF 2.0 (`.gltf` with embedded or external buffers/images, and binary `.glb`)
/// into [`ModelData`]: triangle meshes, metallic-roughness materials, base-color/normal
/// textures and the node hierarchy of the default scene.
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
    let materials = doc
        .materials()
        .map(|m| {
            let pbr = m.pbr_metallic_roughness();
            MaterialData {
                name: m.name().map(str::to_string),
                base_color: pbr.base_color_factor(),
                base_color_texture: pbr.base_color_texture().and_then(|i| tex(i.texture())),
                metallic: pbr.metallic_factor(),
                roughness: pbr.roughness_factor(),
                emissive: m.emissive_factor(),
                normal_texture: m.normal_texture().and_then(|n| tex(n.texture())),
                alpha_mode: match m.alpha_mode() {
                    gltf::material::AlphaMode::Opaque => AlphaMode::Opaque,
                    gltf::material::AlphaMode::Mask => AlphaMode::Mask(m.alpha_cutoff().unwrap_or(0.5)),
                    gltf::material::AlphaMode::Blend => AlphaMode::Blend,
                },
                double_sided: m.double_sided(),
                ..MaterialData::default() // STUB(P-MAT): the other slots (PM2)
            }
        })
        .collect();

    let mut meshes = Vec::new();
    for mesh in doc.meshes() {
        let mut primitives = Vec::new();
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
            primitives.push(Primitive {
                mesh: m,
                material: prim.material().index(),
            });
        }
        meshes.push(ModelMesh {
            name: mesh.name().map(str::to_string),
            primitives,
            // STUB(P-MAT): TANGENT, TEXCOORD_1 into `extras` (PM2)
            ..ModelMesh::default()
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
