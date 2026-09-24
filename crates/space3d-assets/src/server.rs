//! The [`AssetServer`]: path → handle bookkeeping and non-blocking loading.

use crate::assets::{Asset, Assets};
use crate::error::AssetError;
use crate::handle::{AssetId, AssetIndex, Handle, HandleProvider, StrongHandle, UntypedAssetId};
use crate::loader::{AssetLoader, ErasedAsset, ErasedLoader, LoadContext, MAX_DEPENDENCY_ROUNDS};
use crate::path::AssetPath;
use crate::source::{AssetSource, default_source};
use space3d_core::Resources;
use space3d_tasks::{Task, TaskState};
use std::any::{TypeId, type_name};
use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// Loading status of an asset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LoadState {
    /// Unknown to the server (never loaded, or released).
    #[default]
    NotLoaded,
    /// I/O or decoding in progress.
    Loading,
    /// Data is available in [`Assets<T>`].
    Loaded,
    /// Loading failed; the asset has no data.
    Failed(AssetError),
}

impl LoadState {
    /// `Loaded`?
    pub fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded)
    }
    /// `Loading`?
    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }
    /// `Failed(_)`?
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
    /// The failure, if any.
    pub fn error(&self) -> Option<&AssetError> {
        match self {
            Self::Failed(e) => Some(e),
            _ => None,
        }
    }
}

type InsertFn = fn(&mut Resources, AssetIndex, ErasedAsset) -> Result<(), AssetError>;

struct TypeInfo {
    name: &'static str,
    provider: Arc<HandleProvider>,
    insert: InsertFn,
}

struct AssetInfo {
    path: AssetPath,
    state: LoadState,
    dependencies: Vec<AssetPath>,
}

struct Loaded {
    value: ErasedAsset,
    dependencies: Vec<AssetPath>,
}

type LoadResult = Result<Loaded, AssetError>;

struct InFlight {
    task: Task<LoadResult>,
    reload: bool,
}

#[derive(Clone)]
struct Sources {
    default: Arc<dyn AssetSource>,
    named: HashMap<String, Arc<dyn AssetSource>>,
}

impl Sources {
    fn get(&self, path: &AssetPath) -> Result<Arc<dyn AssetSource>, AssetError> {
        match path.source() {
            None => Ok(self.default.clone()),
            Some(name) => self
                .named
                .get(name)
                .cloned()
                .ok_or_else(|| AssetError::UnknownSource(name.to_string())),
        }
    }
}

/// Loads assets by path without blocking the frame.
///
/// [`AssetServer::load`] returns a [`Handle`] immediately; bytes are read on the
/// `space3d_tasks` I/O runtime (tokio fs natively, `fetch` on the web), decoded on the
/// compute pool, and inserted into [`Assets<T>`] by the `assets::process_loads` system in
/// `Stage::PreUpdate`, which also emits [`crate::AssetEvent`]s.
pub struct AssetServer {
    sources: Arc<Sources>,
    loaders: Vec<Arc<dyn ErasedLoader>>,
    types: HashMap<TypeId, TypeInfo>,
    by_path: HashMap<(TypeId, AssetPath), (AssetIndex, Weak<StrongHandle>)>,
    infos: HashMap<UntypedAssetId, AssetInfo>,
    in_flight: HashMap<UntypedAssetId, InFlight>,
}

impl Default for AssetServer {
    /// Server whose default source is the platform default for `"assets"`.
    fn default() -> Self {
        Self::from_arc(default_source("assets"))
    }
}

impl AssetServer {
    /// Server reading unqualified paths from `default_source`. No loaders are registered;
    /// [`crate::AssetsPlugin`] adds the built-in ones.
    pub fn new(default_source: impl AssetSource) -> Self {
        Self::from_arc(Arc::new(default_source))
    }

