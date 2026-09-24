//! Network time: an injectable clock, RTT estimation, server-tick estimation, client tick
//! pacing (time dilation) and the interpolation render clock.
//!
//! Nothing in this crate reads the wall clock directly: everything goes through
//! [`NetClockHandle`] so the harness can drive server, clients and the memory network from one
//! [`ManualClock`] (AGENTS rule 4). Plugins insert a [`SystemClock`] handle if none exists.
//!
//! Tick pacing needs no `space3d-core` change: [`TickPacer`] yields a scale in
//! `[1 - max_scale_delta, 1 + max_scale_delta]` that the client driver applies to
//! `TickRate::hz` (so the fixed accumulator runs slightly faster/slower), while
//! `sim::run_sim` pins `Time::fixed_delta` to the nominal step for the simulation itself.

use crate::Tick;
use crate::protocol::control::Pong;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Monotonic time source.
pub trait NetClock: Send + Sync + 'static {
    /// Time since an arbitrary fixed origin.
    fn now(&self) -> Duration;
}

/// Real monotonic clock (`web_time::Instant`, works on wasm).
#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    origin: web_time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: web_time::Instant::now(),
        }
    }
}

impl NetClock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// Virtual clock advanced explicitly (tests, harness). Clones share the same time.
#[derive(Clone, Debug, Default)]
pub struct ManualClock {
    nanos: Arc<AtomicU64>,
}

impl ManualClock {
    /// Clock at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance by `d`.
    pub fn advance(&self, d: Duration) {
        self.nanos.fetch_add(d.as_nanos() as u64, Ordering::SeqCst);
    }

    /// Jump to `t` (must not go backwards).
    pub fn set(&self, t: Duration) {
        self.nanos.store(t.as_nanos() as u64, Ordering::SeqCst);
    }
}

impl NetClock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }
}

/// Resource holding the clock used by all replication systems of an `App`.
#[derive(Clone)]
pub struct NetClockHandle(pub Arc<dyn NetClock>);

impl NetClockHandle {
    /// Current time.
    pub fn now(&self) -> Duration {
        self.0.now()
    }
}

impl Default for NetClockHandle {
    fn default() -> Self {
        Self(Arc::new(SystemClock::default()))
    }
}

/// RFC 6298 smoothed RTT (α = 1/8, β = 1/4) plus a windowed minimum.
#[derive(Clone, Debug, Default)]
pub struct RttEstimator {
    srtt: Option<Duration>,
    rttvar: Duration,
    min_rtt: Option<Duration>,
    samples: u64,
}

impl RttEstimator {
    /// Add a sample.
    pub fn sample(&mut self, rtt: Duration) {
        todo!()
    }

    /// Smoothed RTT (zero before the first sample).
    pub fn srtt(&self) -> Duration {
        self.srtt.unwrap_or_default()
    }

    /// RTT variation (jitter estimate).
    pub fn rttvar(&self) -> Duration {
        self.rttvar
    }

    /// Smallest recent sample.
    pub fn min_rtt(&self) -> Option<Duration> {
        self.min_rtt
    }

    /// `srtt + 4 · rttvar` (loss timeout for acked policies), at least 30 ms.
    pub fn rto(&self) -> Duration {
        todo!()
    }

    /// Number of samples so far.
    pub fn samples(&self) -> u64 {
        self.samples
    }
}

/// Estimates the server's current (fractional) tick from Pong samples: keeps the last 16
/// samples, uses the median of the half with the lowest RTT, slews at most 1 tick/s and
/// snaps when the error exceeds 5 ticks.
#[derive(Clone, Debug)]
pub struct ServerClock {
    tick_hz: f64,
    offset_ticks: Option<f64>,
    samples: Vec<(f64, Duration)>,
}

impl ServerClock {
    /// For a room ticking at `tick_hz`.
    pub fn new(tick_hz: f64) -> Self {
        Self {
            tick_hz,
            offset_ticks: None,
            samples: Vec::new(),
        }
    }

    /// Feed a Pong received at `now` whose ping was sent at `pong.client_time_us`.
    pub fn on_pong(&mut self, pong: &Pong, now: Duration) {
        todo!()
    }

