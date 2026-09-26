use crate::error::AssetError;
use crate::loader::{AssetLoader, LoadContext};
use serde::{Deserialize, Serialize};
use space3d_core::ImageData;

/// Six decoded cube faces in wgpu order (+X, −X, +Y, −Y, +Z, −Z) and the convention they were
/// authored in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CubemapData {
    /// The faces.
    pub faces: [ImageData; 6],
    /// How to interpret them.
    pub convention: CubeConvention,
}

/// Cube face conventions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CubeConvention {
    /// wgpu / OpenGL order and orientation.
    #[default]
    Wgpu,
    /// three.js `CubeTextureLoader` (mirrored: ±X swapped, faces flipped horizontally).
    ThreeJs,
}

/// `.skybox` JSON manifest:
/// `{"faces": ["right.jpg", "left.jpg", "top.jpg", "bottom.jpg", "front.jpg", "back.jpg"],
/// "convention": "threejs"}` (faces relative to the manifest; convention optional).
#[derive(Deserialize)]
struct Manifest {
    faces: [String; 6],
    #[serde(default)]
    convention: CubeConvention,
}

fn manifest(bytes: &[u8], ctx: &LoadContext) -> Result<Manifest, AssetError> {
    serde_json::from_slice(bytes).map_err(|e| AssetError::decode(ctx.path(), e))
}

/// Loads a `.skybox` JSON manifest and its six face images into [`CubemapData`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SkyboxLoader;

impl AssetLoader for SkyboxLoader {
    type Asset = CubemapData;

    fn extensions(&self) -> &[&str] {
        &["skybox"]
    }

    fn dependencies(&self, bytes: &[u8], ctx: &LoadContext) -> Result<Vec<String>, AssetError> {
        Ok(manifest(bytes, ctx)?.faces.to_vec())
    }

    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<CubemapData, AssetError> {
        let m = manifest(bytes, ctx)?;
        let mut faces = Vec::with_capacity(6);
        for uri in &m.faces {
            let data = ctx
                .dependency(uri)
                .ok_or_else(|| AssetError::decode(ctx.path(), format!("missing face {uri}")))?;
            let ext = uri.rsplit('.').next();
            faces.push(crate::edit::texture::decode_image_with_hint(data, ext)?);
        }
        let faces: [ImageData; 6] = faces.try_into().expect("six faces");
        Ok(CubemapData {
            faces,
            convention: m.convention,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::texture::encode_png;

    #[test]
    fn loads_a_manifest_and_its_faces() {
        let json = br#"{"faces": ["a.png", "b.png", "c.png", "d.png", "e.png", "f.png"], "convention": "threejs"}"#;
        let mut ctx = LoadContext::new("sky/space.skybox");
        assert_eq!(SkyboxLoader.dependencies(json, &ctx).unwrap().len(), 6);
        for (i, name) in ["a", "b", "c", "d", "e", "f"].iter().enumerate() {
            let png = encode_png(&ImageData::filled(2, 2, [i as u8, 0, 0, 255])).unwrap();
            ctx = ctx.with_dependency(&format!("{name}.png"), png);
        }
        let cube = SkyboxLoader.load(json, &ctx).unwrap();
        assert_eq!(cube.convention, CubeConvention::ThreeJs);
        assert_eq!(cube.faces[5].get(0, 0), [5, 0, 0, 255]);
        let missing = LoadContext::new("sky/space.skybox");
        assert!(SkyboxLoader.load(json, &missing).is_err());
        assert!(SkyboxLoader.dependencies(b"{}", &missing).is_err());
    }
}