    /// Like [`AssetServer::new`] for an already shared source.
    pub fn from_arc(default_source: Arc<dyn AssetSource>) -> Self {
        Self {
            sources: Arc::new(Sources {
                default: default_source,
                named: HashMap::new(),
            }),
            loaders: Vec::new(),
            types: HashMap::new(),
            by_path: HashMap::new(),
            infos: HashMap::new(),
            in_flight: HashMap::new(),
        }
    }

    /// Replace the source used for paths without a `name://` prefix.
    pub fn set_default_source(&mut self, source: impl AssetSource) {
        self.set_default_source_arc(Arc::new(source));
    }

    /// Like [`AssetServer::set_default_source`] for a shared source.
    pub fn set_default_source_arc(&mut self, source: Arc<dyn AssetSource>) {
        Arc::make_mut(&mut self.sources).default = source;
    }

    /// The source used for paths without a `name://` prefix.
    pub fn default_source(&self) -> &Arc<dyn AssetSource> {
        &self.sources.default
    }

    /// Register a named source, addressed as `name://path`.
    pub fn add_source(&mut self, name: impl Into<String>, source: impl AssetSource) {
        self.add_source_arc(name, Arc::new(source));
    }

    /// Like [`AssetServer::add_source`] for a shared source.
    pub fn add_source_arc(&mut self, name: impl Into<String>, source: Arc<dyn AssetSource>) {
        Arc::make_mut(&mut self.sources).named.insert(name.into(), source);
    }

    /// Register a loader. Later registrations win for the same (type, extension).
    pub fn register_loader<L: AssetLoader>(&mut self, loader: L) {
        self.loaders.push(Arc::new(loader));
    }

    /// Whether some loader produces `T` from files with this extension.
    pub fn has_loader<T: Asset>(&self, extension: &str) -> bool {
        let probe = AssetPath::new(&format!("probe.{extension}"));
        self.find_loader(TypeId::of::<T>(), &probe).is_some()
    }

    /// Whether `T` was registered with [`crate::AssetApp::init_asset`].
    pub fn is_registered<T: Asset>(&self) -> bool {
        self.types.contains_key(&TypeId::of::<T>())
    }

    pub(crate) fn register_type<T: Asset>(&mut self, provider: Arc<HandleProvider>) {
        debug_assert_eq!(provider.type_id, TypeId::of::<T>());
        self.types.insert(
            TypeId::of::<T>(),
            TypeInfo {
                name: type_name::<T>(),
                provider,
                insert: insert_erased::<T>,
            },
        );
    }

    /// Start loading `path` as a `T` (or return the existing handle if it is already
    /// loading/loaded). Never blocks.
    ///
    /// # Panics
    /// If `T` was not registered with [`crate::AssetApp::init_asset`].
    pub fn load<T: Asset>(&mut self, path: impl Into<AssetPath>) -> Handle<T> {
        let path = path.into();
        if let Some(h) = self.get_handle::<T>(&path) {
            return h;
        }
        let type_id = TypeId::of::<T>();
        let provider = match self.types.get(&type_id) {
            Some(t) => t.provider.clone(),
            None => panic!(
                "asset type `{}` is not registered; call `app.init_asset::<T>()` first",
                type_name::<T>()
            ),
        };
        let strong = provider.strong(provider.reserve());
        let id = UntypedAssetId {
            type_id,
            index: strong.index,
        };
        self.by_path
            .insert((type_id, path.clone()), (strong.index, Arc::downgrade(&strong)));
        self.infos.insert(
            id,
            AssetInfo {
                path,
                state: LoadState::Loading,
                dependencies: Vec::new(),
            },
        );
        self.start_load(id, false);
        Handle::from_strong(strong)
    }

    /// Strong handle for an asset previously requested with [`AssetServer::load`], if it is
    /// still alive.
    pub fn get_handle<T: Asset>(&self, path: impl Into<AssetPath>) -> Option<Handle<T>> {
        let (_, weak) = self.by_path.get(&(TypeId::of::<T>(), path.into()))?;
        weak.upgrade().map(Handle::from_strong)
    }