    /// Server tick (fractional) the server is simulating at local time `now`.
    pub fn estimate(&self, now: Duration) -> Option<f64> {
        todo!()
    }

    /// Whether enough samples arrived to trust [`Self::estimate`].
    pub fn is_synced(&self) -> bool {
        self.offset_ticks.is_some()
    }

    /// Forget all samples (after reconnect).
    pub fn reset(&mut self) {
        self.offset_ticks = None;
        self.samples.clear();
    }
}

/// Tick-pacing configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PacerConfig {
    /// Max relative speed change (default 0.05 → scale in `[0.95, 1.05]`).
    pub max_scale_delta: f64,
    /// Desired server-reported input slack in ticks (default 1.5; raised by `2 · jitter_ticks`
    /// and after detected head-of-line stalls).
    pub target_slack_ticks: f64,
    /// Error beyond which the client jumps its predicted tick instead of dilating (default 8).
    pub hard_resync_ticks: f64,
}

impl Default for PacerConfig {
    fn default() -> Self {
        Self {
            max_scale_delta: 0.05,
            target_slack_ticks: 1.5,
            hard_resync_ticks: 8.0,
        }
    }
}

/// What the pacer wants the client driver to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PaceAction {
    /// Run fixed ticks at `nominal_hz * scale`.
    Scale(f64),
    /// Jump the predicted tick by this many ticks (positive = forward) and clear prediction history.
    Resync(i64),
}

/// PI controller on the server's `input_slack` feedback (how many ticks early this client's
/// input for a tick arrived): too little slack → speed up, too much → slow down.
#[derive(Clone, Debug)]
pub struct TickPacer {
    config: PacerConfig,
    integral: f64,
    scale: f64,
}

impl TickPacer {
    /// New pacer at scale 1.
    pub fn new(config: PacerConfig) -> Self {
        Self {
            config,
            integral: 0.0,
            scale: 1.0,
        }
    }

    /// Feed one snapshot's feedback. `input_slack = i8::MIN` means "your input was missing".
    pub fn on_feedback(&mut self, input_slack: i8, jitter_ticks: f64) -> PaceAction {
        todo!()
    }

    /// Current scale.
    pub fn scale(&self) -> f64 {
        self.scale
    }
}

/// Interpolation configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InterpConfig {
    /// Lower bound of the adaptive interpolation delay (default 35 ms).
    pub min_delay: Duration,
    /// Upper bound (default 250 ms).
    pub max_delay: Duration,
    /// Jitter multiplier: `delay = send_interval + k · jitter + ½ tick` (default 2).
    pub jitter_k: f32,
    /// Longest extrapolation when the buffer runs dry (default 100 ms), then freeze.
    pub max_extrapolation: Duration,
}

impl Default for InterpConfig {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_millis(35),
            max_delay: Duration::from_millis(250),
            jitter_k: 2.0,
            max_extrapolation: Duration::from_millis(100),
        }
    }
}

/// Render clock for remote entities: `render_tick = server_estimate - delay`, where the delay
/// adapts to jitter and is slewed by at most ±5 % playback speed (snaps beyond 10 ticks).
#[derive(Clone, Debug)]
pub struct InterpClock {
    config: InterpConfig,
    render_tick: Option<f64>,
    delay_ticks: f64,
}

impl InterpClock {
    /// New clock.
    pub fn new(config: InterpConfig) -> Self {
        Self {
            config,
            render_tick: None,
            delay_ticks: 0.0,
        }
    }

    /// Advance by the frame delta and return the render tick (fractional).
    pub fn update(
        &mut self,
        frame_dt: Duration,
        server_estimate: f64,
        jitter: Duration,
        tick_hz: f64,
        send_interval: u8,
    ) -> f64 {
        todo!()
    }

    /// Current interpolation delay in ticks.
    pub fn delay_ticks(&self) -> f64 {
        self.delay_ticks
    }

    /// Last render tick.
    pub fn render_tick(&self) -> Option<f64> {
        self.render_tick
    }
}

/// Convert a fractional tick to the tick it lies in.
pub fn floor_tick(t: f64) -> Tick {
    Tick(t.max(0.0).floor() as u32)
}
