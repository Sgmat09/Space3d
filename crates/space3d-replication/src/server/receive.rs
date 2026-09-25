//! `replication::server_receive` (PreUpdate): drain the room inbox.
//!
//! * `Join` → schema/game/capacity/joinable checks (`Reject` + close on failure), resume
//!   verification, supersede, slot allocation, `Welcome` (+ fresh resume token), events.
//! * `Packet` → per-link token buckets (packets/s, bytes/s, messages/s) → `decode_packet`
//!   (strike on error) → acks (datagram) → Time (Pong queued) → Input (buffer) → Control →
//!   messages (size cap, per-type rate limit, `NetMessage::validate`, `Events<FromClient<M>>`).
//! * `Closed` → slot `Disconnected { until }` + `PlayerDisconnected`.
//!
//! Strikes: `strikes.max` within `strikes.window` → `Disconnect(ProtocolViolation)`.
//! Rate limits: overflow is dropped; drops in 3 consecutive 1 s windows (or ten seconds' worth
//! of the packet allowance within one window) → `Disconnect(RateLimited)` ([`OverflowWindows`]).

use super::handshake::SecretSource;
use super::slots::Slot;
use super::{
    JoinRequest, LeaveReason, NetInbox, NetServer, PendingEvent, PlayerDisconnected, PlayerIdentity, PlayerJoined,
    PlayerResumed, RoomInbound, SlotState,
};
use crate::protocol::control::{ControlMsg, TimeMsg};
use crate::protocol::handshake::{DisconnectCode, HelloAuth, RejectCode, ResumeToken, Welcome};
use crate::protocol::input::InputWindow;
use crate::protocol::{ChannelId, Packet, PacketKind, WireLimits, decode_packet, encode_handshake};
use crate::registry::{MessageReject, ReplicationRegistry};
use crate::time::NetClockHandle;
use crate::transport::{CloseCode, Lane};
use crate::wire::Reader;
use crate::{LinkId, NetClientId};
use space3d_core::{Resources, SystemCtx, TickRate};
use std::time::Duration;

/// Deterministic token bucket (time injected by the caller).
#[derive(Clone, Debug)]
pub struct TokenBucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Duration,
}

impl TokenBucket {
    /// `rate` per second with `burst` capacity (starts full).
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate,
            burst,
            tokens: burst,
            last: Duration::ZERO,
        }
    }

    /// Start counting at `now` (avoids a refill burst from time zero).
    pub fn starting_at(mut self, now: Duration) -> Self {
        self.last = now;
        self
    }

    /// Take `n` tokens at `now`; `false` if not enough.
    pub fn take(&mut self, now: Duration, n: f64) -> bool {
        let dt = now.saturating_sub(self.last).as_secs_f64();
        self.last = now.max(self.last);
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Tokens currently available (without refilling).
    pub fn tokens(&self) -> f64 {
        self.tokens
    }
}

/// Per-link inbound limits (internal to the replication builder).
#[derive(Clone, Debug)]
pub struct LinkLimiter {
    /// Packets per second.
    pub packets: TokenBucket,
    /// Bytes per second.
    pub bytes: TokenBucket,
    /// Messages per second (all types).
    pub messages: TokenBucket,
    /// Strike timestamps within the window.
    pub strikes: Vec<Duration>,
    /// Per message kind buckets.
    pub per_kind: crate::FastMap<u16, TokenBucket>,
    /// Rate-limit drops per 1 s window (sustained overflow → `RateLimited`).
    pub overflow: OverflowWindows,
}

impl LinkLimiter {
    /// Limiter for `limits` at a room ticking at `tick_hz`, starting at `now`.
    ///
    /// Packets: `input_packets_per_sec` (never below 1.5 × the tick rate) plus 30/s of headroom
    /// for pings, control and message-only packets, with a burst of half that: 120/s, burst 60
    /// by default. The burst absorbs a TCP stall of about a second (the stream then delivers
    /// the queued packets at once); a stream sends each input once, so a dropped input packet is
    /// a lost input.
    pub fn new(limits: &WireLimits, tick_hz: f64, now: Duration) -> Self {
        let pps = f64::from(limits.input_packets_per_sec.max(1)).max((tick_hz * 1.5).ceil()) + 30.0;
        Self {
            packets: TokenBucket::new(pps, (pps / 2.0).max(60.0)).starting_at(now),
            bytes: TokenBucket::new(f64::from(limits.max_packet) * 16.0, f64::from(limits.max_packet) * 8.0)
                .starting_at(now),
            messages: TokenBucket::new(
                f64::from(limits.messages_per_sec.max(1)),
                f64::from(limits.messages_per_sec.max(1)) * 2.0,
            )
            .starting_at(now),
            strikes: Vec::new(),
            per_kind: Default::default(),
            overflow: OverflowWindows::starting_at(now, pps),
        }
    }
}

