//! Bit packing shared with the shaders: IEEE half floats (`Float16x4` vertex attributes) and
//! octahedral unit vectors in two snorm16 (`misc` words of a particle).

use space3d_core::glam::{Vec2, Vec3};

/// Largest finite half float.
pub(crate) const F16_MAX: f32 = 65_504.0;

/// `x` as IEEE binary16 bits, rounded to nearest even; values beyond ±65504 clamp to it and NaN
/// becomes 0 (instance data must stay finite on every backend).
pub(crate) fn f16_bits(x: f32) -> u16 {
    if x.is_nan() {
        return 0;
    }
    let bits = x.clamp(-F16_MAX, F16_MAX).to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x7f_ffff;
    let e = exp - 127 + 15;
    if e <= 0 {
        // Subnormal half (or zero).
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = m >> shift;
        let rem = m & ((1 << shift) - 1);
        let halfway = 1 << (shift - 1);
        let r = if rem > halfway || (rem == halfway && half & 1 == 1) {
            half + 1
        } else {
            half
        };
        return sign | r as u16;
    }
    let mut h = ((e as u32) << 10) | (man >> 13);
    let rem = man & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && h & 1 == 1) {
        h += 1; // a carry into the exponent is the correct rounding
    }
    sign | h as u16
}

/// The value of half-float bits (tests).
#[cfg(test)]
pub(crate) fn f16_value(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let man = (h & 0x3ff) as f32;
    match exp {
        0 => sign * man * 2f32.powi(-24),
        31 => sign * f32::INFINITY,
        _ => sign * (1.0 + man / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// Octahedral encoding of a unit vector into two snorm16 (`x` low, `y` high).
pub(crate) fn oct_encode(n: Vec3) -> u32 {
    let n = n.normalize_or(Vec3::Y);
    let l1 = n.x.abs() + n.y.abs() + n.z.abs();
    let mut p = Vec2::new(n.x, n.y) / l1;
    if n.z < 0.0 {
        let s = Vec2::new(p.x.signum(), p.y.signum());
        p = (Vec2::ONE - Vec2::new(p.y.abs(), p.x.abs())) * s;
    }
    let q = |v: f32| ((v.clamp(-1.0, 1.0) * 32_767.0).round() as i16) as u16 as u32;
    q(p.x) | q(p.y) << 16
}

/// Inverse of [`oct_encode`].
pub(crate) fn oct_decode(w: u32) -> Vec3 {
    let d = |b: u32| ((b & 0xffff) as u16 as i16) as f32 / 32_767.0;
    let p = Vec2::new(d(w), d(w >> 16));
    let mut n = Vec3::new(p.x, p.y, 1.0 - p.x.abs() - p.y.abs());
    if n.z < 0.0 {
        let s = Vec2::new(n.x.signum(), n.y.signum());
        let q = (Vec2::ONE - Vec2::new(n.y.abs(), n.x.abs())) * s;
        n.x = q.x;
        n.y = q.y;
    }
    n.normalize_or(Vec3::Y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats_round_to_nearest_even() {
        for (x, h) in [
            (0.0f32, 0x0000u16),
            (-0.0, 0x8000),
            (1.0, 0x3c00),
            (0.5, 0x3800),
            (-2.0, 0xc000),
            (65_504.0, 0x7bff),
            (1e9, 0x7bff),
            (-1e9, 0xfbff),
            (6.0e-8, 0x0001),
            (1e-9, 0x0000),
            // 1 + 2⁻¹¹ and 1 + 3·2⁻¹¹ are exactly halfway: ties go to the even mantissa.
            (1.0 + 1.0 / 2048.0, 0x3c00),
            (1.0 + 3.0 / 2048.0, 0x3c02),
            (0.333_333_34, 0x3555),
            (f32::NAN, 0),
        ] {
            let got = f16_bits(x);
            assert_eq!(got, h, "{x}: {got:#06x}");
        }
        for x in [0.1f32, 3.75, -12.5, 1000.0, 1e-5, 0.999] {
            let back = f16_value(f16_bits(x));
            assert!((back - x).abs() <= x.abs() * 1e-3 + 1e-7, "{x} → {back}");
        }
    }

    #[test]
    fn octahedral_round_trip() {
        for n in [
            Vec3::X,
            Vec3::NEG_X,
            Vec3::Y,
            Vec3::NEG_Y,
            Vec3::Z,
            Vec3::NEG_Z,
            Vec3::new(0.3, -0.8, -0.5).normalize(),
            Vec3::new(-0.7, 0.1, 0.7).normalize(),
        ] {
            let back = oct_decode(oct_encode(n));
            assert!((back - n).length() < 1e-3, "{n} → {back}");
        }
    }
}
