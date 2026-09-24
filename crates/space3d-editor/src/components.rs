//! Editor-side component support: scene-registry registrations for render components,
//! [`MeshSource`] (re-creatable meshes), [`EditorInternal`] and generic JSON access to
//! registered components.

use serde::{Deserialize, Serialize};
use space3d_core::hecs::{Entity, World};
use space3d_core::{MeshData, Name, Parent, Transform};
use space3d_render::{Camera3d, DirectionalLight, Material, Mesh3d, PointLight, Projection, RenderTarget, Visibility};
use space3d_scene::{ComponentRegistry, ComponentValue, Scene, SceneEntity, StableId, save_entities};

/// Marker for entities owned by the editor itself (model preview camera, ...): hidden from
/// the hierarchy and never saved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditorInternal;

/// How to (re)create an entity's mesh. Saved in scenes (as `"MeshSource"`), so meshes survive
/// save/load; the editor turns it into a [`Mesh3d`] when the entity has none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MeshSource {
    /// `MeshData::cube(size)`.
    Cube {
        /// Edge length.
        size: f32,
    },
    /// `MeshData::plane(size)`.
    Plane {
        /// Edge length.
        size: f32,
    },
    /// `MeshData::sphere(radius, sectors, stacks)`.
    Sphere {
        /// Radius.
        radius: f32,
        /// Longitudinal segments.
        sectors: u32,
        /// Latitudinal segments.
        stacks: u32,
    },
    /// Inline mesh data (e.g. from the model editor).
    Data(MeshData),
}

impl MeshSource {
    /// Build the mesh.
    pub fn mesh(&self) -> MeshData {
        match self {
            MeshSource::Cube { size } => MeshData::cube(*size),
            MeshSource::Plane { size } => MeshData::plane(*size),
            MeshSource::Sphere {
                radius,
                sectors,
                stacks,
            } => MeshData::sphere(*radius, *sectors, *stacks),
            MeshSource::Data(m) => m.clone(),
        }
    }

