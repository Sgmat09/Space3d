//! Curves, gradients and random ranges of the effect data model (baked into `LUT_WIDTH`
//! samples; `t ∈ [0, 1]` = age / lifetime).

use serde::{Deserialize, Serialize};
use space3d_core::glam::Vec4;

/// A scalar over a particle's life.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Curve {
    /// One value.
    Constant(f32),
    /// From the first value at `t = 0` to the second at `t = 1`.
    Linear(f32, f32),
    /// `(t, value)` keys sorted by `t`, linear between keys, clamped outside.
    Keys(Vec<(f32, f32)>),
    /// `(t, value)` keys, Catmull-Rom between keys (clamped).
    Smooth(Vec<(f32, f32)>),
    /// A game-compatible fade shape (1 → 0).
    Preset(FadeCurve),
}

impl Default for Curve {
    fn default() -> Self {
        Self::Constant(1.0)
    }
}

/// Fade shapes, 1 → 0 over the life (the game's `fx::particles::Fade`, renamed so the facade
/// never sees two `Fade`s).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FadeCurve {
    /// `1 − t`.
    #[default]
    Linear,
    /// Ease out.
    Out,
    /// A short flash, then a quick fade.
    Flash,
    /// Fade in, then out.
    InOut,
    /// Stays bright, then a long tail.
    Linger,
    /// Burns bright, then collapses.
    Burn,
    /// Holds, then drops at the end.
    Hold,
}

/// Piecewise-linear interpolation of sorted `(t, value)` keys, clamped outside.
fn keys_eval(keys: &[(f32, f32)], t: f32) -> f32 {
    let Some(&(t0, v0)) = keys.first() else {
        return 1.0;
    };
    if t <= t0 {
        return v0;
    }
    for w in keys.windows(2) {
        let ((a, va), (b, vb)) = (w[0], w[1]);
        if t <= b {
            let f = if b > a { (t - a) / (b - a) } else { 1.0 };
            return va + (vb - va) * f;
        }
    }
    keys.last().map_or(1.0, |k| k.1)
}

/// Catmull-Rom through sorted `(t, value)` keys, the end keys repeated as outer control points,
/// clamped outside the keys.
fn smooth_eval(keys: &[(f32, f32)], t: f32) -> f32 {
    let Some(&(t0, v0)) = keys.first() else {
        return 1.0;
    };
    if t <= t0 {
        return v0;
    }
    let n = keys.len();
    for i in 0..n.saturating_sub(1) {
        let ((a, p1), (b, p2)) = (keys[i], keys[i + 1]);
        if t <= b {
            let p0 = keys[i.saturating_sub(1)].1;
            let p3 = keys[(i + 2).min(n - 1)].1;
            let u = if b > a { (t - a) / (b - a) } else { 1.0 };
            let (u2, u3) = (u * u, u * u * u);
            return 0.5
                * (2.0 * p1
                    + (p2 - p0) * u
                    + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * u2
                    + (3.0 * (p1 - p2) + p3 - p0) * u3);
        }
    }
    keys[n - 1].1
}

impl FadeCurve {
    /// The fade at `t` (clamped to 0..1): the game's `fx::particles::Fade::at` formulas.
    pub fn eval(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => 1.0 - t,
            Self::Out => (1.0 - t) * (1.0 - t),
            Self::Flash => (-t * 7.0).exp() * (1.0 - t),
            Self::InOut => (t / 0.12).min(1.0) * (1.0 - t).powf(1.4),
            Self::Linger => (t / 0.1).min(1.0) * (1.0 - t * t),
            Self::Burn => (1.0 - t).powf(2.8),
            Self::Hold => 1.0 - t * t,
        }
    }
}

impl Curve {
    /// The value at `t` (clamped to 0..1).
    pub fn eval(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Constant(v) => *v,
            Self::Linear(a, b) => a + (b - a) * t,
            Self::Keys(k) => keys_eval(k, t),
            Self::Smooth(k) => smooth_eval(k, t),
            Self::Preset(f) => f.eval(t),
        }
    }

    /// Largest value over `0..=1` (sampled at the LUT resolution; bounds and culling).
    pub fn max_value(&self) -> f32 {
        match self {
            Self::Constant(v) => *v,
            Self::Linear(a, b) => a.max(*b),
            Self::Preset(_) => 1.0,
            _ => (0..crate::effect::LUT_WIDTH)
                .map(|i| self.eval(i as f32 / (crate::effect::LUT_WIDTH - 1) as f32))
                .fold(f32::MIN, f32::max),
        }
    }

    /// An ease-out from `a` to `b` (`a + (b − a)·(1 − (1 − t)²)`, the game's size easing), as
    /// nine keys.
    pub fn ease_out(a: f32, b: f32) -> Self {
        Self::Keys(
            (0..=8)
                .map(|i| {
                    let t = i as f32 / 8.0;
                    (t, a + (b - a) * (1.0 - (1.0 - t) * (1.0 - t)))
                })
                .collect(),
        )
    }
}

