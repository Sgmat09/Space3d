//! Filesystem watching for hot reload (native + `hot-reload` feature).

use crate::error::AssetError;
use crate::path::AssetPath;
use crate::server::AssetServer;
use notify::{EventKind, RecursiveMode, Watcher, event::ModifyKind};
use parking_lot::Mutex;
use space3d_core::{App, Stage, SystemCtx};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resource that watches the asset root directory. Changed files are mapped to asset paths
/// and passed to [`AssetServer::reload`] in `Stage::PreUpdate`; completed reloads replace the
/// data and emit [`crate::AssetEvent::Modified`].
pub struct HotReloadWatcher {
    root: PathBuf,
    changed: Arc<Mutex<Vec<PathBuf>>>,
    _watcher: Mutex<notify::RecommendedWatcher>,
}

impl HotReloadWatcher {
    /// Start watching `root` recursively.
    pub fn new(root: &Path) -> Result<Self, AssetError> {
        let root = root.canonicalize().map_err(|e| AssetError::io(root.display(), e))?;
        let changed: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let sink = changed.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) if is_content_change(&ev.kind) => sink.lock().extend(ev.paths),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "asset watcher error"),
        })
        .map_err(|e| AssetError::io(root.display(), e))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| AssetError::io(root.display(), e))?;
        tracing::info!(root = %root.display(), "watching assets for hot reload");
        Ok(Self {
            root,
            changed,
            _watcher: Mutex::new(watcher),
        })
    }

    /// The (canonicalized) watched directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Asset paths changed since the last call (deduplicated).
    pub fn drain_changed(&self) -> Vec<AssetPath> {
        let raw = std::mem::take(&mut *self.changed.lock());
        let mut out: Vec<AssetPath> = Vec::new();
        for p in raw {
            let rel = p.strip_prefix(&self.root).map(Path::to_path_buf).ok().or_else(|| {
                p.canonicalize()
                    .ok()?
                    .strip_prefix(&self.root)
                    .ok()
                    .map(Path::to_path_buf)
            });
            if let Some(rel) = rel {
                let ap = AssetPath::new(&rel.to_string_lossy());
                if !ap.path().is_empty() && !out.contains(&ap) {
                    out.push(ap);
                }
            }
        }
        out
    }
}

fn is_content_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Any | EventKind::Other => true,
        EventKind::Modify(m) => !matches!(m, ModifyKind::Metadata(_)),
        EventKind::Access(_) | EventKind::Remove(_) => false,
    }
}

fn hot_reload_system(ctx: &mut SystemCtx) {
    let Some(watcher) = ctx.resources.get::<HotReloadWatcher>() else {
        return;
    };
    let changed = watcher.drain_changed();
    if changed.is_empty() {
        return;
    }
    let server = ctx.resources.expect_mut::<AssetServer>();
    for path in changed {
        let n = server.reload(path.clone());
        if n > 0 {
            tracing::info!(asset = %path, assets = n, "hot reloading");
        }
    }
}

pub(crate) fn install(app: &mut App, root: &str) {
    match HotReloadWatcher::new(Path::new(root)) {
        Ok(w) => {
            app.insert_resource(w);
            app.add_system_named(Stage::PreUpdate, "assets::hot_reload", hot_reload_system);
        }
        Err(e) => tracing::warn!(root, error = %e, "hot reload disabled"),
    }
}
