//! Sans-IO admission and routing of links (driven by the ingress task or `TestCluster`;
//! unit-testable with a manual clock and a fake `LinkTx`).
//!
//! Per link: `Opened` → IP bans, `Origin` allowlist, per-IP concurrency + handshake rate
//! (`ConnectionLimiter`), IP strike lockout → pending (handshake deadline). The first packet
//! must be a `Hello` (≤ 4 KiB): structure (`parse_hello`), game id, draining, then by
//! `HelloAuth`:
//! * `Ticket` → `verify::<JoinClaims>` (kind, kid, MAC, exp/iat ± leeway), `server_id == self`,
//!   `room == spec.room_id`, player not banned, `GameDefinition::validate_spec`, `jti` unused
//!   (`NonceCache`) → [`GateOutcome::Admit`] (the room is created lazily from the spec; while
//!   draining only rooms that already run here are joined, creating one is refused).
//! * `Resume` → route by the token's room (the room verifies the secret and the ban list) →
//!   `Admit` with `identity: None` (resumes are accepted while draining).
//! * `Dev` → only if `allow_dev_auth` (loopback-bound, enforced by config validation); joins
//!   `GameDefinition::dev_room_spec()`.
//!
//! Anything but a Hello first, or no Hello within the deadline → close + IP strike; five
//! strikes within a minute lock the IP out for the rest of that minute. After admission every
//! packet/close of the link is forwarded to its room, within a per-link packet budget (excess
//! dropped here; sustained flooding closes the link) so no client can fill the room's shared
//! inbox. IP/CIDR bans also close links already open from the banned range.

use crate::game::{GameDefinition, RoomSpec};
use crate::security::bans::BanList;
use crate::security::netaddr::{Cidr, rate_key};
use crate::security::ratelimit::{ConnRefusal, ConnectionLimiter, TokenBucket};
use crate::security::token::{JoinClaims, KeyRing, NonceCache, NonceCheck, TokenError, verify};
use space3d_replication::protocol::handshake::{Disconnect, DisconnectCode, HelloAuth, RejectCode};
use space3d_replication::protocol::{PacketKind, encode_handshake, peek};
use space3d_replication::server::handshake::{parse_hello, resume_route};
use space3d_replication::server::receive::OverflowWindows;
use space3d_replication::server::{JoinRequest, PlayerIdentity, RoomInbound};
use space3d_replication::transport::{CloseCode, LinkEvent, LinkTx, PeerInfo};
use space3d_replication::{FastMap, LinkId, RoomId};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

/// Gatekeeper settings.
#[derive(Clone)]
pub struct GatekeeperConfig {
    /// This server's id (tickets must name it).
    pub server_id: String,
    /// Accepted `Hello::game_id`.
    pub game_id: String,
    /// Hello deadline.
    pub handshake_timeout: Duration,
    /// Accept `HelloAuth::Dev`.
    pub allow_dev_auth: bool,
    /// Per-IP concurrent connections.
    pub max_connections_per_ip: u32,
    /// Handshakes per IP per minute.
    pub handshakes_per_ip_per_min: u32,
    /// Trusted proxies for `X-Forwarded-For` (already applied by the transport; kept for
    /// transports that report raw peers).
    pub trusted_proxies: Vec<Cidr>,
    /// Browser origins (empty = any).
    pub allowed_origins: Vec<String>,
    /// Token clock leeway.
    pub leeway: Duration,
    /// The game (spec validation, dev room). `None`: every spec is accepted and dev joins go
    /// to room 1.
    pub game: Option<Arc<dyn GameDefinition>>,
    /// Strikes per IP within a minute before its new connections are refused.
    pub max_ip_strikes: u32,
    /// Packets per second one admitted link may push into its room's shared inbox (4× the
    /// room's own per-link allowance). Excess is dropped here; sustained or extreme excess
    /// closes the link (`Disconnect(RateLimited)` + IP strike), so one client can never fill
    /// the inbox that every player of the room shares.
    pub link_packets_per_sec: u32,
}

impl std::fmt::Debug for GatekeeperConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatekeeperConfig")
            .field("server_id", &self.server_id)
            .field("game_id", &self.game_id)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("allow_dev_auth", &self.allow_dev_auth)
            .field("max_connections_per_ip", &self.max_connections_per_ip)
            .field("handshakes_per_ip_per_min", &self.handshakes_per_ip_per_min)
            .field("link_packets_per_sec", &self.link_packets_per_sec)
            .field("allowed_origins", &self.allowed_origins)
            .finish_non_exhaustive()
    }
}

impl Default for GatekeeperConfig {
    fn default() -> Self {
        Self {
            server_id: "local-1".into(),
            game_id: String::new(),
            handshake_timeout: Duration::from_secs(5),
            allow_dev_auth: false,
            max_connections_per_ip: 8,
            handshakes_per_ip_per_min: 20,
            trusted_proxies: Vec::new(),
            allowed_origins: Vec::new(),
            leeway: Duration::from_secs(5),
            game: None,
            max_ip_strikes: 5,
            link_packets_per_sec: 360,
        }
    }
}