/// Rate-limit drops counted per 1 s window. A token bucket lets a packet through every time
/// it refills, so "no packet passed for 3 s" never happens under a flood; overflow is instead
/// judged per window: a kick when drops happened in [`SUSTAINED_WINDOWS`] consecutive windows
/// followed by another drop (≥ 3 s of sustained overflow), or at once when one window drops
/// more than [`OverflowWindows::hard_limit`] (ten seconds' worth of the packet allowance; an
/// honest client cannot produce that, even replaying a long stall).
#[derive(Clone, Debug)]
pub struct OverflowWindows {
    start: Duration,
    drops: u32,
    streak: u32,
    hard_limit: u32,
}

/// Length of one overflow window.
pub const OVERFLOW_WINDOW: Duration = Duration::from_secs(1);
/// Consecutive windows with drops before a `RateLimited` kick.
pub const SUSTAINED_WINDOWS: u32 = 3;

impl OverflowWindows {
    /// Windows starting at `now` for a link allowed `packets_per_sec`.
    pub fn starting_at(now: Duration, packets_per_sec: f64) -> Self {
        Self {
            start: now,
            drops: 0,
            streak: 0,
            hard_limit: ((packets_per_sec * 10.0) as u32).max(1_000),
        }
    }

    /// Drops in the current window.
    pub fn drops(&self) -> u32 {
        self.drops
    }

    /// Drops above this many within one window kick at once.
    pub fn hard_limit(&self) -> u32 {
        self.hard_limit
    }

    fn roll(&mut self, now: Duration) {
        if now < self.start + OVERFLOW_WINDOW {
            return;
        }
        let elapsed = (now - self.start).as_nanos() / OVERFLOW_WINDOW.as_nanos();
        // The window that just ended continues the streak only if it had drops and is
        // directly followed by the new one (windows without packets had no drops).
        self.streak = if self.drops > 0 && elapsed == 1 {
            self.streak + 1
        } else {
            0
        };
        self.drops = 0;
        self.start += OVERFLOW_WINDOW * elapsed.min(u128::from(u32::MAX)) as u32;
    }

    /// Record a dropped packet or message at `now`; `true` = kick (`RateLimited`).
    pub fn record_drop(&mut self, now: Duration) -> bool {
        self.roll(now);
        self.drops = self.drops.saturating_add(1);
        self.streak >= SUSTAINED_WINDOWS || self.drops > self.hard_limit
    }
}

/// The `replication::server_receive` system.
pub(crate) fn server_receive(ctx: &mut SystemCtx) {
    crate::freeze_registry(ctx);
    let now = ctx.resources.init::<NetClockHandle>().now();
    let hz = ctx.resources.get::<TickRate>().map_or(60.0, |r| r.hz);
    let resources = &mut *ctx.resources;
    resources.scope::<NetServer, _>(|res, server| {
        server.slots.tick_hz = hz;
        let mut inbound = Vec::new();
        if let Some(inbox) = res.get::<NetInbox>() {
            while inbound.len() < server.config.max_inbound_per_tick {
                match inbox.try_recv() {
                    Some(m) => inbound.push(m),
                    None => break,
                }
            }
        }
        res.scope::<ReplicationRegistry, _>(|res, registry| {
            for msg in inbound {
                match msg {
                    RoomInbound::Join(req) => handle_join(server, registry, res, *req, now),
                    RoomInbound::Packet { link, bytes } => handle_packet(server, registry, res, link, &bytes, now),
                    RoomInbound::Closed { link, reason } => handle_closed(server, link, &reason),
                }
            }
        });
        check_links(server, now);
        server.flush_events(res);
    });
}

fn reject(req: &JoinRequest, code: RejectCode, message: &str) {
    let pkt = super::handshake::reject_packet(code, message, None);
    let _ = req.tx.send(Lane::Reliable, pkt);
    req.tx.close(CloseCode::POLICY, message);
}

