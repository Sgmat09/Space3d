//! Player-service role: accounts, sessions, profiles & player data, leaderboards, matchmaking,
//! lobbies, fleet registry, room checkpoints (split mode), bans and audit log.
//!
//! Threading: axum handlers run on the I/O runtime and may `await` `Db` tasks (`Task::wait`)
//! — they are not frame code. Argon2 runs on the compute pool. Matchmaking, lobby starts, fleet
//! lease sweeps and allocation run in the supervisor `App` ([`PlayerServicePlugin`] systems,
//! 10 Hz) against [`PlayerService`] state, so they are deterministic under a manual clock.

pub mod auth;
pub mod fleet;
pub mod lobbies;
pub mod matchmaker;
pub mod results;
pub mod routes;
pub mod schema;
pub mod store;

use crate::clock::UnixClock;
use crate::config::ServerConfig;
use crate::game::RoomSpec;
use crate::gs::fleet::{FleetDown, FleetUp, InProcessFleetPeer};
use crate::lifecycle::{DrainGates, DrainReport};
use crate::metrics::PlayerServiceMetrics;
use crate::security::bans::BanList;
use crate::security::ratelimit::KeyedLimiter;
use crate::security::token::{JoinClaims, KeyRing, sign};
use parking_lot::Mutex;
use space3d_core::{App, Stage};
use space3d_net::{ClientId, Message, ServerEvent, WsServer};
use space3d_replication::api::{Assignment, TicketId};
use space3d_replication::{MatchId, PlayerId, RoomId};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// Per-IP / per-username limiters of the HTTP API.
#[derive(Debug)]
pub struct Limiters {
    /// Requests per IP.
    pub http: KeyedLimiter<IpAddr>,
    /// Password logins / registrations per IP.
    pub login_ip: KeyedLimiter<IpAddr>,
    /// Failed password logins per (username, address prefix): charged only when the password
    /// was wrong, so another address's bad guesses never lock the account's owner out.
    pub login_user_ip: KeyedLimiter<(String, IpAddr)>,
    /// Failed password logins per username from anywhere (backstop against distributed
    /// guessing; 4× the per-address budget, charged only on failures).
    pub login_user: KeyedLimiter<String>,
    /// Guest logins / refreshes per IP (4× the password limit).
    pub guest_ip: KeyedLimiter<IpAddr>,
    /// Quick-play requests per account.
    pub quick_play: KeyedLimiter<PlayerId>,
    /// Rooms quick play may open per account (joining an open room is not limited by it).
    pub room_create: KeyedLimiter<PlayerId>,
}

/// Quick-play requests per account per second (burst [`QUICK_PLAY_BURST`]).
pub const QUICK_PLAY_PER_SEC: f64 = 1.0;
/// See [`QUICK_PLAY_PER_SEC`].
pub const QUICK_PLAY_BURST: f64 = 10.0;
/// New rooms one account may open through quick play per minute (burst
/// [`QUICK_PLAY_ROOMS_BURST`]): drop-in rooms outlive their creator by `empty_room_ttl`.
pub const QUICK_PLAY_ROOMS_PER_MIN: f64 = 6.0;
/// See [`QUICK_PLAY_ROOMS_PER_MIN`].
pub const QUICK_PLAY_ROOMS_BURST: f64 = 3.0;

impl Limiters {
    fn new(config: &ServerConfig) -> Self {
        let p = &config.player_service;
        let per_min = f64::from(p.login_attempts_per_min);
        Self {
            http: KeyedLimiter::new(
                f64::from(p.http_rps_per_ip),
                f64::from(p.http_rps_per_ip) * 2.0,
                100_000,
            ),
            login_ip: KeyedLimiter::new(per_min / 60.0, per_min, 100_000),
            login_user_ip: KeyedLimiter::new(per_min / 60.0, per_min, 100_000),
            login_user: KeyedLimiter::new(per_min * 4.0 / 60.0, per_min * 4.0, 100_000),
            guest_ip: KeyedLimiter::new(per_min * 4.0 / 60.0, per_min * 4.0, 100_000),
            quick_play: KeyedLimiter::new(QUICK_PLAY_PER_SEC, QUICK_PLAY_BURST, 100_000),
            room_create: KeyedLimiter::new(QUICK_PLAY_ROOMS_PER_MIN / 60.0, QUICK_PLAY_ROOMS_BURST, 100_000),
        }
    }
}

