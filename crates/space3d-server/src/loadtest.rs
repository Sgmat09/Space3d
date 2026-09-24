//! Load-test library: N headless client `App`s (real `ClientReplicationPlugin`, no render)
//! over real WebSockets, stepped at the tick rate on `clients / bots_per_thread` threads (each
//! thread builds its own `!Send` apps). A game wraps it in a five-line binary:
//!
//! ```ignore
//! fn main() {
//!     let cfg = LoadTestConfig::from_args();            // --url/--ps, --clients 200, --ramp 20, ...
//!     let report = space3d_server::loadtest::run(cfg, arena::protocol, |i| arena::WanderBot::new(i as u64)).unwrap();
//!     println!("{}", serde_json::to_string_pretty(&report).unwrap());
//!     std::process::exit(if report.slo_passed { 0 } else { 1 });
//! }
//! ```
//!
//! Report: connect success + handshake p50/p99, RTT distribution, snapshot rate received,
//! down/up bytes/s per client, rollbacks/s, disconnect reasons, and the server tick p99 from
//! the `/metrics` `space3d_tick_seconds` histogram (scraped before/after, deltas only). SLO
//! gates → `slo_passed`. Nightly CI, not `cargo test` (the in-repo e2e test runs a tiny one).
//!
//! Real time drives everything here (it measures a real deployment); the run is bounded by
//! `ramp + duration + 10 s`.

use serde::{Deserialize, Serialize};
use space3d_core::{App, EventReader, Events};
use space3d_net::Http;
use space3d_replication::api::{Assignment, MatchmakeRequest, QuickPlayRequest, SessionTokens, TicketId, TicketStatus};
use space3d_replication::bots::{Bot, install_bot};
use space3d_replication::client::{ClientConfig, ClientReplicationPlugin, ConnectTarget, ConnectionEvent, NetClient};
use space3d_replication::services::{DeviceCredentials, OnlineError, PlayerServiceClient};
use space3d_replication::{NetInput, Tick};
use space3d_tasks::Task;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

impl Default for LoadTestConfig {
    fn default() -> Self {
        Self {
            target: LoadTarget::DirectDev {
                url: "ws://127.0.0.1:7777/play".into(),
            },
            clients: 200,
            ramp_per_sec: 20.0,
            duration: Duration::from_secs(60),
            bots_per_thread: 25,
            tick_hz: 60.0,
            metrics_url: None,
            slo: Slo::default(),
            game_id: String::new(),
        }
    }
}

/// Command line of load-test binaries.
#[derive(clap::Parser, Debug)]
#[command(about = "Space3d load test: headless bots over real WebSockets")]
struct LoadTestArgs {
    /// Game server `/play` URL (dev auth). Mutually exclusive with `--ps`.
    #[arg(long)]
    url: Option<String>,
    /// Player-service base URL (guest login + matchmaking / quick play).
    #[arg(long)]
    ps: Option<String>,
    /// Queue (matchmaking) or mode (with `--quick-play`).
    #[arg(long, default_value = "default")]
    queue: String,
    /// Use quick play instead of matchmaking.
    #[arg(long)]
    quick_play: bool,
    /// Number of bots.
    #[arg(long, default_value_t = 200)]
    clients: usize,
    /// New bots per second.
    #[arg(long, default_value_t = 20.0)]
    ramp: f64,
    /// Seconds to run after the ramp.
    #[arg(long, default_value_t = 60)]
    duration: u64,
    /// Bots per OS thread.
    #[arg(long, default_value_t = 25)]
    bots_per_thread: usize,
    /// Client tick rate.
    #[arg(long, default_value_t = 60.0)]
    tick_hz: f64,
    /// Server `/metrics` URL (admin port) to scrape the tick histogram from.
    #[arg(long)]
    metrics: Option<String>,
    /// `Hello::game_id`.
    #[arg(long, default_value = "")]
    game_id: String,
    /// SLO: fraction of bots that must connect.
    #[arg(long, default_value_t = 1.0)]
    slo_connect: f64,
    /// SLO: server tick p99 ceiling in ms (needs `--metrics`).
    #[arg(long)]
    slo_tick_p99_ms: Option<f64>,
    /// SLO: minimum snapshot rate as a fraction of nominal.
    #[arg(long, default_value_t = 0.95)]
    slo_snapshot_rate: f64,
    /// SLO: unexpected disconnects allowed.
    #[arg(long, default_value_t = 0)]
    slo_max_disconnects: u32,
}

