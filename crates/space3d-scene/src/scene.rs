//! [`Scene`] data, RON/JSON (de)serialization, [`save_world`] and [`Prefab`]s.
use crate::error::SceneError;
use crate::registry::{ComponentRegistry, ComponentValue, LoadContext, PARENT, SaveContext, StableId};
use serde::{Deserialize, Serialize};
use space3d_core::hecs::{Entity, EntityBuilder, World};
use space3d_core::{Parent, Transform};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// On-disk encoding of a [`Scene`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SceneFormat {
    /// Rusty Object Notation (pretty-printed). The default.
    #[default]
    Ron,
    /// JSON (pretty-printed).
    Json,
}

impl SceneFormat {
    /// Guess from a file extension (`.json` → JSON, anything else → RON).
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        match path.as_ref().extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("json") => Self::Json,
            _ => Self::Ron,
        }
    }
}

/// One entity in a [`Scene`]: its stable id plus named component values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneEntity {
    /// Identity used for entity-reference remapping (and restored as a [`StableId`] component).
    pub id: StableId,
    /// Component values keyed by registered name (sorted for stable output).
    #[serde(default)]
    pub components: BTreeMap<String, ComponentValue>,
}

impl SceneEntity {
    /// Empty entity with the given id.
    pub fn new(id: StableId) -> Self {
        Self {
            id,
            components: BTreeMap::new(),
        }
    }

    /// Whether this entity has a `Parent` component.
    pub fn has_parent(&self) -> bool {
        self.components.contains_key(PARENT)
    }
}

/// Options for [`Scene::spawn_with`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpawnOptions {
    /// Give every spawned entity a fresh [`StableId`] (instancing) instead of the saved one.
    pub fresh_ids: bool,
    /// Place the scene: root transforms are pre-multiplied by this transform
    /// (roots without a `Transform` get it as-is).
    pub root_transform: Option<Transform>,
}

/// Serializable list of entities with named component values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    /// Entities in save order.
    pub entities: Vec<SceneEntity>,
}

/// `parent * child` for TRS transforms (exact for uniform scale).
pub fn compose_transforms(parent: &Transform, child: &Transform) -> Transform {
    Transform {
        translation: parent.translation + parent.rotation * (parent.scale * child.translation),
        rotation: parent.rotation * child.rotation,
        scale: parent.scale * child.scale,
    }
}

/// Save every entity that has at least one registered, serializable component.
///
/// Entities without a [`StableId`] get a fresh one in the scene (the world is not modified).
pub fn save_world(world: &World, registry: &ComponentRegistry) -> Result<Scene, SceneError> {
    let mut entities: Vec<Entity> = world
        .iter()
        .map(|e| e.entity())
        .filter(|&e| {
            registry
                .entries
                .iter()
                .any(|en| en.serde.is_some() && (en.has)(world, e))
        })
        .collect();
    entities.sort_unstable_by_key(|e| (e.id(), e.to_bits()));
    save_entities(world, registry, &entities)
}

/// Save the given entities (in the given order).
pub fn save_entities(world: &World, registry: &ComponentRegistry, entities: &[Entity]) -> Result<Scene, SceneError> {
    let ids: HashMap<Entity, StableId> = entities
        .iter()
        .map(|&e| (e, world.get::<&StableId>(e).map(|id| *id).unwrap_or_default()))
        .collect();
    let ctx = SaveContext { ids: &ids, world };
    let mut scene = Scene::default();
    for &e in entities {
        if !world.contains(e) {
            continue;
        }
        let mut se = SceneEntity::new(ids[&e]);
        for entry in &registry.entries {
            let Some(serde) = &entry.serde else { continue };
            if let Some(v) = (serde.save)(world, e, &ctx) {
                se.components.insert(entry.name.clone(), v?);
            }
        }
        scene.entities.push(se);
    }
    Ok(scene)
}

