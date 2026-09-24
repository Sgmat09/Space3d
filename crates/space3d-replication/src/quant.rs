//! Quantization used by the hot codecs. Quantized values are what both sides simulate on
//! (see `ComponentRule::quantize_authoritative`), so misprediction checks compare exact bytes.

use crate::mapping::{DecodeCtx, EncodeCtx};
use crate::registry::NetCodec;
use crate::wire::{DecodeError, Reader, Writer};
use space3d_core::Transform;
use space3d_core::glam::{Quat, Vec3};

/// Steps per unit for `resolution`, snapped to an integer when `1/resolution` is one up to
/// `f32` noise (0.001 → exactly 1000), so decimal resolutions dequantize exactly.
fn steps_per_unit(resolution: f32) -> f64 {
    let inv = 1.0 / f64::from(resolution);
    let r = inv.round();
    if r >= 1.0 && (inv - r).abs() <= 1e-6 * inv {
        r
    } else {
        inv
    }
}

/// `v / resolution` rounded to the nearest integer (saturating; NaN → 0).
pub fn quantize_f32(v: f32, resolution: f32) -> i32 {
    let q = (f64::from(v) * steps_per_unit(resolution)).round();
    if q.is_nan() {
        0
    } else {
        // `as` saturates for out-of-range floats.
        q as i32
    }
}

/// Inverse of [`quantize_f32`].
pub fn dequantize_f32(q: i32, resolution: f32) -> f32 {
    (f64::from(q) / steps_per_unit(resolution)) as f32
}

const INV_SQRT2: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Smallest-three quaternion: 2-bit index of the dropped largest component + three
/// components in `bits` bits each (`bits <= 10`, so the result fits in 32 bits). The sign is
/// normalised so the dropped component is positive.
pub fn encode_quat_smallest_three(q: Quat, bits: u8) -> u32 {
    encode_quat_packed(q, bits.clamp(2, 10)) as u32
}

/// Inverse of [`encode_quat_smallest_three`] (renormalised).
pub fn decode_quat_smallest_three(packed: u32, bits: u8) -> Quat {
    decode_quat_packed(u64::from(packed), bits.clamp(2, 10))
}

/// Bytes used by [`encode_quat_packed`] for `bits` per component (`⌈(2 + 3·bits) / 8⌉`).
pub fn quat_packed_len(bits: u8) -> usize {
    (2 + 3 * usize::from(bits.clamp(2, MAX_ROT_BITS))).div_ceil(8)
}

/// Largest supported bits per smallest-three component.
pub const MAX_ROT_BITS: u8 = 15;

/// Smallest-three packed into the low `2 + 3·bits` bits of a `u64` (`bits` in `2..=15`):
/// the dropped-component index is in the top two of those bits.
pub fn encode_quat_packed(q: Quat, bits: u8) -> u64 {
    let bits = u32::from(bits.clamp(2, MAX_ROT_BITS));
    let max = ((1u32 << bits) - 1) as f32;
    let mut c = q.to_array();
    let len2: f32 = c.iter().map(|v| v * v).sum();
    if !len2.is_finite() || len2 < 1e-12 {
        c = Quat::IDENTITY.to_array();
    } else {
        let inv = len2.sqrt().recip();
        for v in &mut c {
            *v *= inv;
        }
    }
    let mut largest = 0usize;
    for i in 1..4 {
        if c[i].abs() > c[largest].abs() {
            largest = i;
        }
    }
    if c[largest] < 0.0 {
        for v in &mut c {
            *v = -*v;
        }
    }
    let mut packed = (largest as u64) << (3 * bits);
    let mut shift = 2 * bits;
    for (i, v) in c.iter().enumerate() {
        if i == largest {
            continue;
        }
        let n = ((v.clamp(-INV_SQRT2, INV_SQRT2) / INV_SQRT2 + 1.0) * 0.5 * max).round() as u64;
        packed |= n.min(max as u64) << shift;
        shift = shift.saturating_sub(bits);
    }
    packed
}