/// Shared player-service state (behind `Arc`; used by HTTP handlers and supervisor systems).
pub struct PlayerService {
    /// Config.
    pub config: ServerConfig,
    /// Storage.
    pub store: Arc<dyn store::PlayerStore>,
    /// Token keys.
    pub keys: Arc<KeyRing>,
    /// Wall clock.
    pub clock: Arc<dyn UnixClock>,
    /// Matchmaking queues.
    pub matchmaker: Mutex<matchmaker::Matchmaker>,
    /// Game-server registry.
    pub fleet: Mutex<fleet::FleetRegistry>,
    /// Lobbies.
    pub lobbies: Mutex<lobbies::Lobbies>,
    /// Bans (mirrors the store).
    pub bans: BanList,
    /// Metrics.
    pub metrics: PlayerServiceMetrics,
    /// Rate limiters.
    pub limits: Mutex<Limiters>,
    /// Revoked session ids (refresh families) → access-token expiry horizon (unix ms).
    pub revoked: Mutex<HashMap<[u8; 16], i64>>,
    /// Messages for game servers, flushed by the supervisor (`"*"` = every server).
    pub fleet_out: Mutex<Vec<(String, FleetDown)>>,
    /// Fleet bearer token (game servers; internal endpoints).
    pub fleet_token: String,
    /// Trusted proxies for `X-Forwarded-For`.
    pub trusted_proxies: Vec<crate::security::netaddr::Cidr>,
    /// Database (backups).
    pub db: space3d_db::Db,
}

impl PlayerService {
    /// Open the store (migrations), load bans and revoked sessions; resolves when ready.
    pub fn open(
        config: ServerConfig,
        db: space3d_db::Db,
        keys: Arc<KeyRing>,
        clock: Arc<dyn UnixClock>,
        metrics: PlayerServiceMetrics,
        fleet_token: String,
        bans: BanList,
    ) -> space3d_tasks::Task<Result<Arc<Self>, crate::ServerError>> {
        let migrated = db.migrate(schema::migrations());
        space3d_tasks::spawn(async move {
            migrated
                .wait()
                .await
                .ok_or_else(|| crate::ServerError::Other("db worker gone".into()))??;
            let store: Arc<dyn store::PlayerStore> = Arc::new(store::SqliteStore::new(db.clone()));
            let now = clock.now_ms();
            let stored_bans = store
                .bans()
                .wait()
                .await
                .ok_or_else(|| crate::ServerError::Other("db worker gone".into()))?
                .map_err(|e| crate::ServerError::Other(e.to_string()))?;
            for b in stored_bans {
                bans.upsert(b);
            }
            let session_ms = config.player_service.session_ttl.as_millis() as i64;
            let revoked: HashMap<[u8; 16], i64> = store
                .revoked_families(now - config.player_service.refresh_ttl.as_millis() as i64)
                .wait()
                .await
                .and_then(Result::ok)
                .unwrap_or_default()
                .into_iter()
                .map(|f| (f, now + session_ms))
                .collect();
            let lease = config.fleet.lease_ttl.as_millis() as i64;
            let orphan = config.persistence.recovery_max_age.as_millis() as i64;
            let trusted_proxies = config
                .security
                .trusted_proxies
                .iter()
                .filter_map(|c| crate::security::netaddr::Cidr::parse(c).ok())
                .collect();
            Ok(Arc::new(Self {
                matchmaker: Mutex::new(matchmaker::Matchmaker::new(&config.matchmaking.queues)),
                fleet: Mutex::new(fleet::FleetRegistry::new(lease, orphan)),
                lobbies: Mutex::new(lobbies::Lobbies::default()),
                limits: Mutex::new(Limiters::new(&config)),
                revoked: Mutex::new(revoked),
                fleet_out: Mutex::new(Vec::new()),
                trusted_proxies,
                config,
                store,
                keys,
                clock,
                bans,
                metrics,
                fleet_token,
                db,
            }))
        })
    }

