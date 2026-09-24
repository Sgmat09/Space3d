//! Snapshot interpolation for remote entities. The per-component buffers are the jitter buffer.
//!
//! `render_tick = server_estimate − interp_delay` ([`crate::time::InterpClock`]). For each
//! `Interpolate`/`PredictAndInterpolate` component of a non-predicted entity, the client keeps
//! a [`SnapshotBuffer<T>`] and writes `T::interpolate(a, b, frac)` for the bracketing samples.
//! Past the last sample it holds (or extrapolates up to `max_extrapolation`), then freezes.
//! Gap fix: when a sample arrives more than `send_interval` after the previous one (the entity
//! was at rest and unchanged values were not sent), a copy of the previous value is inserted at
//! `tick − send_interval` so the entity does not slide across the gap.

use crate::Tick;
use std::collections::VecDeque;

/// Time-ordered samples of one component (capacity 32).
#[derive(Clone, Debug)]
pub struct SnapshotBuffer<T> {
    samples: VecDeque<(Tick, T)>,
}

impl<T> Default for SnapshotBuffer<T> {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
        }
    }
}

impl<T: Clone> SnapshotBuffer<T> {
    /// Maximum samples kept.
    pub const CAPACITY: usize = 32;

    /// Insert a sample (ignores duplicates / out-of-order older than the oldest kept).
    pub fn push(&mut self, tick: Tick, value: T, send_interval: u8) {
        todo!()
    }

    /// Value at fractional `render_tick` using `lerp`; `None` if empty.
    pub fn sample(&self, render_tick: f64, lerp: fn(&T, &T, f32) -> T, max_extrapolate_ticks: f64) -> Option<T> {
        todo!()
    }

    /// Drop samples older than `tick` (keeping one before it).
    pub fn prune(&mut self, tick: Tick) {
        todo!()
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}
