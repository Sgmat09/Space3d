//! Ribbon histories (design-particles §5 step 6, §8.5): a ring of points per ribbon layer of an
//! effect instance ("track"), appended on the fixed tick when the emitter moved `min_distance`
//! (or after `max_interval`), each point stamped with its birth time, the emitter's tint and the
//! distance travelled along the trail. The render node draws the live points plus a head at the
//! emitter's render pose. CPU state only: nothing is replayed, so ribbons are identical on both
//! backends and for any render cadence.

use crate::effect::RibbonLayer;
use crate::registry::EffectHandle;
use space3d_core::glam::{Vec3, Vec4};

/// The per-tick parameters of a ribbon layer (plain `Copy` data per track).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RibbonSim {
    pub start: f32,
    pub offset: Vec3,
    pub min_distance: f32,
    pub max_interval: f32,
    pub lifetime: f32,
    pub tinted: bool,
    pub max_points: u32,
}

impl RibbonSim {
    pub fn new(r: &RibbonLayer) -> Self {
        Self {
            start: r.start.max(0.0),
            offset: r.offset,
            min_distance: r.min_distance.max(0.0),
            max_interval: r.max_interval.max(0.0),
            lifetime: r.lifetime.max(1e-4),
            tinted: r.tinted,
            max_points: r.max_points.clamp(2, crate::effect::MAX_RIBBON_POINTS),
        }
    }
}

/// A point of a ribbon's history.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RibbonPoint {
    /// World position.
    pub pos: Vec3,
    /// Birth, in runtime seconds (`Runtime::time` at the end of its tick).
    pub time: f64,
    /// Distance along the trail from its first point (world units).
    pub dist: f64,
    /// The emitter's tint at the point's birth (white for untinted layers).
    pub tint: Vec4,
}

/// A ribbon layer of an effect instance: `cap` points from `base` in the point pool, a ring
/// written at `head`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Track {
    pub live: bool,
    /// Allocation order (draw order tie-break).
    pub serial: u64,
    pub instance: u32,
    pub effect: EffectHandle,
    /// Index into `ParticleEffect::ribbons`.
    pub layer: u16,
    pub base: u32,
    pub cap: u32,
    /// Ring position of the next point.
    pub head: u32,
    /// Points stored (≤ `cap`).
    pub len: u32,
    /// Tick of the newest point (every point is dead `window_ticks(lifetime)` ticks later).
    pub last_tick: u64,
    pub sim: RibbonSim,
}

impl Track {
    /// Ring index of the `k`-th stored point, oldest first.
    pub fn slot(&self, k: u32) -> usize {
        (self.base + (self.head + self.cap - self.len + k) % self.cap) as usize
    }

    /// The newest point's slot.
    pub fn newest(&self) -> Option<usize> {
        (self.len > 0).then(|| self.slot(self.len - 1))
    }
}

/// Whether a point should be appended at `pos` (runtime time `now`) given the newest point.
pub(crate) fn due(sim: &RibbonSim, newest: Option<&RibbonPoint>, pos: Vec3, now: f64, scale: f32) -> Option<f64> {
    let Some(last) = newest else {
        return Some(0.0);
    };
    let d = pos.distance(last.pos);
    let moved = d >= sim.min_distance * scale.abs() && d > 1e-6;
    let waited = sim.max_interval > 0.0 && now - last.time >= sim.max_interval as f64 - 1e-9;
    (moved || waited).then_some(last.dist + d as f64)
}