fn handle_join(
    server: &mut NetServer,
    registry: &ReplicationRegistry,
    res: &mut Resources,
    req: JoinRequest,
    now: Duration,
) {
    let hello = &req.hello;
    if let Err(code) = hello.validate() {
        server.stats.rejects += 1;
        return reject(&req, code, "malformed hello");
    }
    if !server.config.game_id.is_empty() && hello.game_id != server.config.game_id {
        server.stats.rejects += 1;
        return reject(&req, RejectCode::WrongGame, "wrong game");
    }
    if hello.schema_hash != registry.schema_hash() {
        server.stats.rejects += 1;
        return reject(
            &req,
            RejectCode::SchemaMismatch,
            &format!(
                "schema {:016x} != server {:016x}",
                hello.schema_hash,
                registry.schema_hash()
            ),
        );
    }
    let secret = next_secret(res);
    // Resume.
    if let HelloAuth::Resume(bytes) = &hello.auth {
        let Ok(token) = ResumeToken::decode(bytes) else {
            server.stats.rejects += 1;
            return reject(&req, RejectCode::ResumeExpired, "bad resume token");
        };
        let client = (token.room == server.config.room)
            .then(|| server.slots.verify_resume(&token, server.tick))
            .flatten();
        let Some(client) = client else {
            server.stats.rejects += 1;
            return reject(&req, RejectCode::ResumeExpired, "resume token expired");
        };
        // Replace a half-dead old link.
        server.drop_link(client, Some((DisconnectCode::Superseded, "resumed elsewhere")), false);
        let player = server.slots.get(client).map(|s| s.identity.player);
        bind_link(server, client, &req, secret, now);
        server.stats.resumes += 1;
        send_welcome(server, client, &req, true);
        if let Some(player) = player {
            server
                .pending
                .push(PendingEvent::Resumed(PlayerResumed { client, player }));
        }
        return;
    }
    let identity = match (&req.identity, &hello.auth) {
        (Some(id), _) => id.clone(),
        (None, HelloAuth::Dev { name }) => PlayerIdentity::dev(name),
        (None, _) => {
            server.stats.rejects += 1;
            return reject(&req, RejectCode::BadTicket, "unauthenticated");
        }
    };
    // Same player again: supersede the old link, keep the slot.
    if let Some(client) = server.slots.by_player.get(&identity.player).copied() {
        if server.slots.get(client).is_some_and(|s| s.brain.is_some()) {
            server.stats.rejects += 1;
            return reject(&req, RejectCode::Internal, "player is a server bot");
        }
        server.drop_link(client, Some((DisconnectCode::Superseded, "connected elsewhere")), false);
        if let Some(slot) = server.slots.get_mut(client) {
            slot.identity = identity.clone();
        }
        bind_link(server, client, &req, secret, now);
        server.stats.resumes += 1;
        send_welcome(server, client, &req, true);
        server.pending.push(PendingEvent::Resumed(PlayerResumed {
            client,
            player: identity.player,
        }));
        return;
    }
    if !server.joinable {
        server.stats.rejects += 1;
        return reject(&req, RejectCode::RoomClosed, "room is not accepting players");
    }
    let Some(client) = server.slots.allocate(server.config.max_clients) else {
        server.stats.rejects += 1;
        return reject(&req, RejectCode::RoomFull, "room full");
    };
    let slot = Slot::new(client, identity.clone(), server.tick, &server.config, now);
    server.slots.insert(slot);
    bind_link(server, client, &req, secret, now);
    server.stats.joins += 1;
    send_welcome(server, client, &req, false);
    server
        .pending
        .push(PendingEvent::Joined(PlayerJoined { client, identity }));
}

fn next_secret(res: &mut Resources) -> [u8; 16] {
    match res.get_mut::<SecretSource>() {
        Some(s) => s.generate(),
        None => {
            let mut s = SecretSource::seeded(0x5eed);
            let out = s.generate();
            res.insert(s);
            out
        }
    }
}

