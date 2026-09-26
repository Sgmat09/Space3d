//! The tick log: what every fixed tick decided (spawn records, emitter transforms, pool
//! clears, world attractors), replayed by the simulation backend inside the render frame
//! (design-particles §3, §5). The simulated state is a pure function of the logged ticks, so
//! rendering every tick, rendering once after `n` ticks and replaying only the last `W` ticks
//! from an empty pool give identical particles.

use crate::effect::Falloff;
use space3d_core::glam::{Quat, Vec3};

/// Particles to emit this tick for one layer instance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SpawnRecord {
    /// Layer instance (entry index).
    pub entry: u32,
    /// Spawn index of the first particle (ring slot = index mod capacity).
    pub first: u64,
    /// Particles.
    pub count: u32,
    /// Emission time of the first particle, as a fraction (0..1) of the tick.
    pub t0: f32,
    /// Emission time of the last particle (the others evenly between: `cpu::subtick`).
    pub t1: f32,
    /// Extra age in seconds (prewarm: particles synthesized for past ticks).
    pub extra_age: f32,
}

/// An effect instance's emitter frame during one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct XformRow {
    /// Instance index.
    pub instance: u32,
    /// Position at the start of the tick.
    pub prev_pos: Vec3,
    /// Rotation at the start of the tick.
    pub prev_rot: Quat,
    /// Position at the end of the tick.
    pub pos: Vec3,
    /// Rotation at the end of the tick.
    pub rot: Quat,
    /// World velocity (measured motion + `ParticleEmitter::velocity`).
    pub velocity: Vec3,
    /// Uniform effect scale.
    pub scale: f32,
    /// `ParticleEmitter::rate_scale` (the throttle; `ThrottleResponse::speed` of new particles).
    pub throttle: f32,
}

impl XformRow {
    pub const IDENTITY: Self = Self {
        instance: u32::MAX,
        prev_pos: Vec3::ZERO,
        prev_rot: Quat::IDENTITY,
        pos: Vec3::ZERO,
        rot: Quat::IDENTITY,
        velocity: Vec3::ZERO,
        scale: 1.0,
        throttle: 1.0,
    };
}

/// A world attractor (`ParticleAttractor`) during one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WorldAttractor {
    pub position: Vec3,
    pub strength: f32,
    pub radius: f32,
    pub falloff: Falloff,
    pub mask: u32,
}

/// One logged tick: ranges into the log's shared vectors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TickFrame {
    pub tick: u64,
    /// Seconds simulated this tick (`fixed_delta × time_scale`).
    pub dt: f32,
    pub substeps: u32,
    /// Runtime time at the start of the tick (turbulence scroll).
    pub time: f64,
    pub records: (u32, u32),
    pub rows: (u32, u32),
    pub clears: (u32, u32),
    pub attractors: (u32, u32),
}

/// The log (part of the runtime resource). Its vectors are reused (cleared, never dropped).
#[derive(Debug, Default)]
pub(crate) struct TickLog {
    pub frames: Vec<TickFrame>,
    pub records: Vec<SpawnRecord>,
    pub rows: Vec<XformRow>,
    /// Pool ranges `(start, len)` zeroed after the tick's simulation.
    pub clears: Vec<(u32, u32)>,
    pub attractors: Vec<WorldAttractor>,
    /// Frames were dropped since the last replay: clear the pool before replaying.
    pub reset: bool,
}

fn len(v: usize) -> u32 {
    v as u32
}

impl TickLog {
    /// Start logging tick `tick`.
    pub fn begin(&mut self, tick: u64, dt: f32, substeps: u32, time: f64) {
        let (r, w, c, a) = (
            len(self.records.len()),
            len(self.rows.len()),
            len(self.clears.len()),
            len(self.attractors.len()),
        );
        self.frames.push(TickFrame {
            tick,
            dt,
            substeps,
            time,
            records: (r, r),
            rows: (w, w),
            clears: (c, c),
            attractors: (a, a),
        });
    }

