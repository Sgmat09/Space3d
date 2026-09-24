//! Asynchronous scene loading: [`SceneCommands`] resource + PreUpdate spawn system.
use crate::error::SceneError;
use crate::registry::ComponentRegistry;
use crate::scene::{Scene, SceneFormat, SpawnOptions};
use space3d_core::hecs::Entity;
use space3d_core::{Events, SystemCtx, Transform};
use space3d_tasks::{Task, TaskState, spawn_compute};
use std::collections::HashMap;
use std::path::PathBuf;

/// Handle for one queued load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SceneLoadId(pub u64);

/// Progress of a queued load.
#[derive(Clone, Debug, PartialEq)]
pub enum SceneLoadStatus {
    /// Still reading/parsing off-thread (or waiting for the next `PreUpdate`).
    Pending,
    /// Spawned; entities in scene order.
    Spawned(Vec<Entity>),
    /// Reading, parsing or spawning failed.
    Failed(SceneError),
    /// Unknown id (never queued, or forgotten).
    Unknown,
}

/// Event sent when a queued load finishes (successfully or not).
#[derive(Clone, Debug, PartialEq)]
pub struct SceneLoaded {
    /// Which load finished.
    pub id: SceneLoadId,
    /// Spawned entities, or the error.
    pub result: Result<Vec<Entity>, SceneError>,
}

struct Pending {
    id: SceneLoadId,
    task: Task<Result<Scene, SceneError>>,
    options: SpawnOptions,
}

/// Queue scene loads from paths or bytes. Reading + parsing happen off the main thread via
/// [`space3d_tasks::spawn_compute`]; spawning happens in `Stage::PreUpdate` once ready, after
/// which [`SceneLoaded`] is sent and [`SceneCommands::status`] reports the entities.
#[derive(Default)]
pub struct SceneCommands {
    next: u64,
    pending: Vec<Pending>,
    done: HashMap<SceneLoadId, Result<Vec<Entity>, SceneError>>,
}

fn read_path(path: &PathBuf) -> Result<Vec<u8>, SceneError> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::read(path).map_err(|e| SceneError::Io(format!("{}: {e}", path.display())))
    }
    #[cfg(target_arch = "wasm32")]
    {
        Err(SceneError::Unsupported(format!(
            "no filesystem on the web ({}); fetch the bytes and use load_bytes",
            path.display()
        )))
    }
}

impl SceneCommands {
    fn queue(&mut self, task: Task<Result<Scene, SceneError>>, options: SpawnOptions) -> SceneLoadId {
        let id = SceneLoadId(self.next);
        self.next += 1;
        self.pending.push(Pending { id, task, options });
        id
    }

    /// Load a scene file (format from extension) keeping its stable ids.
    pub fn load_path(&mut self, path: impl Into<PathBuf>) -> SceneLoadId {
        self.load_path_with(path, SpawnOptions::default())
    }

    /// Load a scene file with explicit [`SpawnOptions`].
    pub fn load_path_with(&mut self, path: impl Into<PathBuf>, options: SpawnOptions) -> SceneLoadId {
        let path = path.into();
        let task = spawn_compute(move || {
            let bytes = read_path(&path)?;
            Scene::from_bytes(&bytes, SceneFormat::from_path(&path))
        });
        self.queue(task, options)
    }

    /// Parse and spawn in-memory scene bytes, keeping stable ids.
    pub fn load_bytes(&mut self, bytes: impl Into<Vec<u8>>, format: SceneFormat) -> SceneLoadId {
        self.load_bytes_with(bytes, format, SpawnOptions::default())
    }

    /// Parse and spawn in-memory scene bytes with explicit [`SpawnOptions`].
    pub fn load_bytes_with(
        &mut self,
        bytes: impl Into<Vec<u8>>,
        format: SceneFormat,
        options: SpawnOptions,
    ) -> SceneLoadId {
        let bytes = bytes.into();
        let task = spawn_compute(move || Scene::from_bytes(&bytes, format));
        self.queue(task, options)
    }

    /// Instantiate a prefab file at `at` (fresh stable ids).
    pub fn instantiate_path(&mut self, path: impl Into<PathBuf>, at: Transform) -> SceneLoadId {
        self.load_path_with(
            path,
            SpawnOptions {
                fresh_ids: true,
                root_transform: Some(at),
            },
        )
    }

    /// Spawn an already-parsed scene on the next `PreUpdate`.
    pub fn spawn(&mut self, scene: Scene, options: SpawnOptions) -> SceneLoadId {
        self.queue(spawn_compute(move || Ok(scene)), options)
    }

    /// Current status of a load.
    pub fn status(&self, id: SceneLoadId) -> SceneLoadStatus {
        if let Some(r) = self.done.get(&id) {
            return match r {
                Ok(v) => SceneLoadStatus::Spawned(v.clone()),
                Err(e) => SceneLoadStatus::Failed(e.clone()),
            };
        }
        if self.pending.iter().any(|p| p.id == id) {
            SceneLoadStatus::Pending
        } else {
            SceneLoadStatus::Unknown
        }
    }

    /// Remove and return a finished result (frees its bookkeeping).
    pub fn take_result(&mut self, id: SceneLoadId) -> Option<Result<Vec<Entity>, SceneError>> {
        self.done.remove(&id)
    }

    /// Number of loads still in flight.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn take_ready(&mut self) -> Vec<(SceneLoadId, Result<Scene, SceneError>, SpawnOptions)> {
        let mut ready = Vec::new();
        self.pending.retain_mut(|p| match p.task.state() {
            TaskState::Pending => true,
            TaskState::Ready => {
                ready.push((p.id, p.task.poll().expect("ready"), p.options));
                false
            }
            TaskState::Cancelled => {
                ready.push((p.id, Err(SceneError::Io("load task was cancelled".into())), p.options));
                false
            }
        });
        ready
    }
}

/// `PreUpdate` system installed by [`crate::ScenePlugin`]: spawns finished loads.
pub fn spawn_loaded_scenes(ctx: &mut SystemCtx) {
    let SystemCtx { world, resources } = ctx;
    let ready = resources.expect_mut::<SceneCommands>().take_ready();
    if ready.is_empty() {
        return;
    }
    let results: Vec<(SceneLoadId, Result<Vec<Entity>, SceneError>)> = {
        let registry = resources.expect::<ComponentRegistry>();
        ready
            .into_iter()
            .map(|(id, scene, options)| (id, scene.and_then(|s| s.spawn_with(world, registry, &options))))
            .collect()
    };
    for (id, result) in results {
        if let Err(err) = &result {
            tracing::warn!(?id, %err, "scene load failed");
        }
        resources.init::<Events<SceneLoaded>>().send(SceneLoaded {
            id,
            result: result.clone(),
        });
        resources.expect_mut::<SceneCommands>().done.insert(id, result);
    }
}
