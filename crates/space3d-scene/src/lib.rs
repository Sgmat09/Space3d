//! Space3d scenes: component registry, RON/JSON scene files, prefabs, async loading and
//! in-memory world snapshots.
//!
//! * [`ComponentRegistry`] maps component types to names + serde functions. Core
//!   `Transform`, `Name` and `Parent` are registered by default; `Parent` is saved as the
//!   parent's [`StableId`] and remapped to the new `Entity` when spawned.
//! * [`save_world`] → [`Scene`] → [`Scene::to_ron`]/[`Scene::to_json`] and back with
//!   [`Scene::from_ron`] + [`Scene::spawn_into`].
//! * [`Prefab::instantiate`] spawns a scene with fresh ids at a root transform.
//! * [`ScenePlugin`] + [`SceneCommands`] load scenes asynchronously (parse off-thread with
//!   `space3d_tasks::spawn_compute`, spawn in `Stage::PreUpdate`).
//! * [`WorldSnapshot`] captures/restores registered components in place for rollback and
//!   deterministic replays.
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_scene::*;
//!
//! let registry = ComponentRegistry::default();
//! let mut world = World::new();
//! let parent = world.spawn((Name("ship".into()), Transform::from_xyz(1.0, 2.0, 3.0)));
//! world.spawn((Name("turret".into()), Transform::IDENTITY, Parent(parent)));
//!
//! let ron = save_world(&world, &registry).unwrap().to_ron().unwrap();
//! let mut other = World::new();
//! let spawned = Scene::from_ron(&ron).unwrap().spawn_into(&mut other, &registry).unwrap();
//! assert_eq!(other.get::<&Parent>(spawned[1]).unwrap().0, spawned[0]);
//! ```
#![warn(missing_docs)]

mod commands;
mod error;
mod registry;
mod scene;
mod snapshot;

pub use commands::{SceneCommands, SceneLoadId, SceneLoadStatus, SceneLoaded, spawn_loaded_scenes};
pub use error::SceneError;
pub use registry::{ComponentRegistry, ComponentValue, LoadContext, PARENT, SaveContext, StableId, TRANSFORM};
pub use scene::{Prefab, Scene, SceneEntity, SceneFormat, SpawnOptions, compose_transforms, save_entities, save_world};
pub use snapshot::WorldSnapshot;

pub use ron;
pub use serde_json;

use space3d_core::{App, Plugin, Stage};

/// Inserts [`ComponentRegistry`] (defaults, unless one already exists) and [`SceneCommands`],
/// registers the [`SceneLoaded`] event and the `PreUpdate` spawn system.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScenePlugin;

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ComponentRegistry>();
        app.init_resource::<SceneCommands>();
        app.add_event::<SceneLoaded>();
        app.add_system_named(Stage::PreUpdate, "scene::spawn_loaded", spawn_loaded_scenes);
    }
}
