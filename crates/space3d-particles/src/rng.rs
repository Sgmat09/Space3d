//! Deterministic integer randomness shared by the CPU simulation and the particle shaders
//! (bit-identical: `s3dp_pcg` / `s3dp_rnd` in `shaders/sprite.wgsl`, and the renderer's
//! `s3d_pcg`). Every random attribute of a particle is `rnd(seed, stream)` with a fixed stream
//! per attribute ([`stream`]), so adding an attribute never shifts the others (graphics spec
//! §4.3: "PCG keyed by (emitter seed, spawn index)").

/// The golden-ratio increment of the streams.
pub(crate) const GOLDEN: u32 = 0x9E37_79B9;

/// PCG-RXS-M-XS 32-bit hash (Jarzynski & Olano), bit-identical to the WGSL `s3d_pcg`.
#[inline]
pub(crate) fn pcg(v: u32) -> u32 {
    let s = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let w = ((s >> ((s >> 28) + 4)) ^ s).wrapping_mul(277_803_737);
    (w >> 22) ^ w
}

/// Uniform in `[0, 1)` with 24 bits: `f32(pcg(seed + stream·φ) >> 8) · 2⁻²⁴` (exact in f32 on
/// every backend).
#[inline]
pub(crate) fn rnd(seed: u32, stream: u32) -> f32 {
    (pcg(seed.wrapping_add(stream.wrapping_mul(GOLDEN))) >> 8) as f32 * (1.0 / 16_777_216.0)
}

/// `lo + (hi − lo) · rnd(seed, stream)`.
#[inline]
pub(crate) fn range(seed: u32, stream: u32, lo: f32, hi: f32) -> f32 {
    lo + (hi - lo) * rnd(seed, stream)
}

/// `fma(hi − lo, rnd, lo)`: one rounding, so the GPU kernels (WGSL `fma`) compute the very same
/// value whatever the shader compiler contracts. Used for the values that decide a particle's
/// fate (lifetime, delay), so both backends kill and reveal particles on the same tick.
#[inline]
pub(crate) fn range_exact(seed: u32, stream: u32, lo: f32, hi: f32) -> f32 {
    (hi - lo).mul_add(rnd(seed, stream), lo)
}

/// Seed of the particle with spawn index `index` of a layer instance seeded `entry_seed`.
#[inline]
pub(crate) fn particle_seed(entry_seed: u32, index: u64) -> u32 {
    pcg(entry_seed ^ pcg(index as u32 ^ pcg((index >> 32) as u32 ^ 0x68E3_1DA4)))
}

/// Combine two seeds.
#[inline]
pub(crate) fn mix(a: u32, b: u32) -> u32 {
    pcg(a ^ pcg(b.wrapping_add(GOLDEN)))
}

/// Random streams, one per particle attribute.
pub(crate) mod stream {
    pub const LIFETIME: u32 = 0;
    pub const SPEED: u32 = 1;
    pub const SHAPE_A: u32 = 2;
    pub const SHAPE_B: u32 = 3;
    pub const SHAPE_C: u32 = 4;
    pub const DIR_A: u32 = 5;
    pub const DIR_B: u32 = 6;
    pub const SIZE: u32 = 7;
    pub const ROTATION: u32 = 8;
    pub const ANGULAR: u32 = 9;
    pub const COLOR: u32 = 10;
    pub const COVERAGE: u32 = 11;
    pub const DELAY: u32 = 12;
    pub const MIRROR: u32 = 13;
    pub const FLICKER: u32 = 14;
    pub const BITS: u32 = 15;
    pub const SPREAD_A: u32 = 16;
    pub const SPREAD_B: u32 = 17;
    /// Burst counts (keyed by burst and cycle, not by particle).
    pub const COUNT: u32 = 18;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parity vectors, computed independently (Python, 32-bit wrapping arithmetic); the WGSL
    /// copies are checked against the same function on the GPU in `tests/render.rs`.
    #[test]
    fn pcg_parity_vectors() {
        for (v, h) in [
            (0u32, 0x07bb_2fe2u32),
            (1, 0xa8be_ea3c),
            (2, 0x7a7e_cc88),
            (42, 0x48f4_32ff),
            (0x9E37_79B9, 0x9031_965f),
            (u32::MAX, 0xe62a_4902),
            (123_456_789, 0xfea7_91ca),
        ] {
            assert_eq!(pcg(v), h, "pcg({v:#x})");
        }
        let r24 = |seed, k| rnd(seed, k) * 16_777_216.0;
        assert_eq!(r24(7, 0), 8_283_922.0);
        assert_eq!(r24(7, 1), 2_261_196.0);
        assert_eq!(r24(0xdead_beef, 5), 14_931_482.0);
    }

    #[test]
    fn streams_are_uniform_and_independent() {
        let n = 20_000u32;
        let mut sum = [0.0f64; 3];
        for i in 0..n {
            let s = particle_seed(0x1234, i as u64);
            for (k, acc) in sum.iter_mut().enumerate() {
                let r = rnd(s, k as u32);
                assert!((0.0..1.0).contains(&r));
                *acc += r as f64;
            }
        }
        for acc in sum {
            assert!((acc / n as f64 - 0.5).abs() < 0.01, "{}", acc / n as f64);
        }
        assert_ne!(particle_seed(1, 5), particle_seed(1, 5 + (1 << 32)), "high bits matter");
        assert_ne!(mix(1, 2), mix(2, 1));
        assert_eq!(range(9, 3, 2.0, 2.0), 2.0);
        assert_eq!(range_exact(9, 3, 2.0, 2.0), 2.0);
        assert!((range_exact(9, 3, 1.0, 3.0) - range(9, 3, 1.0, 3.0)).abs() < 1e-6);
    }
}