    fn current(&mut self) -> &mut TickFrame {
        self.frames.last_mut().expect("TickLog::begin first")
    }

    pub fn record(&mut self, r: SpawnRecord) {
        self.records.push(r);
        self.current().records.1 += 1;
    }

    pub fn row(&mut self, r: XformRow) {
        self.rows.push(r);
        self.current().rows.1 += 1;
    }

    pub fn clear_range(&mut self, start: u32, len: u32) {
        self.clears.push((start, len));
        self.current().clears.1 += 1;
    }

    pub fn attractor(&mut self, a: WorldAttractor) {
        self.attractors.push(a);
        self.current().attractors.1 += 1;
    }

    /// Oldest tick still in the log (not yet replayed or trimmed).
    pub fn first_tick(&self) -> Option<u64> {
        self.frames.first().map(|f| f.tick)
    }

    /// Keep at most `keep` frames: drop the oldest and remember to reset the pool before the
    /// next replay (exact when `keep` covers every live particle's life, §3 "catch-up window").
    pub fn trim(&mut self, keep: usize) {
        let keep = keep.max(1);
        if self.frames.len() <= keep {
            return;
        }
        let drop = self.frames.len() - keep;
        let first = self.frames[drop];
        self.frames.drain(..drop);
        self.records.drain(..first.records.0 as usize);
        self.rows.drain(..first.rows.0 as usize);
        self.clears.drain(..first.clears.0 as usize);
        self.attractors.drain(..first.attractors.0 as usize);
        for f in &mut self.frames {
            f.records = (f.records.0 - first.records.0, f.records.1 - first.records.0);
            f.rows = (f.rows.0 - first.rows.0, f.rows.1 - first.rows.0);
            f.clears = (f.clears.0 - first.clears.0, f.clears.1 - first.clears.0);
            f.attractors = (f.attractors.0 - first.attractors.0, f.attractors.1 - first.attractors.0);
        }
        self.reset = true;
    }

    /// Everything was replayed.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.records.clear();
        self.rows.clear();
        self.clears.clear();
        self.attractors.clear();
        self.reset = false;
    }

    /// Sum of the vectors' capacities (reallocation tracking).
    pub fn capacity(&self) -> usize {
        self.frames.capacity()
            + self.records.capacity()
            + self.rows.capacity()
            + self.clears.capacity()
            + self.attractors.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(entry: u32) -> SpawnRecord {
        SpawnRecord {
            entry,
            first: 0,
            count: 1,
            t0: 0.0,
            t1: 0.0,
            extra_age: 0.0,
        }
    }

    #[test]
    fn trimming_keeps_the_newest_frames_and_rebases_ranges() {
        let mut log = TickLog::default();
        for t in 1..=5u64 {
            log.begin(t, 0.1, 1, t as f64);
            for _ in 0..t {
                log.record(rec(t as u32));
            }
            log.row(XformRow::IDENTITY);
            if t == 4 {
                log.clear_range(3, 2);
            }
        }
        assert_eq!(log.records.len(), 15);
        log.trim(2);
        assert!(log.reset);
        assert_eq!(log.first_tick(), Some(4));
        assert_eq!(log.frames.len(), 2);
        let f = log.frames[0];
        assert_eq!((f.records, f.rows, f.clears), ((0, 4), (0, 1), (0, 1)));
        assert!(
            log.records[f.records.0 as usize..f.records.1 as usize]
                .iter()
                .all(|r| r.entry == 4)
        );
        let g = log.frames[1];
        assert_eq!((g.records, g.clears), ((4, 9), (1, 1)));
        assert_eq!(log.clears, vec![(3, 2)]);
        log.trim(5);
        assert_eq!(log.frames.len(), 2, "nothing to trim");
        log.clear();
        assert!(!log.reset && log.first_tick().is_none() && log.records.is_empty());
    }
}
