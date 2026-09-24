//! Undo/redo of editor operations ([`EditorHistory`], [`EditCommand`]).
//!
//! Commands address entities by [`StableId`] rather than `Entity`, so they stay valid when an
//! undo re-creates a deleted entity (with a new handle but the same stable id).

use crate::components::{ensure_stable_id, find_by_id, remove_component, set_component_value};
use space3d_core::Parent;
use space3d_core::hecs::{Component, Entity, World};
use space3d_render::{Camera3d, Material, Mesh3d};
use space3d_scene::{ComponentRegistry, ComponentValue, Scene, SceneEntity, StableId, save_entities};

type Apply = Box<dyn Fn(&mut World, &ComponentRegistry) + Send + Sync>;

/// One undoable operation: closures that redo and undo it.
pub struct EditCommand {
    label: String,
    undo: Apply,
    redo: Apply,
    merge_key: Option<String>,
}

impl std::fmt::Debug for EditCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditCommand").field("label", &self.label).finish()
    }
}

impl EditCommand {
    /// Command from raw closures.
    pub fn new(
        label: impl Into<String>,
        undo: impl Fn(&mut World, &ComponentRegistry) + Send + Sync + 'static,
        redo: impl Fn(&mut World, &ComponentRegistry) + Send + Sync + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            undo: Box::new(undo),
            redo: Box::new(redo),
            merge_key: None,
        }
    }

    /// Consecutive commands with the same key merge into one undo step (e.g. all frames of a
    /// slider drag) until [`EditorHistory::end_merge`].
    pub fn merge_key(mut self, key: impl Into<String>) -> Self {
        self.merge_key = Some(key.into());
        self
    }

    /// Label shown in menus.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Set (or remove, with `None`) a typed component on the entity with stable id `id`.
    pub fn set<T: Component + Clone>(
        label: impl Into<String>,
        id: StableId,
        before: Option<T>,
        after: Option<T>,
    ) -> Self {
        fn apply<T: Component + Clone>(w: &mut World, id: StableId, v: &Option<T>) {
            let Some(e) = find_by_id(w, id) else { return };
            match v {
                Some(v) => {
                    let _ = w.insert_one(e, v.clone());
                }
                None => {
                    let _ = w.remove_one::<T>(e);
                }
            }
        }
        Self::new(
            label,
            move |w, _| apply(w, id, &before),
            move |w, _| apply(w, id, &after),
        )
    }

    /// Set (or remove) a registered component through its JSON value.
    pub fn set_json(
        label: impl Into<String>,
        id: StableId,
        name: impl Into<String>,
        before: Option<ComponentValue>,
        after: Option<ComponentValue>,
    ) -> Self {
        let name: String = name.into();
        let n2 = name.clone();
        let apply = move |w: &mut World, r: &ComponentRegistry, name: &str, v: &Option<ComponentValue>| {
            let Some(e) = find_by_id(w, id) else { return };
            let result = match v {
                Some(v) => set_component_value(w, r, e, name, v.clone()),
                None => {
                    remove_component(w, e, name);
                    Ok(())
                }
            };
            if let Err(err) = result {
                tracing::warn!("editor: could not apply {name}: {err}");
            }
        };
        let apply2 = apply;
        Self::new(
            label,
            move |w, r| apply(w, r, &name, &before),
            move |w, r| apply2(w, r, &n2, &after),
        )
    }

    /// Change the parent of `id` (`None` = root).
    pub fn reparent(id: StableId, before: Option<StableId>, after: Option<StableId>) -> Self {
        fn apply(w: &mut World, id: StableId, parent: Option<StableId>) {
            let Some(e) = find_by_id(w, id) else { return };
            match parent.and_then(|p| find_by_id(w, p)) {
                Some(p) => {
                    let _ = w.insert_one(e, Parent(p));
                }
                None => {
                    let _ = w.remove_one::<Parent>(e);
                }
            }
        }
        Self::new(
            "Reparent",
            move |w, _| apply(w, id, before),
            move |w, _| apply(w, id, after),
        )
    }

    /// Entities that were just spawned (redo re-spawns them, undo despawns them).
    pub fn spawned(label: impl Into<String>, snapshots: Vec<EntitySnapshot>) -> Self {
        let s2 = snapshots.clone();
        Self::new(
            label,
            move |w, _| EntitySnapshot::despawn_all(w, &snapshots),
            move |w, r| EntitySnapshot::restore_all(w, r, &s2),
        )
    }

    /// Entities that were just despawned (undo re-spawns them).
    pub fn despawned(label: impl Into<String>, snapshots: Vec<EntitySnapshot>) -> Self {
        let s2 = snapshots.clone();
        Self::new(
            label,
            move |w, r| EntitySnapshot::restore_all(w, r, &snapshots),
            move |w, _| EntitySnapshot::despawn_all(w, &s2),
        )
    }
}

