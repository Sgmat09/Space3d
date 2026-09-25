//! [`ModelData`]: a CPU-side scene (meshes + materials + textures + node hierarchy)
//! produced by the glTF and OBJ loaders.

use glam::Mat4;
use serde::{Deserialize, Serialize};
use space3d_core::{ImageData, MeshData, Transform};

/// A loaded model: meshes, materials, decoded textures and a node hierarchy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelData {
    /// Meshes, each made of one or more primitives.
    pub meshes: Vec<ModelMesh>,
    /// Materials referenced by [`Primitive::material`].
    pub materials: Vec<MaterialData>,
    /// Decoded RGBA8 textures referenced by [`MaterialData`] texture indices.
    pub images: Vec<ImageData>,
    /// Scene nodes (flat list; hierarchy via `children`/`parent`).
    pub nodes: Vec<NodeData>,
    /// Indices of root nodes of the default scene.
    pub roots: Vec<usize>,
}

/// A named mesh made of primitives (one per material in glTF terms).
///
/// Construct it with functional record update (`..Default::default()`): fields may be added.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelMesh {
    /// Optional name from the source file.
    pub name: Option<String>,
    /// Geometry chunks.
    pub primitives: Vec<Primitive>,
    /// Extra vertex streams of the primitives: `extras[i]` belongs to `primitives[i]` (shorter
    /// or empty: the rest have none). Code that reorders, splits or removes primitives keeps
    /// it in step or clears it; a stream whose length is not the primitive's vertex count is
    /// ignored (see [`ModelMesh::primitive_extras`]).
    #[serde(default)]
    pub extras: Vec<PrimitiveExtras>,
}

impl ModelMesh {
    /// The extra vertex streams of primitive `index` that match its vertex count (`None`
    /// without any).
    pub fn primitive_extras(&self, index: usize) -> Option<PrimitiveExtras> {
        let n = self.primitives.get(index)?.mesh.positions.len();
        let extras = self.extras.get(index)?;
        let tangents = if extras.tangents.len() == n {
            extras.tangents.clone()
        } else {
            Vec::new()
        };
        let uvs1 = if extras.uvs1.len() == n {
            extras.uvs1.clone()
        } else {
            Vec::new()
        };
        (!tangents.is_empty() || !uvs1.is_empty()).then_some(PrimitiveExtras { tangents, uvs1 })
    }
}

/// Geometry with a single material.
///
/// Its fields are frozen (code builds it with exhaustive literals); per-vertex data beyond
/// [`MeshData`] lives in [`ModelMesh::extras`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Primitive {
    /// Triangle geometry. `normals` and `uvs` always match `positions` in length.
    pub mesh: MeshData,
    /// Index into [`ModelData::materials`].
    pub material: Option<usize>,
}

/// Extra vertex streams of a [`Primitive`] ([`ModelMesh::extras`]), one entry per vertex of
/// its mesh; empty when the source has none.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrimitiveExtras {
    /// glTF `TANGENT` `[x, y, z, w]` (w = ±1 bitangent sign).
    pub tangents: Vec<[f32; 4]>,
    /// glTF `TEXCOORD_1`.
    pub uvs1: Vec<[f32; 2]>,
}

/// How a material's alpha channel is interpreted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum AlphaMode {
    /// Alpha ignored.
    #[default]
    Opaque,
    /// Alpha tested against the cutoff.
    Mask(f32),
    /// Alpha blended.
    Blend,
}

