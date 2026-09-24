//! Scene-level editor operations: add/delete/duplicate entities, new/save/load scenes and the
//! [`MeshSource`] → `Mesh3d` resolver.
//!
//! GPU handles (`Mesh3d`, a material's `base_color_texture`) are session-local and never
//! written to scene files; entities rebuild their mesh from a [`MeshSource`]. Entities whose
//! mesh has no `MeshSource` (meshes a game created with `RenderAssets::add_mesh`) and material
//! textures keep their handles across a [`load_scene`] in the same session — matched by
//! [`StableId`] — but a scene file opened in a new session cannot restore them
//! ([`session_only_render_state`] counts such entities).

use crate::components::{EditorInternal, MeshSource, ensure_stable_id};
use crate::history::{EditCommand, EditorHistory, EntitySnapshot};
use space3d_core::glam::{Vec3, Vec4};
use space3d_core::hecs::{Entity, World};
use space3d_core::{Name, Parent, SystemCtx, Transform};
use space3d_render::{
    Camera3d, DirectionalLight, Material, Mesh3d, MeshHandle, PointLight, RenderAssets, TextureHandle,
};
use space3d_scene::{ComponentRegistry, Scene, SceneError, StableId, save_entities, save_world};
use std::collections::{HashMap, HashSet};

/// Things the "Add" menu can create.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AddKind {
    /// Entity with only a name + transform.
    Empty,
    /// Unit cube.
    Cube,
    /// Sphere.
    Sphere,
    /// Plane.
    Plane,
    /// Sun light.
    DirectionalLight,
    /// Point light.
    PointLight,
    /// Camera.
    Camera,
}

impl AddKind {
    /// All kinds, in menu order.
    pub const ALL: [AddKind; 7] = [
        AddKind::Empty,
        AddKind::Cube,
        AddKind::Sphere,
        AddKind::Plane,
        AddKind::DirectionalLight,
        AddKind::PointLight,
        AddKind::Camera,
    ];

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            AddKind::Empty => "Empty",
            AddKind::Cube => "Cube",
            AddKind::Sphere => "Sphere",
            AddKind::Plane => "Plane",
            AddKind::DirectionalLight => "Directional light",
            AddKind::PointLight => "Point light",
            AddKind::Camera => "Camera",
        }
    }
}

/// Spawn a new entity of `kind` (undoable). Returns it.
pub fn add_entity(sys: &mut SystemCtx, history: &mut EditorHistory, kind: AddKind, parent: Option<Entity>) -> Entity {
    let name = Name(kind.label().to_owned());
    let t = Transform::IDENTITY;
    let e = match kind {
        AddKind::Empty => sys.world.spawn((name, t)),
        AddKind::Cube | AddKind::Sphere | AddKind::Plane => {
            let source = match kind {
                AddKind::Cube => MeshSource::Cube { size: 1.0 },
                AddKind::Sphere => MeshSource::Sphere {
                    radius: 0.5,
                    sectors: 32,
                    stacks: 16,
                },
                _ => MeshSource::Plane { size: 4.0 },
            };
            sys.world.spawn((name, t, source, Material::srgb(0.8, 0.8, 0.8)))
        }
        AddKind::DirectionalLight => sys.world.spawn((
            name,
            Transform::from_xyz(0.0, 4.0, 0.0).looking_at(Vec3::new(-0.4, -1.0, -0.6), Vec3::Y),
            DirectionalLight::default(),
        )),
        AddKind::PointLight => sys
            .world
            .spawn((name, Transform::from_xyz(0.0, 2.0, 0.0), PointLight::default())),
        AddKind::Camera => sys.world.spawn((
            name,
            Transform::from_xyz(0.0, 2.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y),
            Camera3d::default(),
        )),
    };
    if let Some(p) = parent.filter(|p| sys.world.contains(*p)) {
        ensure_stable_id(sys.world, p);
        let _ = sys.world.insert_one(e, Parent(p));
    }
    resolve_mesh_sources(sys.world, sys.resources.get_mut::<RenderAssets>());
    record_spawn(sys, history, &format!("Add {}", kind.label()), &[e]);
    e
}

