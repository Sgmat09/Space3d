//! Where an effect instance's emitter has been during the last life window: 8 buckets of `len`
//! ticks, each with the bounding box of the emitter positions and the fastest emitter speed in
//! it. Kept by the tick system (so it does not depend on when frames are rendered) and read by
//! the render node: the depth-sorting center of a world-space draw (identical on both backends)
//! and the GPU backend's conservative culling bounds (whose particles the CPU never sees).

use space3d_core::glam::Vec3;

const BUCKETS: usize = 8;
/// Epoch of an empty bucket.
const EMPTY: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bucket {
    epoch: u64,
    min: Vec3,
    max: Vec3,
    speed: f32,
}

const EMPTY_BUCKET: Bucket = Bucket {
    epoch: EMPTY,
    min: Vec3::ZERO,
    max: Vec3::ZERO,
    speed: 0.0,
};

/// The recent path of one emitter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Path {
    /// Ticks per bucket: `ceil(window / 7)`, so the 7 full buckets before the current one span
    /// at least the window.
    len: u32,
    buckets: [Bucket; BUCKETS],
}

impl Default for Path {
    fn default() -> Self {
        Self {
            len: 1,
            buckets: [EMPTY_BUCKET; BUCKETS],
        }
    }
}

impl Path {
    /// The emitter moved from `a` to `b` at `speed` during tick `tick`; its particles live at
    /// most `window` ticks.
    pub fn record(&mut self, tick: u64, window: u32, a: Vec3, b: Vec3, speed: f32) {
        let len = window.div_ceil(BUCKETS as u32 - 1).max(1);
        if len != self.len {
            *self = Self { len, ..Self::default() };
        }
        let epoch = tick / len as u64;
        let bucket = &mut self.buckets[(epoch % BUCKETS as u64) as usize];
        if bucket.epoch != epoch {
            *bucket = Bucket {
                epoch,
                min: a.min(b),
                max: a.max(b),
                speed,
            };
        } else {
            bucket.min = bucket.min.min(a).min(b);
            bucket.max = bucket.max.max(a).max(b);
            bucket.speed = bucket.speed.max(speed);
        }
    }

    /// Bounds of the positions and the fastest speed over (at least) the last window before
    /// `tick`; `None` before the first record.
    pub fn bounds(&self, tick: u64) -> Option<(Vec3, Vec3, f32)> {
        let now = tick / self.len as u64;
        let mut out: Option<(Vec3, Vec3, f32)> = None;
        for b in &self.buckets {
            if b.epoch == EMPTY || b.epoch > now || b.epoch + (BUCKETS as u64) <= now {
                continue;
            }
            out = Some(match out {
                None => (b.min, b.max, b.speed),
                Some((lo, hi, s)) => (lo.min(b.min), hi.max(b.max), s.max(b.speed)),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_cover_the_window_and_forget_older_positions() {
        let mut p = Path::default();
        // Window 14 ticks: buckets of 2 ticks; position x = tick.
        for t in 0..100u64 {
            let x = t as f32;
            p.record(t, 14, Vec3::new(x - 1.0, 0.0, 0.0), Vec3::new(x, 0.0, 0.0), x);
            let (lo, hi, s) = p.bounds(t).unwrap();
            assert_eq!(hi.x, x);
            assert_eq!(s, x);
            // Everything within the last 14 ticks is covered, nothing older than 16.
            let oldest = t.saturating_sub(14) as f32 - 1.0;
            assert!(lo.x <= oldest.max(-1.0), "tick {t}: {lo}");
            assert!(lo.x >= t as f32 - 17.0, "tick {t}: {lo}");
        }
        assert!(Path::default().bounds(5).is_none());
        // A new window size restarts the history.
        p.record(100, 70, Vec3::splat(5.0), Vec3::splat(5.0), 0.0);
        assert_eq!(p.bounds(100), Some((Vec3::splat(5.0), Vec3::splat(5.0), 0.0)));
    }
}