/// Attach `req`'s link to `client`, rotating the resume secret and resetting per-link state.
fn bind_link(server: &mut NetServer, client: NetClientId, req: &JoinRequest, secret: [u8; 16], now: Duration) {
    let max = server.config.client_bandwidth_max;
    let default_rate = server.config.client_bandwidth;
    let limits = server.config.limits;
    let tick_hz = server.tick_hz();
    let Some(slot) = server.slots.get_mut(client) else {
        return;
    };
    if slot.link.is_some() {
        slot.epoch = slot.epoch.wrapping_add(1);
    } else if slot.resume_secret != [0; 16] {
        // Resume or supersede of a disconnected slot.
        slot.epoch = slot.epoch.wrapping_add(1);
    }
    slot.link = Some((req.link, req.tx.clone()));
    slot.resume_secret = secret;
    slot.state = SlotState::Syncing;
    slot.last_rx = now;
    slot.clear_link_state();
    slot.view.reset_for_new_link();
    slot.inputs.reset();
    slot.limits = LinkLimiter::new(&limits, tick_hz, now);
    slot.datagram = req.tx.caps().true_datagrams;
    let wanted = req.hello.caps.max_down_bytes_per_sec;
    slot.down_rate = if wanted == 0 {
        default_rate
    } else {
        wanted.min(max).max(4096)
    };
    if req.hello.client_time_us > 0 {
        // Nothing to compare against on the server clock; RTT comes from client reports.
    }
    server.slots.by_link.insert(req.link, client);
}

fn send_welcome(server: &mut NetServer, client: NetClientId, req: &JoinRequest, resumed: bool) {
    let hz = server.tick_hz();
    let Some(slot) = server.slots.get(client) else { return };
    let token = ResumeToken {
        room: server.config.room,
        client,
        epoch: slot.epoch,
        secret: slot.resume_secret,
    };
    let welcome = Welcome {
        client,
        player: slot.identity.player,
        room: server.config.room,
        room_epoch: server.config.room_epoch,
        tick_hz: hz.round().clamp(1.0, f64::from(u16::MAX)) as u16,
        send_interval: server.config.send_interval.max(1),
        server_tick: server.tick,
        resume: token.encode(),
        resume_window_s: server.config.resume_window.as_secs().min(u64::from(u16::MAX)) as u16,
        limits: server.config.limits,
        down_bytes_per_sec: slot.down_rate,
        resumed,
        echo_client_time_us: req.hello.client_time_us,
    };
    let _ = req
        .tx
        .send(Lane::Reliable, encode_handshake(PacketKind::Welcome, &welcome));
}

fn handle_closed(server: &mut NetServer, link: LinkId, reason: &str) {
    let Some(client) = server.slots.by_link.get(&link).copied() else {
        return;
    };
    tracing::debug!(room = %server.config.room, ?client, reason, "link closed");
    server.stats.links_lost += 1;
    link_lost(server, client);
}

/// The link of `client` is gone (closed, timed out): keep the slot for the resume window.
pub(crate) fn link_lost(server: &mut NetServer, client: NetClientId) {
    server.drop_link(client, None, true);
    let until = server.resume_deadline();
    let Some(slot) = server.slots.get_mut(client) else {
        return;
    };
    if matches!(slot.state, SlotState::Disconnected { .. }) {
        return;
    }
    slot.state = SlotState::Disconnected { until };
    let player = slot.identity.player;
    server.pending.push(PendingEvent::Disconnected(PlayerDisconnected {
        client,
        player,
        resume_until: until,
    }));
}

/// Idle timeouts and links closed underneath us.
fn check_links(server: &mut NetServer, now: Duration) {
    let idle = server.config.idle_timeout;
    let mut lost = Vec::new();
    let mut idle_kick = Vec::new();
    for slot in server.slots.iter() {
        if let Some((_, tx)) = &slot.link {
            if !tx.is_open() {
                lost.push(slot.client);
            } else if now.saturating_sub(slot.last_rx) > idle {
                idle_kick.push(slot.client);
            }
        }
    }
    server.stats.links_lost += lost.len() as u64;
    for c in idle_kick {
        server.drop_link(c, Some((DisconnectCode::Timeout, "idle timeout")), true);
        lost.push(c);
    }
    for c in lost {
        link_lost(server, c);
    }
}

/// Count a rate-limit drop; kicks (`RateLimited`, no resume) on sustained or extreme overflow.
fn rate_limit_drop(server: &mut NetServer, client: NetClientId, now: Duration) {
    let Some(slot) = server.slots.get_mut(client) else {
        return;
    };
    if slot.limits.overflow.record_drop(now) {
        let player = slot.identity.player;
        tracing::warn!(event = "rate_limited", room = %server.config.room, ?client, "kicking");
        server.kick(player, DisconnectCode::RateLimited, "rate limited", false);
    }
}