/// Inverse of [`encode_quat_packed`] (renormalised).
pub fn decode_quat_packed(packed: u64, bits: u8) -> Quat {
    let bits = u32::from(bits.clamp(2, MAX_ROT_BITS));
    let mask = (1u64 << bits) - 1;
    let max = mask as f32;
    let largest = ((packed >> (3 * bits)) & 3) as usize;
    let mut c = [0f32; 4];
    let mut shift = 2 * bits;
    let mut sum = 0.0f32;
    for (i, slot) in c.iter_mut().enumerate() {
        if i == largest {
            continue;
        }
        let n = (packed >> shift) & mask;
        let v = (n as f32 / max * 2.0 - 1.0) * INV_SQRT2;
        *slot = v;
        sum += v * v;
        shift = shift.saturating_sub(bits);
    }
    c[largest] = (1.0 - sum).max(0.0).sqrt();
    let q = Quat::from_array(c);
    let len = q.length();
    if len.is_finite() && len > 1e-6 {
        q / len
    } else {
        Quat::IDENTITY
    }
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

/// Scale resolution of [`TransformCodec`] (1/1024).
pub const SCALE_RESOLUTION: f32 = 1.0 / 1024.0;

/// Quantized `Transform` codec (the default for `"core.transform"`):
/// `u8 flags | vari32 × 3 translation (in units of resolution) | smallest-three rotation in
/// ⌈(2 + 3·rot_bits)/8⌉ bytes LE (4 at the default 10 bits) | [vari32 × 3 scale in 1/1024]`.
/// About 11–13 bytes for a moving entity at 1 mm resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformCodec {
    /// Translation resolution in metres (default 0.001).
    pub resolution: f32,
    /// Bits per smallest-three component, 2..=15 (default 10: mean error ≈ 0.05°, worst case
    /// ≈ 0.17°; 11 bits: worst case < 0.1° for one more byte).
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

const FLAG_SCALE: u8 = 0x01;

impl NetCodec<Transform> for TransformCodec {
    fn encode(&self, value: &Transform, w: &mut Writer, _ctx: &EncodeCtx<'_>) {
        let s = value.scale;
        let qs = [
            quantize_f32(s.x, SCALE_RESOLUTION),
            quantize_f32(s.y, SCALE_RESOLUTION),
            quantize_f32(s.z, SCALE_RESOLUTION),
        ];
        let one = quantize_f32(1.0, SCALE_RESOLUTION);
        let with_scale = match self.scale {
            ScaleMode::Omit => false,
            ScaleMode::OmitIfOne => qs != [one; 3],
            ScaleMode::Full => true,
        };
        w.u8(if with_scale { FLAG_SCALE } else { 0 });
        let t = value.translation;
        w.vari32(quantize_f32(t.x, self.resolution));
        w.vari32(quantize_f32(t.y, self.resolution));
        w.vari32(quantize_f32(t.z, self.resolution));
        let packed = encode_quat_packed(value.rotation, self.rot_bits);
        w.raw(&packed.to_le_bytes()[..quat_packed_len(self.rot_bits)]);
        if with_scale {
            for q in qs {
                w.vari32(q);
            }
        }
    }

    fn decode(&self, r: &mut Reader<'_>, _ctx: &mut DecodeCtx<'_>) -> Result<Transform, DecodeError> {
        let flags = r.u8()?;
        if flags & !FLAG_SCALE != 0 {
            return Err(DecodeError::Invalid("transform flags"));
        }
        let translation = Vec3::new(
            dequantize_f32(r.vari32()?, self.resolution),
            dequantize_f32(r.vari32()?, self.resolution),
            dequantize_f32(r.vari32()?, self.resolution),
        );
        let mut raw = [0u8; 8];
        let n = quat_packed_len(self.rot_bits);
        raw[..n].copy_from_slice(r.raw(n)?);
        let rotation = decode_quat_packed(u64::from_le_bytes(raw), self.rot_bits);
        let scale = if flags & FLAG_SCALE != 0 {
            Vec3::new(
                dequantize_f32(r.vari32()?, SCALE_RESOLUTION),
                dequantize_f32(r.vari32()?, SCALE_RESOLUTION),
                dequantize_f32(r.vari32()?, SCALE_RESOLUTION),
            )
        } else {
            Vec3::ONE
        };
        Ok(Transform {
            translation,
            rotation,
            scale,
        })
    }

    fn schema_id(&self) -> String {
        format!(
            "transform:res={}:rot={}:scale={:?}",
            self.resolution, self.rot_bits, self.scale
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn angle_deg(a: Quat, b: Quat) -> f32 {
        let d = a.dot(b).abs().min(1.0);
        (2.0 * d.acos()).to_degrees()
    }

    fn roundtrip(codec: &TransformCodec, t: &Transform) -> (Vec<u8>, Transform) {
        let mut w = Writer::new();
        codec.encode(t, &mut w, &EncodeCtx::empty());
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        let back = codec.decode(&mut r, &mut DecodeCtx::empty()).unwrap();
        assert!(r.is_empty());
        (bytes, back)
    }

    #[test]
    fn quantize_basics() {
        assert_eq!(quantize_f32(1.0, 0.001), 1000);
        assert_eq!(quantize_f32(-0.0014, 0.001), -1);
        assert_eq!(quantize_f32(f32::NAN, 0.001), 0);
        assert_eq!(quantize_f32(f32::INFINITY, 0.001), i32::MAX);
        assert_eq!(dequantize_f32(1500, 0.001), 1.5);
    }

    #[test]
    fn transform_codec_sizes() {
        let codec = TransformCodec::default();
        let (bytes, back) = roundtrip(&codec, &Transform::from_xyz(1.25, 0.0, -3.5));
        assert!(bytes.len() <= 14, "{} bytes", bytes.len());
        assert_eq!(back.scale, Vec3::ONE);
        let scaled = Transform::from_xyz(0.0, 0.0, 0.0).with_scale(Vec3::splat(2.0));
        let (bytes2, back2) = roundtrip(&codec, &scaled);
        assert!(bytes2.len() > bytes.len());
        assert_eq!(back2.scale, Vec3::splat(2.0));
        // Garbage flags are rejected, never panic.
        let mut r = Reader::new(&[0xff]);
        assert!(codec.decode(&mut r, &mut DecodeCtx::empty()).is_err());
    }

    #[test]
    fn smallest_three_mean_error_below_a_tenth_degree() {
        let mut rng = crate::transport::conditioner::SplitMix64::new(1);
        let mut sum = 0.0f32;
        let n = 5000;
        for _ in 0..n {
            let axis = Vec3::new(rng.next_f32() - 0.5, rng.next_f32() - 0.5, rng.next_f32() - 0.5);
            if axis.length() < 0.01 {
                continue;
            }
            let q = Quat::from_axis_angle(axis.normalize(), (rng.next_f32() - 0.5) * 12.0);
            sum += angle_deg(q, decode_quat_smallest_three(encode_quat_smallest_three(q, 10), 10));
        }
        assert!(sum / (n as f32) < 0.1, "mean {}°", sum / (n as f32));
        assert_eq!(quat_packed_len(10), 4);
        assert_eq!(quat_packed_len(11), 5);
    }

    #[test]
    fn degenerate_quaternions_decode_to_identity() {
        for q in [
            Quat::from_xyzw(0.0, 0.0, 0.0, 0.0),
            Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0),
        ] {
            let back = decode_quat_smallest_three(encode_quat_smallest_three(q, 10), 10);
            assert!(angle_deg(back, Quat::IDENTITY) < 0.2);
        }
    }

    proptest! {
        #[test]
        fn prop_translation_error_bounded(x in -10_000.0f32..10_000.0, y in -100.0f32..100.0, z in -10_000.0f32..10_000.0) {
            let codec = TransformCodec::default();
            let t = Transform::from_xyz(x, y, z);
            let (bytes, back) = roundtrip(&codec, &t);
            prop_assert!((back.translation - t.translation).abs().max_element() <= 0.0005 + 1e-3 * 1e-3 * x.abs().max(z.abs()));
            // Deterministic: re-encoding the decoded value gives the same bytes.
            let (bytes2, _) = roundtrip(&codec, &back);
            prop_assert_eq!(bytes, bytes2);
        }

        #[test]
        fn prop_smallest_three_error_below_bound(ax in -1.0f32..1.0, ay in -1.0f32..1.0, az in -1.0f32..1.0, angle in -6.3f32..6.3) {
            let axis = Vec3::new(ax, ay, az);
            prop_assume!(axis.length() > 0.01);
            let q = Quat::from_axis_angle(axis.normalize(), angle);
            let back = decode_quat_smallest_three(encode_quat_smallest_three(q, 10), 10);
            // 10 bits per component: ≈ 0.05° typical, < 0.2° worst case.
            prop_assert!(angle_deg(q, back) < 0.2, "error {}°", angle_deg(q, back));
            prop_assert!((back.length() - 1.0).abs() < 1e-4);
            // 11 bits (one more byte): < 0.1° worst case.
            let back11 = decode_quat_packed(encode_quat_packed(q, 11), 11);
            prop_assert!(angle_deg(q, back11) < 0.1, "error {}° at 11 bits", angle_deg(q, back11));
        }

        #[test]
        fn prop_transform_decoder_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..32)) {
            let codec = TransformCodec { scale: ScaleMode::Full, ..Default::default() };
            let _ = codec.decode(&mut Reader::new(&bytes), &mut DecodeCtx::empty());
        }
    }
}
