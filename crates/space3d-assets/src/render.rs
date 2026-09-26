//! Bridge from loaded models to the renderer (feature `render`, enabled by the facade's `render`
//! feature): [`upload_model`] / [`prepare_model_async`] register a [`ModelData`]'s meshes,
//! textures and materials in `RenderAssets`, [`spawn_model`] spawns its parts, and
//! [`material_from_data`] / [`apply_material_maps`] convert material descriptions (graphics
//! spec §2.4).
//!
//! The conversion covers every slot (with ORM packing), color space (base color and emissive
//! sRGB, normal / metallic-roughness / occlusion linear), blend mode, `texCoord` set, the
//! glTF samplers and `KHR_texture_transform` (one transform per material). Primitives with a
//! normal map but no `TANGENT` get MikkTSpace tangents in [`prepare_model`].

use crate::loaders::{CubeConvention, CubemapData};
use crate::model::{AlphaMode, MaterialData, ModelData, PrimitiveExtras, SamplerData, TextureInfo, WrapMode};
use glam::{Mat4, Vec2, Vec3, Vec4};
use space3d_core::{Entity, Parent, Transform, World};
use space3d_render::{
    AddressMode, BlendMode, CubemapSource, FilterMode, MapMask, Material, Mesh3d, MeshExtras, MeshHandle, RenderAssets,
    SamplerSettings, TextureHandle, TextureSource, TextureTransform,
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

/// CPU work of an upload: MikkTSpace tangents (splitting vertices at mirrored UVs) for the
/// primitives with a normal map but no `TANGENT`, when `generate_tangents`. Run it through
/// [`prepare_model_async`] off the frame (AGENTS rule 2).
pub fn prepare_model(model: &ModelData, o: &ModelUploadOptions) -> PreparedModel {
    let mut model = model.clone();
    if o.generate_tangents {
        let normal_mapped = |m: Option<usize>| {
            m.and_then(|i| model.materials.get(i))
                .is_some_and(|m| m.normal_texture.is_some())
        };
        let wanted: Vec<Vec<bool>> = model
            .meshes
            .iter()
            .map(|mesh| mesh.primitives.iter().map(|p| normal_mapped(p.material)).collect())
            .collect();
        for (mesh, wanted) in model.meshes.iter_mut().zip(wanted) {
            for (p, wanted) in wanted.into_iter().enumerate() {
                let current = mesh.primitive_extras(p).unwrap_or_default();
                if !wanted || !current.tangents.is_empty() {
                    continue;
                }
                let prim = &mut mesh.primitives[p];
                let (data, tangents, sources) = crate::edit::mesh::tangents::mikktspace_with_sources(&prim.mesh);
                if tangents.is_empty() {
                    continue;
                }
                let uvs1 = if current.uvs1.is_empty() {
                    Vec::new()
                } else {
                    sources.iter().map(|&s| current.uvs1[s as usize]).collect()
                };
                prim.mesh = data;
                if mesh.extras.len() <= p {
                    mesh.extras.resize(p + 1, PrimitiveExtras::default());
                }
                mesh.extras[p] = PrimitiveExtras { tangents, uvs1 };
            }
        }
    }
    PreparedModel {
        model,
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
        // One texture per (image, color space, sampler).
        let mut cache: HashMap<(usize, bool, SamplerSettings), TextureHandle> = HashMap::new();
        let sampler_override = self.options.sampler_override;
        let materials: Vec<Material> = model
            .materials
            .iter()
            .map(|data| {
                let mut texture = |index: usize, role: TextureRole| -> Option<TextureHandle> {
                    let image = model.images.get(index)?;
                    let srgb = role.is_srgb();
                    let sampler = sampler_override.unwrap_or_else(|| sampler_settings(&slot(data, role).sampler));
                    Some(*cache.entry((index, srgb, sampler)).or_insert_with(|| {
                        let mut source = if srgb {
                            TextureSource::from_image(image.clone())
                        } else {
                            TextureSource::from_image_linear(image.clone())
                        };
                        source.sampler = sampler;
                        let h = assets.add_texture(source);
                        textures.push(h);
                        h
                    }))
                };
                material_from_data(data, &mut texture)
            })
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
                    let streams = mesh.primitive_extras(p).unwrap_or_default();
                    let extras = MeshExtras::new()
                        .with_tangents(streams.tangents)
                        .with_uvs1(streams.uvs1);
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

    /// The material map of this role.
    fn map(self) -> MapMask {
        match self {
            Self::BaseColor => MapMask::BASE_COLOR,
            Self::Normal => MapMask::NORMAL,
            Self::MetallicRoughness => MapMask::METALLIC_ROUGHNESS,
            Self::Occlusion => MapMask::OCCLUSION,
            Self::Emissive => MapMask::EMISSIVE,
        }
    }
}

/// The texture details of a material slot.
fn slot(data: &MaterialData, role: TextureRole) -> &TextureInfo {
    let t = &data.texture_info;
    match role {
        TextureRole::BaseColor => &t.base_color,
        TextureRole::Normal => &t.normal,
        TextureRole::MetallicRoughness => &t.metallic_roughness,
        TextureRole::Occlusion => &t.occlusion,
        TextureRole::Emissive => &t.emissive,
    }
}

/// The renderer's sampler for a glTF sampler: nearest when magnification is nearest; clamped
/// when both axes clamp (mirrored repeat samples as repeat).
fn sampler_settings(s: &SamplerData) -> SamplerSettings {
    SamplerSettings {
        filter: if s.nearest_mag {
            FilterMode::Nearest
        } else {
            FilterMode::Linear
        },
        address: if s.wrap_s == WrapMode::ClampToEdge && s.wrap_t == WrapMode::ClampToEdge {
            AddressMode::Clamp
        } else {
            AddressMode::Repeat
        },
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
    if m.base_color_texture.is_some() && data.texture_info.base_color.tex_coord == 1 {
        m.uv1_maps = m.uv1_maps | MapMask::BASE_COLOR;
    }
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
    // Second UV set (glTF `texCoord` 1) per map.
    for (role, handle) in [
        (TextureRole::Normal, material.normal_texture),
        (TextureRole::MetallicRoughness, material.metallic_roughness_texture),
        (TextureRole::Occlusion, material.occlusion_texture),
        (TextureRole::Emissive, material.emissive_texture),
    ] {
        if handle.is_some() && slot(data, role).tex_coord == 1 {
            material.uv1_maps = material.uv1_maps | role.map();
        }
    }
    // One UV transform per material: the base color's, else the first slot with one.
    let slots = [
        (data.base_color_texture, &data.texture_info.base_color),
        (data.normal_texture, &data.texture_info.normal),
        (data.metallic_roughness_texture, &data.texture_info.metallic_roughness),
        (data.occlusion_texture, &data.texture_info.occlusion),
        (data.emissive_texture, &data.texture_info.emissive),
    ];
    let mut transforms = slots
        .iter()
        .filter(|(image, _)| image.is_some())
        .filter_map(|(_, info)| info.transform);
    if let Some(t) = transforms.next() {
        if transforms.any(|o| (o.offset, o.rotation, o.scale) != (t.offset, t.rotation, t.scale)) {
            tracing::warn!(
                material = data.name.as_deref().unwrap_or("?"),
                "texture slots use different KHR_texture_transforms; the first applies to every map"
            );
        }
        material.uv_transform = TextureTransform {
            offset: Vec2::from_array(t.offset),
            rotation: t.rotation,
            scale: Vec2::from_array(t.scale),
        };
    }
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

    /// The migration guide's step 3 (reviewer round M2): `apply_material_maps` keeps the
    /// caller's emissive factor (× `emissive_strength`), so a custom material with no emissive
    /// — the game's painted hull and trim — gets no light from its emissive map (the ships' nav
    /// lights stayed dark); seeding the glTF factor where there is a map, as the guide shows and
    /// `material_from_data` does, lights it.
    #[test]
    fn emissive_maps_need_a_factor() {
        let data = MaterialData {
            emissive_texture: Some(0),
            emissive: [1.0, 1.0, 1.0],
            ..Default::default()
        };
        let mut assets = RenderAssets::default();
        let map = assets.add_image(ImageData::new(1, 1));
        let mut tex = |_: usize, _: TextureRole| Some(map);
        let painted = Material::srgb(0.5, 0.5, 0.55);
        let mut as_is = painted.clone();
        apply_material_maps(&mut as_is, &data, &mut tex);
        assert_eq!(
            (as_is.emissive_texture, as_is.emissive),
            (Some(map), Vec3::ZERO),
            "the map alone is dark"
        );
        let mut seeded = painted;
        if data.emissive_texture.is_some() && seeded.emissive == Vec3::ZERO {
            seeded.emissive = Vec3::from_array(data.emissive);
        }
        apply_material_maps(&mut seeded, &data, &mut tex);
        assert_eq!(seeded.emissive, Vec3::ONE);
        assert_eq!(seeded.emissive, material_from_data(&data, &mut tex).emissive);
    }

    /// `prepare_model` generates MikkTSpace tangents for normal-mapped primitives without
    /// `TANGENT` (carrying the second UV set through vertex splits) and leaves the others.
    #[test]
    fn prepare_model_generates_tangents() {
        let mut model = ModelData::from_mesh(MeshData::cube(1.0));
        model.meshes[0].primitives.push(crate::model::Primitive {
            mesh: MeshData::cube(1.0),
            material: None,
        });
        model.images.push(ImageData::new(1, 1));
        model.materials[0].normal_texture = Some(0);
        model.meshes[0].extras = vec![PrimitiveExtras {
            tangents: Vec::new(),
            uvs1: vec![[0.5, 0.25]; 24],
        }];
        let prepared = prepare_model(&model, &ModelUploadOptions::default());
        let mesh = &prepared.model.meshes[0];
        let extras = mesh.primitive_extras(0).expect("generated");
        assert_eq!(extras.tangents.len(), 24);
        assert_eq!(extras.uvs1, vec![[0.5, 0.25]; 24], "second UV set kept");
        assert!(extras.tangents.iter().all(|t| t[3] == 1.0));
        assert_eq!(mesh.primitive_extras(1), None, "no normal map: no tangents");
        // Given tangents are kept; generation can be turned off.
        let mut given = model.clone();
        given.meshes[0].extras[0].tangents = vec![[0.0, 1.0, 0.0, -1.0]; 24];
        let kept = prepare_model(&given, &ModelUploadOptions::default());
        assert_eq!(kept.model.meshes[0].extras[0].tangents[0], [0.0, 1.0, 0.0, -1.0]);
        let off = ModelUploadOptions {
            generate_tangents: false,
            ..ModelUploadOptions::default()
        };
        assert_eq!(prepare_model(&model, &off).model, model);
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
