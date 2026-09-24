use crate::edit::texture::decode_image_with_hint;
use crate::error::AssetError;
use crate::loader::{AssetLoader, LoadContext};
use space3d_core::ImageData;

/// Decodes common image formats (PNG, JPEG, BMP, TGA, GIF, HDR, ICO, WebP, QOI, PNM, DDS,
/// TIFF) into RGBA8 [`ImageData`] via the `image` crate.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageLoader;

impl AssetLoader for ImageLoader {
    type Asset = ImageData;

    fn extensions(&self) -> &[&str] {
        &[
            "png", "jpg", "jpeg", "bmp", "tga", "gif", "hdr", "ico", "webp", "qoi", "pnm", "pbm", "pgm", "ppm", "pam",
            "dds", "tif", "tiff",
        ]
    }

    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<ImageData, AssetError> {
        let ext = ctx.path().extension();
        decode_image_with_hint(bytes, ext.as_deref()).map_err(|e| match e {
            AssetError::Decode { message, .. } => AssetError::decode(ctx.path(), message),
            other => other,
        })
    }
}