/// Record already spawned entities as one undoable "spawn" step.
pub fn record_spawn(sys: &mut SystemCtx, history: &mut EditorHistory, label: &str, entities: &[Entity]) {
    let Some(registry) = sys.resources.get::<ComponentRegistry>() else {
        return;
    };
    let snaps: Vec<EntitySnapshot> = entities
        .iter()
        .filter_map(|&e| EntitySnapshot::capture(sys.world, registry, e))
        .collect();
    history.push(EditCommand::spawned(label, snaps));
}

/// `e` and all its descendants (parents first).
pub fn subtree(world: &World, e: Entity) -> Vec<Entity> {
    let links: Vec<(Entity, Entity)> = world
        .query::<(Entity, &Parent)>()
        .iter()
        .map(|(c, p)| (c, p.0))
        .collect();
    let mut out = vec![e];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i];
        for &(c, p) in &links {
            if p == cur && !out.contains(&c) {
                out.push(c);
            }
        }
        i += 1;
    }
    out
}

/// Delete `e` and its descendants (undoable).
pub fn delete_entity(sys: &mut SystemCtx, history: &mut EditorHistory, e: Entity) {
    if !sys.world.contains(e) {
        return;
    }
    let Some(registry) = sys.resources.get::<ComponentRegistry>() else {
        return;
    };
    let entities = subtree(sys.world, e);
    let snaps: Vec<EntitySnapshot> = entities
        .iter()
        .filter_map(|&x| EntitySnapshot::capture(sys.world, registry, x))
        .collect();
    let label = format!("Delete {}", crate::components::display_name(sys.world, e));
    history.execute(EditCommand::despawned(label, snaps), sys.world, registry);
}

/// Duplicate `e` (with descendants, fresh ids, render components shared). Returns the copy.
pub fn duplicate_entity(sys: &mut SystemCtx, history: &mut EditorHistory, e: Entity) -> Option<Entity> {
    if !sys.world.contains(e) {
        return None;
    }
    let registry = sys.resources.get::<ComponentRegistry>()?;
    let entities = subtree(sys.world, e);
    for &x in &entities {
        ensure_stable_id(sys.world, x);
    }
    let scene = save_entities(sys.world, registry, &entities).ok()?;
    let spawned = scene
        .spawn_with(
            sys.world,
            registry,
            &space3d_scene::SpawnOptions {
                fresh_ids: true,
                root_transform: None,
            },
        )
        .ok()?;
    // Copy session-local render components. Copies with a `MeshSource` get a mesh of their
    // own (below): the editor rebuilds that mesh in place when the source is edited.
    for (&src, &dst) in entities.iter().zip(&spawned) {
        if !sys.world.satisfies::<&MeshSource>(src)
            && let Ok(m) = sys.world.get::<&Mesh3d>(src).map(|m| *m)
        {
            let _ = sys.world.insert_one(dst, m);
        }
        if let Ok(m) = sys.world.get::<&Material>(src).map(|m| (*m).clone()) {
            let _ = sys.world.insert_one(dst, m);
        }
    }
    resolve_mesh_sources(sys.world, sys.resources.get_mut::<RenderAssets>());
    // The duplicated root keeps the original's parent (it was outside the saved set).
    if let Ok(p) = sys.world.get::<&Parent>(e).map(|p| p.0) {
        let _ = sys.world.insert_one(spawned[0], Parent(p));
    }
    if let Ok(mut n) = sys.world.get::<&mut Name>(spawned[0]) {
        n.0.push_str(" copy");
    }
    record_spawn(sys, history, "Duplicate", &spawned);
    Some(spawned[0])
}

