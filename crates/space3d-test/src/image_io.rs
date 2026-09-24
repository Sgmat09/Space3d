//! PNG encoding/decoding of [`ImageData`] (RGBA8, sRGB).

use space3d_core::ImageData;
use std::path::Path;

/// PNG I/O errors.
#[derive(Debug, thiserror::Error)]
pub enum ImageIoError {
    /// File access failed.
    #[error("{path}: {source}")]
    Io {
        /// Offending path.
        path: String,
        /// Underlying error.
        source: std::io::Error,
    },
    /// Encoding or decoding failed.
    #[error("PNG codec error: {0}")]
    Codec(String),
    /// Pixel buffer does not match `width * height * 4`.
    #[error("invalid image buffer: {width}x{height} with {len} bytes")]
    InvalidBuffer {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// Buffer length.
        len: usize,
    },
}

/// Encode as an RGBA8 PNG.
pub fn encode_png(image: &ImageData) -> Result<Vec<u8>, ImageIoError> {
    use image::ImageEncoder;
    let expected = image.width as usize * image.height as usize * 4;
    if image.pixels.len() != expected {
        return Err(ImageIoError::InvalidBuffer {
            width: image.width,
            height: image.height,
            len: image.pixels.len(),
        });
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            &image.pixels,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| ImageIoError::Codec(e.to_string()))?;
    Ok(out)
}

/// Decode any PNG into RGBA8.
pub fn decode_png(bytes: &[u8]) -> Result<ImageData, ImageIoError> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|e| ImageIoError::Codec(e.to_string()))?
        .into_rgba8();
    Ok(ImageData {
        width: img.width(),
        height: img.height(),
        pixels: img.into_raw(),
    })
}

/// Write a PNG file, creating parent directories.
pub fn save_png(path: impl AsRef<Path>, image: &ImageData) -> Result<(), ImageIoError> {
    let path = path.as_ref();
    let io = |source| ImageIoError::Io {
        path: path.display().to_string(),
        source,
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    std::fs::write(path, encode_png(image)?).map_err(io)
}

/// Read a PNG file.
pub fn load_png(path: impl AsRef<Path>) -> Result<ImageData, ImageIoError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|source| ImageIoError::Io {
        path: path.display().to_string(),
        source,
    })?;
    decode_png(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip() {
        let mut img = ImageData::checker(8, 2, [255, 0, 0, 255], [0, 0, 255, 128]);
        img.set(3, 4, [1, 2, 3, 4]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.png");
        save_png(&path, &img).unwrap();
        assert_eq!(load_png(&path).unwrap(), img);
        assert!(matches!(
            load_png(dir.path().join("missing.png")),
            Err(ImageIoError::Io { .. })
        ));
        assert!(matches!(decode_png(b"not a png"), Err(ImageIoError::Codec(_))));
        let bad = ImageData {
            width: 2,
            height: 2,
            pixels: vec![0; 3],
        };
        assert!(matches!(encode_png(&bad), Err(ImageIoError::InvalidBuffer { .. })));
    }
}
