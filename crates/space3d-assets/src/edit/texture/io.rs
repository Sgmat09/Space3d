//! Image encode/decode helpers.

use crate::error::AssetError;
use image::{ExtendedColorType, ImageEncoder, ImageFormat, ImageReader, codecs::png::PngEncoder};
use space3d_core::ImageData;
use std::io::Cursor;

/// Decode any supported format (detected from the bytes) into RGBA8.
pub fn decode_image(bytes: &[u8]) -> Result<ImageData, AssetError> {
    decode_image_with_hint(bytes, None)
}

/// Decode into RGBA8, using `extension` (e.g. `"tga"`) when the format can't be detected
/// from the bytes.
pub fn decode_image_with_hint(bytes: &[u8], extension: Option<&str>) -> Result<ImageData, AssetError> {
    let err = |e: &dyn std::fmt::Display| AssetError::decode("<image>", e);
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| err(&e))?;
    if reader.format().is_none()
        && let Some(f) = extension.and_then(ImageFormat::from_extension)
    {
        reader.set_format(f);
    }
    let rgba = reader.decode().map_err(|e| err(&e))?.into_rgba8();
    Ok(ImageData {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

/// Encode as PNG (RGBA8).
pub fn encode_png(img: &ImageData) -> Result<Vec<u8>, AssetError> {
    let expected = img.width as usize * img.height as usize * 4;
    if img.pixels.len() != expected {
        return Err(AssetError::Other(format!(
            "image has {} bytes, expected {expected} for {}×{}",
            img.pixels.len(),
            img.width,
            img.height
        )));
    }
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(&img.pixels, img.width, img.height, ExtendedColorType::Rgba8)
        .map_err(|e| AssetError::Other(format!("PNG encode failed: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::texture::white_noise;

    #[test]
    fn png_roundtrip() {
        let mut img = white_noise(13, 7, 42);
        img.set(0, 0, [1, 2, 3, 4]);
        let png = encode_png(&img).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        assert_eq!(decode_image(&png).unwrap(), img);
        assert!(decode_image(b"not an image").is_err());
        let bad = ImageData {
            width: 2,
            height: 2,
            pixels: vec![0; 3],
        };
        assert!(encode_png(&bad).is_err());
    }
}
