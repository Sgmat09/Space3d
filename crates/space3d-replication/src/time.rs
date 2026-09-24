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

/// RFC 6298 smoothed RTT (α = 1/8, β = 1/4) plus a windowed minimum (last 32 samples).
#[derive(Clone, Debug, Default)]
pub struct RttEstimator {
    srtt: Option<Duration>,
    rttvar: Duration,
    min_rtt: Option<Duration>,
    samples: u64,
    recent: std::collections::VecDeque<Duration>,
}

impl RttEstimator {
    /// Window of the minimum filter.
    pub const MIN_WINDOW: usize = 32;

    /// Add a sample.
    pub fn sample(&mut self, rtt: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(rtt);
                self.rttvar = rtt / 2;
            }
            Some(srtt) => {
                let diff = srtt.abs_diff(rtt);
                self.rttvar = self.rttvar * 3 / 4 + diff / 4;
                self.srtt = Some(srtt * 7 / 8 + rtt / 8);
            }
        }
        self.recent.push_back(rtt);
        if self.recent.len() > Self::MIN_WINDOW {
            self.recent.pop_front();
        }
        self.min_rtt = self.recent.iter().min().copied();
        self.samples += 1;
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
        (self.srtt() + self.rttvar * 4).max(Duration::from_millis(30))
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
    /// `estimate(now) = offset_ticks + now · tick_hz`.
    offset_ticks: Option<f64>,
    samples: Vec<(f64, Duration)>,
    last_update: Duration,
}

impl ServerClock {
    /// Samples kept.
    pub const WINDOW: usize = 16;
    /// Error (ticks) beyond which the estimate snaps instead of slewing.
    pub const SNAP_TICKS: f64 = 5.0;
    /// Maximum slew in ticks per second.
    pub const SLEW_TICKS_PER_SEC: f64 = 1.0;

    /// For a room ticking at `tick_hz`.
    pub fn new(tick_hz: f64) -> Self {
        Self {
            tick_hz,
            offset_ticks: None,
            samples: Vec::new(),
            last_update: Duration::ZERO,
        }
    }

    /// Room tick rate.
    pub fn tick_hz(&self) -> f64 {
        self.tick_hz
    }

    /// Feed a Pong received at `now` whose ping was sent at `pong.client_time_us`.
    pub fn on_pong(&mut self, pong: &Pong, now: Duration) {
        let sent = Duration::from_micros(pong.client_time_us);
        let rtt = now
            .saturating_sub(sent)
            .saturating_sub(Duration::from_micros(u64::from(pong.hold_us)));
        let server_now = f64::from(pong.server_tick.0)
            + f64::from(pong.tick_frac) / 65536.0
            + rtt.as_secs_f64() / 2.0 * self.tick_hz;
        self.samples.push((server_now - now.as_secs_f64() * self.tick_hz, rtt));
        if self.samples.len() > Self::WINDOW {
            self.samples.remove(0);
        }
        let mut by_rtt = self.samples.clone();
        by_rtt.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.total_cmp(&b.0)));
        by_rtt.truncate(by_rtt.len().div_ceil(2));
        let mut offsets: Vec<f64> = by_rtt.iter().map(|s| s.0).collect();
        offsets.sort_by(f64::total_cmp);
        let target = offsets[offsets.len() / 2];
        match self.offset_ticks {
            None => self.offset_ticks = Some(target),
            Some(cur) => {
                let err = target - cur;
                if err.abs() > Self::SNAP_TICKS {
                    self.offset_ticks = Some(target);
                } else {
                    let dt = now.saturating_sub(self.last_update).as_secs_f64();
                    let max = (Self::SLEW_TICKS_PER_SEC * dt).max(0.0);
                    self.offset_ticks = Some(cur + err.clamp(-max, max));
                }
            }
        }
        self.last_update = now;
    }

    /// Server tick (fractional) the server is simulating at local time `now`.
    pub fn estimate(&self, now: Duration) -> Option<f64> {
        self.offset_ticks.map(|o| o + now.as_secs_f64() * self.tick_hz)
    }

    /// Whether enough samples arrived to trust [`Self::estimate`].
    pub fn is_synced(&self) -> bool {
        self.offset_ticks.is_some()
    }

    /// Number of samples in the window.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
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
///
/// The wire value is in **quarter ticks** (`i8`, `i8::MIN` = the input was missing); see
/// [`TickPacer::on_feedback`]. Head-of-line stalls (a burst of missing inputs) raise the target
/// slack by one tick for a while.
#[derive(Clone, Debug)]
pub struct TickPacer {
    config: PacerConfig,
    integral: f64,
    scale: f64,
    stall_boost: f64,
}