    /// Current wall time (unix ms).
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Monotonic-ish time for rate limiters.
    pub fn limiter_now(&self) -> Duration {
        Duration::from_millis(self.now_ms().max(0) as u64)
    }

    /// Real client IP of a request.
    pub fn client_ip(&self, peer: SocketAddr, forwarded_for: Option<&str>) -> IpAddr {
        crate::security::netaddr::resolve_client_ip(peer, forwarded_for, &self.trusted_proxies)
    }

    /// Revoke a session id (access tokens die within the cache horizon).
    pub fn revoke_sid(&self, sid: [u8; 16]) {
        let until = self.now_ms() + self.config.player_service.session_ttl.as_millis() as i64;
        let mut r = self.revoked.lock();
        let now = self.now_ms();
        r.retain(|_, u| *u > now);
        r.insert(sid, until);
    }

    /// Whether a session id was revoked.
    pub fn is_revoked(&self, sid: &[u8; 16]) -> bool {
        self.revoked.lock().get(sid).is_some_and(|u| *u > self.now_ms())
    }

    /// Queue a message for a game server (`"*"` = all).
    pub fn send_fleet(&self, server: &str, msg: FleetDown) {
        self.fleet_out.lock().push((server.to_owned(), msg));
    }

    /// Mint a join ticket for `player` into `spec`'s room on `server` (TTL from config, single use).
    pub fn issue_ticket(
        &self,
        player: PlayerId,
        display_name: &str,
        server: &fleet::ServerRecord,
        spec: &RoomSpec,
        team: Option<u16>,
        extra: serde_json::Value,
    ) -> Assignment {
        let now = self.now_ms();
        let exp = now + self.config.player_service.join_ticket_ttl.as_millis() as i64;
        let claims = JoinClaims {
            player,
            display_name: display_name.to_owned(),
            team,
            server_id: server.registration.server_id.clone(),
            room: spec.room_id,
            spec: spec.clone(),
            jti: crate::security::random_bytes::<16>(),
            iat_ms: now,
            exp_ms: exp,
            extra,
        };
        Assignment {
            server_url: server.registration.public_url.clone(),
            server_id: server.registration.server_id.clone(),
            room_id: spec.room_id,
            join_ticket: sign(&self.keys, &claims),
            expires_ms: exp,
        }
    }

    fn ticket_reserve_ms(&self) -> i64 {
        self.config.player_service.join_ticket_ttl.as_millis() as i64
    }

    /// Default capacity of a drop-in room of `mode` (from a queue with that mode, else 16).
    pub fn mode_capacity(&self, mode: &str) -> u16 {
        self.config
            .matchmaking
            .queues
            .iter()
            .find(|q| q.mode == mode)
            .map_or(16, |q| q.teams.saturating_mul(q.team_size).max(1))
    }

    /// Allocate a fresh room of `mode` for `seats` players (drop-in / lobbies / matches).
    #[allow(clippy::too_many_arguments)]
    pub fn allocate_room(
        &self,
        region: &str,
        mode: &str,
        max_players: u16,
        seats: u32,
        teams: Vec<Vec<PlayerId>>,
        match_id: Option<MatchId>,
        params: BTreeMap<String, String>,
    ) -> Option<(fleet::ServerRecord, RoomSpec)> {
        self.allocate_room_for(
            region,
            mode,
            max_players,
            seats,
            teams,
            match_id,
            params,
            &fleet::Build::default(),
        )
    }

    /// [`Self::allocate_room`] on a game server that can host `build`.
    #[allow(clippy::too_many_arguments)]
    pub fn allocate_room_for(
        &self,
        region: &str,
        mode: &str,
        max_players: u16,
        seats: u32,
        teams: Vec<Vec<PlayerId>>,
        match_id: Option<MatchId>,
        params: BTreeMap<String, String>,
        build: &fleet::Build,
    ) -> Option<(fleet::ServerRecord, RoomSpec)> {
        let allocated = {
            let mut fleet = self.fleet.lock();
            self.allocate_room_in(
                &mut fleet,
                region,
                mode,
                max_players,
                seats,
                teams,
                match_id,
                params,
                build,
            )
        };
        let (server, spec) = allocated?;
        self.prepare_room(&server, &spec);
        Some((server, spec))
    }