/// Entities that belong to the edited scene: have at least one serializable registered
/// component and are not [`EditorInternal`]. Assigns [`space3d_scene::StableId`]s to entities
/// that have none (so references between them can be saved).
pub fn scene_entities(world: &mut World, registry: &ComponentRegistry) -> Vec<Entity> {
    let candidates: Vec<Entity> = world
        .iter()
        .map(|e| e.entity())
        .filter(|&e| !world.satisfies::<&EditorInternal>(e))
        .collect();
    for &e in &candidates {
        ensure_stable_id(world, e);
    }
    let Ok(scene) = save_world(world, registry) else {
        return Vec::new();
    };
    let ids: HashSet<StableId> = scene.entities.iter().map(|se| se.id).collect();
    let mut out: Vec<Entity> = world
        .query::<(Entity, &StableId)>()
        .without::<&EditorInternal>()
        .iter()
        .filter(|(_, id)| ids.contains(id))
        .map(|(e, _)| e)
        .collect();
    out.sort_unstable_by_key(|e| (e.id(), e.to_bits()));
    out
}

/// Serialize the edited scene (editor-internal entities excluded).
pub fn save_scene(world: &mut World, registry: &ComponentRegistry) -> Result<Scene, SceneError> {
    let entities = scene_entities(world, registry);
    for &e in &entities {
        ensure_stable_id(world, e);
    }
    save_entities(world, registry, &entities)
}

/// Render state of a scene entity that scene files cannot carry.
#[derive(Clone, Copy, Debug, Default)]
struct SessionLocal {
    /// `Mesh3d` of an entity without a [`MeshSource`].
    mesh: Option<Mesh3d>,
    /// The material's base-color texture.
    texture: Option<TextureHandle>,
}

fn session_local(world: &World, e: Entity) -> SessionLocal {
    let mesh = if world.satisfies::<&MeshSource>(e) {
        None
    } else {
        world.get::<&Mesh3d>(e).ok().map(|m| *m)
    };
    let texture = world.get::<&Material>(e).ok().and_then(|m| m.base_color_texture);
    SessionLocal { mesh, texture }
}

/// Number of scene entities whose mesh (a `Mesh3d` without [`MeshSource`]) or material
/// texture is not stored in scene files: they survive [`load_scene`] in this session, but not
/// opening the file in a new one.
pub fn session_only_render_state(world: &mut World, registry: &ComponentRegistry) -> usize {
    scene_entities(world, registry)
        .into_iter()
        .filter(|&e| {
            let l = session_local(world, e);
            l.mesh.is_some() || l.texture.is_some()
        })
        .count()
}

/// Despawn the scene entities. Returns the session-local render state by stable id and the
/// editor-created meshes (those of [`MeshSource`] entities) that were in use.
fn despawn_scene(
    world: &mut World,
    registry: &ComponentRegistry,
) -> (HashMap<StableId, SessionLocal>, Vec<MeshHandle>) {
    let entities = scene_entities(world, registry);
    let mut local = HashMap::new();
    let mut editor_meshes = Vec::new();
    for &e in &entities {
        let l = session_local(world, e);
        if (l.mesh.is_some() || l.texture.is_some())
            && let Ok(id) = world.get::<&StableId>(e).map(|id| *id)
        {
            local.insert(id, l);
        }
        if world.satisfies::<&MeshSource>(e)
            && let Ok(m) = world.get::<&Mesh3d>(e)
        {
            editor_meshes.push(m.0);
        }
    }
    for e in entities {
        let _ = world.despawn(e);
    }
    (local, editor_meshes)
}

/// Free the meshes in `candidates` that no entity uses any more.
fn free_unused_meshes(world: &World, assets: &mut RenderAssets, candidates: Vec<MeshHandle>) {
    if candidates.is_empty() {
        return;
    }
    let used: HashSet<MeshHandle> = world.query::<&Mesh3d>().iter().map(|m| m.0).collect();
    let mut freed = HashSet::new();
    for h in candidates {
        if !used.contains(&h) && freed.insert(h) {
            assets.remove_mesh(h);
        }
    }
}

