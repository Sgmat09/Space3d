//! Bridge from loaded models to the renderer (feature `render`, enabled by the facade's `render`
//! feature): [`upload_model`] / [`prepare_model_async`] register a [`ModelData`]'s meshes,
//! textures and materials in `RenderAssets`, [`spawn_model`] spawns its parts, and
//! [`material_from_data`] / [`apply_material_maps`] convert material descriptions (graphics
//! spec §2.4).
//!
//! F2 skeleton: the conversion covers every slot, color space and blend mode; MikkTSpace
//! tangents, texture transforms and samplers per slot land with the materials milestone PM2
//! (`STUB(P-MAT)`).

use crate::loaders::{CubeConvention, CubemapData};
use crate::model::{AlphaMode, MaterialData, ModelData};
use glam::{Mat4, Vec3, Vec4};
use space3d_core::{Entity, Parent, Transform, World};
use space3d_render::{
    BlendMode, CubemapSource, Material, Mesh3d, MeshExtras, MeshHandle, RenderAssets, SamplerSettings, TextureHandle,
    TextureSource,
};
use space3d_tasks::Task;
use std::collections::HashMap;

/// Options of [`prepare_model`] / [`upload_model`].
#[derive(Clone, Debug, PartialEq)]
pub struct ModelUploadOptions {
    /// Compute MikkTSpace tangents for normal-mapped primitives without `TANGENT`. Default true.
    pub generate_tangents: bool,
    /// Sampler of every uploaded texture instead of the glTF samplers.
    pub sampler_override: Option<SamplerSettings>,
}

impl Default for ModelUploadOptions {
    fn default() -> Self {
        Self {
            generate_tangents: true,
            sampler_override: None,
        }
    }
}

/// A model with its CPU work done ([`prepare_model`]), ready for a cheap
/// [`PreparedModel::upload`].
#[derive(Clone, Debug)]
pub struct PreparedModel {
    model: ModelData,
    options: ModelUploadOptions,
}

/// CPU work of an upload (tangents, vertex splitting): run it through
/// [`prepare_model_async`] off the frame (AGENTS rule 2).
pub fn prepare_model(model: &ModelData, o: &ModelUploadOptions) -> PreparedModel {
    // STUB(P-MAT): MikkTSpace for normal-mapped primitives lacking TANGENT.
    PreparedModel {
        model: model.clone(),
        options: o.clone(),
    }
}

/// [`prepare_model`] on the compute pool.
pub fn prepare_model_async(model: ModelData, o: ModelUploadOptions) -> Task<PreparedModel> {
    space3d_tasks::spawn_compute(move || prepare_model(&model, &o))
}

impl PreparedModel {
    /// Register meshes, textures and materials (no GPU work: the renderer uploads them next
    /// frame).
    pub fn upload(self, assets: &mut RenderAssets) -> RenderModel {
        let model = &self.model;
        let mut textures = Vec::new();
        let mut cache: HashMap<(usize, bool), TextureHandle> = HashMap::new();
        let sampler = self.options.sampler_override;
        let mut texture = |index: usize, role: TextureRole| -> Option<TextureHandle> {
            let image = model.images.get(index)?;
            let srgb = role.is_srgb();
            Some(*cache.entry((index, srgb)).or_insert_with(|| {
                let mut source = if srgb {
                    TextureSource::from_image(image.clone())
                } else {
                    TextureSource::from_image_linear(image.clone())
                };
                if let Some(s) = sampler {
                    source.sampler = s;
                }
                let h = assets.add_texture(source);
                textures.push(h);
                h
            }))
        };
        let materials: Vec<Material> = model
            .materials
            .iter()
            .map(|m| material_from_data(m, &mut texture))
            .collect();
        let mut meshes: HashMap<(usize, usize), MeshHandle> = HashMap::new();
        let mut parts = Vec::new();
        for (node_index, node) in model.nodes.iter().enumerate() {
            let Some(mesh_index) = node.mesh else { continue };
            let Some(mesh) = model.meshes.get(mesh_index) else {
                continue;
            };
            for (p, prim) in mesh.primitives.iter().enumerate() {
                let handle = *meshes.entry((mesh_index, p)).or_insert_with(|| {
                    let extras = MeshExtras::new()
                        .with_tangents(prim.tangents.clone())
                        .with_uvs1(prim.uvs1.clone());
                    assets.add_mesh_with(prim.mesh.clone(), extras)
                });
                parts.push(RenderPart {
                    name: node.name.clone().or_else(|| mesh.name.clone()),
                    node: Some(node_index),
                    material_index: prim.material,
                    mesh: handle,
                    material: prim
                        .material
                        .and_then(|i| materials.get(i).cloned())
                        .unwrap_or_default(),
                    transform: model.global_transform(node_index),
                });
            }
        }
        RenderModel { parts, textures }
    }
}