    /// Short label.
    pub fn label(&self) -> &'static str {
        match self {
            MeshSource::Cube { .. } => "cube",
            MeshSource::Plane { .. } => "plane",
            MeshSource::Sphere { .. } => "sphere",
            MeshSource::Data(_) => "mesh data",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct MaterialRepr {
    base_color: [f32; 4],
    metallic: f32,
    roughness: f32,
    emissive: [f32; 3],
    #[serde(default)]
    unlit: bool,
    #[serde(default)]
    double_sided: bool,
    #[serde(default)]
    alpha_blend: bool,
}

#[derive(Serialize, Deserialize)]
struct DirectionalLightRepr {
    color: [f32; 3],
    intensity: f32,
}

#[derive(Serialize, Deserialize)]
struct PointLightRepr {
    color: [f32; 3],
    intensity: f32,
    range: f32,
}

#[derive(Serialize, Deserialize)]
enum ProjectionRepr {
    Perspective { fov_y_degrees: f32, near: f32, far: f32 },
    Orthographic { height: f32, near: f32, far: f32 },
}

#[derive(Serialize, Deserialize)]
struct CameraRepr {
    projection: ProjectionRepr,
    clear_color: Option<[f32; 4]>,
    #[serde(default)]
    order: i32,
    #[serde(default)]
    viewport: Option<[f32; 4]>,
    active: bool,
}

/// Registration names used by the editor.
pub mod names {
    /// [`space3d_render::Material`].
    pub const MATERIAL: &str = "Material";
    /// [`space3d_render::DirectionalLight`].
    pub const DIRECTIONAL_LIGHT: &str = "DirectionalLight";
    /// [`space3d_render::PointLight`].
    pub const POINT_LIGHT: &str = "PointLight";
    /// [`space3d_render::Camera3d`] (primary-target cameras only).
    pub const CAMERA: &str = "Camera3d";
    /// [`space3d_render::Visibility`].
    pub const VISIBILITY: &str = "Visibility";
    /// [`super::MeshSource`].
    pub const MESH_SOURCE: &str = "MeshSource";
}

/// Register render components (+ [`MeshSource`]) in a scene registry so the editor can save,
/// load, snapshot and JSON-edit them. Types that are already registered are left alone.
/// `Mesh3d` is snapshot-only (GPU handles are session-local); `MeshSource` is saved instead.
pub fn register_editor_components(r: &mut ComponentRegistry) {
    if !r.is_registered::<Material>() {
        r.register_mapped::<Material, MaterialRepr>(
            names::MATERIAL,
            |m, _| {
                Some(MaterialRepr {
                    base_color: m.base_color.to_array(),
                    metallic: m.metallic,
                    roughness: m.roughness,
                    emissive: m.emissive.to_array(),
                    unlit: m.unlit,
                    double_sided: m.double_sided,
                    alpha_blend: m.alpha_blend,
                })
            },
            |r, _| {
                Some(Material {
                    base_color: r.base_color.into(),
                    base_color_texture: None,
                    metallic: r.metallic,
                    roughness: r.roughness,
                    emissive: r.emissive.into(),
                    unlit: r.unlit,
                    double_sided: r.double_sided,
                    alpha_blend: r.alpha_blend,
                })
            },
        );
    }
    if !r.is_registered::<DirectionalLight>() {
        r.register_mapped::<DirectionalLight, DirectionalLightRepr>(
            names::DIRECTIONAL_LIGHT,
            |l, _| {
                Some(DirectionalLightRepr {
                    color: l.color.to_array(),
                    intensity: l.intensity,
                })
            },
            |r, _| {
                Some(DirectionalLight {
                    color: r.color.into(),
                    intensity: r.intensity,
                })
            },
        );
    }
    if !r.is_registered::<PointLight>() {
        r.register_mapped::<PointLight, PointLightRepr>(
            names::POINT_LIGHT,
            |l, _| {
                Some(PointLightRepr {
                    color: l.color.to_array(),
                    intensity: l.intensity,
                    range: l.range,
                })
            },
            |r, _| {
                Some(PointLight {
                    color: r.color.into(),
                    intensity: r.intensity,
                    range: r.range,
                })
            },
        );
    }
    if !r.is_registered::<Camera3d>() {
        r.register_mapped::<Camera3d, CameraRepr>(
            names::CAMERA,
            |c, _| {
                if c.target != RenderTarget::Primary {
                    return None;
                }
                Some(CameraRepr {
                    projection: match c.projection {
                        Projection::Perspective { fov_y, near, far } => ProjectionRepr::Perspective {
                            fov_y_degrees: fov_y.to_degrees(),
                            near,
                            far,
                        },
                        Projection::Orthographic { height, near, far } => {
                            ProjectionRepr::Orthographic { height, near, far }
                        }
                    },
                    clear_color: c.clear_color.map(|c| c.to_array()),
                    order: c.order,
                    viewport: c.viewport,
                    active: c.active,
                })
            },
            |r, _| {
                Some(Camera3d {
                    projection: match r.projection {
                        ProjectionRepr::Perspective {
                            fov_y_degrees,
                            near,
                            far,
                        } => Projection::Perspective {
                            fov_y: fov_y_degrees.to_radians(),
                            near,
                            far,
                        },
                        ProjectionRepr::Orthographic { height, near, far } => {
                            Projection::Orthographic { height, near, far }
                        }
                    },
                    clear_color: r.clear_color.map(Into::into),
                    target: RenderTarget::Primary,
                    order: r.order,
                    viewport: r.viewport,
                    active: r.active,
                })
            },
        );
    }
    if !r.is_registered::<Visibility>() {
        r.register_mapped::<Visibility, bool>(
            names::VISIBILITY,
            |v, _| Some(*v == Visibility::Visible),
            |b, _| Some(if b { Visibility::Visible } else { Visibility::Hidden }),
        );
    }
    if !r.is_registered::<MeshSource>() {
        r.register::<MeshSource>(names::MESH_SOURCE);
    }
    if !r.is_registered::<Mesh3d>() {
        r.register_snapshot_only::<Mesh3d>("Mesh3d");
    }
}

/// The entity's [`StableId`], inserting a fresh one if it has none.
pub fn ensure_stable_id(world: &mut World, e: Entity) -> Option<StableId> {
    if let Ok(id) = world.get::<&StableId>(e) {
        return Some(*id);
    }
    let id = StableId::new();
    world.insert_one(e, id).ok()?;
    Some(id)
}

/// Entity carrying `id`.
pub fn find_by_id(world: &World, id: StableId) -> Option<Entity> {
    world
        .query::<(Entity, &StableId)>()
        .iter()
        .find(|(_, s)| **s == id)
        .map(|(e, _)| e)
}

/// Serializable registered components of `e` as JSON values (name → value).
///
/// Makes sure `e` and its parent have stable ids first so references can be expressed.
pub fn component_values(
    world: &mut World,
    registry: &ComponentRegistry,
    e: Entity,
) -> Option<std::collections::BTreeMap<String, ComponentValue>> {
    ensure_stable_id(world, e)?;
    if let Ok(p) = world.get::<&Parent>(e).map(|p| p.0)
        && world.contains(p)
    {
        ensure_stable_id(world, p);
    }
    let scene = save_entities(world, registry, &[e]).ok()?;
    scene.entities.into_iter().next().map(|se| se.components)
}

/// Deserialize `value` as the registered component `name` and insert it into `e`
/// (replacing any previous value).
///
/// Entity references are resolved against entities of `world` carrying [`StableId`]s.
pub fn set_component_value(
    world: &mut World,
    registry: &ComponentRegistry,
    e: Entity,
    name: &str,
    value: ComponentValue,
) -> Result<(), String> {
    if !world.contains(e) {
        return Err("entity no longer exists".into());
    }
    if name == space3d_scene::PARENT {
        // References must resolve in the real world; handled without a scratch world.
        let id: StableId = serde_json::from_value(value).map_err(|err| err.to_string())?;
        let parent = find_by_id(world, id).ok_or("parent not found")?;
        if parent == e {
            return Err("an entity cannot be its own parent".into());
        }
        world.insert_one(e, Parent(parent)).map_err(|err| err.to_string())?;
        return Ok(());
    }
    let mut se = SceneEntity::new(StableId::new());
    se.components.insert(name.to_owned(), value);
    let scene = Scene { entities: vec![se] };
    let mut scratch = World::new();
    let spawned = scene
        .spawn_into(&mut scratch, registry)
        .map_err(|err| err.to_string())?;
    let t = spawned[0];
    let _ = scratch.remove_one::<StableId>(t);
    let taken = scratch.take(t).map_err(|err| err.to_string())?;
    world.insert(e, taken).map_err(|err| err.to_string())
}

/// Remove a component by registration name — supported for the core and render types the
/// editor knows (`Name`, `Transform`, `Parent`, `Material`, lights, `Camera3d`,
/// `Visibility`, `MeshSource`, `Mesh3d`).
pub fn remove_component(world: &mut World, e: Entity, name: &str) -> bool {
    fn rm<T: space3d_core::hecs::Component>(w: &mut World, e: Entity) -> bool {
        w.remove_one::<T>(e).is_ok()
    }
    match name {
        "Name" => rm::<Name>(world, e),
        space3d_scene::TRANSFORM => rm::<Transform>(world, e),
        space3d_scene::PARENT => rm::<Parent>(world, e),
        names::MATERIAL => rm::<Material>(world, e),
        names::DIRECTIONAL_LIGHT => rm::<DirectionalLight>(world, e),
        names::POINT_LIGHT => rm::<PointLight>(world, e),
        names::CAMERA => rm::<Camera3d>(world, e),
        names::VISIBILITY => rm::<Visibility>(world, e),
        names::MESH_SOURCE => rm::<MeshSource>(world, e),
        "Mesh3d" => rm::<Mesh3d>(world, e),
        _ => false,
    }
}

/// Whether [`remove_component`] can remove `name`.
pub fn can_remove(name: &str) -> bool {
    matches!(
        name,
        "Name"
            | space3d_scene::TRANSFORM
            | space3d_scene::PARENT
            | names::MATERIAL
            | names::DIRECTIONAL_LIGHT
            | names::POINT_LIGHT
            | names::CAMERA
            | names::VISIBILITY
            | names::MESH_SOURCE
            | "Mesh3d"
    )
}

/// Display name of an entity: its [`Name`], else a description of what it is.
pub fn display_name(world: &World, e: Entity) -> String {
    if let Ok(n) = world.get::<&Name>(e) {
        return n.0.clone();
    }
    let kind = if world.satisfies::<&Camera3d>(e) {
        "Camera"
    } else if world.satisfies::<&DirectionalLight>(e) {
        "Directional light"
    } else if world.satisfies::<&PointLight>(e) {
        "Point light"
    } else if world.satisfies::<&Mesh3d>(e) {
        "Mesh"
    } else {
        "Entity"
    };
    format!("{kind} #{}", e.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::glam::Vec4;

    fn registry() -> ComponentRegistry {
        let mut r = ComponentRegistry::default();
        register_editor_components(&mut r);
        r
    }

    #[test]
    fn json_roundtrip_of_render_components() {
        let r = registry();
        let mut w = World::new();
        let e = w.spawn((
            Name("cube".into()),
            Transform::from_xyz(1.0, 2.0, 3.0),
            Material::color(Vec4::new(0.5, 0.25, 1.0, 1.0)),
            MeshSource::Cube { size: 2.0 },
            Visibility::Hidden,
        ));
        let values = component_values(&mut w, &r, e).unwrap();
        for k in ["Name", "Transform", "Material", "MeshSource", "Visibility"] {
            assert!(values.contains_key(k), "{k} missing from {values:?}");
        }
        let mut mat = values["Material"].clone();
        mat["metallic"] = serde_json::json!(0.75);
        set_component_value(&mut w, &r, e, "Material", mat).unwrap();
        assert_eq!(w.get::<&Material>(e).unwrap().metallic, 0.75);
        assert_eq!(w.get::<&Name>(e).unwrap().0, "cube", "other components untouched");
        assert!(set_component_value(&mut w, &r, e, "Material", serde_json::json!({"bad": 1})).is_err());
        assert!(remove_component(&mut w, e, "Visibility"));
        assert!(!w.satisfies::<&Visibility>(e));
    }

    #[test]
    fn parent_references_resolve_in_the_real_world() {
        let r = registry();
        let mut w = World::new();
        let a = w.spawn((Name("a".into()),));
        let b = w.spawn((Name("b".into()),));
        let id = ensure_stable_id(&mut w, a).unwrap();
        set_component_value(&mut w, &r, b, "Parent", serde_json::to_value(id).unwrap()).unwrap();
        assert_eq!(w.get::<&Parent>(b).unwrap().0, a);
        let values = component_values(&mut w, &r, b).unwrap();
        assert_eq!(values["Parent"], serde_json::to_value(id).unwrap());
        assert!(set_component_value(&mut w, &r, a, "Parent", serde_json::to_value(id).unwrap()).is_err());
    }
}