/// Everything needed to re-create an entity: its serializable registered components plus the
/// session-local render components (`Mesh3d`, full `Material`, full `Camera3d`).
#[derive(Clone, Debug)]
pub struct EntitySnapshot {
    /// Registered components + stable id.
    pub scene: SceneEntity,
    mesh: Option<Mesh3d>,
    material: Option<Material>,
    camera: Option<Camera3d>,
}

impl EntitySnapshot {
    /// Capture `e` (assigning a stable id if needed).
    pub fn capture(world: &mut World, registry: &ComponentRegistry, e: Entity) -> Option<Self> {
        ensure_stable_id(world, e)?;
        if let Ok(p) = world.get::<&Parent>(e).map(|p| p.0) {
            ensure_stable_id(world, p);
        }
        let scene = save_entities(world, registry, &[e]).ok()?.entities.pop()?;
        Some(Self {
            scene,
            mesh: world.get::<&Mesh3d>(e).ok().map(|m| *m),
            material: world.get::<&Material>(e).ok().map(|m| (*m).clone()),
            camera: world.get::<&Camera3d>(e).ok().map(|c| *c),
        })
    }

    /// Stable id of the captured entity.
    pub fn id(&self) -> StableId {
        self.scene.id
    }

    fn restore_all(world: &mut World, registry: &ComponentRegistry, all: &[EntitySnapshot]) {
        let scene = Scene {
            entities: all.iter().map(|s| s.scene.clone()).collect(),
        };
        match scene.spawn_into(world, registry) {
            Ok(spawned) => {
                for (e, s) in spawned.into_iter().zip(all) {
                    if let Some(m) = s.mesh {
                        let _ = world.insert_one(e, m);
                    }
                    if let Some(m) = &s.material {
                        let _ = world.insert_one(e, m.clone());
                    }
                    if let Some(c) = s.camera {
                        let _ = world.insert_one(e, c);
                    }
                }
            }
            Err(err) => tracing::warn!("editor: could not restore entities: {err}"),
        }
    }

    fn despawn_all(world: &mut World, all: &[EntitySnapshot]) {
        for s in all {
            if let Some(e) = find_by_id(world, s.id()) {
                let _ = world.despawn(e);
            }
        }
    }
}

/// Undo/redo stacks (resource inserted by `EditorPlugin`).
pub struct EditorHistory {
    undo: Vec<EditCommand>,
    redo: Vec<EditCommand>,
    max_steps: usize,
    merging: bool,
}

impl Default for EditorHistory {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            max_steps: 200,
            merging: false,
        }
    }
}

impl std::fmt::Debug for EditorHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorHistory")
            .field("undo", &self.undo.len())
            .field("redo", &self.redo.len())
            .finish()
    }
}

impl EditorHistory {
    /// Record a command whose effect has already been applied.
    pub fn push(&mut self, cmd: EditCommand) {
        self.redo.clear();
        if self.merging
            && cmd.merge_key.is_some()
            && let Some(last) = self.undo.last_mut()
            && last.merge_key == cmd.merge_key
        {
            last.redo = cmd.redo;
            return;
        }
        self.merging = cmd.merge_key.is_some();
        self.undo.push(cmd);
        if self.undo.len() > self.max_steps {
            self.undo.remove(0);
        }
    }

    /// Apply a command and record it.
    pub fn execute(&mut self, cmd: EditCommand, world: &mut World, registry: &ComponentRegistry) {
        (cmd.redo)(world, registry);
        self.push(cmd);
    }

    /// Stop merging into the last command (call when a drag/edit gesture ends).
    pub fn end_merge(&mut self) {
        self.merging = false;
    }

    /// Undo the last command; returns its label.
    pub fn undo(&mut self, world: &mut World, registry: &ComponentRegistry) -> Option<String> {
        self.merging = false;
        let cmd = self.undo.pop()?;
        (cmd.undo)(world, registry);
        let label = cmd.label.clone();
        self.redo.push(cmd);
        Some(label)
    }