impl TickPacer {
    /// Proportional gain (scale per tick of error).
    pub const KP: f64 = 0.02;
    /// Integral gain per feedback sample.
    pub const KI: f64 = 0.002;

    /// New pacer at scale 1.
    pub fn new(config: PacerConfig) -> Self {
        Self {
            config,
            integral: 0.0,
            scale: 1.0,
            stall_boost: 0.0,
        }
    }

    /// Target slack in ticks for the current jitter.
    pub fn target(&self, jitter_ticks: f64) -> f64 {
        self.config.target_slack_ticks + 2.0 * jitter_ticks.max(0.0) + self.stall_boost
    }

    /// Feed one snapshot's feedback. `input_slack` is in quarter ticks; `i8::MIN` means
    /// "your input was missing".
    pub fn on_feedback(&mut self, input_slack: i8, jitter_ticks: f64) -> PaceAction {
        let slack = if input_slack == i8::MIN {
            None
        } else {
            Some(f64::from(input_slack) / 4.0)
        };
        self.on_slack(slack, jitter_ticks)
    }

    /// Feed a decoded slack in ticks (`None` = input missing).
    pub fn on_slack(&mut self, slack: Option<f64>, jitter_ticks: f64) -> PaceAction {
        let slack = match slack {
            Some(s) => {
                self.stall_boost = (self.stall_boost - 0.01).max(0.0);
                s
            }
            None => {
                // Missing input: likely a stall on a stream transport; ask for more margin.
                self.stall_boost = (self.stall_boost + 0.25).min(2.0);
                -1.0
            }
        };
        let error = self.target(jitter_ticks) - slack;
        if error.abs() > self.config.hard_resync_ticks {
            self.integral = 0.0;
            self.scale = 1.0;
            return PaceAction::Resync(error.round() as i64);
        }
        let max = self.config.max_scale_delta;
        self.integral = (self.integral + Self::KI * error).clamp(-max, max);
        self.scale = 1.0 + (Self::KP * error + self.integral).clamp(-max, max);
        PaceAction::Scale(self.scale)
    }

