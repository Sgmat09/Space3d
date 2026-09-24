//! Built-in loaders: images (`image` crate), glTF 2.0 (`gltf`), Wavefront OBJ (`tobj`).

mod gltf;
mod image;
mod obj;

pub use self::gltf::{GltfLoader, load_gltf};
pub use self::image::ImageLoader;
pub use self::obj::{ObjLoader, load_obj};

use crate::assets::Asset;
use crate::error::AssetError;
use crate::loader::{AssetLoader, LoadContext};
use crate::model::ModelData;
use crate::server::AssetServer;
use space3d_core::MeshData;

/// Adapts a [`ModelData`] loader into a [`MeshData`] loader by baking all node transforms
/// into one mesh ([`ModelData::merged_mesh`]).
#[derive(Clone, Debug, Default)]
pub struct MeshLoader<L>(pub L);

impl<L: AssetLoader<Asset = ModelData>> AssetLoader for MeshLoader<L> {
    type Asset = MeshData;
    fn extensions(&self) -> &[&str] {
        self.0.extensions()
    }
    fn dependencies(&self, bytes: &[u8], ctx: &LoadContext) -> Result<Vec<String>, AssetError> {
        self.0.dependencies(bytes, ctx)
    }
    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<MeshData, AssetError> {
        Ok(self.0.load(bytes, ctx)?.merged_mesh())
    }
}

/// Register the built-in loaders on a server:
/// * [`ImageLoader`] → `ImageData`
/// * [`GltfLoader`], [`ObjLoader`] → [`ModelData`]
/// * [`MeshLoader`] over glTF/OBJ (without textures) → `MeshData`
pub fn register_builtin_loaders(server: &mut AssetServer) {
    server.register_loader(ImageLoader);
    server.register_loader(GltfLoader::default());
    server.register_loader(ObjLoader::default());
    server.register_loader(MeshLoader(GltfLoader { load_images: false }));
    server.register_loader(MeshLoader(ObjLoader { load_materials: false }));
}

/// Decode an asset synchronously with a loader, resolving no external dependencies beyond
/// those already in `ctx`. Useful in tools and tests.
pub fn load_sync<L: AssetLoader>(loader: &L, bytes: &[u8], ctx: &LoadContext) -> Result<L::Asset, AssetError>
where
    L::Asset: Asset,
{
    loader.load(bytes, ctx)
}

/// Shared post-processing: guarantee `uvs`/`normals` match `positions` (flat normals are
/// generated when missing) and indices form whole triangles.
pub(crate) fn finish_mesh(mesh: &mut MeshData) {
    mesh.indices.truncate(mesh.indices.len() / 3 * 3);
    let n = mesh.positions.len();
    if mesh.uvs.len() != n {
        mesh.uvs = vec![[0.0; 2]; n];
    }
    if !mesh.colors.is_empty() && mesh.colors.len() != n {
        mesh.colors.clear();
    }
    if mesh.normals.len() != n {
        mesh.normals.clear();
        crate::edit::mesh::recalculate_flat_normals(mesh);
    }
}

/// Magenta/black checker used when a referenced texture can't be read or decoded.
pub(crate) fn missing_texture() -> space3d_core::ImageData {
    space3d_core::ImageData::checker(2, 1, [255, 0, 255, 255], [0, 0, 0, 255])
}

/// Decode `%XX` escapes (used by glTF/OBJ URIs).
pub(crate) fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b.bin"), "a b.bin");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("x%2"), "x%2");
    }
}
