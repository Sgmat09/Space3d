//! [`ComponentRegistry`]: named, serde-capable component types plus stable entity ids.
use crate::error::SceneError;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use space3d_core::hecs::{Component, Entity, EntityBuilder, World};
use space3d_core::{GlobalTransform, Name, Parent, Transform};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

/// Format-agnostic value of one serialized component (a JSON value tree; it is written
/// as plain maps/lists/numbers in RON too).
pub type ComponentValue = serde_json::Value;

/// Persistent entity identity that survives save/load (hecs `Entity` ids do not).
///
/// Entity references inside components (e.g. [`Parent`]) are saved as `StableId`s and
/// remapped to fresh `Entity` handles when a scene is spawned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StableId(pub uuid::Uuid);

impl StableId {
    /// New random (v4) id.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// Deterministic id from a number (handy in tests and generated content).
    pub const fn from_u128(v: u128) -> Self {
        Self(uuid::Uuid::from_u128(v))
    }
}

impl Default for StableId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for StableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Lookup available to component serializers while saving: `Entity` → [`StableId`].
pub struct SaveContext<'a> {
    pub(crate) ids: &'a HashMap<Entity, StableId>,
    pub(crate) world: &'a World,
}

impl SaveContext<'_> {
    /// Stable id of `entity`: the id assigned for this save, or its [`StableId`] component.
    pub fn stable_id(&self, entity: Entity) -> Option<StableId> {
        self.ids
            .get(&entity)
            .copied()
            .or_else(|| self.world.get::<&StableId>(entity).ok().map(|id| *id))
    }
}

/// Lookup available to component deserializers while spawning: [`StableId`] → `Entity`.
pub struct LoadContext<'a> {
    pub(crate) scene: &'a HashMap<StableId, Entity>,
    pub(crate) world: &'a HashMap<StableId, Entity>,
}

impl LoadContext<'_> {
    /// Entity for a stable id: entities of the scene being spawned win, then entities already
    /// in the world carrying that [`StableId`].
    pub fn entity(&self, id: StableId) -> Option<Entity> {
        self.scene.get(&id).or_else(|| self.world.get(&id)).copied()
    }
}

type Boxed = Box<dyn Any + Send + Sync>;
type SaveFn = Box<dyn Fn(&World, Entity, &SaveContext) -> Option<Result<ComponentValue, SceneError>> + Send + Sync>;
type LoadFn = Box<dyn Fn(&mut EntityBuilder, ComponentValue, &LoadContext) -> Result<(), SceneError> + Send + Sync>;

pub(crate) struct Serde {
    pub save: SaveFn,
    pub load: LoadFn,
}

pub(crate) struct Entry {
    pub name: String,
    pub type_id: TypeId,
    /// `None` = snapshot-only (not written to scenes).
    pub serde: Option<Serde>,
    pub has: fn(&World, Entity) -> bool,
    pub clone_out: fn(&World, Entity) -> Option<Boxed>,
    pub add_boxed: fn(&Boxed, &mut EntityBuilder),
    pub remove: fn(&mut World, Entity),
}

fn has<T: Component>(w: &World, e: Entity) -> bool {
    w.satisfies::<&T>(e)
}
fn clone_out<T: Component + Clone>(w: &World, e: Entity) -> Option<Boxed> {
    w.get::<&T>(e).ok().map(|c| Box::new((*c).clone()) as Boxed)
}
fn add_boxed<T: Component + Clone>(v: &Boxed, b: &mut EntityBuilder) {
    if let Some(c) = v.downcast_ref::<T>() {
        b.add(c.clone());
    }
}
fn remove<T: Component>(w: &mut World, e: Entity) {
    let _ = w.remove_one::<T>(e);
}

/// Name of the built-in [`Parent`] registration (scene roots are entities without it).
pub const PARENT: &str = "Parent";
/// Name of the built-in [`Transform`] registration.
pub const TRANSFORM: &str = "Transform";

/// Registry of component types that scenes, prefabs and snapshots understand.
///
/// [`ComponentRegistry::default`] registers the core types: [`StableId`] (stored as the
/// scene entity id), [`Name`], [`Transform`], [`Parent`] (remapped through stable ids) and
/// [`GlobalTransform`] (snapshot-only; it is derived data).
pub struct ComponentRegistry {
    pub(crate) entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
    by_type: HashMap<TypeId, usize>,
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        let mut r = Self::empty();
        r.register_snapshot_only::<StableId>("StableId");
        r.register::<Name>("Name");
        r.register::<Transform>(TRANSFORM);
        r.register_mapped::<Parent, StableId>(
            PARENT,
            |p, ctx| ctx.stable_id(p.0),
            |id, ctx| ctx.entity(id).map(Parent),
        );
        r.register_snapshot_only::<GlobalTransform>("GlobalTransform");
        r
    }
}