impl GatekeeperConfig {
    /// Settings from the server config.
    pub fn from_config(config: &crate::ServerConfig, game: Arc<dyn GameDefinition>) -> Self {
        Self {
            server_id: config.server_id.clone(),
            game_id: game.game_id().to_owned(),
            handshake_timeout: config.limits.handshake_timeout,
            allow_dev_auth: config.game.allow_dev_auth,
            max_connections_per_ip: config.limits.max_connections_per_ip,
            handshakes_per_ip_per_min: config.limits.handshakes_per_ip_per_min,
            trusted_proxies: config
                .security
                .trusted_proxies
                .iter()
                .filter_map(|c| Cidr::parse(c).ok())
                .collect(),
            allowed_origins: config.game.allowed_origins.clone(),
            leeway: Duration::from_secs(5),
            game: Some(game),
            max_ip_strikes: 5,
            link_packets_per_sec: config.limits.input_packets_per_sec.saturating_mul(4).max(240),
        }
    }
}

/// What the ingress task must do with an event.
pub enum GateOutcome {
    /// Nothing (buffered, or dropped).
    None,
    /// Route a join to `room`, creating it from `spec` if it does not exist.
    Admit {
        /// Target room.
        room: RoomId,
        /// Spec from the ticket / dev room (`None` for resumes: the room must exist).
        spec: Option<RoomSpec>,
        /// The `RoomInbound::Join`.
        inbound: RoomInbound,
    },
    /// Forward a packet/close of an admitted link.
    Forward {
        /// Room.
        room: RoomId,
        /// Message.
        inbound: RoomInbound,
    },
    /// Send `Reject` and close.
    Refuse {
        /// Link sender.
        tx: Arc<dyn LinkTx>,
        /// Code.
        code: RejectCode,
        /// Detail.
        message: String,
    },
}

impl std::fmt::Debug for GateOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateOutcome::None => f.write_str("None"),
            GateOutcome::Admit { room, spec, inbound } => f
                .debug_struct("Admit")
                .field("room", room)
                .field("spec", spec)
                .field("inbound", inbound)
                .finish(),
            GateOutcome::Forward { room, inbound } => f
                .debug_struct("Forward")
                .field("room", room)
                .field("inbound", inbound)
                .finish(),
            GateOutcome::Refuse { code, message, .. } => f
                .debug_struct("Refuse")
                .field("code", code)
                .field("message", message)
                .finish_non_exhaustive(),
        }
    }
}

/// Stable metric label of a reject code.
pub fn reject_label(code: RejectCode) -> &'static str {
    match code {
        RejectCode::VersionMismatch => "version_mismatch",
        RejectCode::SchemaMismatch => "schema_mismatch",
        RejectCode::WrongGame => "wrong_game",
        RejectCode::BadTicket => "bad_ticket",
        RejectCode::TicketExpired => "ticket_expired",
        RejectCode::TicketReused => "ticket_reused",
        RejectCode::WrongServer => "wrong_server",
        RejectCode::Banned => "banned",
        RejectCode::RoomFull => "room_full",
        RejectCode::RoomNotFound => "room_not_found",
        RejectCode::RoomClosed => "room_closed",
        RejectCode::ResumeExpired => "resume_expired",
        RejectCode::ServerDraining => "server_draining",
        RejectCode::RateLimited => "rate_limited",
        RejectCode::DevAuthDisabled => "dev_auth_disabled",
        RejectCode::Malformed => "malformed",
        RejectCode::Internal => "internal",
    }
}

/// The gatekeeper state machine.
pub struct Gatekeeper {
    config: GatekeeperConfig,
    keys: Arc<KeyRing>,
    nonces: NonceCache,
    bans: BanList,
    limiter: ConnectionLimiter,
    draining: bool,
    pending: FastMap<LinkId, Pending>,
    routed: FastMap<LinkId, Routed>,
    /// Links counted by the limiter → their IP (released on `Closed`).
    counted: FastMap<LinkId, IpAddr>,
    strikes: FastMap<IpAddr, (u32, Duration)>,
    /// `BanList::generation` last enforced on connected links.
    bans_seen: u64,
    /// Packets dropped by the per-link budget (metric source).
    dropped_packets: u64,
}

struct Pending {
    tx: Arc<dyn LinkTx>,
    peer: PeerInfo,
    opened_at: Duration,
}

struct Routed {
    room: RoomId,
    tx: Arc<dyn LinkTx>,
    ip: IpAddr,
    /// Per-link packet budget into the room inbox.
    budget: TokenBucket,
    overflow: OverflowWindows,
    /// Closed for flooding; later packets are ignored until `Closed` arrives.
    cut: bool,
}

const STRIKE_WINDOW: Duration = Duration::from_secs(60);