/// A linear RGBA color over the life (at least one key).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gradient {
    /// `(t, linear RGBA)` keys sorted by `t`.
    pub keys: Vec<(f32, [f32; 4])>,
}

impl Default for Gradient {
    fn default() -> Self {
        Self::constant(Vec4::ONE)
    }
}

impl Gradient {
    /// One color.
    pub fn constant(c: Vec4) -> Self {
        Self {
            keys: vec![(0.0, c.to_array())],
        }
    }
    /// From `a` at `t = 0` to `b` at `t = 1`.
    pub fn linear(a: Vec4, b: Vec4) -> Self {
        Self {
            keys: vec![(0.0, a.to_array()), (1.0, b.to_array())],
        }
    }
    /// The color at `t` (clamped to 0..1).
    pub fn eval(&self, t: f32) -> Vec4 {
        let t = t.clamp(0.0, 1.0);
        let Some(&(t0, c0)) = self.keys.first() else {
            return Vec4::ONE;
        };
        if t <= t0 {
            return Vec4::from_array(c0);
        }
        for w in self.keys.windows(2) {
            let ((a, ca), (b, cb)) = (w[0], w[1]);
            if t <= b {
                let f = if b > a { (t - a) / (b - a) } else { 1.0 };
                return Vec4::from_array(ca).lerp(Vec4::from_array(cb), f);
            }
        }
        self.keys.last().map_or(Vec4::ONE, |k| Vec4::from_array(k.1))
    }
}

/// A random value in `min..=max` per particle. Deserializes from `1.0` or `(0.5, 1.0)`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "RangeRepr<f32>", into = "RangeRepr<f32>")]
pub struct FloatRange {
    /// Lower bound.
    pub min: f32,
    /// Upper bound.
    pub max: f32,
}

impl FloatRange {
    /// Always `v`.
    pub const fn constant(v: f32) -> Self {
        Self { min: v, max: v }
    }
    /// `min..=max`.
    pub const fn new(min: f32, max: f32) -> Self {
        Self { min, max }
    }
}

impl Default for FloatRange {
    fn default() -> Self {
        Self::constant(0.0)
    }
}

impl From<f32> for FloatRange {
    fn from(v: f32) -> Self {
        Self::constant(v)
    }
}

impl From<(f32, f32)> for FloatRange {
    fn from((min, max): (f32, f32)) -> Self {
        Self { min, max }
    }
}

/// A random particle count in `min..=max`. Deserializes from `12` or `(8, 16)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RangeRepr<u32>", into = "RangeRepr<u32>")]
pub struct CountRange {
    /// Lower bound.
    pub min: u32,
    /// Upper bound.
    pub max: u32,
}

impl From<u32> for CountRange {
    fn from(v: u32) -> Self {
        Self { min: v, max: v }
    }
}

impl From<(u32, u32)> for CountRange {
    fn from((min, max): (u32, u32)) -> Self {
        Self { min, max }
    }
}

/// A random color between `a` and `b` per particle (linear RGBA).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorRange {
    /// One end.
    pub a: [f32; 4],
    /// The other end.
    pub b: [f32; 4],
}

impl Default for ColorRange {
    fn default() -> Self {
        Self {
            a: [1.0; 4],
            b: [1.0; 4],
        }
    }
}

/// Serde form of the ranges: a single value or a pair.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
enum RangeRepr<T> {
    One(T),
    Two(T, T),
}

impl From<RangeRepr<f32>> for FloatRange {
    fn from(r: RangeRepr<f32>) -> Self {
        match r {
            RangeRepr::One(v) => Self::constant(v),
            RangeRepr::Two(a, b) => Self::new(a, b),
        }
    }
}

impl From<FloatRange> for RangeRepr<f32> {
    fn from(r: FloatRange) -> Self {
        if r.min == r.max {
            Self::One(r.min)
        } else {
            Self::Two(r.min, r.max)
        }
    }
}