/// Record a strike; kicks on too many. Returns `true` when the client was kicked.
pub(crate) fn strike(server: &mut NetServer, client: NetClientId, now: Duration, why: &str) -> bool {
    server.stats.strikes += 1;
    let policy = server.config.strikes;
    let Some(slot) = server.slots.get_mut(client) else {
        return true;
    };
    tracing::debug!(room = %server.config.room, ?client, why, "strike");
    slot.limits.strikes.retain(|t| now.saturating_sub(*t) <= policy.window);
    slot.limits.strikes.push(now);
    if slot.limits.strikes.len() as u32 >= policy.max {
        let player = slot.identity.player;
        tracing::warn!(event = "protocol_violation", room = %server.config.room, ?client, why, "kicking");
        server.kick(player, DisconnectCode::ProtocolViolation, why, false);
        return true;
    }
    false
}

fn handle_packet(
    server: &mut NetServer,
    registry: &ReplicationRegistry,
    res: &mut Resources,
    link: LinkId,
    bytes: &[u8],
    now: Duration,
) {
    let Some(client) = server.slots.by_link.get(&link).copied() else {
        return;
    };
    server.stats.packets_in += 1;
    server.stats.bytes_in += bytes.len() as u64;
    let tick = server.tick;
    {
        let Some(slot) = server.slots.get_mut(client) else {
            return;
        };
        slot.last_rx = now;
        slot.last_packet_tick = tick;
        slot.stats.bytes_in += bytes.len() as u64;
        let ok = slot.limits.packets.take(now, 1.0) && slot.limits.bytes.take(now, bytes.len() as f64);
        if !ok {
            server.stats.rate_limited_packets += 1;
            rate_limit_drop(server, client, now);
            return;
        }
    }
    let limits = server.config.limits;
    let packet = match decode_packet(bytes, &limits) {
        Ok(p) => p,
        Err(e) => {
            strike(server, client, now, &format!("decode: {e}"));
            return;
        }
    };
    let data = match packet {
        Packet::Data(d) => d,
        Packet::Disconnect(d) => {
            server.stats.count_disconnect(DisconnectCode::Leave);
            let reason = if d.code == DisconnectCode::Leave {
                LeaveReason::Left
            } else {
                LeaveReason::Kicked(d.code, d.message.clone())
            };
            server.free_slot(client, reason);
            return;
        }
        Packet::Hello(_) | Packet::Welcome(_) | Packet::Reject(_) => {
            strike(server, client, now, "unexpected handshake packet");
            return;
        }
    };
    if let Some(ack) = data.header.ack {
        super::send::on_ack(server, client, ack, now);
    }
    for block in &data.blocks {
        if server.slots.get(client).is_none_or(|s| s.link.is_none()) {
            return; // kicked while processing
        }
        match block.channel {
            ChannelId::Time => match crate::wire::postcard_exact::<TimeMsg>(block.payload) {
                Ok(TimeMsg::Ping(p)) => {
                    if let Some(slot) = server.slots.get_mut(client)
                        && slot.pongs.len() < 16
                    {
                        slot.pongs.push((p.id, p.t_us, now));
                    }
                }
                Ok(TimeMsg::Pong(_)) => {}
                Err(_) => {
                    strike(server, client, now, "bad time block");
                }
            },
            ChannelId::Input => {
                let mut r = Reader::new(block.payload);
                match InputWindow::decode(&mut r, &limits) {
                    Ok(win) if r.is_empty() => receive_inputs(server, registry, client, &win, now),
                    _ => {
                        server.stats.inputs_invalid += 1;
                        strike(server, client, now, "bad input block");
                    }
                }
            }
            ChannelId::Control => match crate::wire::postcard_exact::<ControlMsg>(block.payload) {
                Ok(msg) => handle_control(server, client, msg, now),
                Err(_) => {
                    strike(server, client, now, "bad control block");
                }
            },
            ChannelId::MsgReliable | ChannelId::MsgUnreliable => {
                receive_message(server, registry, res, client, block.payload, now)
            }
            ChannelId::Snapshot => {
                strike(server, client, now, "snapshot from client");
            }
        }
    }
}

