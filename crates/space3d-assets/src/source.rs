//! Where asset bytes come from: filesystem (native), HTTP `fetch` (web), or memory.

use crate::error::AssetError;
use crate::path::AssetPath;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed future returned by [`AssetSource::read`]. `Send` on native; not on wasm, where
/// browser futures are single-threaded.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxedFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
/// Boxed future returned by [`AssetSource::read`]. `Send` on native; not on wasm, where
/// browser futures are single-threaded.
#[cfg(target_arch = "wasm32")]
pub type BoxedFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;

/// A place asset bytes can be read from. Reads are asynchronous and run on the
/// `space3d_tasks` runtime, never on the frame thread.
pub trait AssetSource: Send + Sync + 'static {
    /// Read the whole file at `path` (its [`AssetPath::path`] part).
    fn read(&self, path: &AssetPath) -> BoxedFuture<Result<Vec<u8>, AssetError>>;

    /// Local directory backing this source, if any. Used by hot reload.
    fn local_root(&self) -> Option<PathBuf> {
        None
    }
}

fn ready<T: 'static + Send>(v: T) -> BoxedFuture<T> {
    Box::pin(std::future::ready(v))
}

/// Reads files below a root directory with `tokio::fs` (native). On wasm every read
/// fails with [`AssetError::Unsupported`]; use [`HttpSource`] instead.
#[derive(Clone, Debug)]
pub struct FileSource {
    root: PathBuf,
}

impl FileSource {
    /// Serve files below `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Absolute/joined filesystem path for an asset path.
    pub fn full_path(&self, path: &AssetPath) -> PathBuf {
        self.root.join(path.path())
    }
}

impl AssetSource for FileSource {
    fn read(&self, path: &AssetPath) -> BoxedFuture<Result<Vec<u8>, AssetError>> {
        if path.escapes_root() || path.path().is_empty() {
            return ready(Err(AssetError::InvalidPath(path.to_string())));
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let full = self.full_path(path);
            let display = path.to_string();
            Box::pin(async move {
                tokio::fs::read(&full).await.map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => AssetError::NotFound(display),
                    _ => AssetError::io(display, e),
                })
            })
        }
        #[cfg(target_arch = "wasm32")]
        {
            ready(Err(AssetError::Unsupported(format!(
                "FileSource cannot read '{path}' in the browser; use HttpSource"
            ))))
        }
    }

    fn local_root(&self) -> Option<PathBuf> {
        cfg!(not(target_arch = "wasm32")).then(|| self.root.clone())
    }
}

/// Fetches files over HTTP relative to a base URL (web, via `window.fetch`).
///
/// The base may be relative (`"assets"`) so it resolves against the page URL. On native
/// targets reads fail with [`AssetError::Unsupported`] (use `space3d-net` for HTTP there).
#[derive(Clone, Debug)]
pub struct HttpSource {
    base_url: String,
}

impl HttpSource {
    /// Serve files below `base_url` (e.g. `"assets"` or `"https://cdn.example.com/game"`).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// URL for an asset path.
    pub fn url(&self, path: &AssetPath) -> String {
        if self.base_url.is_empty() {
            path.path().to_string()
        } else {
            format!("{}/{}", self.base_url, path.path())
        }
    }
}

