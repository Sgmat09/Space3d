//! Spawn scheduling of one layer instance for one tick (design-particles §5 step 3): exact
//! fixed-point rate accumulators, bursts on the tick whose interval contains them (with the
//! sub-tick fraction), quality / LOD / budget multipliers. Pure CPU, no allocation.

use super::Entry;
use super::log::{SpawnRecord, TickLog};
use crate::effect::ParticleLayer;
use crate::rng::{mix, rnd, stream};

/// 32.32 fixed point.
const ONE: f64 = 4_294_967_296.0;
const FRACTION: u64 = 0xffff_ffff;

/// Inputs of one tick of one layer instance.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SpawnTick {
    pub entry: u32,
    /// Effect time at the start of the tick.
    pub time: f64,
    /// Tick length in seconds.
    pub dt: f32,
    /// The instance emits (playing, attached, within its emission window).
    pub emitting: bool,
    pub looping: bool,
    /// Emission window end of continuous layers and endless bursts (non-looping effects).
    pub duration: f32,
    /// `ParticleEmitter::rate_scale` (throttle).
    pub rate_scale: f32,
    /// Quality × LOD × budget shrink.
    pub mul: f32,
    /// World distance the emitter moved this tick.
    pub distance: f32,
    /// Seconds already lived at the end of the tick (prewarm).
    pub extra_age: f32,
}

/// `n` scaled by `mul`, kept at ≥ 1 when `n` ≥ 1 and `mul` > 0 (bursts never vanish).
fn scaled_count(n: u32, mul: f32) -> u32 {
    if n == 0 || mul <= 0.0 {
        return 0;
    }
    ((n as f32 * mul).round() as u32).max(1)
}

fn push(entry: &mut Entry, log: &mut TickLog, count: u32, t0: f32, t1: f32, extra_age: f32, index: u32) {
    let count = count.min(entry.capacity);
    if count == 0 {
        return;
    }
    log.record(SpawnRecord {
        entry: index,
        first: entry.spawned,
        count,
        t0,
        t1,
        extra_age,
    });
    entry.spawned += count as u64;
}

/// The continuous emission parameters of a layer, kept in its layer instances
/// (`LayerSim::spawn`) for the render-time bridge ([`continuous`] without the layer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Continuous {
    pub rate: f32,
    pub per_distance: f32,
    pub max_rate_scale: f32,
    /// `ParticleLayer::start`.
    pub start: f32,
    /// `ParticleLayer::lod`: quality and LOD scale the rates.
    pub lod: bool,
    /// `ThrottleResponse::rate`: the rates scale by `throttle^rate_exponent`.
    pub rate_exponent: f32,
}

impl Continuous {
    pub fn new(l: &ParticleLayer) -> Self {
        Self {
            rate: l.spawn.rate,
            per_distance: l.spawn.rate_per_distance,
            max_rate_scale: l.spawn.max_rate_scale,
            start: l.start,
            lod: l.lod,
            rate_exponent: l.throttle.rate,
        }
    }
}