/// Metallic-roughness PBR material description.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaterialData {
    /// Optional name.
    pub name: Option<String>,
    /// Linear RGBA base color factor.
    pub base_color: [f32; 4],
    /// Index into [`ModelData::images`] for the base color (sRGB) texture.
    pub base_color_texture: Option<usize>,
    /// Metallic factor (0..1).
    pub metallic: f32,
    /// Roughness factor (0..1).
    pub roughness: f32,
    /// Linear RGB emissive factor.
    pub emissive: [f32; 3],
    /// Index into [`ModelData::images`] for the tangent-space normal map.
    pub normal_texture: Option<usize>,
    /// Alpha handling.
    pub alpha_mode: AlphaMode,
    /// Disable back-face culling.
    pub double_sided: bool,
    /// Index into [`ModelData::images`] for the metallic-roughness (linear) texture.
    #[serde(default)]
    pub metallic_roughness_texture: Option<usize>,
    /// Index into [`ModelData::images`] for the occlusion (linear) texture.
    #[serde(default)]
    pub occlusion_texture: Option<usize>,
    /// Index into [`ModelData::images`] for the emissive (sRGB) texture.
    #[serde(default)]
    pub emissive_texture: Option<usize>,
    /// Normal map XY scale. Default 1.
    #[serde(default = "one")]
    pub normal_scale: f32,
    /// Occlusion strength. Default 1.
    #[serde(default = "one")]
    pub occlusion_strength: f32,
    /// `KHR_materials_emissive_strength` (× `emissive`). Default 1.
    #[serde(default = "one")]
    pub emissive_strength: f32,
    /// `KHR_materials_unlit`.
    #[serde(default)]
    pub unlit: bool,
    /// Per-slot texture coordinates, transforms and samplers.
    #[serde(default)]
    pub texture_info: MaterialTextureInfo,
}

fn one() -> f32 {
    1.0
}

/// Per-slot texture details of a [`MaterialData`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterialTextureInfo {
    /// Base color slot.
    pub base_color: TextureInfo,
    /// Metallic-roughness slot.
    pub metallic_roughness: TextureInfo,
    /// Normal slot.
    pub normal: TextureInfo,
    /// Occlusion slot.
    pub occlusion: TextureInfo,
    /// Emissive slot.
    pub emissive: TextureInfo,
}

/// How a material slot samples its texture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextureInfo {
    /// glTF `texCoord` (0 or 1 are used).
    pub tex_coord: u32,
    /// `KHR_texture_transform`.
    pub transform: Option<UvTransformData>,
    /// The glTF sampler.
    pub sampler: SamplerData,
}

/// `KHR_texture_transform` of a slot: `uv' = T(offset) · R(rotation) · S(scale) · uv`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UvTransformData {
    /// Translation.
    pub offset: [f32; 2],
    /// Rotation in radians.
    pub rotation: f32,
    /// Scale.
    pub scale: [f32; 2],
    /// `texCoord` override.
    pub tex_coord: Option<u32>,
}

impl Default for UvTransformData {
    fn default() -> Self {
        Self {
            offset: [0.0; 2],
            rotation: 0.0,
            scale: [1.0; 2],
            tex_coord: None,
        }
    }
}

/// A glTF sampler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct SamplerData {
    /// Wrap along u.
    pub wrap_s: WrapMode,
    /// Wrap along v.
    pub wrap_t: WrapMode,
    /// Nearest-neighbour magnification.
    pub nearest_mag: bool,
    /// Nearest-neighbour minification.
    pub nearest_min: bool,
}

/// Texture wrap mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WrapMode {
    /// Repeat.
    #[default]
    Repeat,
    /// Clamp to the edge.
    ClampToEdge,
    /// Mirrored repeat.
    MirroredRepeat,
}

impl Default for MaterialData {
    fn default() -> Self {
        Self {
            name: None,
            base_color: [1.0; 4],
            base_color_texture: None,
            metallic: 0.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            normal_texture: None,
            alpha_mode: AlphaMode::Opaque,
            double_sided: false,
            metallic_roughness_texture: None,
            occlusion_texture: None,
            emissive_texture: None,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
            emissive_strength: 1.0,
            unlit: false,
            texture_info: MaterialTextureInfo::default(),
        }
    }
}

/// A node in the model's hierarchy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeData {
    /// Optional name.
    pub name: Option<String>,
    /// Local transform relative to the parent.
    pub transform: Transform,
    /// Index into [`ModelData::meshes`].
    pub mesh: Option<usize>,
    /// Child node indices.
    pub children: Vec<usize>,
    /// Parent node index (`None` for roots).
    pub parent: Option<usize>,
}

impl ModelData {
    /// A model with a single mesh, default material and one root node.
    pub fn from_mesh(mesh: MeshData) -> Self {
        Self {
            meshes: vec![ModelMesh {
                name: None,
                primitives: vec![Primitive {
                    mesh,
                    material: Some(0),
                }],
                ..Default::default()
            }],
            materials: vec![MaterialData::default()],
            images: Vec::new(),
            nodes: vec![NodeData {
                mesh: Some(0),
                ..Default::default()
            }],
            roots: vec![0],
        }
    }