impl From<RangeRepr<u32>> for CountRange {
    fn from(r: RangeRepr<u32>) -> Self {
        match r {
            RangeRepr::One(v) => v.into(),
            RangeRepr::Two(a, b) => (a, b).into(),
        }
    }
}

impl From<CountRange> for RangeRepr<u32> {
    fn from(r: CountRange) -> Self {
        if r.min == r.max {
            Self::One(r.min)
        } else {
            Self::Two(r.min, r.max)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_and_gradients() {
        assert_eq!(Curve::default().eval(0.3), 1.0);
        assert_eq!(Curve::Linear(2.0, 0.0).eval(0.25), 1.5);
        let k = Curve::Keys(vec![(0.0, 0.0), (0.5, 1.0), (1.0, 0.0)]);
        assert_eq!((k.eval(0.25), k.eval(0.5), k.eval(2.0)), (0.5, 1.0, 0.0));
        let g = Gradient::linear(Vec4::ZERO, Vec4::ONE);
        assert_eq!(g.eval(0.5), Vec4::splat(0.5));
        assert_eq!(Gradient::default().eval(0.9), Vec4::ONE);
    }

    #[test]
    fn smooth_curves_pass_through_their_keys() {
        let keys = vec![(0.0, 0.0), (0.3, 1.0), (0.7, 0.2), (1.0, 0.5)];
        let c = Curve::Smooth(keys.clone());
        for (t, v) in keys {
            assert!((c.eval(t) - v).abs() < 1e-6, "{t}: {}", c.eval(t));
        }
        // Between keys it is smooth: no kink at 0.3 (left and right slopes agree).
        let h = 1e-3;
        let left = (c.eval(0.3) - c.eval(0.3 - h)) / h;
        let right = (c.eval(0.3 + h) - c.eval(0.3)) / h;
        assert!((left - right).abs() < 0.5, "{left} vs {right}");
        assert_eq!(Curve::Smooth(vec![(0.5, 2.0)]).eval(0.9), 2.0);
        assert_eq!(Curve::Smooth(vec![]).eval(0.9), 1.0);
    }

    /// The presets are the game's `Fade::at` shapes.
    #[test]
    fn fade_presets_match_the_game() {
        let at = |f: FadeCurve, t: f32| Curve::Preset(f).eval(t);
        for t in [0.0f32, 0.05, 0.25, 0.5, 0.9, 1.0] {
            assert_eq!(at(FadeCurve::Linear, t), 1.0 - t);
            assert_eq!(at(FadeCurve::Out, t), (1.0 - t) * (1.0 - t));
            assert_eq!(at(FadeCurve::Flash, t), (-t * 7.0).exp() * (1.0 - t));
            assert_eq!(at(FadeCurve::InOut, t), (t / 0.12).min(1.0) * (1.0 - t).powf(1.4));
            assert_eq!(at(FadeCurve::Linger, t), (t / 0.1).min(1.0) * (1.0 - t * t));
            assert_eq!(at(FadeCurve::Burn, t), (1.0 - t).powf(2.8));
            assert_eq!(at(FadeCurve::Hold, t), 1.0 - t * t);
        }
        assert_eq!(at(FadeCurve::Hold, 2.0), 0.0, "clamped");
        assert_eq!(Curve::Linear(1.0, 3.0).max_value(), 3.0);
        assert!((Curve::Keys(vec![(0.0, 0.0), (0.5, 4.0), (1.0, 0.0)]).max_value() - 4.0).abs() < 0.05);
        let e = Curve::ease_out(1.0, 3.0);
        assert_eq!((e.eval(0.0), e.eval(1.0)), (1.0, 3.0));
        assert!(e.eval(0.5) > 2.0, "eases out");
    }

    #[test]
    fn ranges_read_one_value_or_a_pair() {
        let r: FloatRange = ron::from_str("1.5").unwrap();
        assert_eq!(r, FloatRange::constant(1.5));
        let r: FloatRange = ron::from_str("(0.5, 1.0)").unwrap();
        assert_eq!(r, FloatRange::new(0.5, 1.0));
        assert_eq!(ron::to_string(&r).unwrap(), "(0.5,1.0)");
        let c: CountRange = ron::from_str("(8, 16)").unwrap();
        assert_eq!((c.min, c.max), (8, 16));
        assert_eq!(ron::to_string(&CountRange::from(3)).unwrap(), "3");
    }
}