impl LoadTestConfig {
    /// Parse the process arguments (`--url` | `--ps`, `--clients`, `--ramp`, `--duration`,
    /// `--bots-per-thread`, `--tick-hz`, `--metrics`, `--game-id`, `--slo-*`). Exits with a
    /// usage message on bad input.
    pub fn from_args() -> Self {
        use clap::Parser;
        Self::from_parsed(LoadTestArgs::parse())
    }

    /// Like [`Self::from_args`] over explicit arguments (the first one is the program name).
    pub fn try_from_args<I, T>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        use clap::Parser;
        LoadTestArgs::try_parse_from(args)
            .map(Self::from_parsed)
            .map_err(|e| e.to_string())
    }

    fn from_parsed(a: LoadTestArgs) -> Self {
        let target = match (a.ps, a.url) {
            (Some(url), _) => LoadTarget::PlayerService {
                url,
                queue: a.queue,
                quick_play: a.quick_play,
            },
            (None, Some(url)) => LoadTarget::DirectDev { url },
            (None, None) => LoadTarget::DirectDev {
                url: "ws://127.0.0.1:7777/play".into(),
            },
        };
        Self {
            target,
            clients: a.clients,
            ramp_per_sec: a.ramp,
            duration: Duration::from_secs(a.duration),
            bots_per_thread: a.bots_per_thread,
            tick_hz: a.tick_hz,
            slo: Slo {
                connect_success: a.slo_connect,
                tick_p99_ms: a.slo_tick_p99_ms.or(a.metrics.as_ref().map(|_| 4.0)),
                snapshot_rate: a.slo_snapshot_rate,
                max_unexpected_disconnects: a.slo_max_disconnects,
            },
            metrics_url: a.metrics,
            game_id: a.game_id,
        }
    }
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

// ---- per-bot state -----------------------------------------------------------------------------

enum Stage {
    /// Not started yet.
    Waiting,
    /// Guest login in flight.
    LoggingIn(Task<Result<SessionTokens, OnlineError>>),
    /// Matchmaking ticket request in flight.
    Queueing(Task<Result<TicketId, OnlineError>>),
    /// Polling a ticket.
    Polling {
        ticket: TicketId,
        inflight: Option<Task<Result<TicketStatus, OnlineError>>>,
        next_at: Instant,
    },
    /// Quick play in flight.
    QuickPlay(Task<Result<Assignment, OnlineError>>),
    /// `NetClient::connect` called.
    Joined,
    /// Gave up before joining (the reason is recorded).
    Failed,
}

struct BotSlot {
    index: usize,
    start_at: Instant,
    app: Option<App>,
    api: Option<PlayerServiceClient>,
    stage: Stage,
    events: EventReader,
    connect_started: Option<Instant>,
    connected_at: Option<Instant>,
    handshake: Option<Duration>,
    ever_connected: bool,
    last_snapshot: Tick,
    snapshots: u64,
    rtt_ms: Vec<f64>,
    bytes_at_connect: (u64, u64),
    disconnects: Vec<String>,
}

/// What one bot measured.
#[derive(Clone, Debug, Default)]
struct BotResult {
    attempted: bool,
    connected: bool,
    handshake_ms: Option<f64>,
    rtt_ms: Vec<f64>,
    connected_secs: f64,
    down_bytes: u64,
    up_bytes: u64,
    snapshots: u64,
    rollbacks: u64,
    disconnects: Vec<String>,
}

struct ThreadPlan<I: NetInput, B: Bot<I>> {
    config: Arc<LoadTestConfig>,
    protocol: fn(&mut App),
    make_bot: Arc<dyn Fn(usize) -> B + Send + Sync>,
    indices: Vec<usize>,
    started: Instant,
    _input: std::marker::PhantomData<fn() -> I>,
}