    /// Quick play for `player`: the fullest open room of `mode` with a free seat, else a fresh
    /// one (capacity and params from the queue with that mode) on a server that hosts the mode.
    /// Finding and allocating happen under one fleet lock, so concurrent quick plays fill one
    /// new room instead of each opening their own. The seat is reserved for `player`, and a
    /// player holds at most one such reservation (a new quick play releases the previous one);
    /// opening a fresh room also takes a token of the per-account room budget.
    pub fn quick_play_room(
        &self,
        region: &str,
        mode: &str,
        build: &fleet::Build,
        player: PlayerId,
    ) -> Result<(fleet::ServerRecord, RoomSpec), QuickPlayRefusal> {
        let now = self.now_ms();
        let reserve = self.ticket_reserve_ms();
        let (found, fresh) = {
            let mut fleet = self.fleet.lock();
            let (found, fresh) = match fleet.open_room_for(mode, build, 1, now, reserve) {
                Some(open) => (open, false),
                None => {
                    let queue = self.config.matchmaking.queues.iter().find(|q| q.mode == mode);
                    if queue.is_none() && fleet.mode_unknown(build, mode) {
                        return Err(QuickPlayRefusal::UnknownMode);
                    }
                    if !self.limits.lock().room_create.check(&player, self.limiter_now()) {
                        return Err(QuickPlayRefusal::RoomRate);
                    }
                    let params = queue.map(|q| q.params.clone()).unwrap_or_default();
                    let capacity = self.mode_capacity(mode);
                    let room = self
                        .allocate_room_in(&mut fleet, region, mode, capacity, 1, Vec::new(), None, params, build)
                        .ok_or(QuickPlayRefusal::NoCapacity)?;
                    (room, true)
                }
            };
            fleet.hold_last_reservation(player);
            (found, fresh)
        };
        if fresh {
            self.prepare_room(&found.0, &found.1);
        }
        Ok(found)
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_room_in(
        &self,
        fleet: &mut fleet::FleetRegistry,
        region: &str,
        mode: &str,
        max_players: u16,
        seats: u32,
        teams: Vec<Vec<PlayerId>>,
        match_id: Option<MatchId>,
        params: BTreeMap<String, String>,
        build: &fleet::Build,
    ) -> Option<(fleet::ServerRecord, RoomSpec)> {
        let now = self.now_ms();
        // A queue may pin the game (`params.game_id`); the client's build narrows it further.
        let mut build = build.clone();
        if build.game_id.is_empty() {
            build.game_id = params.get("game_id").cloned().unwrap_or_default();
        }
        let server = fleet.allocate_mode(region, &build, mode, seats, now, self.ticket_reserve_ms())?;
        let spec = RoomSpec {
            room_id: random_room_id(),
            mode: mode.to_owned(),
            map: params.get("map").cloned().unwrap_or_default(),
            seed: u64::from_le_bytes(crate::security::random_bytes::<8>()),
            max_players: max_players.max(seats.min(u32::from(u16::MAX)) as u16),
            match_id,
            teams,
            persistent: params.get("persistent").is_some_and(|v| v == "true"),
            params,
        };
        fleet.note_room(&spec, &server.registration.server_id, now);
        Some((server, spec))
    }

    /// Pre-warm a freshly allocated room on its game server (the ticket alone also creates it).
    fn prepare_room(&self, server: &fleet::ServerRecord, spec: &RoomSpec) {
        self.send_fleet(
            &server.registration.server_id,
            FleetDown::PrepareRoom { spec: spec.clone() },
        );
    }
}

/// Why quick play found no room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickPlayRefusal {
    /// No compatible server with free seats / room slots.
    NoCapacity,
    /// No configured queue and no server hosts the mode.
    UnknownMode,
    /// The account opened too many rooms recently.
    RoomRate,
}