fn handle_control(server: &mut NetServer, client: NetClientId, msg: ControlMsg, now: Duration) {
    let max = server.config.client_bandwidth_max;
    match msg {
        ControlMsg::Leave => {
            server.stats.count_disconnect(DisconnectCode::Leave);
            server.free_slot(client, LeaveReason::Left);
        }
        ControlMsg::SetRate { max_down_bytes_per_sec } => {
            if let Some(slot) = server.slots.get_mut(client) {
                slot.down_rate = max_down_bytes_per_sec.clamp(4096, max.max(4096));
            }
        }
        ControlMsg::InterestHint { center, radius } => {
            if let Some(slot) = server.slots.get_mut(client)
                && center.iter().all(|v| v.is_finite())
                && radius.is_finite()
            {
                slot.interest_hint = Some((center, radius.clamp(0.0, 1000.0)));
            }
        }
        ControlMsg::Report(r) => {
            if let Some(slot) = server.slots.get_mut(client) {
                slot.report = Some(r);
                slot.stats.interp_delay_ticks = r.interp_delay_ms;
                // The client's measured RTT: the only source on streams, and a seed for the
                // datagram RTO until acks provide samples (bounded: it is client-supplied and
                // only ever affects this client's own resend timing and stats).
                if (!slot.datagram || slot.rtt.samples() == 0) && r.rtt_ms.is_finite() && r.rtt_ms > 0.0 {
                    slot.rtt.sample(Duration::from_secs_f32(r.rtt_ms.min(5_000.0) / 1000.0));
                }
            }
        }
        ControlMsg::Notice(_) | ControlMsg::Kick { .. } => {
            strike(server, client, now, "server-only control message");
        }
    }
}

fn receive_inputs(
    server: &mut NetServer,
    registry: &ReplicationRegistry,
    client: NetClientId,
    win: &InputWindow<'_>,
    now: Duration,
) {
    let Some(input) = registry.input() else {
        strike(server, client, now, "input without a registered input type");
        return;
    };
    let tick = server.tick;
    let next = tick.next();
    let max_ahead = server.config.max_input_ahead;
    let limit = tick.0.saturating_add(max_ahead).saturating_add(1);
    if win.newest_tick.0 > limit {
        server.stats.inputs_invalid += 1;
        // An honest client can overshoot after a clock disturbance: drop the inputs but report
        // the slack so its pacer pulls back. Only absurd ticks count as a violation.
        if let Some(slot) = server.slots.get_mut(client) {
            slot.inputs.observe_slack(win.newest_tick.since(next) as f32);
        }
        if win.newest_tick.0 > tick.0.saturating_add(max_ahead.saturating_mul(4)) {
            strike(server, client, now, "input too far ahead");
        }
        return;
    }
    let mut strikes = 0;
    {
        let Some(slot) = server.slots.get_mut(client) else {
            return;
        };
        slot.view_delay_q8 = win.view_delay_q8;
        // Slack of the newest input: how many ticks before it is needed it arrived.
        slot.inputs.observe_slack(win.newest_tick.since(next) as f32);
        for (t, payload) in win.iter() {
            if t < next {
                if !slot.inputs.has_consumed(t) {
                    // Arrived after its tick was simulated.
                    if t == win.newest_tick {
                        server.stats.inputs_late += 1;
                        slot.stats.input_late += 1;
                    }
                }
                continue;
            }
            if slot.inputs.contains(t) {
                continue;
            }
            match input.ops.decode(payload) {
                Ok((value, ok)) => {
                    if !ok {
                        strikes += 1;
                        server.stats.inputs_invalid += 1;
                    }
                    slot.inputs.insert(t, value);
                }
                Err(_) => {
                    strikes += 1;
                    server.stats.inputs_invalid += 1;
                    slot.inputs.insert(t, input.ops.neutral());
                }
            }
        }
    }
    for _ in 0..strikes {
        if strike(server, client, now, "invalid input") {
            return;
        }
    }
}