fn build_client<I: NetInput, B: Bot<I>>(plan: &ThreadPlan<I, B>, index: usize) -> App {
    let config = &plan.config;
    let mut app = App::new().with_tickrate(config.tick_hz);
    app.add_plugin(ClientReplicationPlugin {
        config: ClientConfig {
            game_id: config.game_id.clone(),
            ..Default::default()
        },
    });
    (plan.protocol)(&mut app);
    install_bot::<I>(&mut app, (plan.make_bot)(index));
    app
}

fn advance_stage(slot: &mut BotSlot, target: &LoadTarget, http: &Http, now: Instant) {
    let fail = |slot: &mut BotSlot, what: &str, e: OnlineError| {
        slot.disconnects
            .push(format!("{what}: {}", e.code().unwrap_or("error")));
        slot.stage = Stage::Failed;
    };
    loop {
        match std::mem::replace(&mut slot.stage, Stage::Failed) {
            Stage::Waiting => match target {
                LoadTarget::DirectDev { url } => {
                    connect(
                        slot,
                        ConnectTarget::Dev {
                            url: url.clone(),
                            name: format!("bot-{}", slot.index),
                        },
                    );
                    return;
                }
                LoadTarget::PlayerService { url, .. } => {
                    let api = PlayerServiceClient::new(http, url);
                    let name = format!("bot-{}", slot.index);
                    slot.stage = Stage::LoggingIn(api.login_guest(&DeviceCredentials::generate(), Some(&name)));
                    slot.api = Some(api);
                    return;
                }
            },
            Stage::LoggingIn(mut task) => match task.poll() {
                None => {
                    slot.stage = Stage::LoggingIn(task);
                    return;
                }
                Some(Err(e)) => return fail(slot, "login", e),
                Some(Ok(_)) => {
                    let (LoadTarget::PlayerService { queue, quick_play, .. }, Some(api)) = (target, &slot.api) else {
                        return;
                    };
                    slot.stage = if *quick_play {
                        Stage::QuickPlay(api.quick_play(&QuickPlayRequest {
                            mode: queue.clone(),
                            ..Default::default()
                        }))
                    } else {
                        let t = api.matchmake(&MatchmakeRequest {
                            queue: queue.clone(),
                            ..Default::default()
                        });
                        Stage::Queueing(space3d_tasks::spawn(async move {
                            t.wait()
                                .await
                                .unwrap_or(Err(OnlineError::Net(space3d_net::NetError::Closed)))
                                .map(|r| r.ticket_id)
                        }))
                    };
                }
            },
            Stage::Queueing(mut task) => match task.poll() {
                None => {
                    slot.stage = Stage::Queueing(task);
                    return;
                }
                Some(Err(e)) => return fail(slot, "matchmake", e),
                Some(Ok(ticket)) => {
                    slot.stage = Stage::Polling {
                        ticket,
                        inflight: None,
                        next_at: now,
                    }
                }
            },
            Stage::Polling {
                ticket,
                mut inflight,
                next_at,
            } => {
                let api = slot.api.clone().expect("logged in");
                match inflight.as_mut().map(Task::poll) {
                    None if now >= next_at => {
                        slot.stage = Stage::Polling {
                            inflight: Some(api.poll_ticket(&ticket, 0)),
                            ticket,
                            next_at,
                        };
                        return;
                    }
                    None | Some(None) => {
                        slot.stage = Stage::Polling {
                            ticket,
                            inflight,
                            next_at,
                        };
                        return;
                    }
                    Some(Some(Err(e))) => return fail(slot, "ticket", e),
                    Some(Some(Ok(TicketStatus::Found { assignment }))) => {
                        connect(slot, ticket_target(&assignment));
                        return;
                    }
                    Some(Some(Ok(TicketStatus::Searching { .. }))) => {
                        slot.stage = Stage::Polling {
                            ticket,
                            inflight: None,
                            next_at: now + Duration::from_millis(250),
                        };
                        return;
                    }
                    Some(Some(Ok(other))) => {
                        slot.disconnects.push(format!("ticket: {other:?}").to_lowercase());
                        slot.stage = Stage::Failed;
                        return;
                    }
                }
            }
            Stage::QuickPlay(mut task) => match task.poll() {
                None => {
                    slot.stage = Stage::QuickPlay(task);
                    return;
                }
                Some(Err(e)) => return fail(slot, "quick_play", e),
                Some(Ok(assignment)) => {
                    connect(slot, ticket_target(&assignment));
                    return;
                }
            },
            s @ (Stage::Joined | Stage::Failed) => {
                slot.stage = s;
                return;
            }
        }
    }
}