/// A random non-zero room id.
pub fn random_room_id() -> RoomId {
    loop {
        let id = u64::from_le_bytes(crate::security::random_bytes::<8>());
        if id != 0 {
            return RoomId(id);
        }
    }
}

/// Where a fleet connection lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Conn {
    Ws(ClientId),
    Local(usize),
}

/// Supervisor-side fleet endpoint state (resource).
pub struct FleetHub {
    ws: Option<WsServer>,
    local: Vec<InProcessFleetPeer>,
    /// Connection → registered server id.
    conns: BTreeMap<Conn, Option<String>>,
    /// WS connections not registered yet → when they connected (unix ms).
    unregistered_since: BTreeMap<ClientId, i64>,
}

impl FleetHub {
    /// Hub over the internal WS endpoint and in-process peers.
    pub fn new(ws: Option<WsServer>, local: Vec<InProcessFleetPeer>) -> Self {
        let conns = (0..local.len()).map(|i| (Conn::Local(i), None)).collect();
        Self {
            ws,
            local,
            conns,
            unregistered_since: BTreeMap::new(),
        }
    }

    /// Close WS connections that did not register within
    /// [`crate::gs::fleet::FLEET_REGISTER_DEADLINE`]; returns how many.
    pub fn expire_unregistered(&mut self, now_ms: i64) -> usize {
        let deadline = crate::gs::fleet::FLEET_REGISTER_DEADLINE.as_millis() as i64;
        let expired: Vec<ClientId> = self
            .unregistered_since
            .iter()
            .filter(|(_, since)| now_ms.saturating_sub(**since) >= deadline)
            .map(|(c, _)| *c)
            .collect();
        for client in &expired {
            self.unregistered_since.remove(client);
            if let Some(ws) = &self.ws {
                ws.disconnect_with(*client, 1008, "fleet registration timeout");
            }
            tracing::warn!(event = "fleet_register_timeout", client = client.0, "fleet link closed");
        }
        expired.len()
    }

    /// Fleet connections that have not registered yet.
    pub fn unregistered(&self) -> usize {
        self.unregistered_since.len()
    }

    /// Attach another in-process game server (tests and embedders running several game
    /// servers next to one player service).
    pub fn add_local(&mut self, peer: InProcessFleetPeer) {
        self.conns.insert(Conn::Local(self.local.len()), None);
        self.local.push(peer);
    }

    fn send(&self, server: &str, msg: &FleetDown) {
        for (conn, id) in &self.conns {
            if server != "*" && id.as_deref() != Some(server) {
                continue;
            }
            if id.is_none() {
                continue;
            }
            match conn {
                Conn::Ws(client) => {
                    if let (Some(ws), Ok(text)) = (&self.ws, serde_json::to_string(msg)) {
                        let _ = ws.send(*client, Message::Text(text));
                    }
                }
                Conn::Local(i) => {
                    let _ = self.local[*i].down.try_send(msg.clone());
                }
            }
        }
    }

    /// Inbound messages: `(connection, message)`; handles connects/disconnects.
    fn poll(&mut self, now_ms: i64) -> Vec<(Conn, FleetUp)> {
        let mut out = Vec::new();
        if let Some(ws) = &self.ws {
            for ev in ws.drain() {
                match ev {
                    ServerEvent::Connected { client, .. } => {
                        self.conns.insert(Conn::Ws(client), None);
                        self.unregistered_since.insert(client, now_ms);
                    }
                    ServerEvent::Disconnected { client, reason } => {
                        self.unregistered_since.remove(&client);
                        if let Some(Some(id)) = self.conns.remove(&Conn::Ws(client)) {
                            tracing::warn!(server = %id, %reason, "fleet link closed");
                        }
                    }
                    ServerEvent::Message { client, message } => {
                        let parsed = message.as_text().map(serde_json::from_str::<FleetUp>);
                        match parsed {
                            Some(Ok(msg)) => out.push((Conn::Ws(client), msg)),
                            _ => {
                                tracing::warn!(event = "bad_fleet_message", "closing fleet link");
                                ws.disconnect_with(client, 1008, "bad fleet message");
                            }
                        }
                    }
                }
            }
        }
        for (i, peer) in self.local.iter().enumerate() {
            while let Ok(msg) = peer.up.try_recv() {
                out.push((Conn::Local(i), msg));
            }
        }
        out
    }

