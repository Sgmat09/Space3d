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
    /// Sustained rate-limit overflow since.
    pub overflow_since: Option<Duration>,
}

impl LinkLimiter {
    /// Limiter for `limits`, starting at `now`.
    pub fn new(limits: &WireLimits, now: Duration) -> Self {
        let pps = f64::from(limits.input_packets_per_sec.max(1));
        Self {
            // Inputs plus some headroom for pings / control / messages.
            packets: TokenBucket::new(pps + 30.0, 60.0).starting_at(now),
            bytes: TokenBucket::new(f64::from(limits.max_packet) * 16.0, f64::from(limits.max_packet) * 8.0)
                .starting_at(now),
            messages: TokenBucket::new(
                f64::from(limits.messages_per_sec.max(1)),
                f64::from(limits.messages_per_sec.max(1)) * 2.0,
            )
            .starting_at(now),
            strikes: Vec::new(),
            per_kind: Default::default(),
            overflow_since: None,
        }
    }
}

/// Sustained rate-limit overflow before a `RateLimited` kick.
const OVERFLOW_KICK_AFTER: Duration = Duration::from_secs(3);

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
    slot.limits = LinkLimiter::new(&limits, now);
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
    for c in idle_kick {
        server.drop_link(c, Some((DisconnectCode::Timeout, "idle timeout")), true);
        lost.push(c);
    }
    for c in lost {
        link_lost(server, c);
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
            let since = *slot.limits.overflow_since.get_or_insert(now);
            if now.saturating_sub(since) >= OVERFLOW_KICK_AFTER {
                let player = slot.identity.player;
                server.kick(player, DisconnectCode::RateLimited, "rate limited", false);
            }
            return;
        }
        slot.limits.overflow_since = None;
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
        ControlMsg::Leave => server.free_slot(client, LeaveReason::Left),
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
        return; // dropped (rate limit)
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