    /// Current scale.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Forget the controller state (after a resync / reconnect).
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.scale = 1.0;
    }

    /// Settings.
    pub fn config(&self) -> &PacerConfig {
        &self.config
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
    /// Max playback speed deviation.
    pub const MAX_SPEED_DELTA: f64 = 0.05;
    /// Error (ticks) beyond which the render clock snaps.
    pub const SNAP_TICKS: f64 = 10.0;

    /// New clock.
    pub fn new(config: InterpConfig) -> Self {
        Self {
            config,
            render_tick: None,
            delay_ticks: 0.0,
        }
    }

    /// Target delay in ticks for the given conditions.
    pub fn target_delay_ticks(&self, jitter: Duration, tick_hz: f64, send_interval: u8) -> f64 {
        let raw =
            f64::from(send_interval.max(1)) + f64::from(self.config.jitter_k) * jitter.as_secs_f64() * tick_hz + 0.5;
        let lo = self.config.min_delay.as_secs_f64() * tick_hz;
        let hi = self.config.max_delay.as_secs_f64() * tick_hz;
        raw.clamp(lo, hi.max(lo))
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
        let target = server_estimate - self.target_delay_ticks(jitter, tick_hz, send_interval);
        let render = match self.render_tick {
            None => target,
            Some(r) => {
                let advanced = r + frame_dt.as_secs_f64() * tick_hz;
                let err = target - advanced;
                if err.abs() > Self::SNAP_TICKS {
                    target
                } else {
                    let speed = 1.0 + (err * 0.1).clamp(-Self::MAX_SPEED_DELTA, Self::MAX_SPEED_DELTA);
                    r + frame_dt.as_secs_f64() * tick_hz * speed
                }
            }
        };
        self.render_tick = Some(render);
        self.delay_ticks = server_estimate - render;
        render
    }

    /// Longest extrapolation in ticks.
    pub fn max_extrapolation_ticks(&self, tick_hz: f64) -> f64 {
        self.config.max_extrapolation.as_secs_f64() * tick_hz
    }

    /// Current interpolation delay in ticks.
    pub fn delay_ticks(&self) -> f64 {
        self.delay_ticks
    }

    /// Last render tick.
    pub fn render_tick(&self) -> Option<f64> {
        self.render_tick
    }

    /// Forget the render position (reconnect).
    pub fn reset(&mut self) {
        self.render_tick = None;
    }
}

/// Convert a fractional tick to the tick it lies in.
pub fn floor_tick(t: f64) -> Tick {
    Tick(t.max(0.0).floor() as u32)
}