/// Replace the edited scene with `scene` (despawns current scene entities).
///
/// Session-local render state is carried over to loaded entities with the same
/// [`StableId`]: the `Mesh3d` of entities without a [`MeshSource`] (which the file cannot
/// describe) and material textures. [`MeshSource`] entities get fresh meshes, and the meshes
/// the editor created for the replaced entities are freed once nothing uses them — so clear
/// any [`EditorHistory`] that could re-create the old entities.
pub fn load_scene(
    world: &mut World,
    registry: &ComponentRegistry,
    mut assets: Option<&mut RenderAssets>,
    scene: &Scene,
) -> Result<Vec<Entity>, SceneError> {
    let (local, editor_meshes) = despawn_scene(world, registry);
    let result = scene.spawn_into(world, registry);
    if let Ok(spawned) = &result {
        for &e in spawned {
            let Some(l) = world.get::<&StableId>(e).ok().and_then(|id| local.get(&*id).copied()) else {
                continue;
            };
            if let Some(m) = l.mesh
                && !world.satisfies::<&MeshSource>(e)
                && !world.satisfies::<&Mesh3d>(e)
            {
                let _ = world.insert_one(e, m);
            }
            if let Some(t) = l.texture
                && let Ok(mut mat) = world.get::<&mut Material>(e)
                && mat.base_color_texture.is_none()
            {
                mat.base_color_texture = Some(t);
            }
        }
    }
    resolve_mesh_sources(world, assets.as_deref_mut());
    if let Some(assets) = assets {
        free_unused_meshes(world, assets, editor_meshes);
    }
    result
}

/// Despawn all scene entities (see [`scene_entities`]) and free the meshes the editor created
/// for them (from [`MeshSource`]s) once unused.
pub fn clear_scene(world: &mut World, registry: &ComponentRegistry, assets: Option<&mut RenderAssets>) {
    let (_, editor_meshes) = despawn_scene(world, registry);
    if let Some(assets) = assets {
        free_unused_meshes(world, assets, editor_meshes);
    }
}