impl ComponentRegistry {
    /// Registry with the core components (same as `default()`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with nothing registered at all.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            by_name: HashMap::new(),
            by_type: HashMap::new(),
        }
    }

    fn insert(&mut self, entry: Entry) -> &mut Self {
        // Re-registering a name or a type replaces the previous registration.
        if let Some(&i) = self
            .by_name
            .get(&entry.name)
            .or_else(|| self.by_type.get(&entry.type_id))
        {
            let old = std::mem::replace(&mut self.entries[i], entry);
            self.by_name.remove(&old.name);
            self.by_type.remove(&old.type_id);
            let e = &self.entries[i];
            self.by_name.insert(e.name.clone(), i);
            self.by_type.insert(e.type_id, i);
        } else {
            let i = self.entries.len();
            self.by_name.insert(entry.name.clone(), i);
            self.by_type.insert(entry.type_id, i);
            self.entries.push(entry);
        }
        self
    }

    fn base<T: Component + Clone>(name: &str, serde: Option<Serde>) -> Entry {
        Entry {
            name: name.to_owned(),
            type_id: TypeId::of::<T>(),
            serde,
            has: has::<T>,
            clone_out: clone_out::<T>,
            add_boxed: add_boxed::<T>,
            remove: remove::<T>,
        }
    }

    /// Register a plain serde component under `name` (the key used in scene files).
    pub fn register<T>(&mut self, name: &str) -> &mut Self
    where
        T: Component + Clone + Serialize + DeserializeOwned,
    {
        let n = name.to_owned();
        let n2 = name.to_owned();
        let serde = Serde {
            save: Box::new(move |w, e, _| {
                let c = w.get::<&T>(e).ok()?;
                Some(serde_json::to_value(&*c).map_err(|err| SceneError::Component {
                    name: n.clone(),
                    message: err.to_string(),
                }))
            }),
            load: Box::new(move |b, v, _| {
                let c: T = serde_json::from_value(v).map_err(|err| SceneError::Component {
                    name: n2.clone(),
                    message: err.to_string(),
                })?;
                b.add(c);
                Ok(())
            }),
        };
        self.insert(Self::base::<T>(name, Some(serde)))
    }

    /// Register a component that is saved through a serializable representation `R`,
    /// typically to turn `Entity` references into [`StableId`]s.
    ///
    /// `to_repr` returning `None` omits the component from the saved entity; `from_repr`
    /// returning `None` (e.g. unresolvable reference) skips it with a warning.
    pub fn register_mapped<T, R>(
        &mut self,
        name: &str,
        to_repr: fn(&T, &SaveContext) -> Option<R>,
        from_repr: fn(R, &LoadContext) -> Option<T>,
    ) -> &mut Self
    where
        T: Component + Clone,
        R: Serialize + DeserializeOwned + 'static,
    {
        let n = name.to_owned();
        let n2 = name.to_owned();
        let serde = Serde {
            save: Box::new(move |w, e, ctx| {
                let c = w.get::<&T>(e).ok()?;
                let Some(repr) = to_repr(&c, ctx) else {
                    tracing::warn!(component = %n, ?e, "component could not be mapped for saving; omitted");
                    return None;
                };
                Some(serde_json::to_value(&repr).map_err(|err| SceneError::Component {
                    name: n.clone(),
                    message: err.to_string(),
                }))
            }),
            load: Box::new(move |b, v, ctx| {
                let repr: R = serde_json::from_value(v).map_err(|err| SceneError::Component {
                    name: n2.clone(),
                    message: err.to_string(),
                })?;
                match from_repr(repr, ctx) {
                    Some(c) => {
                        b.add(c);
                    }
                    None => tracing::warn!(component = %n2, "component reference could not be resolved; skipped"),
                }
                Ok(())
            }),
        };
        self.insert(Self::base::<T>(name, Some(serde)))
    }

    /// Register a component that is only captured by [`crate::WorldSnapshot`] (never written
    /// to scene files), e.g. derived or runtime-only state.
    pub fn register_snapshot_only<T: Component + Clone>(&mut self, name: &str) -> &mut Self {
        self.insert(Self::base::<T>(name, None))
    }

    /// Whether a component with this scene name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Whether type `T` is registered.
    pub fn is_registered<T: 'static>(&self) -> bool {
        self.by_type.contains_key(&TypeId::of::<T>())
    }

    /// Registered names, in registration order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// Number of registrations.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn by_name(&self, name: &str) -> Option<&Entry> {
        self.by_name.get(name).map(|&i| &self.entries[i])
    }

    pub(crate) fn by_type(&self, t: TypeId) -> Option<&Entry> {
        self.by_type.get(&t).map(|&i| &self.entries[i])
    }
}
