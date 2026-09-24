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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelMesh {
    /// Optional name from the source file.
    pub name: Option<String>,
    /// Geometry chunks.
    pub primitives: Vec<Primitive>,
}

/// Geometry with a single material.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Primitive {
    /// Triangle geometry. `normals` and `uvs` always match `positions` in length.
    pub mesh: MeshData,
    /// Index into [`ModelData::materials`].
    pub material: Option<usize>,
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
}