/// `Duration` as whole microseconds (wire timestamps).
pub fn as_micros(d: Duration) -> u64 {
    d.as_micros().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::conditioner::SplitMix64;

    #[test]
    fn rtt_estimator_follows_rfc6298() {
        let mut r = RttEstimator::default();
        assert_eq!(r.rto(), Duration::from_millis(30));
        r.sample(Duration::from_millis(100));
        assert_eq!(r.srtt(), Duration::from_millis(100));
        assert_eq!(r.rttvar(), Duration::from_millis(50));
        r.sample(Duration::from_millis(200));
        assert_eq!(r.srtt(), Duration::from_micros(112_500));
        assert_eq!(r.rttvar(), Duration::from_micros(62_500));
        assert_eq!(r.min_rtt(), Some(Duration::from_millis(100)));
        assert_eq!(r.rto(), Duration::from_micros(112_500 + 250_000));
        for _ in 0..40 {
            r.sample(Duration::from_millis(150));
        }
        // The window forgot the 100 ms sample.
        assert_eq!(r.min_rtt(), Some(Duration::from_millis(150)));
    }

    /// Simulates a server at 60 Hz, a client whose clock drifts 300 ppm, and 100 ± 30 ms RTT.
    #[test]
    fn server_clock_converges_under_jitter_and_drift() {
        let hz = 60.0;
        let mut clock = ServerClock::new(hz);
        let mut rng = SplitMix64::new(3);
        let drift = 1.0003;
        let epoch_ticks = 1000.0; // server started earlier
        let mut max_err_after_3s: f64 = 0.0;
        for i in 0..40u32 {
            // Client local time of the ping (pings at 4 Hz after an initial burst).
            let t_local = if i < 8 {
                0.1 * f64::from(i)
            } else {
                0.8 + 0.25 * f64::from(i - 8)
            };
            let up = 0.035 + 0.03 * f64::from(rng.next_f32());
            let down = 0.035 + 0.03 * f64::from(rng.next_f32());
            // True (server) time = local * drift.
            let server_time_at_rx = t_local * drift + up;
            let server_tick = epoch_ticks + server_time_at_rx * hz;
            let pong = Pong {
                id: i as u16,
                client_time_us: (t_local * 1e6) as u64,
                server_tick: Tick(server_tick.floor() as u32),
                tick_frac: ((server_tick.fract()) * 65536.0) as u16,
                hold_us: 0,
            };
            let recv_local = (server_time_at_rx + down) / drift;
            clock.on_pong(&pong, Duration::from_secs_f64(recv_local));
            let est = clock.estimate(Duration::from_secs_f64(recv_local)).unwrap();
            let truth = epoch_ticks + recv_local * drift * hz;
            if recv_local > 3.0 {
                max_err_after_3s = max_err_after_3s.max((est - truth).abs());
            }
        }
        assert!(max_err_after_3s <= 1.0, "error {max_err_after_3s} ticks");
    }

    #[test]
    fn server_clock_snaps_on_large_error() {
        let mut c = ServerClock::new(60.0);
        let pong = |tick: u32, t: u64| Pong {
            id: 0,
            client_time_us: t,
            server_tick: Tick(tick),
            tick_frac: 0,
            hold_us: 0,
        };
        c.on_pong(&pong(100, 0), Duration::ZERO);
        assert_eq!(c.estimate(Duration::ZERO), Some(100.0));
        // Server restarted from a checkpoint far away: all samples jump.
        for _ in 0..16 {
            c.on_pong(&pong(1000, 0), Duration::ZERO);
        }
        assert_eq!(c.estimate(Duration::ZERO), Some(1000.0));
        c.reset();
        assert!(!c.is_synced());
    }

    #[test]
    fn pacer_speeds_up_when_late_and_resyncs_when_far() {
        let mut p = TickPacer::new(PacerConfig::default());
        match p.on_feedback(0, 0.0) {
            PaceAction::Scale(s) => assert!(s > 1.0 && s <= 1.05),
            other => panic!("{other:?}"),
        }
        let mut p = TickPacer::new(PacerConfig::default());
        match p.on_feedback(4 * 6, 0.0) {
            PaceAction::Scale(s) => assert!((0.95..1.0).contains(&s)),
            other => panic!("{other:?}"),
        }
        // 20 ticks late → jump forward.
        assert!(matches!(p.on_feedback(-80, 0.0), PaceAction::Resync(n) if n > 8));
        // Missing input raises the target.
        let t0 = p.target(0.0);
        p.on_feedback(i8::MIN, 0.0);
        assert!(p.target(0.0) > t0);
    }

    #[test]
    fn pacer_converges_in_closed_loop() {
        // Plant: slack integrates (scale - 1) * ticks; the pacer should settle near the target.
        let mut p = TickPacer::new(PacerConfig::default());
        let mut slack = 6.0f64;
        for _ in 0..3000 {
            let q = (slack * 4.0).round().clamp(-127.0, 127.0) as i8;
            match p.on_feedback(q, 0.0) {
                PaceAction::Scale(s) => slack += (s - 1.0) * 2.0,
                PaceAction::Resync(n) => slack += n as f64,
            }
        }
        assert!((slack - 1.5).abs() < 0.3, "slack {slack}");
    }

    #[test]
    fn interp_clock_is_monotonic_and_tracks_delay() {
        let mut c = InterpClock::new(InterpConfig::default());
        let hz = 60.0;
        let mut rng = SplitMix64::new(9);
        let mut last = f64::MIN;
        let mut server = 100.0;
        for _ in 0..600 {
            let dt = Duration::from_secs_f64(1.0 / 60.0 + 0.004 * (f64::from(rng.next_f32()) - 0.5));
            server += dt.as_secs_f64() * hz;
            // Estimate noise ± 0.3 tick.
            let est = server + 0.6 * (f64::from(rng.next_f32()) - 0.5);
            let r = c.update(dt, est, Duration::from_millis(10), hz, 2);
            assert!(r > last, "render tick went backwards");
            last = r;
        }
        let target = c.target_delay_ticks(Duration::from_millis(10), hz, 2);
        assert!(
            (c.delay_ticks() - target).abs() < 1.0,
            "delay {} vs {}",
            c.delay_ticks(),
            target
        );
        assert!(target >= 35e-3 * hz && target <= 0.25 * hz);
    }
}