fn ticket_target(a: &Assignment) -> ConnectTarget {
    ConnectTarget::Ticket {
        url: a.server_url.clone(),
        ticket: a.join_ticket.clone(),
    }
}

fn connect(slot: &mut BotSlot, target: ConnectTarget) {
    if let Some(app) = &mut slot.app {
        app.resources.expect_mut::<NetClient>().connect(target);
    }
    slot.connect_started = Some(Instant::now());
    slot.stage = Stage::Joined;
}

fn observe(slot: &mut BotSlot, now: Instant) {
    let Some(app) = &slot.app else { return };
    let net = app.resources.expect::<NetClient>();
    if net.is_connected() {
        if slot.connected_at.is_none() {
            slot.connected_at = Some(now);
            slot.ever_connected = true;
            if let Some(start) = slot.connect_started {
                slot.handshake.get_or_insert(now - start);
            }
            let s = net.stats();
            slot.bytes_at_connect = (s.bytes_in, s.bytes_out);
        }
        let tick = net.last_snapshot_tick();
        if tick != slot.last_snapshot {
            slot.last_snapshot = tick;
            slot.snapshots += 1;
        }
        let rtt = net.stats().rtt;
        if !rtt.is_zero() {
            slot.rtt_ms.push(rtt.as_secs_f64() * 1000.0);
        }
    }
    if let Some(events) = app.resources.get::<Events<ConnectionEvent>>() {
        for e in slot.events.read(events) {
            match e {
                ConnectionEvent::Rejected(r) => slot.disconnects.push(format!("rejected: {:?}", r.code)),
                ConnectionEvent::Disconnected { code, reason, .. } => slot.disconnects.push(match code {
                    Some(code) => format!("{code:?}"),
                    None => reason.split(':').next().unwrap_or("closed").trim().to_owned(),
                }),
                _ => {}
            }
        }
    }
}

fn finish(slot: BotSlot, end: Instant) -> BotResult {
    let mut r = BotResult {
        attempted: !matches!(slot.stage, Stage::Waiting),
        connected: slot.ever_connected,
        handshake_ms: slot.handshake.map(|d| d.as_secs_f64() * 1000.0),
        rtt_ms: slot.rtt_ms,
        snapshots: slot.snapshots,
        disconnects: slot.disconnects,
        ..Default::default()
    };
    if let (Some(app), Some(at)) = (&slot.app, slot.connected_at) {
        let s = app.resources.expect::<NetClient>().stats();
        r.connected_secs = end.saturating_duration_since(at).as_secs_f64();
        r.down_bytes = s.bytes_in.saturating_sub(slot.bytes_at_connect.0);
        r.up_bytes = s.bytes_out.saturating_sub(slot.bytes_at_connect.1);
        r.rollbacks = s.rollbacks;
    }
    r
}

fn run_thread<I: NetInput, B: Bot<I>>(plan: ThreadPlan<I, B>) -> Vec<BotResult> {
    let config = plan.config.clone();
    let http = Http::new();
    let step = Duration::from_secs_f64(1.0 / config.tick_hz.max(1.0));
    let ramp = |i: usize| Duration::from_secs_f64(i as f64 / config.ramp_per_sec.max(0.001));
    let mut slots: Vec<BotSlot> = plan
        .indices
        .iter()
        .map(|&index| BotSlot {
            index,
            start_at: plan.started + ramp(index),
            app: None,
            api: None,
            stage: Stage::Waiting,
            events: EventReader::default(),
            connect_started: None,
            connected_at: None,
            handshake: None,
            ever_connected: false,
            last_snapshot: Tick::default(),
            snapshots: 0,
            rtt_ms: Vec::new(),
            bytes_at_connect: (0, 0),
            disconnects: Vec::new(),
        })
        .collect();
    let end = plan.started + ramp(config.clients) + config.duration;
    let mut last = Instant::now();
    let mut next_frame = Instant::now();
    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        let dt = now - last;
        last = now;
        for slot in &mut slots {
            if now < slot.start_at {
                continue;
            }
            if slot.app.is_none() {
                slot.app = Some(build_client(&plan, slot.index));
            }
            advance_stage(slot, &config.target, &http, now);
            if let Some(app) = &mut slot.app {
                app.update(dt);
            }
            observe(slot, now);
        }
        next_frame += step;
        let now = Instant::now();
        if next_frame > now {
            std::thread::sleep(next_frame - now);
        } else {
            next_frame = now;
        }
    }
    let end = Instant::now();
    // Leave cleanly, then give the goodbyes a moment to flush.
    for slot in &mut slots {
        if let Some(app) = &mut slot.app {
            app.resources.expect_mut::<NetClient>().disconnect();
        }
    }
    for _ in 0..3 {
        for slot in &mut slots {
            if let Some(app) = &mut slot.app {
                app.update(step);
            }
        }
        std::thread::sleep(step);
    }
    slots.into_iter().map(|s| finish(s, end)).collect()
}

