use crate::error::AssetError;
use crate::loader::{AssetLoader, LoadContext};
use space3d_core::HdrImageData;

/// Decodes Radiance `.hdr` images into linear float [`HdrImageData`] (HDR environments and
/// skies; `ImageLoader` decodes the same files to 8-bit `ImageData`).
#[derive(Clone, Copy, Debug, Default)]
pub struct HdrImageLoader;

impl AssetLoader for HdrImageLoader {
    type Asset = HdrImageData;

    fn extensions(&self) -> &[&str] {
        &["hdr"]
    }

    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<HdrImageData, AssetError> {
        let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Hdr)
            .map_err(|e| AssetError::decode(ctx.path(), e))?
            .into_rgba32f();
        let (w, h) = img.dimensions();
        let pixels = img.pixels().map(|p| p.0).collect();
        Ok(HdrImageData::new(w, h, pixels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_radiance_hdr() {
        let mut bytes = Vec::new();
        let img = image::Rgb32FImage::from_pixel(3, 2, image::Rgb([8.0, 0.5, 0.25]));
        image::DynamicImage::ImageRgb32F(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Hdr)
            .unwrap();
        let hdr = HdrImageLoader.load(&bytes, &LoadContext::new("sky.hdr")).unwrap();
        assert_eq!((hdr.width, hdr.height), (3, 2));
        let [r, g, b, a] = hdr.get(2, 1);
        assert!((r - 8.0).abs() < 0.1 && (g - 0.5).abs() < 0.01 && (b - 0.25).abs() < 0.01 && a == 1.0);
        assert!(HdrImageLoader.load(b"nope", &LoadContext::new("x.hdr")).is_err());
    }
}
