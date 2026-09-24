//! Load-test library: N headless client `App`s (real `ClientReplicationPlugin`, no render)
//! over real WebSockets, stepped at the tick rate on `clients / bots_per_thread` threads (each
//! thread builds its own `!Send` apps). A game wraps it in a five-line binary:
//!
//! ```ignore
//! fn main() {
//!     let cfg = LoadTestConfig::from_args();            // --url/--ps, --clients 200, --ramp 20/s, ...
//!     let report = space3d_server::loadtest::run(cfg, arena::protocol, |i| arena::WanderBot::new(i as u64)).unwrap();
//!     std::process::exit(if report.slo_passed { 0 } else { 1 });
//! }
//! ```
//!
//! Report: connect success + handshake p50/p99, RTT distribution, snapshot rate received,
//! down/up bytes/s per client, rollbacks/s, disconnect reasons, and server `/metrics` tick
//! histogram deltas (scraped before/after). SLO gates → non-zero exit. Nightly CI, not
//! `cargo test`.

use serde::{Deserialize, Serialize};
use space3d_core::App;
use space3d_replication::NetInput;
use space3d_replication::bots::Bot;
use std::time::Duration;

/// How bots get in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoadTarget {
    /// Straight to a game server with dev auth (`allow_dev_auth` must be on).
    DirectDev {
        /// `ws://host:7777/play`.
        url: String,
    },
    /// Through the player service: guest login → matchmaking/quick play → ticket.
    PlayerService {
        /// Base URL.
        url: String,
        /// Queue (matchmaking) or mode (quick play).
        queue: String,
        /// Use quick play instead of matchmaking.
        quick_play: bool,
    },
}

/// Service-level objectives (a breach makes `slo_passed` false).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Slo {
    /// Fraction of clients that must connect.
    pub connect_success: f64,
    /// Server tick p99 ceiling (ms), from `/metrics`.
    pub tick_p99_ms: Option<f64>,
    /// Minimum received snapshot rate as a fraction of nominal.
    pub snapshot_rate: f64,
    /// Unexpected disconnects allowed.
    pub max_unexpected_disconnects: u32,
}

impl Default for Slo {
    fn default() -> Self {
        Self {
            connect_success: 1.0,
            tick_p99_ms: Some(4.0),
            snapshot_rate: 0.95,
            max_unexpected_disconnects: 0,
        }
    }
}

/// Load-test settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadTestConfig {
    /// Target.
    pub target: LoadTarget,
    /// Simulated clients.
    pub clients: usize,
    /// New clients per second.
    pub ramp_per_sec: f64,
    /// Test duration after ramp-up.
    pub duration: Duration,
    /// Clients per OS thread.
    pub bots_per_thread: usize,
    /// Client tick rate.
    pub tick_hz: f64,
    /// Server `/metrics` URL to scrape.
    pub metrics_url: Option<String>,
    /// Gates.
    pub slo: Slo,
    /// Game id / version for the Hello.
    pub game_id: String,
}

/// Results.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadTestReport {
    /// Clients that connected / attempted.
    pub connected: usize,
    /// Attempted.
    pub attempted: usize,
    /// Handshake p50 / p99 (ms).
    pub handshake_ms: (f64, f64),
    /// RTT p50 / p99 (ms).
    pub rtt_ms: (f64, f64),
    /// Down bytes/s per client p50 / p99.
    pub down_bytes_per_sec: (f64, f64),
    /// Up bytes/s per client p50 / p99.
    pub up_bytes_per_sec: (f64, f64),
    /// Received snapshot rate / nominal.
    pub snapshot_rate: f64,
    /// Rollbacks per client per second.
    pub rollbacks_per_sec: f64,
    /// Disconnect reasons → count.
    pub disconnects: std::collections::BTreeMap<String, u32>,
    /// Server tick p99 (ms) from `/metrics`.
    pub server_tick_p99_ms: Option<f64>,
    /// All SLOs met.
    pub slo_passed: bool,
}

/// Run a load test (blocking; for binaries only).
pub fn run<I, B>(
    config: LoadTestConfig,
    protocol: fn(&mut App),
    make_bot: impl Fn(usize) -> B + Send + Sync + 'static,
) -> Result<LoadTestReport, crate::ServerError>
where
    I: NetInput,
    B: Bot<I>,
{
    todo!()
}
