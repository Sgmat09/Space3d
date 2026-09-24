use std::time::Duration;

/// Fixed simulation rate. Insert/replace as a resource or via [`crate::App::with_tickrate`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TickRate {
    pub hz: f64,
    /// Upper bound on fixed steps per frame (spiral-of-death guard).
    pub max_steps_per_frame: u32,
}

impl TickRate {
    pub fn new(hz: f64) -> Self {
        assert!(hz > 0.0, "tickrate must be positive");
        Self { hz, max_steps_per_frame: 8 }
    }
    pub fn step(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.hz)
    }
}

impl Default for TickRate {
    fn default() -> Self {
        Self::new(60.0)
    }
}

/// Frame + fixed-step clock. Read it from resources inside any system.
#[derive(Clone, Debug, Default)]
pub struct Time {
    /// Real time since previous frame (clamped to 250 ms).
    pub delta: Duration,
    pub elapsed: Duration,
    pub frame: u64,
    /// Number of fixed ticks executed since start.
    pub tick: u64,
    /// Fixed step length.
    pub fixed_delta: Duration,
    /// Leftover fraction of a fixed step, for render interpolation (0..1).
    pub alpha: f32,
    /// Fixed steps that were dropped because the frame was too slow.
    pub dropped_ticks: u64,
    pub(crate) accumulator: Duration,
}

impl Time {
    pub fn delta_secs(&self) -> f32 {
        self.delta.as_secs_f32()
    }
    pub fn fixed_delta_secs(&self) -> f32 {
        self.fixed_delta.as_secs_f32()
    }
    pub fn elapsed_secs(&self) -> f64 {
        self.elapsed.as_secs_f64()
    }
}
