//! In-memory [`WorldSnapshot`]s for tests, rollback and deterministic replays.
use crate::registry::ComponentRegistry;
use space3d_core::hecs::{Entity, EntityBuilder, World};
use std::any::{Any, TypeId};
use std::collections::HashSet;

type Boxed = Box<dyn Any + Send + Sync>;

/// Exact copy of every entity id plus all registered components (serializable *and*
/// snapshot-only ones), captured by cloning — no serialization, no float round-off.
///
/// [`WorldSnapshot::restore`] brings the world back to the captured state **in place**:
/// the same `Entity` handles are reused, so references held elsewhere (resources, other
/// components) stay valid. Entities spawned after the capture are despawned, despawned
/// ones are re-created with their original handle, and registered components are
/// overwritten/removed to match. Unregistered components of surviving entities are left
/// untouched (and are lost for re-created entities).
pub struct WorldSnapshot {
    entities: Vec<(Entity, Vec<(TypeId, Boxed)>)>,
}

impl WorldSnapshot {
    /// Capture the world using `registry` to decide which components to copy.
    pub fn capture(world: &World, registry: &ComponentRegistry) -> Self {
        let mut ids: Vec<Entity> = world.iter().map(|e| e.entity()).collect();
        ids.sort_unstable_by_key(|e| (e.id(), e.to_bits()));
        let entities = ids
            .into_iter()
            .map(|e| {
                let comps = registry
                    .entries
                    .iter()
                    .filter_map(|en| (en.clone_out)(world, e).map(|v| (en.type_id, v)))
                    .collect();
                (e, comps)
            })
            .collect();
        Self { entities }
    }

    /// Restore the captured state (see type docs). Use the same registry as for capture.
    pub fn restore(&self, world: &mut World, registry: &ComponentRegistry) {
        let keep: HashSet<Entity> = self.entities.iter().map(|(e, _)| *e).collect();
        let extra: Vec<Entity> = world.iter().map(|e| e.entity()).filter(|e| !keep.contains(e)).collect();
        for e in extra {
            let _ = world.despawn(e);
        }
        for (e, comps) in &self.entities {
            let e = *e;
            if !world.contains(e) {
                world.spawn_at(e, ());
            }
            let present: HashSet<TypeId> = comps.iter().map(|(t, _)| *t).collect();
            for entry in &registry.entries {
                if !present.contains(&entry.type_id) && (entry.has)(world, e) {
                    (entry.remove)(world, e);
                }
            }
            let mut b = EntityBuilder::new();
            for (t, v) in comps {
                if let Some(entry) = registry.by_type(*t) {
                    (entry.add_boxed)(v, &mut b);
                }
            }
            world.insert(e, b.build()).expect("entity exists");
        }
    }

    /// Number of captured entities.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// `true` if the world was empty.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Whether `entity` was alive at capture time.
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.iter().any(|(e, _)| *e == entity)
    }
}