    fn reject(&mut self, conn: Conn, why: &str) {
        if let (Conn::Ws(client), Some(ws)) = (conn, &self.ws) {
            ws.disconnect_with(client, 1008, why);
        }
        self.conns.insert(conn, None);
    }
}

/// Supervisor plugin for the player-service role: runs fleet I/O, matchmaking, lobby
/// allocation and lease sweeps (the HTTP API is bound by `ServerMain`).
pub struct PlayerServicePlugin {
    /// Service state.
    pub service: Arc<PlayerService>,
    /// In-process fleet peers (all-in-one mode).
    pub local_fleet: Vec<InProcessFleetPeer>,
    /// Internal fleet WebSocket endpoint (split mode).
    pub fleet_ws: Option<WsServer>,
}

impl space3d_core::Plugin for PlayerServicePlugin {
    fn build(&self, app: &mut App) {
        let svc = self.service.clone();
        app.insert_resource(FleetHub::new(self.fleet_ws.clone(), self.local_fleet.clone()));
        let mut last_mm = i64::MIN;
        let mut last_sessions = i64::MIN;
        let mut sessions: Option<space3d_tasks::Task<Result<u64, store::StoreError>>> = None;
        app.add_system_named(Stage::Update, "ps::supervisor", move |ctx| {
            let now = svc.now_ms();
            ctx.resources.scope::<FleetHub, _>(|_, hub| fleet_io(&svc, hub, now));
            let interval = svc.config.matchmaking.interval.as_millis() as i64;
            if now.saturating_sub(last_mm) >= interval {
                last_mm = now;
                run_matchmaking(&svc, now);
                run_lobbies(&svc, now);
                publish_metrics(&svc);
            }
            // Active sessions: a COUNT on the store every 10 s (polled, never awaited here).
            if let Some(task) = &mut sessions {
                match task.state() {
                    space3d_tasks::TaskState::Pending => {}
                    space3d_tasks::TaskState::Ready => {
                        if let Some(Ok(n)) = task.poll() {
                            svc.metrics.set_sessions_active(n);
                        }
                        sessions = None;
                    }
                    space3d_tasks::TaskState::Cancelled => sessions = None,
                }
            }
            if sessions.is_none() && now.saturating_sub(last_sessions) >= SESSIONS_METRIC_EVERY_MS {
                last_sessions = now;
                sessions = Some(svc.store.active_sessions(now));
            }
            let out = std::mem::take(&mut *svc.fleet_out.lock());
            let hub = ctx.resources.expect::<FleetHub>();
            for (server, msg) in out {
                hub.send(&server, &msg);
            }
            if let Some(gates) = ctx.resources.get_mut::<DrainGates>() {
                gates.set(
                    "player-service",
                    DrainReport {
                        idle: true,
                        flushed: true,
                    },
                );
            }
        });
    }
}

fn fleet_io(svc: &Arc<PlayerService>, hub: &mut FleetHub, now: i64) {
    hub.expire_unregistered(now);
    for (conn, msg) in hub.poll(now) {
        let registered = hub.conns.get(&conn).cloned().flatten();
        let server = match (&registered, &msg) {
            (_, FleetUp::Register(reg)) => {
                if !crate::security::ct_eq(reg.token.as_bytes(), svc.fleet_token.as_bytes()) {
                    tracing::warn!(event = "bad_fleet_token", server = %reg.server_id, "fleet registration refused");
                    hub.reject(conn, "bad fleet token");
                    continue;
                }
                hub.conns.insert(conn, Some(reg.server_id.clone()));
                if let Conn::Ws(client) = conn {
                    hub.unregistered_since.remove(&client);
                }
                tracing::info!(server = %reg.server_id, region = %reg.region, url = %reg.public_url, "game server registered");
                reg.server_id.clone()
            }
            (Some(id), _) => id.clone(),
            (None, _) => {
                hub.reject(conn, "register first");
                continue;
            }
        };
        let effects = svc.fleet.lock().on_message(&server, msg, now);
        apply_effects(svc, hub, effects);
    }
    let effects = svc
        .fleet
        .lock()
        .sweep(now, svc.config.fleet.lease_ttl.as_millis() as i64);
    apply_effects(svc, hub, effects);
}

