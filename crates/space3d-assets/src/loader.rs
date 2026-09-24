//! Loader trait for decoding bytes into assets, keyed by file extension.

use crate::assets::Asset;
use crate::error::AssetError;
use crate::path::AssetPath;
use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Decodes raw bytes into an asset. Register with
/// [`crate::AssetApp::register_asset_loader`] or [`crate::AssetServer::register_loader`].
///
/// `load` runs on the compute pool (`space3d_tasks::spawn_compute`), never on the frame
/// thread, so it may be CPU-heavy but must not block on I/O. Files the asset refers to
/// (glTF buffers, MTL libraries, textures) are requested up front through
/// [`AssetLoader::dependencies`] and read asynchronously by the server.
///
/// ```
/// use space3d_assets::{AssetLoader, AssetError, LoadContext};
/// struct TextLoader;
/// impl AssetLoader for TextLoader {
///     type Asset = String;
///     fn extensions(&self) -> &[&str] { &["txt"] }
///     fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<String, AssetError> {
///         String::from_utf8(bytes.to_vec()).map_err(|e| AssetError::decode(ctx.path(), e))
///     }
/// }
/// ```
pub trait AssetLoader: Send + Sync + 'static {
    /// The asset type produced.
    type Asset: Asset;

    /// Lower-case extensions (without the dot) this loader handles, e.g. `["png", "jpg"]`.
    fn extensions(&self) -> &[&str];

    /// Relative URIs of extra files needed to decode `bytes`. Called repeatedly (up to
    /// [`MAX_DEPENDENCY_ROUNDS`] times) with the dependencies read so far available through
    /// `ctx`, so a loader can discover nested dependencies (OBJ → MTL → textures).
    /// Dependencies that fail to read are reported as missing (`None`) to `load`.
    fn dependencies(&self, bytes: &[u8], ctx: &LoadContext) -> Result<Vec<String>, AssetError> {
        let _ = (bytes, ctx);
        Ok(Vec::new())
    }

    /// Decode `bytes` (the file at `ctx.path()`).
    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<Self::Asset, AssetError>;
}

/// Maximum number of dependency-discovery rounds per asset.
pub const MAX_DEPENDENCY_ROUNDS: usize = 8;

/// Information available to a loader: the asset's path and its dependencies' bytes.
#[derive(Clone, Debug, Default)]
pub struct LoadContext {
    path: AssetPath,
    pub(crate) dependencies: HashMap<AssetPath, Option<Vec<u8>>>,
}

impl LoadContext {
    /// Context for decoding the asset at `path` with no dependencies loaded yet.
    pub fn new(path: impl Into<AssetPath>) -> Self {
        Self {
            path: path.into(),
            dependencies: HashMap::new(),
        }
    }

    /// Builder: provide a dependency's bytes (URI relative to the asset, as the loader
    /// would request it). Handy for decoding without the server.
    pub fn with_dependency(mut self, uri: &str, bytes: impl Into<Vec<u8>>) -> Self {
        let p = self.resolve(uri);
        self.dependencies.insert(p, Some(bytes.into()));
        self
    }

    /// Path of the asset being loaded.
    pub fn path(&self) -> &AssetPath {
        &self.path
    }

    /// Resolve a URI relative to the asset.
    pub fn resolve(&self, uri: &str) -> AssetPath {
        self.path.resolve(uri)
    }

    /// Bytes of a dependency, if it was requested and read successfully.
    pub fn dependency(&self, uri: &str) -> Option<&[u8]> {
        self.dependencies.get(&self.resolve(uri))?.as_deref()
    }

    /// Whether a dependency was already requested (successfully or not).
    pub fn was_requested(&self, uri: &str) -> bool {
        self.dependencies.contains_key(&self.resolve(uri))
    }

    /// Resolved paths of all requested dependencies.
    pub fn dependency_paths(&self) -> impl Iterator<Item = &AssetPath> {
        self.dependencies.keys()
    }
}

pub(crate) type ErasedAsset = Box<dyn Any + Send + Sync>;

/// Object-safe view of an [`AssetLoader`].
pub(crate) trait ErasedLoader: Send + Sync + 'static {
    fn asset_type(&self) -> TypeId;
    fn extensions(&self) -> &[&str];
    fn dependencies(&self, bytes: &[u8], ctx: &LoadContext) -> Result<Vec<String>, AssetError>;
    fn load_erased(&self, bytes: &[u8], ctx: &LoadContext) -> Result<ErasedAsset, AssetError>;
}

impl<L: AssetLoader> ErasedLoader for L {
    fn asset_type(&self) -> TypeId {
        TypeId::of::<L::Asset>()
    }
    fn extensions(&self) -> &[&str] {
        AssetLoader::extensions(self)
    }
    fn dependencies(&self, bytes: &[u8], ctx: &LoadContext) -> Result<Vec<String>, AssetError> {
        AssetLoader::dependencies(self, bytes, ctx)
    }
    fn load_erased(&self, bytes: &[u8], ctx: &LoadContext) -> Result<ErasedAsset, AssetError> {
        Ok(Box::new(self.load(bytes, ctx)?))
    }
}
