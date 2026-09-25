use super::{finish_mesh, missing_texture};
use crate::edit::texture::decode_image_with_hint;
use crate::error::AssetError;
use crate::loader::{AssetLoader, LoadContext};
use crate::model::{AlphaMode, MaterialData, ModelData, ModelMesh, NodeData, Primitive};
use crate::path::AssetPath;
use space3d_core::{ImageData, MeshData};
use std::cell::RefCell;
use std::collections::HashMap;

/// Loads Wavefront OBJ (+ MTL materials and their diffuse/bump textures) into [`ModelData`]
/// via `tobj`. Faces are triangulated; texture `v` is flipped to the top-left UV origin used
/// by the engine. Each OBJ object/material group becomes one mesh + root node.
#[derive(Clone, Copy, Debug)]
pub struct ObjLoader {
    /// Read `mtllib` files and the textures they reference.
    pub load_materials: bool,
}

impl Default for ObjLoader {
    fn default() -> Self {
        Self { load_materials: true }
    }
}

fn mtllibs(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let rest = l.strip_prefix("mtllib")?;
            rest.starts_with([' ', '\t']).then(|| rest.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Texture statements may carry options (`-bm 1.0 bump.png`); the file name is last.
fn texture_file(spec: &str) -> &str {
    let spec = spec.trim();
    if spec.starts_with('-') {
        spec.split_whitespace().last().unwrap_or(spec)
    } else {
        spec
    }
}

/// URI (root-relative, so it resolves the same from the OBJ) of a texture named in `mtl_uri`.
fn texture_uri(ctx: &LoadContext, mtl_uri: &str, tex: &str) -> String {
    let full = ctx.resolve(mtl_uri).resolve(&texture_file(tex).replace('\\', "/"));
    format!("/{}", full.path())
}

impl AssetLoader for ObjLoader {
    type Asset = ModelData;

    fn extensions(&self) -> &[&str] {
        &["obj"]
    }

    fn dependencies(&self, bytes: &[u8], ctx: &LoadContext) -> Result<Vec<String>, AssetError> {
        if !self.load_materials {
            return Ok(Vec::new());
        }
        let libs = mtllibs(bytes);
        let mut deps = libs.clone();
        for lib in &libs {
            let Some(mtl) = ctx.dependency(lib) else { continue };
            let Ok((mats, _)) = tobj::load_mtl_buf(&mut &mtl[..]) else {
                continue;
            };
            for m in mats {
                for tex in [&m.diffuse_texture, &m.normal_texture].into_iter().flatten() {
                    deps.push(texture_uri(ctx, lib, tex));
                }
            }
        }
        Ok(deps)
    }

    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<ModelData, AssetError> {
        load_obj(bytes, ctx, self.load_materials)
    }
}

/// Decode an OBJ file. MTL files and textures must already be present in `ctx`; missing
/// materials are skipped and missing textures replaced by a checker.
pub fn load_obj(bytes: &[u8], ctx: &LoadContext, load_materials: bool) -> Result<ModelData, AssetError> {
    let path = ctx.path();
    let opts = tobj::LoadOptions {
        single_index: true,
        triangulate: true,
        ignore_points: true,
        ignore_lines: true,
    };
    // Remember which MTL file each material came from so textures resolve relative to it.
    let origins: RefCell<Vec<(String, usize)>> = RefCell::new(Vec::new());
    let (models, materials) = tobj::load_obj_buf(&mut &bytes[..], &opts, |p| {
        let uri = p.to_string_lossy().replace('\\', "/");
        match ctx.dependency(&uri) {
            Some(b) if load_materials => {
                let r = tobj::load_mtl_buf(&mut &b[..]);
                if let Ok((mats, _)) = &r {
                    origins.borrow_mut().push((uri, mats.len()));
                }
                r
            }
            _ => Err(tobj::LoadError::OpenFileFailed),
        }
    })
    .map_err(|e| AssetError::decode(path, e))?;
    let materials = match materials {
        Ok(m) => m,
        Err(e) => {
            if load_materials {
                tracing::warn!(asset = %path, error = %e, "OBJ materials unavailable");
            }
            Vec::new()
        }
    };
    // Expand per-file counts into a per-material origin list.
    let mut mat_origin: Vec<String> = Vec::with_capacity(materials.len());
    for (uri, count) in origins.into_inner() {
        mat_origin.extend(std::iter::repeat_n(uri, count));
    }

    let mut texture_uris: Vec<String> = Vec::new();
    let mut texture_index: HashMap<String, usize> = HashMap::new();
    let mut texture = |mat: usize, spec: &Option<String>| -> Option<usize> {
        let spec = spec.as_ref()?;
        let origin = mat_origin.get(mat).map(String::as_str).unwrap_or("");
        let uri = if origin.is_empty() {
            format!("/{}", ctx.resolve(texture_file(spec)).path())
        } else {
            texture_uri(ctx, origin, spec)
        };
        Some(*texture_index.entry(uri.clone()).or_insert_with(|| {
            texture_uris.push(uri);
            texture_uris.len() - 1
        }))
    };
    let mats: Vec<MaterialData> = materials
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let d = m.diffuse.unwrap_or([1.0; 3]);
            let alpha = m.dissolve.unwrap_or(1.0);
            MaterialData {
                name: Some(m.name.clone()),
                base_color: [d[0], d[1], d[2], alpha],
                base_color_texture: texture(i, &m.diffuse_texture),
                metallic: 0.0,
                roughness: m.shininess.map_or(1.0, |ns| (2.0 / (ns.max(0.0) + 2.0)).sqrt()),
                emissive: [0.0; 3],
                normal_texture: texture(i, &m.normal_texture),
                alpha_mode: if alpha < 1.0 {
                    AlphaMode::Blend
                } else {
                    AlphaMode::Opaque
                },
                double_sided: false,
                ..MaterialData::default()
            }
        })
        .collect();

    let images: Vec<ImageData> = crate::par::map(texture_uris, |uri| {
        let hint = AssetPath::new(&uri).extension();
        match ctx.dependency(&uri) {
            Some(b) => decode_image_with_hint(b, hint.as_deref()).unwrap_or_else(|e| {
                tracing::warn!(asset = %path, texture = %uri, error = %e, "failed to decode OBJ texture");
                missing_texture()
            }),
            None => {
                tracing::warn!(asset = %path, texture = %uri, "OBJ texture unavailable");
                missing_texture()
            }
        }
    });

    let mut meshes = Vec::with_capacity(models.len());
    let mut nodes = Vec::with_capacity(models.len());
    for model in models {
        let m = model.mesh;
        let n = m.positions.len() / 3;
        let mut mesh = MeshData {
            positions: m.positions.as_chunks::<3>().0.to_vec(),
            normals: if m.normals.len() == n * 3 {
                m.normals.as_chunks::<3>().0.to_vec()
            } else {
                Vec::new()
            },
            uvs: if m.texcoords.len() == n * 2 {
                m.texcoords
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| [c[0], 1.0 - c[1]])
                    .collect()
            } else {
                Vec::new()
            },
            colors: if m.vertex_color.len() == n * 3 {
                m.vertex_color
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|c| [c[0], c[1], c[2], 1.0])
                    .collect()
            } else {
                Vec::new()
            },
            indices: m.indices,
        };
        if mesh.indices.iter().any(|&i| i as usize >= n) {
            return Err(AssetError::decode(path, "OBJ index out of range"));
        }
        finish_mesh(&mut mesh);
        let mesh_index = meshes.len();
        meshes.push(ModelMesh {
            name: (!model.name.is_empty()).then_some(model.name.clone()),
            primitives: vec![Primitive {
                mesh,
                material: m.material_id.filter(|&i| i < mats.len()),
                ..Primitive::default()
            }],
        });
        nodes.push(NodeData {
            name: (!model.name.is_empty()).then_some(model.name),
            mesh: Some(mesh_index),
            ..Default::default()
        });
    }
    let roots = (0..nodes.len()).collect();
    Ok(ModelData {
        meshes,
        materials: mats,
        images,
        nodes,
        roots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_mtllibs_and_texture_names() {
        let obj = b"# c\nmtllib my mats.mtl\nmtllibx nope\n\tmtllib b.mtl\nv 0 0 0\n";
        assert_eq!(mtllibs(obj), vec!["my mats.mtl".to_string(), "b.mtl".to_string()]);
        assert_eq!(texture_file("-bm 0.5 bump.png"), "bump.png");
        assert_eq!(texture_file(" a b.png "), "a b.png");
        let ctx = LoadContext::new("models/x.obj");
        assert_eq!(texture_uri(&ctx, "mats/m.mtl", "t.png"), "/models/mats/t.png");
    }
}