    /// World (model-space) matrix of a node, composing parent transforms.
    pub fn global_transform(&self, node: usize) -> Mat4 {
        let mut m = self.nodes[node].transform.matrix();
        let mut cur = self.nodes[node].parent;
        let mut guard = 0;
        while let Some(p) = cur {
            m = self.nodes[p].transform.matrix() * m;
            cur = self.nodes[p].parent;
            guard += 1;
            if guard > self.nodes.len() {
                break;
            }
        }
        m
    }

    /// Total number of primitives across meshes.
    pub fn primitive_count(&self) -> usize {
        self.meshes.iter().map(|m| m.primitives.len()).sum()
    }

    /// Bake every mesh instance (node transforms applied) into a single [`MeshData`].
    /// If the model has no nodes, meshes are merged untransformed.
    pub fn merged_mesh(&self) -> MeshData {
        let mut out = MeshData::default();
        let mut append = |mesh: &MeshData, m: Option<Mat4>| {
            let mut part = mesh.clone();
            if let Some(m) = m {
                crate::edit::mesh::transform(&mut part, m);
            }
            crate::edit::mesh::append(&mut out, &part);
        };
        if self.nodes.is_empty() {
            for p in self.meshes.iter().flat_map(|m| &m.primitives) {
                append(&p.mesh, None);
            }
        } else {
            for (i, node) in self.nodes.iter().enumerate() {
                if let Some(mesh) = node.mesh.and_then(|m| self.meshes.get(m)) {
                    let m = self.global_transform(i);
                    for p in &mesh.primitives {
                        append(&p.mesh, (m != Mat4::IDENTITY).then_some(m));
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    #[test]
    fn merged_mesh_applies_hierarchy() {
        let mut model = ModelData::from_mesh(MeshData::cube(2.0));
        model.nodes[0].transform = Transform::from_xyz(10.0, 0.0, 0.0);
        model.nodes.push(NodeData {
            transform: Transform::from_xyz(0.0, 5.0, 0.0),
            mesh: Some(0),
            parent: Some(0),
            ..Default::default()
        });
        model.nodes[0].children.push(1);
        let merged = model.merged_mesh();
        assert_eq!(merged.vertex_count(), 48);
        let (min, max) = merged.aabb();
        assert_eq!(min, Vec3::new(9.0, -1.0, -1.0));
        assert_eq!(max, Vec3::new(11.0, 6.0, 1.0));
    }

    /// `Primitive` keeps its 249c874 field set (the game builds `Primitive { mesh, material }`
    /// literally); its extra vertex streams live in `ModelMesh::extras` and only apply where
    /// they match the vertex count.
    #[test]
    fn primitive_extras_follow_their_primitive() {
        let quad = MeshData::plane(1.0);
        let n = quad.positions.len();
        let mut model = ModelData::from_mesh(quad.clone());
        let mesh = &mut model.meshes[0];
        mesh.primitives.push(Primitive {
            mesh: quad.clone(),
            material: None,
        });
        assert_eq!(mesh.primitive_extras(0), None);
        mesh.extras = vec![
            PrimitiveExtras {
                tangents: vec![[1.0, 0.0, 0.0, 1.0]; n],
                uvs1: vec![[0.5; 2]; n + 1],
            },
            PrimitiveExtras {
                tangents: vec![[1.0, 0.0, 0.0, 1.0]; n - 1],
                uvs1: Vec::new(),
            },
        ];
        let first = mesh.primitive_extras(0).unwrap();
        assert_eq!(
            (first.tangents.len(), first.uvs1.len()),
            (n, 0),
            "mismatched uvs1 dropped"
        );
        assert_eq!(mesh.primitive_extras(1), None, "mismatched tangents dropped");
        assert_eq!(mesh.primitive_extras(2), None);
        // Serialized data without `extras` (older files) still loads.
        let legacy: ModelMesh = serde_json::from_str(
            r#"{"name":null,"primitives":[{"mesh":{"positions":[],"normals":[],"uvs":[],"colors":[],"indices":[]},"material":1}]}"#,
        )
        .unwrap();
        assert_eq!((legacy.primitives[0].material, legacy.extras.len()), (Some(1), 0));
    }
}