/// The continuous emission of one tick: `(count, t0, t1)`, the particles the rate accumulator
/// `acc` (32.32 fixed point, advanced) gives for the part of the tick inside `[start, duration)`
/// (endless when looping), and the times of the first and the last of them as fractions of the
/// tick. The time rate and the distance rate feed one accumulator: both are uniform over the
/// tick (the emitter's motion is interpolated linearly), so particle `k` of the tick is emitted
/// exactly when the accumulator crosses its integer, and the crossings are evenly spaced from
/// `t0` to `t1` ([`crate::cpu::subtick`]). A rate that is not a whole number of particles per
/// tick (40/s at 60 Hz: 1, 1, 0, …) therefore still emits an even stream instead of beads. The
/// time rate counts `rate_share` of the span: 1 for a tick, less for a render-time bridge that
/// stands for part of a tick's motion (`Runtime::bridge`). `None` when nothing of the tick
/// emits.
pub(crate) fn continuous(c: &Continuous, acc: &mut u64, s: &SpawnTick, rate_share: f64) -> Option<(u64, f32, f32)> {
    if !s.emitting || s.dt <= 0.0 {
        return None;
    }
    let (a, b) = (s.time, s.time + s.dt as f64);
    let dt = s.dt as f64;
    let lo = a.max(c.start.max(0.0) as f64);
    let hi = if s.looping { b } else { b.min(s.duration as f64) };
    if hi <= lo {
        return None;
    }
    let throttle = s.rate_scale.clamp(0.0, c.max_rate_scale.max(0.0)) as f64;
    let rate_throttle = if throttle <= 0.0 {
        0.0
    } else if c.rate_exponent == 1.0 {
        throttle
    } else {
        throttle.powf(c.rate_exponent.max(0.0) as f64)
    };
    let span = hi - lo;
    let rate = c.rate.max(0.0) as f64 * rate_throttle * s.mul as f64 * span * rate_share;
    let dist = c.per_distance.max(0.0) as f64 * rate_throttle * s.mul as f64 * s.distance as f64 * span / dt;
    let inc = ((rate + dist) * ONE).round().min(u64::MAX as f64 / 4.0) as u64;
    let phase = *acc & FRACTION;
    *acc = phase + inc;
    let n = *acc >> 32;
    *acc &= FRACTION;
    if n == 0 || inc == 0 {
        return Some((0, 0.0, 0.0));
    }
    // Crossing `k` (0-based) at `((k + 1)·2³² − phase) / inc` of the span.
    let cross = |k: u64| ((((k + 1) << 32) - phase) as f64 / inc as f64).min(1.0);
    let at = |x: f64| ((lo - a + x * span) / dt) as f32;
    Some((n, at(cross(0)), at(cross(n - 1))))
}