    /// Re-read and re-decode every asset whose path (or one of whose dependencies) equals
    /// `path`. Completion replaces the data and emits `Modified`. Returns how many assets
    /// were restarted.
    pub fn reload(&mut self, path: impl Into<AssetPath>) -> usize {
        let path = path.into();
        let ids: Vec<UntypedAssetId> = self
            .infos
            .iter()
            .filter(|(_, i)| i.path == path || i.dependencies.contains(&path))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            self.start_load(*id, true);
        }
        ids.len()
    }

    /// Current load state.
    pub fn load_state(&self, id: impl Into<UntypedAssetId>) -> LoadState {
        self.infos
            .get(&id.into())
            .map_or(LoadState::NotLoaded, |i| i.state.clone())
    }

    /// Shorthand for `load_state(id).is_loaded()`.
    pub fn is_loaded(&self, id: impl Into<UntypedAssetId>) -> bool {
        self.infos.get(&id.into()).is_some_and(|i| i.state.is_loaded())
    }

    /// Path an asset was loaded from.
    pub fn path(&self, id: impl Into<UntypedAssetId>) -> Option<&AssetPath> {
        self.infos.get(&id.into()).map(|i| &i.path)
    }

    /// Extra files read while loading the asset (glTF buffers, MTL files, textures...).
    pub fn dependencies(&self, id: impl Into<UntypedAssetId>) -> &[AssetPath] {
        self.infos.get(&id.into()).map_or(&[], |i| i.dependencies.as_slice())
    }

    /// Number of loads currently in flight.
    pub fn pending(&self) -> usize {
        self.in_flight.len()
    }

    /// No loads in flight?
    pub fn is_idle(&self) -> bool {
        self.in_flight.is_empty()
    }

    /// Ids of all tracked (path-loaded) assets of type `T`.
    pub fn ids<T: Asset>(&self) -> Vec<AssetId<T>> {
        self.infos.keys().filter_map(|id| id.typed::<T>()).collect()
    }

    fn find_loader(&self, type_id: TypeId, path: &AssetPath) -> Option<Arc<dyn ErasedLoader>> {
        self.loaders
            .iter()
            .rev()
            .find(|l| l.asset_type() == type_id && l.extensions().iter().any(|e| path.has_extension(e)))
            .cloned()
    }

    fn start_load(&mut self, id: UntypedAssetId, reload: bool) {
        let Some(info) = self.infos.get(&id) else {
            return;
        };
        let path = info.path.clone();
        let type_name = self.types.get(&id.type_id).map_or("?", |t| t.name);
        let prepared = self
            .find_loader(id.type_id, &path)
            .ok_or_else(|| AssetError::NoLoader {
                extension: path.extension().unwrap_or_default(),
                type_name,
            })
            .and_then(|loader| self.sources.get(&path).map(|_| loader));
        let info = self.infos.get_mut(&id).expect("checked above");
        match prepared {
            Ok(loader) => {
                let task = space3d_tasks::spawn(run_load(self.sources.clone(), loader, path));
                self.in_flight.insert(id, InFlight { task, reload });
                if !(reload && info.state.is_loaded()) {
                    info.state = LoadState::Loading;
                }
            }
            Err(e) => {
                tracing::error!(asset = %path, error = %e, "cannot load asset");
                self.in_flight.remove(&id);
                info.state = LoadState::Failed(e);
            }
        }
    }

    /// Collect finished loads and insert them into their [`Assets<T>`]. Runs in
    /// `Stage::PreUpdate` (system `assets::process_loads`); never blocks.
    pub fn process(&mut self, resources: &mut Resources) {
        if self.in_flight.is_empty() {
            return;
        }
        let mut finished = Vec::new();
        self.in_flight.retain(|id, f| match f.task.state() {
            TaskState::Pending => true,
            TaskState::Ready => {
                finished.push((*id, f.reload, f.task.poll()));
                false
            }
            TaskState::Cancelled => {
                finished.push((*id, f.reload, None));
                false
            }
        });
        for (id, reload, result) in finished {
            let Some(ty) = self.types.get(&id.type_id) else {
                continue;
            };
            let Some(info) = self.infos.get_mut(&id) else {
                continue;
            };
            let result = result.unwrap_or_else(|| Err(AssetError::Other("load task was cancelled".into())));
            let outcome = result.and_then(|loaded| {
                (ty.insert)(resources, id.index, loaded.value)?;
                Ok(loaded.dependencies)
            });
            match outcome {
                Ok(deps) => {
                    tracing::debug!(asset = %info.path, reload, "asset loaded");
                    info.state = LoadState::Loaded;
                    info.dependencies = deps;
                }
                Err(e) if reload && info.state.is_loaded() => {
                    tracing::warn!(asset = %info.path, error = %e, "reload failed; keeping previous data");
                }
                Err(e) => {
                    tracing::error!(asset = %info.path, error = %e, "failed to load asset");
                    info.state = LoadState::Failed(e);
                }
            }
        }
    }

    /// Drop all bookkeeping for a released asset.
    pub(crate) fn forget(&mut self, id: UntypedAssetId) {
        self.in_flight.remove(&id);
        if let Some(info) = self.infos.remove(&id) {
            let key = (id.type_id, info.path);
            if matches!(self.by_path.get(&key), Some((idx, _)) if *idx == id.index) {
                self.by_path.remove(&key);
            }
        }
    }
}

