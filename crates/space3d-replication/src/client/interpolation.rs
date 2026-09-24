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
    /// A sample arriving more than `send_interval` ticks after the newest one gets a copy of
    /// the newest value inserted at `tick − send_interval` first (rest → move gap fix).
    pub fn push(&mut self, tick: Tick, value: T, send_interval: u8) {
        let interval = u32::from(send_interval.max(1));
        match self.samples.back() {
            None => self.samples.push_back((tick, value)),
            Some((newest, newest_value)) if tick > *newest => {
                if tick.0 - newest.0 > interval {
                    let hold = (Tick(tick.0 - interval), newest_value.clone());
                    self.samples.push_back(hold);
                }
                self.samples.push_back((tick, value));
            }
            Some(_) => {
                // Out of order (datagram reordering): insert in place unless duplicate / too old.
                let Some(front) = self.samples.front() else { return };
                if tick < front.0 {
                    return;
                }
                match self.samples.binary_search_by_key(&tick, |(t, _)| *t) {
                    Ok(_) => {}
                    Err(pos) => self.samples.insert(pos, (tick, value)),
                }
            }
        }
        while self.samples.len() > Self::CAPACITY {
            self.samples.pop_front();
        }
    }

    /// Value at fractional `render_tick` using `lerp`; `None` if empty. Past the newest sample
    /// the value holds.
    pub fn sample(&self, render_tick: f64, lerp: fn(&T, &T, f32) -> T, max_extrapolate_ticks: f64) -> Option<T> {
        self.sample_with(render_tick, lerp, None, max_extrapolate_ticks)
    }

    /// Like [`Self::sample`], extrapolating past the newest sample with `extrapolate` for at most
    /// `max_extrapolate_ticks`, then freezing.
    pub fn sample_with(
        &self,
        render_tick: f64,
        lerp: fn(&T, &T, f32) -> T,
        extrapolate: Option<crate::registry::ExtrapolateFn<T>>,
        max_extrapolate_ticks: f64,
    ) -> Option<T> {
        let (first_tick, first) = self.samples.front()?;
        if render_tick <= f64::from(first_tick.0) {
            return Some(first.clone());
        }
        let (last_tick, last) = self.samples.back()?;
        let last_t = f64::from(last_tick.0);
        if render_tick >= last_t {
            let over = (render_tick - last_t).min(max_extrapolate_ticks.max(0.0));
            if over > 0.0
                && let Some(f) = extrapolate
                && self.samples.len() >= 2
            {
                let (prev_tick, prev) = &self.samples[self.samples.len() - 2];
                let span = last_t - f64::from(prev_tick.0);
                if span > 0.0
                    && let Some(v) = f(prev, last, (over / span) as f32)
                {
                    return Some(v);
                }
            }
            return Some(last.clone());
        }
        // First sample strictly after the render tick.
        let idx = self.samples.partition_point(|(t, _)| f64::from(t.0) <= render_tick);
        let (bt, b) = &self.samples[idx];
        let (at, a) = &self.samples[idx - 1];
        let span = f64::from(bt.0 - at.0);
        let frac = ((render_tick - f64::from(at.0)) / span).clamp(0.0, 1.0) as f32;
        Some(lerp(a, b, frac))
    }

    /// Drop samples older than `tick` (keeping one before it).
    pub fn prune(&mut self, tick: Tick) {
        while self.samples.len() > 1 && self.samples[1].0 <= tick {
            self.samples.pop_front();
        }
    }

    /// Newest sample.
    pub fn newest(&self) -> Option<&(Tick, T)> {
        self.samples.back()
    }

    /// Oldest sample.
    pub fn oldest(&self) -> Option<&(Tick, T)> {
        self.samples.front()
    }

    /// Drop everything.
    pub fn clear(&mut self) {
        self.samples.clear();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interpolate;

    fn lerp(a: &f32, b: &f32, t: f32) -> f32 {
        a + (b - a) * t
    }

    #[test]
    fn interpolates_between_samples() {
        let mut b = SnapshotBuffer::default();
        b.push(Tick(10), 0.0f32, 2);
        b.push(Tick(12), 2.0, 2);
        b.push(Tick(14), 6.0, 2);
        assert_eq!(b.sample(9.0, lerp, 6.0), Some(0.0));
        assert_eq!(b.sample(11.0, lerp, 6.0), Some(1.0));
        assert_eq!(b.sample(13.5, lerp, 6.0), Some(5.0));
        // Holds past the end without an extrapolator.
        assert_eq!(b.sample(20.0, lerp, 6.0), Some(6.0));
    }

    #[test]
    fn extrapolation_is_capped() {
        let mut b = SnapshotBuffer::default();
        b.push(Tick(10), 0.0f32, 2);
        b.push(Tick(12), 2.0, 2);
        let ex = Some(<f32 as Interpolate>::extrapolate as fn(&f32, &f32, f32) -> Option<f32>);
        assert_eq!(b.sample_with(13.0, lerp, ex, 6.0), Some(3.0));
        // Frozen after 6 ticks of extrapolation.
        assert_eq!(b.sample_with(30.0, lerp, ex, 6.0), Some(8.0));
    }

    #[test]
    fn gap_fix_prevents_sliding_from_rest() {
        let mut b = SnapshotBuffer::default();
        // At rest at 0 since tick 10 (unchanged values are not re-sent), then moves at tick 40.
        b.push(Tick(10), 0.0f32, 2);
        b.push(Tick(40), 4.0, 2);
        // Without the fix the entity would slide from tick 10 to 40; with it, it stays until 38.
        assert_eq!(b.sample(30.0, lerp, 6.0), Some(0.0));
        assert_eq!(b.sample(38.0, lerp, 6.0), Some(0.0));
        assert_eq!(b.sample(39.0, lerp, 6.0), Some(2.0));
    }

    #[test]
    fn out_of_order_and_duplicates() {
        let mut b = SnapshotBuffer::default();
        b.push(Tick(10), 1.0f32, 2);
        b.push(Tick(14), 3.0, 4);
        b.push(Tick(12), 2.0, 2);
        b.push(Tick(12), 9.0, 2);
        b.push(Tick(2), 9.0, 2);
        let ticks: Vec<u32> = b.samples.iter().map(|(t, _)| t.0).collect();
        assert_eq!(ticks, vec![10, 12, 14]);
        b.prune(Tick(13));
        assert_eq!(b.len(), 2);
        for i in 0..40 {
            b.push(Tick(20 + i), i as f32, 1);
        }
        assert_eq!(b.len(), SnapshotBuffer::<f32>::CAPACITY);
    }
}