/// [`prepare_model`] then [`PreparedModel::upload`], on the calling thread (tools and tests).
pub fn upload_model(model: &ModelData, assets: &mut RenderAssets, o: &ModelUploadOptions) -> RenderModel {
    prepare_model(model, o).upload(assets)
}

/// A model registered in `RenderAssets`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RenderModel {
    /// One part per (node, primitive).
    pub parts: Vec<RenderPart>,
    /// Every texture registered for the model.
    pub textures: Vec<TextureHandle>,
}

/// One drawable part of a [`RenderModel`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RenderPart {
    /// Node (or mesh) name.
    pub name: Option<String>,
    /// Node index in the source model.
    pub node: Option<usize>,
    /// Material index in the source model.
    pub material_index: Option<usize>,
    /// The mesh.
    pub mesh: MeshHandle,
    /// The converted material.
    pub material: Material,
    /// Model-space transform.
    pub transform: Mat4,
}

/// What a material texture slot holds (decides its color space).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TextureRole {
    /// Base color (sRGB).
    BaseColor,
    /// Tangent-space normals (linear).
    Normal,
    /// Metallic-roughness (linear).
    MetallicRoughness,
    /// Occlusion (linear).
    Occlusion,
    /// Emissive (sRGB).
    Emissive,
}

impl TextureRole {
    /// Color data (uploaded sRGB): base color and emissive.
    pub fn is_srgb(self) -> bool {
        matches!(self, Self::BaseColor | Self::Emissive)
    }
}

/// Full `MaterialData` → `Material` conversion. `texture(image, role)` supplies (or uploads)
/// the texture of an image index; `role` decides its color space.
pub fn material_from_data(
    data: &MaterialData,
    texture: &mut dyn FnMut(usize, TextureRole) -> Option<TextureHandle>,
) -> Material {
    let (blend, alpha_cutoff) = match data.alpha_mode {
        AlphaMode::Opaque => (BlendMode::Opaque, 0.5),
        AlphaMode::Mask(c) => (BlendMode::AlphaMask, c),
        AlphaMode::Blend => (BlendMode::AlphaBlend, 0.5),
    };
    let mut m = Material {
        base_color: Vec4::from_array(data.base_color),
        base_color_texture: data.base_color_texture.and_then(|i| texture(i, TextureRole::BaseColor)),
        metallic: data.metallic,
        roughness: data.roughness,
        emissive: Vec3::from_array(data.emissive),
        unlit: data.unlit,
        double_sided: data.double_sided,
        alpha_blend: blend == BlendMode::AlphaBlend,
        blend,
        alpha_cutoff,
        ..Material::default()
    };
    apply_material_maps(&mut m, data, texture);
    m
}

/// Add only the map-related state of `data` to a material built by custom code: normal,
/// metallic-roughness, occlusion and emissive maps, `normal_scale`, `occlusion_strength`, ORM
/// packing (the same image for MR and occlusion gives the same handle), and
/// `emissive_strength` × `emissive` (only when the material's emissive is non-zero or the slot
/// exists). Base color, blend and surface factors are left untouched.
pub fn apply_material_maps(
    material: &mut Material,
    data: &MaterialData,
    texture: &mut dyn FnMut(usize, TextureRole) -> Option<TextureHandle>,
) {
    if let Some(i) = data.normal_texture {
        material.normal_texture = texture(i, TextureRole::Normal);
        material.normal_scale = data.normal_scale;
    }
    if let Some(i) = data.metallic_roughness_texture {
        material.metallic_roughness_texture = texture(i, TextureRole::MetallicRoughness);
    }
    if let Some(i) = data.occlusion_texture {
        material.occlusion_texture = if Some(i) == data.metallic_roughness_texture {
            material.metallic_roughness_texture
        } else {
            texture(i, TextureRole::Occlusion)
        };
        material.occlusion_strength = data.occlusion_strength;
    }
    if let Some(i) = data.emissive_texture {
        material.emissive_texture = texture(i, TextureRole::Emissive);
    }
    if material.emissive != Vec3::ZERO || data.emissive_texture.is_some() {
        material.emissive *= data.emissive_strength;
    }
    // STUB(P-MAT): texCoord 1 → uv1_maps, KHR_texture_transform → uv_transform (PM2).
}

