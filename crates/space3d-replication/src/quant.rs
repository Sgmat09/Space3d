//! Quantization used by the hot codecs. Quantized values are what both sides simulate on
//! (see `ComponentRule::quantize_authoritative`), so misprediction checks compare exact bytes.

use crate::mapping::{DecodeCtx, EncodeCtx};
use crate::registry::NetCodec;
use crate::wire::{DecodeError, Reader, Writer};
use space3d_core::Transform;
use space3d_core::glam::Quat;

/// `v / resolution` rounded to the nearest integer (saturating).
pub fn quantize_f32(v: f32, resolution: f32) -> i32 {
    todo!()
}

/// Inverse of [`quantize_f32`].
pub fn dequantize_f32(q: i32, resolution: f32) -> f32 {
    todo!()
}

/// Smallest-three quaternion: 2-bit index of the dropped largest component + three
/// components in `bits` bits each (`bits <= 10`, so the result fits in 32 bits). The sign is
/// normalised so the dropped component is positive.
pub fn encode_quat_smallest_three(q: Quat, bits: u8) -> u32 {
    todo!()
}

/// Inverse of [`encode_quat_smallest_three`] (renormalised).
pub fn decode_quat_smallest_three(packed: u32, bits: u8) -> Quat {
    todo!()
}

/// How [`TransformCodec`] handles `scale`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleMode {
    /// Never sent (decoded as `Vec3::ONE`).
    Omit,
    /// One flag bit; three quantized floats only when scale != 1.
    OmitIfOne,
    /// Always three quantized floats.
    Full,
}

/// Quantized `Transform` codec (the default for `"core.transform"`):
/// `u8 flags | vari32 × 3 translation (in units of resolution) | u32 smallest-three rotation |
/// [vari32 × 3 scale in 1/1024]`. About 11–13 bytes for a moving entity at 1 mm resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformCodec {
    /// Translation resolution in metres (default 0.001).
    pub resolution: f32,
    /// Bits per smallest-three component (default 10 → ~0.1° max error).
    pub rot_bits: u8,
    /// Scale handling (default [`ScaleMode::OmitIfOne`]).
    pub scale: ScaleMode,
}

impl Default for TransformCodec {
    fn default() -> Self {
        Self {
            resolution: 0.001,
            rot_bits: 10,
            scale: ScaleMode::OmitIfOne,
        }
    }
}

impl NetCodec<Transform> for TransformCodec {
    fn encode(&self, value: &Transform, w: &mut Writer, ctx: &EncodeCtx<'_>) {
        todo!()
    }

    fn decode(&self, r: &mut Reader<'_>, ctx: &mut DecodeCtx<'_>) -> Result<Transform, DecodeError> {
        todo!()
    }

    fn schema_id(&self) -> String {
        format!(
            "transform:res={}:rot={}:scale={:?}",
            self.resolution, self.rot_bits, self.scale
        )
    }
}