impl AssetSource for HttpSource {
    fn read(&self, path: &AssetPath) -> BoxedFuture<Result<Vec<u8>, AssetError>> {
        let url = self.url(path);
        #[cfg(target_arch = "wasm32")]
        {
            Box::pin(web::fetch_bytes(url))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            ready(Err(AssetError::Unsupported(format!(
                "HttpSource is web-only (requested {url})"
            ))))
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use super::AssetError;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    fn js_err(url: &str, e: wasm_bindgen::JsValue) -> AssetError {
        AssetError::io(url, format!("{e:?}"))
    }

    pub(super) async fn fetch_bytes(url: String) -> Result<Vec<u8>, AssetError> {
        let promise = if let Some(window) = web_sys::window() {
            window.fetch_with_str(&url)
        } else if let Ok(worker) = js_sys::global().dyn_into::<web_sys::WorkerGlobalScope>() {
            worker.fetch_with_str(&url)
        } else {
            return Err(AssetError::Unsupported(
                "no window or worker global to fetch from".into(),
            ));
        };
        let resp = JsFuture::from(promise).await.map_err(|e| js_err(&url, e))?;
        let resp: web_sys::Response = resp.dyn_into().map_err(|e| js_err(&url, e))?;
        if !resp.ok() {
            return Err(if resp.status() == 404 {
                AssetError::NotFound(url)
            } else {
                AssetError::io(&url, format!("HTTP {} {}", resp.status(), resp.status_text()))
            });
        }
        let buf = JsFuture::from(resp.array_buffer().map_err(|e| js_err(&url, e))?)
            .await
            .map_err(|e| js_err(&url, e))?;
        Ok(js_sys::Uint8Array::new(&buf).to_vec())
    }
}

/// In-memory files, for tests, embedded assets (`include_bytes!`) and generated content.
/// Cloning shares the same storage, so you can keep a clone to mutate files after
/// registering it (then call [`crate::AssetServer::reload`]).
#[derive(Clone, Default)]
pub struct MemorySource {
    files: Arc<RwLock<HashMap<String, Arc<[u8]>>>>,
}

impl MemorySource {
    /// Empty source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder form of [`MemorySource::insert`].
    pub fn with_file(self, path: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.insert(path, bytes);
        self
    }

    /// Add or replace a file.
    pub fn insert(&self, path: &str, bytes: impl Into<Vec<u8>>) {
        let key = AssetPath::new(path).path().to_string();
        self.files.write().insert(key, Arc::from(bytes.into()));
    }

    /// Remove a file; returns whether it existed.
    pub fn remove(&self, path: &str) -> bool {
        self.files.write().remove(AssetPath::new(path).path()).is_some()
    }

    /// Whether a file exists.
    pub fn contains(&self, path: &str) -> bool {
        self.files.read().contains_key(AssetPath::new(path).path())
    }

    /// Copy of a file's bytes.
    pub fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.files.read().get(AssetPath::new(path).path()).map(|b| b.to_vec())
    }

    /// All stored paths (unordered).
    pub fn paths(&self) -> Vec<String> {
        self.files.read().keys().cloned().collect()
    }
}

impl AssetSource for MemorySource {
    fn read(&self, path: &AssetPath) -> BoxedFuture<Result<Vec<u8>, AssetError>> {
        let data = self.files.read().get(path.path()).cloned();
        let display = path.to_string();
        ready(data.map(|d| d.to_vec()).ok_or(AssetError::NotFound(display)))
    }
}

impl std::fmt::Debug for MemorySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySource")
            .field("files", &self.files.read().len())
            .finish()
    }
}

/// The platform's default source for `root`: [`FileSource`] on native, [`HttpSource`] on web.
///
/// On iOS a relative `root` is resolved against the app bundle (the directory of the
/// executable): apps start with `/` as their working directory, and `space3d build --target
/// ios` copies `assets/` into the bundle next to the executable.
pub fn default_source(root: &str) -> Arc<dyn AssetSource> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Arc::new(FileSource::new(platform_root(root)))
    }
    #[cfg(target_arch = "wasm32")]
    {
        Arc::new(HttpSource::new(root))
    }
}

/// Where a native asset root lives: `root` itself, except on iOS where a relative root is
/// inside the app bundle (see [`default_source`]).
#[cfg(not(target_arch = "wasm32"))]
fn platform_root(root: &str) -> PathBuf {
    #[cfg(target_os = "ios")]
    if std::path::Path::new(root).is_relative()
        && let Some(bundle) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    {
        return bundle.join(root);
    }
    PathBuf::from(root)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn memory_source_reads() {
        let m = MemorySource::new().with_file("./a/b.txt", b"hello".to_vec());
        assert!(m.contains("a/b.txt"));
        let got = futures::executor::block_on(m.read(&AssetPath::new("a/b.txt")));
        assert_eq!(got.unwrap(), b"hello");
        let missing = futures::executor::block_on(m.read(&AssetPath::new("nope")));
        assert!(matches!(missing, Err(AssetError::NotFound(_))));
    }

    #[test]
    fn file_source_rejects_escape_and_http_is_unsupported_natively() {
        let f = FileSource::new("assets");
        let r = futures::executor::block_on(f.read(&AssetPath::new("../secret")));
        assert!(matches!(r, Err(AssetError::InvalidPath(_))));
        let h = HttpSource::new("assets/");
        assert_eq!(h.url(&AssetPath::new("x.png")), "assets/x.png");
        let r = futures::executor::block_on(h.read(&AssetPath::new("x.png")));
        assert!(matches!(r, Err(AssetError::Unsupported(_))));
    }
}