/// Give every entity with a [`MeshSource`] but no `Mesh3d` its mesh (and a default material).
pub fn resolve_mesh_sources(world: &mut World, assets: Option<&mut RenderAssets>) {
    let Some(assets) = assets else { return };
    let pending: Vec<(Entity, MeshSource, bool)> = world
        .query::<(Entity, &MeshSource, Option<&Mesh3d>, Option<&Material>)>()
        .iter()
        .filter(|(_, _, m, _)| m.is_none())
        .map(|(e, s, _, mat)| (e, s.clone(), mat.is_some()))
        .collect();
    for (e, source, has_material) in pending {
        let handle = assets.add_mesh(source.mesh());
        let _ = world.insert_one(e, Mesh3d(handle));
        if !has_material {
            let _ = world.insert_one(e, Material::color(Vec4::new(0.8, 0.8, 0.8, 1.0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::App;

    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<RenderAssets>();
        let mut r = ComponentRegistry::default();
        crate::components::register_editor_components(&mut r);
        app.insert_resource(r);
        app
    }

    fn with_ctx<R>(app: &mut App, f: impl FnOnce(&mut SystemCtx) -> R) -> R {
        let mut ctx = SystemCtx {
            world: &mut app.world,
            resources: &mut app.resources,
        };
        f(&mut ctx)
    }

    #[test]
    fn add_delete_duplicate_with_undo() {
        let mut app = app();
        let mut h = EditorHistory::default();
        let cube = with_ctx(&mut app, |c| add_entity(c, &mut h, AddKind::Cube, None));
        assert!(app.world.satisfies::<&Mesh3d>(cube), "mesh resolved from MeshSource");
        let child = with_ctx(&mut app, |c| add_entity(c, &mut h, AddKind::PointLight, Some(cube)));
        assert_eq!(app.world.get::<&Parent>(child).unwrap().0, cube);
        let copy = with_ctx(&mut app, |c| duplicate_entity(c, &mut h, cube)).unwrap();
        assert_eq!(app.world.get::<&Name>(copy).unwrap().0, "Cube copy");
        assert_ne!(
            app.world.get::<&Mesh3d>(copy).unwrap().0,
            app.world.get::<&Mesh3d>(cube).unwrap().0,
            "MeshSource copies own their mesh (edited independently)"
        );
        assert_eq!(app.world.len(), 4, "cube + light + copies of both");
        with_ctx(&mut app, |c| delete_entity(c, &mut h, cube));
        assert_eq!(app.world.len(), 2);
        let reg = app.resources.remove::<ComponentRegistry>().unwrap();
        h.undo(&mut app.world, &reg);
        assert_eq!(app.world.len(), 4);
        h.undo(&mut app.world, &reg);
        assert_eq!(app.world.len(), 2, "duplicate undone");
    }

    #[test]
    fn save_load_roundtrip_excludes_internal_entities() {
        let mut app = app();
        let mut h = EditorHistory::default();
        with_ctx(&mut app, |c| add_entity(c, &mut h, AddKind::Sphere, None));
        with_ctx(&mut app, |c| add_entity(c, &mut h, AddKind::Camera, None));
        app.world
            .spawn((EditorInternal, Name("preview".into()), Transform::IDENTITY));
        let reg = app.resources.remove::<ComponentRegistry>().unwrap();
        let scene = save_scene(&mut app.world, &reg).unwrap();
        assert_eq!(scene.len(), 2);
        let ron = scene.to_ron().unwrap();
        assert!(ron.contains("MeshSource") && ron.contains("Camera3d"));
        let parsed = Scene::from_ron(&ron).unwrap();
        let mut assets = app.resources.remove::<RenderAssets>().unwrap();
        let spawned = load_scene(&mut app.world, &reg, Some(&mut assets), &parsed).unwrap();
        assert_eq!(spawned.len(), 2);
        assert_eq!(app.world.len(), 3, "internal entity kept, scene replaced");
        assert!(spawned.iter().any(|&e| app.world.satisfies::<&Mesh3d>(e)));
    }

    #[test]
    fn load_keeps_session_meshes_and_textures_and_frees_editor_meshes() {
        let mut app = app();
        let mut h = EditorHistory::default();
        let sphere = with_ctx(&mut app, |c| add_entity(c, &mut h, AddKind::Sphere, None));
        let old_sphere_mesh = app.world.get::<&Mesh3d>(sphere).unwrap().0;
        // A game mesh without MeshSource, with a textured material.
        let mut assets = app.resources.remove::<RenderAssets>().unwrap();
        let game_mesh = assets.add_mesh(space3d_core::MeshData::cube(2.0));
        let texture = assets.add_image(space3d_core::ImageData::filled(2, 2, [255, 0, 0, 255]));
        let mut mat = Material::srgb(1.0, 1.0, 1.0);
        mat.base_color_texture = Some(texture);
        app.world
            .spawn((Name("Game mesh".into()), Transform::IDENTITY, Mesh3d(game_mesh), mat));
        let reg = app.resources.remove::<ComponentRegistry>().unwrap();
        assert_eq!(session_only_render_state(&mut app.world, &reg), 1);

        let scene = Scene::from_ron(&save_scene(&mut app.world, &reg).unwrap().to_ron().unwrap()).unwrap();
        let spawned = load_scene(&mut app.world, &reg, Some(&mut assets), &scene).unwrap();
        let loaded_game = spawned
            .iter()
            .copied()
            .find(|&e| app.world.get::<&Name>(e).is_ok_and(|n| n.0 == "Game mesh"))
            .unwrap();
        assert_eq!(app.world.get::<&Mesh3d>(loaded_game).unwrap().0, game_mesh, "mesh kept");
        let kept = app.world.get::<&Material>(loaded_game).unwrap().base_color_texture;
        assert_eq!(kept, Some(texture), "texture kept");
        assert!(assets.mesh_aabb(game_mesh).is_some(), "game mesh never freed");

        let loaded_sphere = spawned
            .iter()
            .copied()
            .find(|&e| app.world.satisfies::<&MeshSource>(e))
            .unwrap();
        let new_sphere_mesh = app.world.get::<&Mesh3d>(loaded_sphere).unwrap().0;
        assert_ne!(new_sphere_mesh, old_sphere_mesh);
        assert!(
            assets.mesh_aabb(old_sphere_mesh).is_none(),
            "replaced editor mesh freed"
        );
        assert!(assets.mesh_aabb(new_sphere_mesh).is_some());

        // New scene: editor meshes freed, game meshes left to the game.
        clear_scene(&mut app.world, &reg, Some(&mut assets));
        assert_eq!(app.world.len(), 0);
        assert!(assets.mesh_aabb(new_sphere_mesh).is_none());
        assert!(assets.mesh_aabb(game_mesh).is_some());
    }
}