// ---- metrics scraping ----------------------------------------------------------------------

/// Parse the cumulative buckets `(le, count)` of histogram `name` from Prometheus text (label
/// sets are summed per `le`).
pub fn parse_histogram(text: &str, name: &str) -> Vec<(f64, u64)> {
    let prefix = format!("{name}_bucket");
    let mut buckets: BTreeMap<u64, (f64, u64)> = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(&prefix) else {
            continue;
        };
        let Some(le_at) = rest.find("le=\"") else { continue };
        let le_str = &rest[le_at + 4..];
        let Some(end) = le_str.find('"') else { continue };
        let le = match &le_str[..end] {
            "+Inf" => f64::INFINITY,
            v => match v.parse::<f64>() {
                Ok(v) => v,
                Err(_) => continue,
            },
        };
        let Some(count) = rest.rsplit(' ').next().and_then(|v| v.trim().parse::<f64>().ok()) else {
            continue;
        };
        let e = buckets.entry(le.to_bits()).or_insert((le, 0));
        e.1 += count as u64;
    }
    let mut v: Vec<(f64, u64)> = buckets.into_values().collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    v
}

/// Upper bound of the `q` quantile of the observations made between two scrapes of the same
/// histogram (`None` without observations; `+Inf` if it falls in the overflow bucket).
pub fn histogram_quantile(before: &[(f64, u64)], after: &[(f64, u64)], q: f64) -> Option<f64> {
    let delta: Vec<(f64, u64)> = after
        .iter()
        .map(|(le, n)| {
            let prev = before.iter().find(|(l, _)| l == le).map_or(0, |(_, p)| *p);
            (*le, n.saturating_sub(prev))
        })
        .collect();
    let total = delta.last()?.1;
    if total == 0 {
        return None;
    }
    let rank = (q.clamp(0.0, 1.0) * total as f64).ceil() as u64;
    delta.iter().find(|(_, n)| *n >= rank.max(1)).map(|(le, _)| *le)
}

fn scrape(http: &Http, url: &str) -> Option<Vec<(f64, u64)>> {
    let res = http.get(url).block()?.ok()?;
    res.is_success()
        .then(|| parse_histogram(&res.text(), "space3d_tick_seconds"))
}

fn percentiles(mut v: Vec<f64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
    (at(0.5), at(0.99))
}