fn apply_effects(svc: &Arc<PlayerService>, hub: &FleetHub, effects: Vec<fleet::FleetEffect>) {
    for e in effects {
        match e {
            fleet::FleetEffect::Send { server, mut msg } => {
                if let FleetDown::Registered { bans, .. } = &mut msg {
                    *bans = svc.bans.snapshot();
                }
                hub.send(&server, &msg);
            }
            fleet::FleetEffect::Apply { server, op } => apply_op(svc.clone(), server, op),
            fleet::FleetEffect::Orphaned { rooms } => {
                tracing::warn!(rooms = rooms.len(), "rooms orphaned; waiting for rejoin");
            }
        }
    }
}

/// Apply one outbox op / fetch asynchronously; the answer is queued in `fleet_out`.
fn apply_op(svc: Arc<PlayerService>, server: String, op: FleetUp) {
    let now = svc.now_ms();
    space3d_tasks::runtime().spawn(async move {
        let answer = match op {
            FleetUp::MatchResult { op_id, result } => {
                let queue_hint = result.mode.clone();
                let r = svc.store.apply_match(op_id.clone(), result, now).wait().await;
                ack(r.map(|r| r.map(|_| ())), op_id, &queue_hint)
            }
            FleetUp::PlayerDataWrite { op_id, player, writes } => {
                let r = svc.store.apply_writes(op_id.clone(), player, writes, now).wait().await;
                ack(r.map(|r| r.map(|_| ())), op_id, "data")
            }
            FleetUp::SubmitScore {
                op_id,
                board,
                player,
                score,
            } => {
                let r = svc
                    .store
                    .submit_score(op_id.clone(), board, player, score, now)
                    .wait()
                    .await;
                ack(r, op_id, "score")
            }
            FleetUp::FetchPlayerData {
                request_id,
                player,
                keys,
            } => {
                let entries = svc
                    .store
                    .get_data(player, keys)
                    .wait()
                    .await
                    .and_then(Result::ok)
                    .unwrap_or_default();
                Some(FleetDown::PlayerData {
                    request_id,
                    player,
                    entries,
                })
            }
            _ => None,
        };
        if let Some(msg) = answer {
            svc.send_fleet(&server, msg);
        }
    });
}

fn ack(result: Option<Result<(), store::StoreError>>, op_id: String, what: &str) -> Option<FleetDown> {
    match result {
        Some(Ok(())) => Some(FleetDown::Ack { op_id }),
        // Permanent errors (bad version, unknown player) are acked so the op does not loop
        // forever; transient DB failures are retried by the game server.
        Some(Err(store::StoreError::Db(e))) => {
            tracing::error!(%op_id, error = %e, kind = what, "applying game-server op failed; will retry");
            None
        }
        Some(Err(e)) => {
            tracing::warn!(%op_id, error = %e, kind = what, "game-server op rejected");
            Some(FleetDown::Ack { op_id })
        }
        None => None,
    }
}