/// Schedule `layer`'s emission for one tick; returns the particles spawned.
pub(crate) fn spawn_tick(entry: &mut Entry, layer: &ParticleLayer, s: &SpawnTick, log: &mut TickLog) -> u32 {
    if !s.emitting || s.dt <= 0.0 {
        return 0;
    }
    let before = entry.spawned;
    let (a, b) = (s.time, s.time + s.dt as f64);
    let dt = s.dt as f64;
    let spawn = &layer.spawn;
    let start = layer.start.max(0.0) as f64;

    // Continuous: the part of the tick inside [start, duration) (endless when looping).
    let mut acc = entry.acc;
    if let Some((n, t0, t1)) = continuous(&Continuous::new(layer), &mut acc, s, 1.0) {
        entry.acc = acc;
        if n > 0 {
            push(entry, log, n.min(u32::MAX as u64) as u32, t0, t1, s.extra_age, s.entry);
        }
    }

    // Bursts: cycle `c` of burst `i` fires in the tick with a ≤ start + time + c·interval < b.
    for (i, burst) in spawn.bursts.iter().enumerate() {
        let t0 = start + burst.time.max(0.0) as f64;
        let mut fire = |c: u64, at: f64, cycles: u32, entry: &mut Entry| {
            let key = mix(entry.seed, (i as u32) << 20 ^ c as u32);
            let (lo, hi) = (
                burst.count.min.min(burst.count.max),
                burst.count.max.max(burst.count.min),
            );
            let n = lo + ((rnd(key, stream::COUNT) * (hi - lo + 1) as f32) as u32).min(hi - lo);
            let n = scaled_count(n, s.mul) * cycles;
            let f = ((at - a) / dt) as f32;
            push(entry, log, n, f, f, s.extra_age, s.entry);
        };
        if burst.interval > 0.0 {
            let interval = burst.interval as f64;
            let first = ((a - t0) / interval).ceil().max(0.0) as u64;
            let mut c = first.saturating_sub(1);
            loop {
                let at = t0 + c as f64 * interval;
                if at >= b || (burst.cycles != 0 && c >= burst.cycles as u64) {
                    break;
                }
                if burst.cycles == 0 && !s.looping && at >= s.duration as f64 {
                    break;
                }
                if at >= a {
                    fire(c, at, 1, entry);
                }
                c += 1;
            }
        } else if t0 >= a && t0 < b {
            // No interval: every cycle at once.
            fire(0, t0, burst.cycles.max(1), entry);
        }
    }
    (entry.spawned - before) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::Burst;
    use crate::runtime::layer::LayerSim;

    fn entry(layer: &ParticleLayer) -> Entry {
        Entry::new(
            0,
            crate::EffectHandle::INVALID,
            0,
            0,
            1 << 16,
            7,
            LayerSim::new(layer),
            1.0,
        )
    }

    fn run(layer: &ParticleLayer, ticks: u32, mut tick: impl FnMut(u32) -> SpawnTick) -> (u64, TickLog) {
        let mut e = entry(layer);
        let mut log = TickLog::default();
        for t in 0..ticks {
            log.begin(t as u64, 1.0 / 60.0, 1, 0.0);
            spawn_tick(&mut e, layer, &tick(t), &mut log);
        }
        (e.spawned, log)
    }

    fn at(t: u32, dt: f32) -> SpawnTick {
        SpawnTick {
            entry: 0,
            time: t as f64 * dt as f64,
            dt,
            emitting: true,
            looping: true,
            duration: 1.0,
            rate_scale: 1.0,
            mul: 1.0,
            distance: 0.0,
            extra_age: 0.0,
        }
    }

    /// Fixed-point accumulators are exact: 10/s at 60 Hz is 100 particles in 600 ticks, evenly
    /// spaced (starting half a period in).
    #[test]
    fn rates_are_exact() {
        let dt = 1.0 / 60.0;
        let layer = ParticleLayer::continuous(10.0, 1.0);
        let (n, log) = run(&layer, 600, |t| at(t, dt));
        assert_eq!(n, 100);
        let ticks: Vec<u64> = log
            .frames
            .iter()
            .filter(|f| f.records.1 > f.records.0)
            .map(|f| f.tick)
            .collect();
        assert_eq!(ticks.len(), 100);
        assert_eq!(
            ticks[0], 2,
            "the accumulator starts at ½ period (3 ticks at 6 ticks per particle)"
        );
        assert!(ticks.windows(2).all(|w| w[1] - w[0] == 6));
        // Throttle and quality scale the rate; max_rate_scale bounds the throttle.
        let (n, _) = run(&layer, 600, |t| SpawnTick {
            rate_scale: 5.0,
            ..at(t, dt)
        });
        assert_eq!(n, 100, "max_rate_scale defaults to 1");
        let (n, _) = run(&layer, 600, |t| SpawnTick { mul: 0.5, ..at(t, dt) });
        assert_eq!(n, 50);
        // A non-looping effect stops at its duration; a stopped one emits nothing.
        let (n, _) = run(&layer, 600, |t| SpawnTick {
            looping: false,
            duration: 2.5,
            ..at(t, dt)
        });
        assert_eq!(n, 25);
        let (n, _) = run(&layer, 60, |t| SpawnTick {
            emitting: false,
            ..at(t, dt)
        });
        assert_eq!(n, 0);
    }

    /// Emission times (in ticks) of every particle of `log`, from its records' first and last
    /// times through the backends' integer sub-tick spread (`cpu::subtick`).
    fn emission_times(log: &TickLog) -> Vec<f64> {
        use crate::cpu::{SUBTICKS, subtick, subtick_q};
        let mut out = Vec::new();
        for f in &log.frames {
            for r in &log.records[f.records.0 as usize..f.records.1 as usize] {
                let (t0, t1) = (subtick_q(r.t0), subtick_q(r.t1));
                for k in 0..r.count {
                    out.push(f.tick as f64 + subtick(t0, t1, k, r.count) as f64 / SUBTICKS as f64);
                }
            }
        }
        out
    }

    /// A rate that is not a whole number of particles per tick (40/s at 60 Hz: 1, 1, 0, …)
    /// emits an evenly spaced stream: each particle at the time the accumulator crosses its
    /// integer, not in the middle of its tick's share (which paired the particles of
    /// consecutive ticks into beads with double gaps). Likewise with the throttle, for mixed
    /// time and distance rates, and at rates above the tick rate.
    #[test]
    fn fractional_rates_emit_evenly() {
        let dt = 1.0 / 60.0;
        let tol = 2.0 / crate::cpu::SUBTICKS as f64;
        for (rate, per_distance, distance, throttle) in [
            (40.0, 0.0, 0.0, 1.0),
            (45.0, 0.0, 0.0, 1.0),
            (180.0, 0.0, 0.0, 0.3),
            (100.0, 0.0, 0.0, 1.0),
            (30.0, 0.4, 1.5, 1.0),
            (0.0, 1.0, 0.7, 1.0),
        ] {
            let mut layer = ParticleLayer::continuous(rate, 1.0);
            layer.spawn.rate_per_distance = per_distance;
            layer.spawn.max_rate_scale = 2.0;
            let (n, log) = run(&layer, 600, |t| SpawnTick {
                rate_scale: throttle,
                distance,
                ..at(t, dt)
            });
            let per_tick = (rate * throttle + per_distance * distance * 60.0 * throttle) as f64 / 60.0;
            let times = emission_times(&log);
            assert_eq!(times.len() as u64, n);
            assert!((n as f64 - 600.0 * per_tick).abs() <= 1.0, "{rate}: {n}");
            let gap = 1.0 / per_tick;
            for w in times.windows(2) {
                assert!(
                    (w[1] - w[0] - gap).abs() <= tol,
                    "rate {rate}, per distance {per_distance}, throttle {throttle}: gap {} (want {gap})",
                    w[1] - w[0]
                );
            }
        }
    }

    #[test]
    fn rate_per_distance_counts_travel() {
        let dt = 1.0 / 60.0;
        let mut layer = ParticleLayer::default();
        layer.spawn.rate_per_distance = 2.0;
        // 0.25 units per tick for 120 ticks: 30 units → 60 particles.
        let (n, _) = run(&layer, 120, |t| SpawnTick {
            distance: 0.25,
            ..at(t, dt)
        });
        assert_eq!(n, 60);
    }

    #[test]
    fn bursts_fire_on_their_tick_with_the_sub_tick_fraction() {
        let dt = 0.1;
        let layer = ParticleLayer {
            start: 0.05,
            spawn: crate::effect::Spawn {
                bursts: vec![
                    Burst {
                        time: 0.1,
                        count: (5, 5).into(),
                        ..Burst::default()
                    },
                    Burst {
                        time: 0.0,
                        count: (2, 2).into(),
                        cycles: 3,
                        interval: 0.2,
                    },
                    Burst {
                        count: (1, 3).into(),
                        cycles: 0,
                        interval: 0.5,
                        ..Burst::default()
                    },
                ],
                ..Default::default()
            },
            ..ParticleLayer::default()
        };
        let (n, log) = run(&layer, 20, |t| SpawnTick {
            looping: false,
            duration: 1.0,
            ..at(t, dt)
        });
        let recs: Vec<(u64, u32, f32)> = log
            .frames
            .iter()
            .flat_map(|f| {
                log.records[f.records.0 as usize..f.records.1 as usize]
                    .iter()
                    .map(move |r| (f.tick, r.count, r.t0))
            })
            .collect();
        // Burst 0 at 0.15 (tick 1, f 0.5); burst 1 at 0.05, 0.25, 0.45; burst 2 at 0.05 and
        // 0.55 (endless, but only while t < duration).
        let ticks: Vec<u64> = recs.iter().map(|r| r.0).collect();
        assert_eq!(ticks, vec![0, 0, 1, 2, 4, 5], "{recs:?}");
        for (i, count) in [(0, 2), (2, 5), (3, 2), (4, 2)] {
            assert_eq!(recs[i].1, count, "{recs:?}");
        }
        assert!((1..=3).contains(&recs[1].1) && (1..=3).contains(&recs[5].1));
        assert!(
            recs.iter().all(|r| (r.2 - 0.5).abs() < 1e-5),
            "every burst is half a tick in"
        );
        assert_eq!(n, recs.iter().map(|r| r.1 as u64).sum::<u64>());
        // Quality keeps bursts at ≥ 1 particle.
        assert_eq!(scaled_count(5, 0.01), 1);
        assert_eq!(scaled_count(0, 1.0), 0);
        assert_eq!(scaled_count(4, 0.0), 0);
    }
}