fn receive_message(
    server: &mut NetServer,
    registry: &ReplicationRegistry,
    res: &mut Resources,
    client: NetClientId,
    payload: &[u8],
    now: Duration,
) {
    let mut r = Reader::new(payload);
    let Ok(kind) = r.varu16() else {
        strike(server, client, now, "bad message header");
        return;
    };
    let Some(entry) = registry.message(kind) else {
        strike(server, client, now, "unknown message kind");
        return;
    };
    if !entry.client_may_send() {
        strike(server, client, now, "message not allowed from clients");
        return;
    }
    let body = &payload[r.position()..];
    if body.len() > entry.config.max_bytes as usize {
        strike(server, client, now, "message too large");
        return;
    }
    let tick = server.tick;
    let (player, allowed) = {
        let Some(slot) = server.slots.get_mut(client) else {
            return;
        };
        let mut allowed = slot.limits.messages.take(now, 1.0);
        if allowed && entry.config.rate_per_sec > 0 {
            let bucket = slot.limits.per_kind.entry(kind).or_insert_with(|| {
                TokenBucket::new(
                    f64::from(entry.config.rate_per_sec),
                    f64::from(entry.config.burst.max(1)),
                )
                .starting_at(now)
            });
            allowed = bucket.take(now, 1.0);
        }
        (slot.identity.player, allowed)
    };
    if !allowed {
        // Dropped; honest clients apply the same limits before sending, so persistent drops
        // count toward the `RateLimited` kick like packet overflow.
        server.stats.rate_limited_messages += 1;
        rate_limit_drop(server, client, now);
        return;
    }
    match entry.ops.deliver_from_client(res, body, client, player, tick) {
        Ok(()) => {}
        Err(MessageReject::Decode(e)) => {
            strike(server, client, now, &format!("message decode: {e}"));
        }
        Err(MessageReject::Invalid(why)) => {
            strike(server, client, now, why);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn overflow_kicks_after_three_windows_of_drops() {
        let mut w = OverflowWindows::starting_at(Duration::ZERO, 120.0);
        // A drop in each of three consecutive 1 s windows: not yet.
        for s in 0..3u64 {
            assert!(!w.record_drop(MS(s * 1000 + 500)));
        }
        // The fourth consecutive window with a drop: sustained for ≥ 3 s → kick.
        assert!(w.record_drop(MS(3_500)));
    }

    #[test]
    fn a_quiet_window_resets_the_streak() {
        let mut w = OverflowWindows::starting_at(Duration::ZERO, 120.0);
        assert!(!w.record_drop(MS(100)));
        assert!(!w.record_drop(MS(1_100)));
        // Nothing dropped in [2 s, 3 s).
        assert!(!w.record_drop(MS(3_100)));
        assert!(!w.record_drop(MS(4_100)));
        assert!(!w.record_drop(MS(5_100)));
        assert!(w.record_drop(MS(6_100)), "three full windows with drops again");
    }

    #[test]
    fn extreme_overflow_kicks_within_one_window() {
        let mut w = OverflowWindows::starting_at(Duration::ZERO, 120.0);
        let limit = w.hard_limit();
        assert_eq!(limit, 1_200, "ten seconds of the allowance");
        for _ in 0..limit {
            assert!(!w.record_drop(MS(10)));
        }
        assert!(w.record_drop(MS(10)));
    }

    #[test]
    fn packet_allowance_covers_the_tick_rate() {
        let limits = WireLimits::default();
        // 60 Hz: 90 inputs + 30 other = 120/s, burst 60.
        let mut l = LinkLimiter::new(&limits, 60.0, Duration::ZERO);
        assert!((0..60).all(|_| l.packets.take(Duration::ZERO, 1.0)));
        assert!(!l.packets.take(Duration::ZERO, 1.0));
        // 128 Hz rooms get at least 1.5 × the tick rate for inputs.
        let mut l = LinkLimiter::new(&limits, 128.0, Duration::ZERO);
        let passed = (1..=1000u64)
            .filter(|i| l.packets.take(MS(*i * 1000 / 128), 1.0))
            .count();
        assert_eq!(passed, 1000, "one packet per 128 Hz tick always passes");
    }

    #[test]
    fn token_bucket_refills() {
        let mut b = TokenBucket::new(10.0, 2.0);
        assert!(b.take(Duration::ZERO, 1.0));
        assert!(b.take(Duration::ZERO, 1.0));
        assert!(!b.take(Duration::ZERO, 1.0));
        assert!(b.take(Duration::from_millis(100), 1.0));
        assert!(!b.take(Duration::from_millis(100), 1.0));
        // Never exceeds the burst.
        assert!(b.take(Duration::from_secs(100), 2.0));
        assert!(!b.take(Duration::from_secs(100), 1.0));
    }
}