/// Aggregate per-bot results and evaluate the SLOs.
fn report(config: &LoadTestConfig, results: &[BotResult], tick_p99_ms: Option<f64>) -> LoadTestReport {
    let attempted = results.iter().filter(|r| r.attempted).count();
    let connected: Vec<&BotResult> = results.iter().filter(|r| r.connected).collect();
    let per_sec = |bytes: u64, secs: f64| if secs > 0.0 { bytes as f64 / secs } else { 0.0 };
    let live: Vec<&&BotResult> = connected.iter().filter(|r| r.connected_secs >= 1.0).collect();
    let nominal_snapshot_hz = config.tick_hz / 2.0;
    let snapshot_rate = if live.is_empty() {
        0.0
    } else {
        live.iter()
            .map(|r| r.snapshots as f64 / r.connected_secs / nominal_snapshot_hz)
            .sum::<f64>()
            / live.len() as f64
    };
    let total_secs: f64 = live.iter().map(|r| r.connected_secs).sum();
    let mut disconnects: BTreeMap<String, u32> = BTreeMap::new();
    for r in results {
        for d in &r.disconnects {
            *disconnects.entry(d.clone()).or_insert(0) += 1;
        }
    }
    let unexpected: u32 = disconnects
        .iter()
        .filter(|(k, _)| !k.contains("Leave"))
        .map(|(_, n)| *n)
        .sum();
    let mut rep = LoadTestReport {
        connected: connected.len(),
        attempted,
        handshake_ms: percentiles(connected.iter().filter_map(|r| r.handshake_ms).collect()),
        rtt_ms: percentiles(connected.iter().flat_map(|r| r.rtt_ms.iter().copied()).collect()),
        down_bytes_per_sec: percentiles(live.iter().map(|r| per_sec(r.down_bytes, r.connected_secs)).collect()),
        up_bytes_per_sec: percentiles(live.iter().map(|r| per_sec(r.up_bytes, r.connected_secs)).collect()),
        snapshot_rate,
        rollbacks_per_sec: if total_secs > 0.0 {
            live.iter().map(|r| r.rollbacks as f64).sum::<f64>() / total_secs
        } else {
            0.0
        },
        disconnects,
        server_tick_p99_ms: tick_p99_ms,
        slo_passed: false,
    };
    let slo = &config.slo;
    let connect_ok = attempted > 0 && rep.connected as f64 >= slo.connect_success * config.clients as f64 - 1e-9;
    let tick_ok = match (slo.tick_p99_ms, rep.server_tick_p99_ms) {
        (Some(limit), Some(p99)) => p99 <= limit,
        (Some(_), None) => config.metrics_url.is_none(),
        (None, _) => true,
    };
    rep.slo_passed =
        connect_ok && tick_ok && rep.snapshot_rate >= slo.snapshot_rate && unexpected <= slo.max_unexpected_disconnects;
    rep
}