/// Spawn `model` under a new root entity at `root`: one child per part (`Mesh3d`, `Material`,
/// the part's model-space transform, `Parent(root)`). Returns the root.
pub fn spawn_model(world: &mut World, model: &RenderModel, root: Transform) -> Entity {
    let root_entity = world.spawn((root,));
    for part in &model.parts {
        let (scale, rotation, translation) = part.transform.to_scale_rotation_translation();
        world.spawn((
            Transform {
                translation,
                rotation,
                scale,
            },
            Mesh3d(part.mesh),
            part.material.clone(),
            Parent(root_entity),
        ));
    }
    root_entity
}

impl CubemapData {
    /// The faces as a renderer cubemap source (three.js faces are converted).
    pub fn to_source(&self) -> CubemapSource {
        let faces = self.faces.clone();
        let source = match self.convention {
            CubeConvention::ThreeJs => CubemapSource::from_faces_threejs(faces),
            _ => CubemapSource::from_faces(faces),
        };
        // Unequal faces cannot form a cubemap: a black one of the first face's size instead.
        source.unwrap_or_else(|| {
            let size = self.faces[0].width.max(1);
            CubemapSource::from_faces(std::array::from_fn(|_| space3d_core::ImageData::new(size, size)))
                .expect("equal square faces")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::{ImageData, MeshData};

    #[test]
    fn materials_convert_every_slot() {
        let data = MaterialData {
            base_color_texture: Some(0),
            normal_texture: Some(1),
            metallic_roughness_texture: Some(2),
            occlusion_texture: Some(2),
            emissive_texture: Some(3),
            emissive: [1.0, 0.5, 0.0],
            emissive_strength: 4.0,
            normal_scale: 0.5,
            alpha_mode: AlphaMode::Mask(0.3),
            ..Default::default()
        };
        let mut assets = RenderAssets::default();
        let handles: Vec<TextureHandle> = (0..4).map(|_| assets.add_image(ImageData::new(1, 1))).collect();
        let mut calls = Vec::new();
        let mut tex = |i: usize, role: TextureRole| {
            calls.push((i, role.is_srgb()));
            handles.get(i).copied()
        };
        let m = material_from_data(&data, &mut tex);
        assert_eq!(m.effective_blend(), BlendMode::AlphaMask);
        assert_eq!(m.alpha_cutoff, 0.3);
        assert_eq!(m.emissive, Vec3::new(4.0, 2.0, 0.0));
        assert_eq!(m.normal_scale, 0.5);
        assert_eq!(m.metallic_roughness_texture, m.occlusion_texture, "ORM packed");
        assert_eq!(calls, [(0, true), (1, false), (2, false), (3, true)]);
        // Custom materials keep their own factors.
        let mut custom = Material::srgb(0.2, 0.3, 0.4);
        apply_material_maps(&mut custom, &MaterialData::default(), &mut |_, _| None);
        assert_eq!(custom, Material::srgb(0.2, 0.3, 0.4));
    }

    #[test]
    fn upload_and_spawn() {
        let mut model = ModelData::from_mesh(MeshData::cube(1.0));
        model.images.push(ImageData::new(2, 2));
        model.materials[0].base_color_texture = Some(0);
        model.materials[0].normal_texture = Some(0);
        let mut assets = RenderAssets::default();
        let r = upload_model(&model, &mut assets, &ModelUploadOptions::default());
        assert_eq!(r.parts.len(), 1);
        assert_eq!(r.textures.len(), 2, "sRGB and linear uploads of one image");
        assert!(assets.contains_mesh(r.parts[0].mesh));
        let mut world = World::new();
        let root = spawn_model(&mut world, &r, Transform::IDENTITY);
        assert_eq!(world.query::<&Parent>().iter().filter(|p| p.0 == root).count(), 1);
        let cube = CubemapData {
            faces: std::array::from_fn(|_| ImageData::new(4, 4)),
            convention: CubeConvention::ThreeJs,
        };
        assert_eq!(cube.to_source().face_size(), 4);
    }
}