    /// Redo the last undone command; returns its label.
    pub fn redo(&mut self, world: &mut World, registry: &ComponentRegistry) -> Option<String> {
        self.merging = false;
        let cmd = self.redo.pop()?;
        (cmd.redo)(world, registry);
        let label = cmd.label.clone();
        self.undo.push(cmd);
        Some(label)
    }

    /// Something to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Something to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Label of the next undo.
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(EditCommand::label)
    }

    /// Label of the next redo.
    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(EditCommand::label)
    }

    /// Number of undo steps.
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Forget everything (e.g. after loading a scene).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.merging = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::{Name, Transform};

    fn registry() -> ComponentRegistry {
        let mut r = ComponentRegistry::default();
        crate::components::register_editor_components(&mut r);
        r
    }

    #[test]
    fn typed_set_merges_and_undoes() {
        let r = registry();
        let mut w = World::new();
        let e = w.spawn((Transform::IDENTITY,));
        let id = ensure_stable_id(&mut w, e).unwrap();
        let mut h = EditorHistory::default();
        let mut prev = Transform::IDENTITY;
        for x in 1..=5 {
            let next = Transform::from_xyz(x as f32, 0.0, 0.0);
            h.execute(
                EditCommand::set("Move", id, Some(prev), Some(next)).merge_key("move"),
                &mut w,
                &r,
            );
            prev = next;
        }
        h.end_merge();
        assert_eq!(h.undo_len(), 1, "drag frames merged");
        assert_eq!(w.get::<&Transform>(e).unwrap().translation.x, 5.0);
        assert_eq!(h.undo(&mut w, &r).as_deref(), Some("Move"));
        assert_eq!(w.get::<&Transform>(e).unwrap().translation.x, 0.0);
        h.redo(&mut w, &r);
        assert_eq!(w.get::<&Transform>(e).unwrap().translation.x, 5.0);
    }

    #[test]
    fn delete_and_undo_restores_hierarchy_and_render_components() {
        let r = registry();
        let mut w = World::new();
        let parent = w.spawn((Name("parent".into()), Transform::IDENTITY));
        let child = w.spawn((
            Name("child".into()),
            Transform::IDENTITY,
            Parent(parent),
            Material::default(),
        ));
        let snaps: Vec<_> = [parent, child]
            .into_iter()
            .map(|e| EntitySnapshot::capture(&mut w, &r, e).unwrap())
            .collect();
        let (pid, cid) = (snaps[0].id(), snaps[1].id());
        let mut h = EditorHistory::default();
        h.execute(EditCommand::despawned("Delete", snaps), &mut w, &r);
        assert_eq!(w.len(), 0);
        h.undo(&mut w, &r);
        let p = find_by_id(&w, pid).unwrap();
        let c = find_by_id(&w, cid).unwrap();
        assert_eq!(w.get::<&Parent>(c).unwrap().0, p);
        assert!(w.satisfies::<&Material>(c));
        assert_eq!(w.get::<&Name>(p).unwrap().0, "parent");
        h.redo(&mut w, &r);
        assert_eq!(w.len(), 0);
    }

    #[test]
    fn json_and_reparent_commands() {
        let r = registry();
        let mut w = World::new();
        let a = w.spawn((Name("a".into()),));
        let b = w.spawn((Name("b".into()),));
        let (ida, idb) = (
            ensure_stable_id(&mut w, a).unwrap(),
            ensure_stable_id(&mut w, b).unwrap(),
        );
        let mut h = EditorHistory::default();
        h.execute(
            EditCommand::set_json(
                "Rename",
                idb,
                "Name",
                Some(serde_json::json!("b")),
                Some(serde_json::json!("bee")),
            ),
            &mut w,
            &r,
        );
        assert_eq!(w.get::<&Name>(b).unwrap().0, "bee");
        h.execute(EditCommand::reparent(idb, None, Some(ida)), &mut w, &r);
        assert_eq!(w.get::<&Parent>(b).unwrap().0, a);
        h.undo(&mut w, &r);
        assert!(!w.satisfies::<&Parent>(b));
        h.undo(&mut w, &r);
        assert_eq!(w.get::<&Name>(b).unwrap().0, "b");
        assert!(!h.can_undo() && h.can_redo());
    }
}