fn insert_erased<T: Asset>(res: &mut Resources, idx: AssetIndex, value: ErasedAsset) -> Result<(), AssetError> {
    let value = value.downcast::<T>().map_err(|_| {
        AssetError::Other(format!(
            "loader produced the wrong type (expected `{}`)",
            type_name::<T>()
        ))
    })?;
    let assets = res
        .get_mut::<Assets<T>>()
        .ok_or_else(|| AssetError::Other(format!("missing resource Assets<{}>", type_name::<T>())))?;
    if assets.insert(AssetId::<T>::from_index(idx), *value) {
        Ok(())
    } else {
        Err(AssetError::Other("asset was released before loading finished".into()))
    }
}

async fn run_load(sources: Arc<Sources>, loader: Arc<dyn ErasedLoader>, path: AssetPath) -> LoadResult {
    let bytes = sources.get(&path)?.read(&path).await?;
    let mut ctx = LoadContext::new(path.clone());
    for _ in 0..MAX_DEPENDENCY_ROUNDS {
        let mut new: Vec<AssetPath> = Vec::new();
        for uri in loader.dependencies(&bytes, &ctx)? {
            let p = ctx.resolve(&uri);
            if !ctx.dependencies.contains_key(&p) && !new.contains(&p) {
                new.push(p);
            }
        }
        if new.is_empty() {
            break;
        }
        let reads = futures::future::join_all(new.iter().map(|p| {
            let src = sources.get(p);
            let p = p.clone();
            async move { src?.read(&p).await }
        }))
        .await;
        for (p, r) in new.into_iter().zip(reads) {
            let bytes = match r {
                Ok(b) => Some(b),
                Err(e) => {
                    tracing::warn!(asset = %path, dependency = %p, error = %e, "failed to read dependency");
                    None
                }
            };
            ctx.dependencies.insert(p, bytes);
        }
    }
    let dependencies: Vec<AssetPath> = ctx.dependencies.keys().cloned().collect();
    let decode = space3d_tasks::spawn_compute(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loader.load_erased(&bytes, &ctx)))
            .unwrap_or_else(|_| Err(AssetError::decode(ctx.path(), "loader panicked")))
    });
    let value = decode
        .wait()
        .await
        .ok_or_else(|| AssetError::decode(&path, "decode task was cancelled"))??;
    Ok(Loaded { value, dependencies })
}