fn run_matchmaking(svc: &Arc<PlayerService>, now: i64) {
    let queues = svc.matchmaker.lock().queue_names();
    // Backfill queues fill open rooms first.
    for q in &queues {
        let Some(cfg) = svc.matchmaker.lock().queue_config(q).cloned() else {
            continue;
        };
        if !cfg.backfill {
            continue;
        }
        for entry in svc.matchmaker.lock().searching(q) {
            let room = svc
                .fleet
                .lock()
                .open_room_for(&cfg.mode, &entry.build, 1, now, svc.ticket_reserve_ms());
            let Some((server, spec)) = room else { break };
            let a = svc.issue_ticket(
                entry.player,
                &entry.display_name,
                &server,
                &spec,
                None,
                serde_json::json!({ "rating": entry.rating }),
            );
            svc.matchmaker.lock().assign_direct(&entry.ticket, a, now);
            svc.metrics
                .mm_wait(q, now.saturating_sub(entry.enqueued_ms) as f64 / 1000.0);
        }
    }
    let formed = svc.matchmaker.lock().tick(now);
    for m in formed {
        let seats = m.players().count() as u32;
        let cfg = svc
            .matchmaker
            .lock()
            .queue_config(&m.queue)
            .cloned()
            .unwrap_or_default();
        let capacity = cfg.teams.saturating_mul(cfg.team_size).max(seats as u16);
        let teams: Vec<Vec<PlayerId>> = m.teams.iter().map(|t| t.iter().map(|e| e.player).collect()).collect();
        let match_id = MatchId(u64::from_le_bytes(crate::security::random_bytes::<8>()));
        // Matched players share one build (the matchmaker groups by it).
        let build = m.players().next().map(|(_, e)| e.build.clone()).unwrap_or_default();
        let Some((server, spec)) = svc.allocate_room_for(
            &m.region,
            &m.mode,
            capacity,
            seats,
            teams,
            Some(match_id),
            cfg.params.clone(),
            &build,
        ) else {
            tracing::warn!(queue = %m.queue, "no game server capacity; requeueing");
            svc.matchmaker.lock().requeue(m);
            continue;
        };
        svc.metrics.match_formed(&m.queue);
        for (team, e) in m.players() {
            let a = svc.issue_ticket(
                e.player,
                &e.display_name,
                &server,
                &spec,
                Some(team),
                serde_json::json!({ "rating": e.rating }),
            );
            svc.metrics
                .mm_wait(&m.queue, now.saturating_sub(e.enqueued_ms) as f64 / 1000.0);
            svc.matchmaker.lock().resolve(&e.ticket, a);
        }
    }
}

fn run_lobbies(svc: &Arc<PlayerService>, now: i64) {
    let starting = svc.lobbies.lock().starting();
    for l in starting {
        let seats = l.members.len() as u32;
        let members: Vec<PlayerId> = l.members.iter().map(|m| m.player_id).collect();
        match svc.allocate_room_for(
            "",
            &l.mode,
            l.max_players,
            seats,
            vec![members],
            None,
            BTreeMap::new(),
            &l.build,
        ) {
            Some((server, spec)) => {
                let assignments = l
                    .members
                    .iter()
                    .map(|m| {
                        (
                            m.player_id,
                            svc.issue_ticket(
                                m.player_id,
                                &m.display_name,
                                &server,
                                &spec,
                                Some(0),
                                serde_json::Value::Null,
                            ),
                        )
                    })
                    .collect();
                svc.lobbies.lock().set_assignments(&l.lobby_id, assignments, now);
            }
            None => svc.lobbies.lock().fail_start(&l.lobby_id, now),
        }
    }
    svc.lobbies.lock().prune(now);
}

/// Period of the `space3d_sessions_active` refresh.
const SESSIONS_METRIC_EVERY_MS: i64 = 10_000;

fn publish_metrics(svc: &Arc<PlayerService>) {
    svc.metrics.set_db_queue_depth(svc.db.queue_depth());
    for ((queue, region), n) in svc.matchmaker.lock().queue_sizes() {
        svc.metrics.queue_size(&queue, &region, n);
    }
    let (states, free) = svc.fleet.lock().census();
    for ((region, state), n) in states {
        svc.metrics.fleet_servers(&region, state, n);
    }
    for (region, seats) in free {
        svc.metrics.free_seats(&region, seats);
    }
}

/// Resource: a running player service.
#[derive(Clone)]
pub struct PlayerServiceHandle {
    /// Bound HTTP address.
    pub addr: SocketAddr,
    /// State.
    pub service: Arc<PlayerService>,
}

impl PlayerServiceHandle {
    /// `http://host:port` reachable from this machine.
    pub fn url(&self) -> String {
        let mut a = self.addr;
        if a.ip().is_unspecified() {
            a.set_ip(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        }
        format!("http://{a}")
    }
}

/// Ticket id for matchmaking.
pub fn new_ticket_id() -> TicketId {
    TicketId(format!(
        "mm_{:032x}",
        u128::from_le_bytes(crate::security::random_bytes::<16>())
    ))
}