fn peer_ip(peer: &PeerInfo) -> IpAddr {
    peer.client_ip
        .or_else(|| peer.addr.map(|a| a.ip()))
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

impl Gatekeeper {
    /// New gatekeeper.
    pub fn new(config: GatekeeperConfig, keys: Arc<KeyRing>, bans: BanList) -> Self {
        let limiter = ConnectionLimiter::new(config.max_connections_per_ip, config.handshakes_per_ip_per_min);
        Self {
            config,
            keys,
            nonces: NonceCache::new(1 << 20),
            bans,
            limiter,
            draining: false,
            pending: Default::default(),
            routed: Default::default(),
            counted: Default::default(),
            strikes: Default::default(),
            bans_seen: 0,
            dropped_packets: 0,
        }
    }

    /// Packets dropped so far by the per-link budget ([`GatekeeperConfig::link_packets_per_sec`]).
    pub fn dropped_packets(&self) -> u64 {
        self.dropped_packets
    }

    /// Settings.
    pub fn config(&self) -> &GatekeeperConfig {
        &self.config
    }

    /// Links waiting for their Hello.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Admitted links.
    pub fn routed_len(&self) -> usize {
        self.routed.len()
    }

    /// Room of an admitted link.
    pub fn room_of(&self, link: LinkId) -> Option<RoomId> {
        self.routed.get(&link).map(|r| r.room)
    }

    /// Sender of an admitted link.
    pub fn link_tx(&self, link: LinkId) -> Option<Arc<dyn LinkTx>> {
        self.routed.get(&link).map(|r| r.tx.clone())
    }

    fn strike(&mut self, ip: IpAddr, now: Duration) {
        let ip = rate_key(ip);
        if self.strikes.len() > 100_000 {
            self.strikes
                .retain(|_, (_, since)| now.saturating_sub(*since) < STRIKE_WINDOW);
        }
        let entry = self.strikes.entry(ip).or_insert((0, now));
        if now.saturating_sub(entry.1) >= STRIKE_WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
        tracing::warn!(event = "strike", %ip, strikes = entry.0, "gateway protocol violation");
    }

    fn locked_out(&self, ip: IpAddr, now: Duration) -> bool {
        self.strikes
            .get(&rate_key(ip))
            .is_some_and(|(n, since)| *n >= self.config.max_ip_strikes && now.saturating_sub(*since) < STRIKE_WINDOW)
    }

    /// Handle one link event at monotonic `now` / wall `now_ms`.
    pub fn handle(&mut self, event: LinkEvent, now: Duration, now_ms: i64) -> GateOutcome {
        match event {
            LinkEvent::Opened { link, peer, tx } => self.on_open(link, peer, tx, now, now_ms),
            LinkEvent::Packet { link, data, .. } => {
                if let Some(r) = self.routed.get_mut(&link) {
                    if r.cut {
                        return GateOutcome::None;
                    }
                    if r.budget.take(now, 1.0) {
                        return GateOutcome::Forward {
                            room: r.room,
                            inbound: RoomInbound::Packet { link, bytes: data },
                        };
                    }
                    self.dropped_packets += 1;
                    if r.overflow.record_drop(now) {
                        r.cut = true;
                        let pkt = encode_handshake(
                            PacketKind::Disconnect,
                            &Disconnect::new(DisconnectCode::RateLimited, "rate limited", false),
                        );
                        let _ = r.tx.send(space3d_replication::transport::Lane::Reliable, pkt);
                        r.tx.close(CloseCode::POLICY, "rate limited");
                        let ip = r.ip;
                        tracing::warn!(event = "link_flood", %ip, ?link, "link closed for flooding its room");
                        self.strike(ip, now);
                    }
                    return GateOutcome::None;
                }
                match self.pending.remove(&link) {
                    Some(p) => self.on_hello(link, p, &data, now, now_ms),
                    // Refused / timed-out link still draining its socket.
                    None => GateOutcome::None,
                }
            }
            LinkEvent::Closed { link, reason } => {
                if let Some(ip) = self.counted.remove(&link) {
                    self.limiter.on_close(ip);
                }
                self.pending.remove(&link);
                match self.routed.remove(&link) {
                    Some(r) => GateOutcome::Forward {
                        room: r.room,
                        inbound: RoomInbound::Closed { link, reason },
                    },
                    None => GateOutcome::None,
                }
            }
        }
    }

    fn on_open(
        &mut self,
        link: LinkId,
        peer: PeerInfo,
        tx: Arc<dyn LinkTx>,
        now: Duration,
        now_ms: i64,
    ) -> GateOutcome {
        let ip = peer_ip(&peer);
        if let Some(ban) = self.bans.ip(ip, now_ms) {
            return GateOutcome::Refuse {
                tx,
                code: RejectCode::Banned,
                message: ban.reason,
            };
        }
        if !super::transport_ws::origin_allowed(&self.config.allowed_origins, peer.origin.as_deref()) {
            return GateOutcome::Refuse {
                tx,
                code: RejectCode::Malformed,
                message: "origin not allowed".into(),
            };
        }
        if self.locked_out(ip, now) {
            return GateOutcome::Refuse {
                tx,
                code: RejectCode::RateLimited,
                message: "too many protocol violations".into(),
            };
        }
        match self.limiter.try_open(ip, now) {
            Ok(()) => {
                self.counted.insert(link, ip);
                self.pending.insert(
                    link,
                    Pending {
                        tx,
                        peer,
                        opened_at: now,
                    },
                );
                GateOutcome::None
            }
            Err(why) => GateOutcome::Refuse {
                tx,
                code: RejectCode::RateLimited,
                message: match why {
                    ConnRefusal::TooManyConnections => "too many connections from this address".into(),
                    ConnRefusal::HandshakeRate => "too many connection attempts".into(),
                },
            },
        }
    }

    fn on_hello(&mut self, link: LinkId, p: Pending, data: &[u8], now: Duration, now_ms: i64) -> GateOutcome {
        let refuse = |tx: Arc<dyn LinkTx>, code: RejectCode, message: &str| GateOutcome::Refuse {
            tx,
            code,
            message: message.to_owned(),
        };
        let ip = peer_ip(&p.peer);
        if !matches!(peek(data), Some((PacketKind::Hello, _))) {
            self.strike(ip, now);
            return refuse(p.tx, RejectCode::Malformed, "expected hello");
        }
        let hello = match parse_hello(data) {
            Ok(h) => h,
            Err(code) => {
                if code == RejectCode::Malformed {
                    self.strike(ip, now);
                }
                return refuse(p.tx, code, "invalid hello");
            }
        };
        if !self.config.game_id.is_empty() && hello.game_id != self.config.game_id {
            return refuse(p.tx, RejectCode::WrongGame, "wrong game");
        }
        match hello.auth.clone() {
            HelloAuth::Ticket(ticket) => {
                // While draining, tickets are still verified and routed: a ticket into a room
                // this server still runs (a rejoin into a match that finishes here) is
                // admitted; one that would create a room is refused by the room directory
                // (`RoomError::Draining` → `Reject(ServerDraining)`).
                let leeway = self.config.leeway.as_millis() as i64;
                let claims = match verify::<JoinClaims>(&self.keys, &ticket, now_ms, leeway) {
                    Ok(c) => c,
                    Err(e) => {
                        let code = match e {
                            TokenError::Expired | TokenError::NotYetValid => RejectCode::TicketExpired,
                            _ => RejectCode::BadTicket,
                        };
                        if code == RejectCode::BadTicket {
                            tracing::warn!(event = "bad_ticket", %ip, error = %e, "join ticket rejected");
                        }
                        return refuse(p.tx, code, &e.to_string());
                    }
                };
                if claims.server_id != self.config.server_id {
                    return refuse(p.tx, RejectCode::WrongServer, "ticket is for another server");
                }
                if claims.room != claims.spec.room_id || claims.room == RoomId(0) {
                    return refuse(p.tx, RejectCode::BadTicket, "ticket room mismatch");
                }
                if let Some(ban) = self.bans.player(claims.player, now_ms) {
                    return refuse(p.tx, RejectCode::Banned, &ban.reason);
                }
                if let Some(game) = &self.config.game
                    && let Err(why) = game.validate_spec(&claims.spec)
                {
                    return refuse(p.tx, RejectCode::RoomNotFound, &why);
                }
                match self
                    .nonces
                    .insert(claims.jti, claims.exp_ms.saturating_add(leeway), now_ms)
                {
                    NonceCheck::Fresh => {}
                    NonceCheck::Replayed => {
                        tracing::warn!(event = "ticket_reuse", %ip, player = %claims.player, "join ticket replayed");
                        return refuse(p.tx, RejectCode::TicketReused, "ticket already used");
                    }
                    NonceCheck::Full => {
                        tracing::warn!(event = "nonce_cache_full", %ip, "ticket nonce cache full; refusing");
                        return refuse(p.tx, RejectCode::RateLimited, "server busy, retry");
                    }
                }
                let identity = PlayerIdentity {
                    player: claims.player,
                    display_name: claims.display_name.clone(),
                    team: claims.team,
                    is_bot: false,
                    extra: claims.extra.clone(),
                };
                self.admit(link, p, hello, Some(identity), claims.room, Some(claims.spec), now)
            }
            HelloAuth::Resume(token) => match resume_route(&token) {
                Ok(t) => self.admit(link, p, hello, None, t.room, None, now),
                Err(_) => {
                    self.strike(ip, now);
                    refuse(p.tx, RejectCode::ResumeExpired, "invalid resume token")
                }
            },
            HelloAuth::Dev { name } => {
                if !self.config.allow_dev_auth {
                    return refuse(p.tx, RejectCode::DevAuthDisabled, "dev auth disabled");
                }
                if self.draining {
                    return refuse(p.tx, RejectCode::ServerDraining, "server draining");
                }
                let spec = match &self.config.game {
                    Some(g) => g.dev_room_spec(),
                    None => RoomSpec {
                        room_id: RoomId(1),
                        mode: "dev".into(),
                        max_players: 64,
                        ..Default::default()
                    },
                };
                let identity = PlayerIdentity::dev(&name);
                if let Some(ban) = self.bans.player(identity.player, now_ms) {
                    return refuse(p.tx, RejectCode::Banned, &ban.reason);
                }
                self.admit(link, p, hello, Some(identity), spec.room_id, Some(spec), now)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn admit(
        &mut self,
        link: LinkId,
        p: Pending,
        hello: space3d_replication::protocol::handshake::Hello,
        identity: Option<PlayerIdentity>,
        room: RoomId,
        spec: Option<RoomSpec>,
        now: Duration,
    ) -> GateOutcome {
        let rate = f64::from(self.config.link_packets_per_sec.max(1));
        self.routed.insert(
            link,
            Routed {
                room,
                tx: p.tx.clone(),
                ip: peer_ip(&p.peer),
                budget: TokenBucket::new(rate, (rate / 3.0).max(60.0)).starting_at(now),
                overflow: OverflowWindows::starting_at(now, rate),
                cut: false,
            },
        );
        GateOutcome::Admit {
            room,
            spec,
            inbound: RoomInbound::Join(Box::new(JoinRequest {
                link,
                tx: p.tx,
                peer: p.peer,
                hello,
                identity,
            })),
        }
    }

    /// The ingress could not deliver an admitted join (room gone / full): forget the route.
    pub fn unroute(&mut self, link: LinkId) {
        self.routed.remove(&link);
    }

    /// Close links whose Hello is overdue (IP strike each); returns how many were closed.
    pub fn sweep(&mut self, now: Duration) -> usize {
        let timeout = self.config.handshake_timeout;
        let overdue: Vec<LinkId> = self
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_sub(p.opened_at) >= timeout)
            .map(|(l, _)| *l)
            .collect();
        for link in &overdue {
            if let Some(p) = self.pending.remove(link) {
                self.strike(peer_ip(&p.peer), now);
                p.tx.close(CloseCode::POLICY, "handshake timeout");
            }
        }
        overdue.len()
    }

    /// IP/CIDR bans added since the last call also end connections already open from the banned
    /// range: `Disconnect(Banned)` (no resume) and close. Cheap when nothing changed (a
    /// generation check); the ingress task calls it with every sweep. Returns links closed.
    pub fn enforce_ip_bans(&mut self, now_ms: i64) -> usize {
        let generation = self.bans.generation();
        if generation == self.bans_seen {
            return 0;
        }
        self.bans_seen = generation;
        if !self.bans.has_ip_bans() {
            return 0;
        }
        let mut closed = 0;
        for r in self.routed.values() {
            if let Some(ban) = self.bans.ip(r.ip, now_ms) {
                let pkt = encode_handshake(
                    PacketKind::Disconnect,
                    &Disconnect::new(DisconnectCode::Banned, &ban.reason, false),
                );
                let _ = r.tx.send(space3d_replication::transport::Lane::Reliable, pkt);
                r.tx.close(CloseCode::POLICY, "banned");
                closed += 1;
            }
        }
        let banned: Vec<LinkId> = self
            .pending
            .iter()
            .filter(|(_, p)| self.bans.ip(peer_ip(&p.peer), now_ms).is_some())
            .map(|(l, _)| *l)
            .collect();
        for link in banned {
            if let Some(p) = self.pending.remove(&link) {
                p.tx.close(CloseCode::POLICY, "banned");
                closed += 1;
            }
        }
        if closed > 0 {
            tracing::info!(
                event = "ban_enforced",
                links = closed,
                "closed links from banned addresses"
            );
        }
        closed
    }

    /// Reject new tickets and dev joins with `ServerDraining` (resumes still pass).
    pub fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
    }

    /// Whether draining.
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// Forget a room's links (room closed).
    pub fn forget_room(&mut self, room: RoomId) {
        self.routed.retain(|_, r| r.room != room);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::bans::{Ban, BanTarget};
    use crate::security::token::{Secret, sign};
    use crate::test_util::FakeLinkTx;
    use space3d_replication::PlayerId;
    use space3d_replication::protocol::handshake::{ClientCaps, Hello};
    use space3d_replication::protocol::{MAGIC, PROTOCOL_VERSION, encode_handshake};
    use std::collections::BTreeMap;

    const S: fn(u64) -> Duration = Duration::from_secs;

    fn keys() -> Arc<KeyRing> {
        let mut k = BTreeMap::new();
        k.insert("k1".to_owned(), Secret::new(vec![9; 32]).unwrap());
        Arc::new(KeyRing::new("k1", k).unwrap())
    }

    fn gate() -> Gatekeeper {
        let config = GatekeeperConfig {
            server_id: "gs-1".into(),
            game_id: "arena".into(),
            max_connections_per_ip: 2,
            handshakes_per_ip_per_min: 10,
            ..Default::default()
        };
        Gatekeeper::new(config, keys(), BanList::default())
    }

    fn spec(room: u64) -> RoomSpec {
        RoomSpec {
            room_id: RoomId(room),
            mode: "duel".into(),
            max_players: 2,
            ..Default::default()
        }
    }

    fn ticket(player: u64, server: &str, room: u64, jti: u8, exp_ms: i64) -> String {
        sign(
            &keys(),
            &JoinClaims {
                player: PlayerId(player),
                display_name: format!("p{player}"),
                team: Some(0),
                server_id: server.into(),
                room: RoomId(room),
                spec: spec(room),
                jti: [jti; 16],
                iat_ms: 0,
                exp_ms,
                extra: serde_json::json!({"rating": 1500}),
            },
        )
    }

    fn hello(auth: HelloAuth) -> Vec<u8> {
        encode_handshake(
            PacketKind::Hello,
            &Hello {
                magic: MAGIC,
                protocol: PROTOCOL_VERSION,
                game_id: "arena".into(),
                game_version: "1".into(),
                schema_hash: 1,
                auth,
                caps: ClientCaps::default(),
                client_time_us: 0,
            },
        )
    }

    fn open(g: &mut Gatekeeper, link: u64, ip: &str, now: Duration) -> (Arc<FakeLinkTx>, GateOutcome) {
        let tx = Arc::new(FakeLinkTx::default());
        let out = g.handle(
            LinkEvent::Opened {
                link: LinkId(link),
                peer: PeerInfo {
                    addr: Some(format!("{ip}:5000").parse().unwrap()),
                    client_ip: Some(ip.parse().unwrap()),
                    transport: "test",
                    ..Default::default()
                },
                tx: tx.clone(),
            },
            now,
            0,
        );
        (tx, out)
    }

    fn packet(g: &mut Gatekeeper, link: u64, data: Vec<u8>, now: Duration, now_ms: i64) -> GateOutcome {
        g.handle(
            LinkEvent::Packet {
                link: LinkId(link),
                lane: space3d_replication::transport::Lane::Reliable,
                data,
            },
            now,
            now_ms,
        )
    }

    fn refused(out: &GateOutcome) -> Option<RejectCode> {
        match out {
            GateOutcome::Refuse { code, .. } => Some(*code),
            _ => None,
        }
    }

    #[test]
    fn valid_ticket_admits_and_forwards() {
        let mut g = gate();
        let (_, out) = open(&mut g, 1, "198.51.100.1", S(0));
        assert!(matches!(out, GateOutcome::None));
        let out = packet(
            &mut g,
            1,
            hello(HelloAuth::Ticket(ticket(7, "gs-1", 5, 1, 60_000))),
            S(1),
            1_000,
        );
        match out {
            GateOutcome::Admit {
                room,
                spec: Some(s),
                inbound: RoomInbound::Join(j),
            } => {
                assert_eq!(room, RoomId(5));
                assert_eq!(s.mode, "duel");
                let id = j.identity.unwrap();
                assert_eq!(id.player, PlayerId(7));
                assert_eq!(id.extra["rating"], 1500);
            }
            other => panic!("expected admit, got {other:?}"),
        }
        assert!(matches!(
            packet(&mut g, 1, vec![4, 0], S(1), 1_000),
            GateOutcome::Forward {
                room: RoomId(5),
                inbound: RoomInbound::Packet { .. }
            }
        ));
        assert!(matches!(
            g.handle(
                LinkEvent::Closed {
                    link: LinkId(1),
                    reason: "bye".into()
                },
                S(2),
                2_000
            ),
            GateOutcome::Forward {
                inbound: RoomInbound::Closed { .. },
                ..
            }
        ));
        assert_eq!(g.routed_len(), 0);
    }

    #[test]
    fn ticket_rejections() {
        let mut g = gate();
        let mut link = 0;
        let mut attempt = |g: &mut Gatekeeper, t: String, now_ms: i64| {
            link += 1;
            let ip = format!("198.51.100.{link}");
            open(g, link, &ip, S(0));
            refused(&packet(g, link, hello(HelloAuth::Ticket(t)), S(0), now_ms))
        };
        assert_eq!(
            attempt(&mut g, ticket(1, "gs-2", 5, 1, 60_000), 0),
            Some(RejectCode::WrongServer)
        );
        assert_eq!(
            attempt(&mut g, ticket(1, "gs-1", 5, 2, 60_000), 70_000),
            Some(RejectCode::TicketExpired)
        );
        assert_eq!(
            attempt(&mut g, "s3.join.k1.xx.yy".into(), 0),
            Some(RejectCode::BadTicket)
        );
        assert_eq!(
            attempt(&mut g, ticket(1, "gs-1", 5, 3, 60_000), 0),
            None,
            "first use ok"
        );
        assert_eq!(
            attempt(&mut g, ticket(1, "gs-1", 5, 3, 60_000), 0),
            Some(RejectCode::TicketReused)
        );
        g.bans.upsert(Ban {
            id: 1,
            target: BanTarget::Player(PlayerId(66)),
            until_ms: None,
            reason: "cheater".into(),
        });
        assert_eq!(
            attempt(&mut g, ticket(66, "gs-1", 5, 4, 60_000), 0),
            Some(RejectCode::Banned)
        );
        // Draining: tickets are still verified and routed (a rejoin into a room that still
        // runs here must work); the room directory refuses to *create* rooms while draining.
        g.set_draining(true);
        assert_eq!(
            attempt(&mut g, ticket(2, "gs-1", 5, 5, 60_000), 0),
            None,
            "routed; creation is refused by RoomManager"
        );
        assert_eq!(
            attempt(&mut g, ticket(3, "gs-2", 5, 6, 60_000), 0),
            Some(RejectCode::WrongServer),
            "still verified while draining"
        );
    }

    #[test]
    fn structure_version_game_and_dev_checks() {
        let mut g = gate();
        open(&mut g, 1, "198.51.100.1", S(0));
        assert_eq!(
            refused(&packet(&mut g, 1, vec![4, 0, 0], S(0), 0)),
            Some(RejectCode::Malformed)
        );
        open(&mut g, 2, "198.51.100.2", S(0));
        let mut bad = hello(HelloAuth::Dev { name: "x".into() });
        bad.truncate(3);
        assert!(refused(&packet(&mut g, 2, bad, S(0), 0)).is_some());
        open(&mut g, 3, "198.51.100.3", S(0));
        assert_eq!(
            refused(&packet(&mut g, 3, hello(HelloAuth::Dev { name: "x".into() }), S(0), 0)),
            Some(RejectCode::DevAuthDisabled)
        );
        open(&mut g, 4, "198.51.100.4", S(0));
        let wrong_game = encode_handshake(
            PacketKind::Hello,
            &Hello {
                magic: MAGIC,
                protocol: PROTOCOL_VERSION,
                game_id: "other".into(),
                game_version: "1".into(),
                schema_hash: 1,
                auth: HelloAuth::Ticket(ticket(1, "gs-1", 5, 9, 60_000)),
                caps: ClientCaps::default(),
                client_time_us: 0,
            },
        );
        assert_eq!(
            refused(&packet(&mut g, 4, wrong_game, S(0), 0)),
            Some(RejectCode::WrongGame)
        );
        open(&mut g, 5, "198.51.100.5", S(0));
        let old = encode_handshake(
            PacketKind::Hello,
            &Hello {
                magic: MAGIC,
                protocol: PROTOCOL_VERSION + 1,
                game_id: "arena".into(),
                game_version: "1".into(),
                schema_hash: 1,
                auth: HelloAuth::Ticket(ticket(1, "gs-1", 5, 10, 60_000)),
                caps: ClientCaps::default(),
                client_time_us: 0,
            },
        );
        assert_eq!(
            refused(&packet(&mut g, 5, old, S(0), 0)),
            Some(RejectCode::VersionMismatch)
        );
    }

    #[test]
    fn resume_routes_by_token_room() {
        use space3d_replication::NetClientId;
        use space3d_replication::protocol::handshake::ResumeToken;
        let mut g = gate();
        g.set_draining(true);
        open(&mut g, 1, "198.51.100.1", S(0));
        let token = ResumeToken {
            room: RoomId(77),
            client: NetClientId(3),
            epoch: 2,
            secret: [1; 16],
        }
        .encode();
        match packet(&mut g, 1, hello(HelloAuth::Resume(token)), S(0), 0) {
            GateOutcome::Admit {
                room,
                spec: None,
                inbound: RoomInbound::Join(j),
            } => {
                assert_eq!(room, RoomId(77));
                assert!(j.identity.is_none(), "the room verifies resumes");
            }
            other => panic!("resume must pass while draining: {other:?}"),
        }
        // A token of the wrong size fails `Hello` validation (the fixed 30-byte encoding).
        open(&mut g, 2, "198.51.100.2", S(0));
        assert_eq!(
            refused(&packet(&mut g, 2, hello(HelloAuth::Resume(vec![1, 2, 3])), S(0), 0)),
            Some(RejectCode::Malformed)
        );
    }

    #[test]
    fn per_ip_limits_timeouts_and_strike_lockout() {
        let mut g = gate();
        let ip = "203.0.113.7";
        assert!(matches!(open(&mut g, 1, ip, S(0)).1, GateOutcome::None));
        assert!(matches!(open(&mut g, 2, ip, S(0)).1, GateOutcome::None));
        assert_eq!(
            refused(&open(&mut g, 3, ip, S(0)).1),
            Some(RejectCode::RateLimited),
            "3rd concurrent"
        );
        // Handshake timeout closes pending links and strikes the IP.
        let (tx, _) = open(&mut g, 4, "203.0.113.8", S(0));
        assert_eq!(g.sweep(S(4)), 0);
        assert_eq!(g.sweep(S(6)), 3);
        assert!(tx.closed().is_some());
        for l in [1, 2, 4] {
            g.handle(
                LinkEvent::Closed {
                    link: LinkId(l),
                    reason: "timeout".into(),
                },
                S(6),
                0,
            );
        }
        // Five violations → the IP is locked out for the rest of the minute.
        for i in 0..5 {
            open(&mut g, 100 + i, "203.0.113.9", S(10));
            assert!(refused(&packet(&mut g, 100 + i, vec![0xff], S(10), 0)).is_some());
            g.handle(
                LinkEvent::Closed {
                    link: LinkId(100 + i),
                    reason: String::new(),
                },
                S(10),
                0,
            );
        }
        assert_eq!(
            refused(&open(&mut g, 200, "203.0.113.9", S(11)).1),
            Some(RejectCode::RateLimited)
        );
        assert!(matches!(open(&mut g, 201, "203.0.113.9", S(75)).1, GateOutcome::None));
    }

    fn admit_dev(g: &mut Gatekeeper, link: u64, ip: &str) -> Arc<FakeLinkTx> {
        let (tx, _) = open(g, link, ip, S(0));
        let out = packet(
            g,
            link,
            hello(HelloAuth::Dev {
                name: format!("p{link}"),
            }),
            S(0),
            0,
        );
        assert!(matches!(out, GateOutcome::Admit { .. }), "{out:?}");
        tx
    }

    fn disconnect_code(tx: &FakeLinkTx) -> Option<DisconnectCode> {
        tx.sent().iter().find_map(
            |(_, p)| match space3d_replication::protocol::decode_packet(p, &Default::default()) {
                Ok(space3d_replication::protocol::Packet::Disconnect(d)) => Some(d.code),
                _ => None,
            },
        )
    }

    /// An IP/CIDR ban added later also ends links already open from the banned range.
    #[test]
    fn ip_bans_close_connected_links() {
        let mut g = gate();
        g.config.allow_dev_auth = true;
        let a = admit_dev(&mut g, 1, "192.0.2.4");
        let b = admit_dev(&mut g, 2, "198.51.100.9");
        let (pending, _) = open(&mut g, 3, "192.0.2.77", S(0));
        assert_eq!(g.enforce_ip_bans(0), 0, "no bans");
        g.bans.upsert(Ban {
            id: 9,
            target: BanTarget::Ip(Cidr::parse("192.0.2.0/24").unwrap()),
            until_ms: None,
            reason: "abuse".into(),
        });
        assert_eq!(g.enforce_ip_bans(1_000), 2, "the admitted and the pending link");
        assert_eq!(disconnect_code(&a), Some(DisconnectCode::Banned));
        assert!(a.closed().is_some() && pending.closed().is_some());
        assert!(b.closed().is_none(), "other ranges untouched");
        assert_eq!(g.enforce_ip_bans(2_000), 0, "only on ban changes");
    }

    /// One admitted link cannot push more than its budget into the shared room inbox: excess
    /// is dropped at the gateway and a sustained flood closes the link (with an IP strike).
    #[test]
    fn per_link_budget_drops_then_cuts_a_flood() {
        let mut g = gate();
        g.config.allow_dev_auth = true;
        g.config.link_packets_per_sec = 120;
        let tx = admit_dev(&mut g, 1, "203.0.113.5");
        let mut forwarded = 0;
        let mut t = Duration::ZERO;
        // 5 s at 600 packets/s (5× the budget).
        for _ in 0..300 {
            t += Duration::from_millis(16);
            for _ in 0..10 {
                if matches!(packet(&mut g, 1, vec![4, 0], t, 0), GateOutcome::Forward { .. }) {
                    forwarded += 1;
                }
            }
            if tx.closed().is_some() {
                break;
            }
        }
        assert!(tx.closed().is_some(), "sustained flood cut");
        assert_eq!(disconnect_code(&tx), Some(DisconnectCode::RateLimited));
        assert!(g.dropped_packets() > 0);
        assert!(t <= Duration::from_secs(5), "cut after {t:?}");
        let secs = t.as_secs_f64();
        assert!(
            f64::from(forwarded) <= 120.0 * secs + 60.0,
            "{forwarded} forwarded in {secs:.1} s"
        );
        // Nothing more is forwarded from the cut link.
        assert!(matches!(packet(&mut g, 1, vec![4, 0], t, 0), GateOutcome::None));
        // An honest link at 70 packets/s is never touched.
        let honest = admit_dev(&mut g, 2, "203.0.113.6");
        for i in 0..600u64 {
            let at = t + Duration::from_millis(i * 1000 / 70);
            assert!(matches!(
                packet(&mut g, 2, vec![4, 0], at, 0),
                GateOutcome::Forward { .. }
            ));
        }
        assert!(honest.closed().is_none());
    }

    #[test]
    fn banned_ip_and_origin() {
        let mut g = gate();
        g.config.allowed_origins = vec!["https://ok.example".into()];
        g.bans.upsert(Ban {
            id: 2,
            target: BanTarget::Ip(Cidr::parse("192.0.2.0/24").unwrap()),
            until_ms: Some(1_000),
            reason: "abuse".into(),
        });
        assert_eq!(refused(&open(&mut g, 1, "192.0.2.4", S(0)).1), Some(RejectCode::Banned));
        let tx = Arc::new(FakeLinkTx::default());
        let out = g.handle(
            LinkEvent::Opened {
                link: LinkId(2),
                peer: PeerInfo {
                    client_ip: Some("198.51.100.1".parse().unwrap()),
                    origin: Some("https://evil.example".into()),
                    ..Default::default()
                },
                tx,
            },
            S(0),
            0,
        );
        assert_eq!(refused(&out), Some(RejectCode::Malformed));
    }
}