impl Scene {
    /// Pretty RON text.
    pub fn to_ron(&self) -> Result<String, SceneError> {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default()).map_err(|e| SceneError::Ron(e.to_string()))
    }

    /// Parse RON text.
    pub fn from_ron(text: &str) -> Result<Self, SceneError> {
        ron::from_str(text).map_err(|e| SceneError::Ron(e.to_string()))
    }

    /// Pretty JSON text.
    pub fn to_json(&self) -> Result<String, SceneError> {
        serde_json::to_string_pretty(self).map_err(|e| SceneError::Json(e.to_string()))
    }

    /// Parse JSON text.
    pub fn from_json(text: &str) -> Result<Self, SceneError> {
        serde_json::from_str(text).map_err(|e| SceneError::Json(e.to_string()))
    }

    /// Encode in the given format.
    pub fn to_bytes(&self, format: SceneFormat) -> Result<Vec<u8>, SceneError> {
        match format {
            SceneFormat::Ron => self.to_ron().map(String::into_bytes),
            SceneFormat::Json => self.to_json().map(String::into_bytes),
        }
    }

    /// Decode from bytes in the given format.
    pub fn from_bytes(bytes: &[u8], format: SceneFormat) -> Result<Self, SceneError> {
        match format {
            SceneFormat::Ron => {
                let text = std::str::from_utf8(bytes).map_err(|e| SceneError::Ron(e.to_string()))?;
                Self::from_ron(text)
            }
            SceneFormat::Json => serde_json::from_slice(bytes).map_err(|e| SceneError::Json(e.to_string())),
        }
    }

    /// Entity with the given stable id.
    pub fn get(&self, id: StableId) -> Option<&SceneEntity> {
        self.entities.iter().find(|e| e.id == id)
    }

    /// Entities without a `Parent` component.
    pub fn roots(&self) -> impl Iterator<Item = &SceneEntity> {
        self.entities.iter().filter(|e| !e.has_parent())
    }

    /// Number of entities.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// `true` if the scene has no entities.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Spawn all entities keeping their saved [`StableId`]s; returns the new entities in
    /// scene order. Entity references (e.g. `Parent`) are remapped to the new entities.
    pub fn spawn_into(&self, world: &mut World, registry: &ComponentRegistry) -> Result<Vec<Entity>, SceneError> {
        self.spawn_with(world, registry, &SpawnOptions::default())
    }

    /// Spawn with [`SpawnOptions`]. On error nothing is left in the world.
    pub fn spawn_with(
        &self,
        world: &mut World,
        registry: &ComponentRegistry,
        options: &SpawnOptions,
    ) -> Result<Vec<Entity>, SceneError> {
        // Validate names before touching the world.
        for se in &self.entities {
            for name in se.components.keys() {
                match registry.by_name(name) {
                    Some(e) if e.serde.is_some() => {}
                    _ => return Err(SceneError::UnknownComponent(name.clone())),
                }
            }
        }

        // Pass 1: allocate entities so references can be resolved in any order.
        let spawned: Vec<Entity> = self
            .entities
            .iter()
            .map(|se| {
                let id = if options.fresh_ids { StableId::new() } else { se.id };
                world.spawn((id,))
            })
            .collect();
        let scene_map: HashMap<StableId, Entity> = self
            .entities
            .iter()
            .map(|se| se.id)
            .zip(spawned.iter().copied())
            .collect();
        let spawned_set: HashSet<Entity> = spawned.iter().copied().collect();
        let world_map: HashMap<StableId, Entity> = world
            .query::<(Entity, &StableId)>()
            .iter()
            .filter(|(e, _)| !spawned_set.contains(e))
            .map(|(e, id)| (*id, e))
            .collect();
        let ctx = LoadContext {
            scene: &scene_map,
            world: &world_map,
        };

        // Pass 2: deserialize everything, then insert.
        let mut builders = Vec::with_capacity(self.entities.len());
        for se in &self.entities {
            let mut b = EntityBuilder::new();
            for (name, value) in &se.components {
                let serde = registry
                    .by_name(name)
                    .and_then(|e| e.serde.as_ref())
                    .expect("validated above");
                if let Err(err) = (serde.load)(&mut b, value.clone(), &ctx) {
                    for &e in &spawned {
                        let _ = world.despawn(e);
                    }
                    return Err(err);
                }
            }
            builders.push(b);
        }
        for (&e, mut b) in spawned.iter().zip(builders) {
            world.insert(e, b.build()).expect("entity spawned above");
        }

        if let Some(root) = options.root_transform {
            for &e in &spawned {
                let is_root = world
                    .get::<&Parent>(e)
                    .map(|p| !spawned_set.contains(&p.0))
                    .unwrap_or(true);
                if !is_root {
                    continue;
                }
                let placed = match world.get::<&Transform>(e) {
                    Ok(t) => compose_transforms(&root, &t),
                    Err(_) => root,
                };
                let _ = world.insert_one(e, placed);
            }
        }
        Ok(spawned)
    }
}

/// Reusable entity template: a [`Scene`] instantiated with fresh ids at a given transform.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Prefab {
    /// Template contents; roots are entities without a `Parent`.
    pub scene: Scene,
}

impl Prefab {
    /// Wrap a scene.
    pub fn new(scene: Scene) -> Self {
        Self { scene }
    }

    /// Capture `root` and all of its descendants (via `Parent`) from a live world.
    pub fn from_world(world: &World, registry: &ComponentRegistry, root: Entity) -> Result<Self, SceneError> {
        let links: Vec<(Entity, Entity)> = world
            .query::<(Entity, &Parent)>()
            .iter()
            .map(|(e, p)| (e, p.0))
            .collect();
        let mut members = vec![root];
        let mut seen: HashSet<Entity> = members.iter().copied().collect();
        let mut i = 0;
        while i < members.len() {
            let parent = members[i];
            let mut kids: Vec<Entity> = links
                .iter()
                .filter(|(c, p)| *p == parent && !seen.contains(c))
                .map(|(c, _)| *c)
                .collect();
            kids.sort_unstable_by_key(|e| e.id());
            seen.extend(kids.iter().copied());
            members.extend(kids);
            i += 1;
        }
        let mut scene = save_entities(world, registry, &members)?;
        // The template root must not keep a link to whatever it was attached to.
        if let Some(r) = scene.entities.first_mut() {
            r.components.remove(PARENT);
        }
        Ok(Self { scene })
    }

    /// Spawn a new instance: fresh [`StableId`]s, roots placed relative to `at`.
    /// Returns the new entities in template order.
    pub fn instantiate(
        &self,
        world: &mut World,
        registry: &ComponentRegistry,
        at: Transform,
    ) -> Result<Vec<Entity>, SceneError> {
        self.scene.spawn_with(
            world,
            registry,
            &SpawnOptions {
                fresh_ids: true,
                root_transform: Some(at),
            },
        )
    }
}