/// Run a load test (blocking; for binaries and tests only): ramps `config.clients` bots at
/// `ramp_per_sec`, keeps them playing for `duration`, then disconnects them and reports.
pub fn run<I, B>(
    config: LoadTestConfig,
    protocol: fn(&mut App),
    make_bot: impl Fn(usize) -> B + Send + Sync + 'static,
) -> Result<LoadTestReport, crate::ServerError>
where
    I: NetInput,
    B: Bot<I>,
{
    if config.clients == 0 || config.bots_per_thread == 0 {
        return Err(crate::ServerError::Other(
            "clients and bots_per_thread must be positive".into(),
        ));
    }
    if !(config.tick_hz > 0.0 && config.ramp_per_sec > 0.0) {
        return Err(crate::ServerError::Other(
            "tick_hz and ramp_per_sec must be positive".into(),
        ));
    }
    let http = Http::new();
    let before = config.metrics_url.as_deref().and_then(|u| scrape(&http, u));
    let config = Arc::new(config);
    let make_bot: Arc<dyn Fn(usize) -> B + Send + Sync> = Arc::new(make_bot);
    let started = Instant::now() + Duration::from_millis(100);
    let threads = config.clients.div_ceil(config.bots_per_thread);
    let mut handles = Vec::with_capacity(threads);
    for t in 0..threads {
        // Interleave bots over threads so the ramp spreads across them.
        let indices: Vec<usize> = (t..config.clients).step_by(threads).collect();
        let plan = ThreadPlan::<I, B> {
            config: config.clone(),
            protocol,
            make_bot: make_bot.clone(),
            indices,
            started,
            _input: std::marker::PhantomData,
        };
        let handle = std::thread::Builder::new()
            .name(format!("space3d-loadtest-{t}"))
            .spawn(move || run_thread(plan))
            .map_err(|e| crate::ServerError::Other(format!("spawn load thread: {e}")))?;
        handles.push(handle);
    }
    let mut results = Vec::with_capacity(config.clients);
    for h in handles {
        results.extend(
            h.join()
                .map_err(|_| crate::ServerError::Other("load thread panicked".into()))?,
        );
    }
    let after = config.metrics_url.as_deref().and_then(|u| scrape(&http, u));
    let tick_p99_ms = match (&before, &after) {
        (Some(b), Some(a)) => histogram_quantile(b, a, 0.99).map(|s| s * 1000.0),
        _ => None,
    };
    let rep = report(&config, &results, tick_p99_ms);
    tracing::info!(
        connected = rep.connected,
        attempted = rep.attempted,
        slo_passed = rep.slo_passed,
        "load test finished"
    );
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_scrapes_and_quantiles() {
        let before = "# HELP space3d_tick_seconds Room tick duration.\n\
            space3d_tick_seconds_bucket{le=\"0.001\"} 10\n\
            space3d_tick_seconds_bucket{le=\"0.004\"} 10\n\
            space3d_tick_seconds_bucket{le=\"+Inf\"} 10\n\
            space3d_tick_seconds_sum 0.005\n";
        let after = "space3d_tick_seconds_bucket{le=\"0.001\"} 108\n\
            space3d_tick_seconds_bucket{le=\"0.004\"} 110\n\
            space3d_tick_seconds_bucket{le=\"+Inf\"} 110\n\
            other_bucket{le=\"0.001\"} 99\n";
        let b = parse_histogram(before, "space3d_tick_seconds");
        let a = parse_histogram(after, "space3d_tick_seconds");
        assert_eq!(b.len(), 3);
        assert_eq!(a[0], (0.001, 108));
        assert_eq!(histogram_quantile(&b, &a, 0.5), Some(0.001));
        assert_eq!(histogram_quantile(&b, &a, 0.99), Some(0.004));
        assert_eq!(histogram_quantile(&b, &b, 0.99), None);
    }

    #[test]
    fn args_parse_into_targets_and_slos() {
        let c = LoadTestConfig::try_from_args([
            "lt",
            "--ps",
            "http://ps:8080",
            "--queue",
            "ffa",
            "--quick-play",
            "--clients",
            "50",
            "--metrics",
            "http://gs:9090/metrics",
        ])
        .unwrap();
        assert_eq!(
            c.target,
            LoadTarget::PlayerService {
                url: "http://ps:8080".into(),
                queue: "ffa".into(),
                quick_play: true
            }
        );
        assert_eq!(c.clients, 50);
        assert_eq!(c.slo.tick_p99_ms, Some(4.0));
        let d = LoadTestConfig::try_from_args(["lt", "--url", "ws://x/play"]).unwrap();
        assert_eq!(
            d.target,
            LoadTarget::DirectDev {
                url: "ws://x/play".into()
            }
        );
        assert!(LoadTestConfig::try_from_args(["lt", "--clients", "many"]).is_err());
    }

    #[test]
    fn report_applies_the_slos() {
        let config = LoadTestConfig {
            clients: 2,
            tick_hz: 60.0,
            ..Default::default()
        };
        let good = BotResult {
            attempted: true,
            connected: true,
            handshake_ms: Some(20.0),
            rtt_ms: vec![10.0, 12.0],
            connected_secs: 10.0,
            down_bytes: 100_000,
            up_bytes: 50_000,
            snapshots: 300,
            rollbacks: 0,
            disconnects: vec![],
        };
        let rep = report(&config, &[good.clone(), good.clone()], None);
        assert!(rep.slo_passed, "{rep:?}");
        assert_eq!(rep.connected, 2);
        assert!((rep.snapshot_rate - 1.0).abs() < 1e-9);
        assert_eq!(rep.down_bytes_per_sec, (10_000.0, 10_000.0));
        let dropped = BotResult {
            disconnects: vec!["SlowConsumer".into()],
            ..good.clone()
        };
        assert!(!report(&config, &[good.clone(), dropped], None).slo_passed);
        let never = BotResult {
            attempted: true,
            ..Default::default()
        };
        assert!(!report(&config, &[good.clone(), never], None).slo_passed);
        let slow = LoadTestConfig {
            metrics_url: Some("http://x/metrics".into()),
            ..config.clone()
        };
        assert!(!report(&slow, &[good.clone(), good.clone()], Some(9.0)).slo_passed);
        assert!(report(&slow, &[good.clone(), good], Some(2.0)).slo_passed);
    }
}
