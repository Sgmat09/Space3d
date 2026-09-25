//! Packet decoding and snapshot application (internal to the replication builder).
//!
//! * Entities: `SPAWN` → spawn (or reuse the entity already mapped to that `NetId` after a
//!   resume `FULL_RESET`), insert `NetEntity`, `Remote`, `Interpolated`/`Predicted`/`Controlled`
//!   markers per op bits and component modes; `HAS_REMOVED` → remove components; despawns →
//!   `RemoteDespawned` then despawn. `FULL_RESET` → despawn every mapped entity not listed.
//! * Per-entity `last_applied_tick`: a record older than or equal to it is skipped (safe under
//!   reordering thanks to the snapshot invariant).
//! * Component values with unresolved entity references are parked in [`PendingRefs`] and
//!   retried after each snapshot (dropped after 1 s, when the holder despawns, or when a newer
//!   value or a removal of the same component arrives).

use super::connection::{Connection, lost};
use super::{ConnectionEvent, ConnectionState, FailReason, Mispredicted, NetClient, RemoteDespawned};
use crate::mapping::DecodeCtx;
use crate::protocol::control::{ControlMsg, TimeMsg};
use crate::protocol::handshake::{Disconnect, HelloAuth, Reject, RejectCode, Welcome};
use crate::protocol::snapshot::{SnapshotView, flags as sflags, op};
use crate::protocol::{AckField, ChannelId, DataPacket, Packet, WireLimits, decode_packet};
use crate::registry::{ClientMode, ReplicationRegistry};
use crate::wire::{DecodeError, Reader};
use crate::{Controlled, Interpolated, NetEntity, NetId, Predicted, Remote, Simulated, Tick};
use space3d_core::{Entity, Events, Resources, SystemCtx, TickRate, World};
use std::time::Duration;

/// Parked component values waiting for referenced entities. At most one value per
/// `(holder, kind)`: a newer value (applied, parked or removed) replaces it, so a parked value
/// is always the newest the server sent for that component.
#[derive(Default)]
pub struct PendingRefs {
    /// `(holder, kind, bytes, first seen)`.
    pub entries: Vec<(NetId, u16, Vec<u8>, Tick)>,
}

impl PendingRefs {
    /// Drop the parked value of `(holder, kind)`, if any.
    pub fn forget(&mut self, holder: NetId, kind: u16) {
        if !self.entries.is_empty() {
            self.entries.retain(|(h, k, _, _)| !(*h == holder && *k == kind));
        }
    }
}

/// Client-side replication metadata of one entity.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EntityMeta {
    pub(crate) last_applied: Tick,
    pub(crate) controlled: bool,
    pub(crate) predicted: bool,
    pub(crate) interpolated: bool,
    /// Kinds the entity has (had) that are predicted / interpolated.
    pub(crate) pred_kinds: u64,
    pub(crate) interp_kinds: u64,
}

/// Limits for server → client packets (the server is trusted more, but decoders stay bounded).
pub(crate) fn client_limits() -> WireLimits {
    WireLimits {
        max_packet: 16 << 20,
        max_blocks: 4096,
        ..WireLimits::default()
    }
}

/// The `replication::client_receive` system (PreUpdate).
pub(crate) fn client_receive(ctx: &mut SystemCtx) {
    let now = ctx.resources.init::<crate::time::NetClockHandle>().now();
    let world = &mut *ctx.world;
    let resources = &mut *ctx.resources;
    if !resources.contains::<NetClient>() {
        return;
    }
    resources.init::<ReplicationRegistry>();
    resources.scope::<NetClient, _>(|res, net| {
        res.scope::<ReplicationRegistry, _>(|res, registry| {
            registry.freeze();
            if net.msg_table.is_empty() {
                for kind in 0..registry.message_names().len() {
                    if let Some(m) = registry.message(kind as u16) {
                        net.msg_table.insert(m.type_id, (m.kind, m.config));
                    }
                }
            }
            super::prediction::remove_visual_offsets(world);
            // Packets first: a `Disconnect` (or the last snapshots) may arrive together with the
            // close, and must be seen before the state machine notices the closed link.
            let mut received = |net: &mut NetClient| {
                while let Some(pkt) = net
                    .conn
                    .as_mut()
                    .and_then(|c| c.transport.as_mut())
                    .and_then(|t| t.recv())
                {
                    handle_packet(net, registry, world, res, now, &pkt);
                }
            };
            received(net);
            super::connection::drive(net, now, registry);
            // A link opened by `drive` may already hold a Welcome (zero-latency transports).
            received(net);
            flush_events(net, res);
        });
    });
}

pub(crate) fn flush_events(net: &mut NetClient, res: &mut Resources) {
    if !net.events.is_empty() {
        let ev = res.init::<Events<ConnectionEvent>>();
        for e in net.events.drain(..) {
            ev.send(e);
        }
    }
    if !net.pending_despawn_events.is_empty() {
        let ev = res.init::<Events<RemoteDespawned>>();
        for e in net.pending_despawn_events.drain(..) {
            ev.send(e);
        }
    }
}

/// `Last`: despawn entities whose `RemoteDespawned` was emitted this frame.
pub(crate) fn client_despawn(ctx: &mut SystemCtx) {
    let Some(net) = ctx.resources.get_mut::<NetClient>() else {
        return;
    };
    for e in net.despawn_queue.drain(..) {
        let _ = ctx.world.despawn(e);
    }
}

fn handle_packet(
    net: &mut NetClient,
    registry: &ReplicationRegistry,
    world: &mut World,
    res: &mut Resources,
    now: Duration,
    bytes: &[u8],
) {
    let Some(mut conn) = net.conn.take() else { return };
    conn.last_rx = now;
    net.stats.bytes_in += bytes.len() as u64;
    match decode_packet(bytes, &client_limits()) {
        Ok(Packet::Welcome(w)) => on_welcome(net, &mut conn, world, res, now, w),
        Ok(Packet::Reject(r)) => on_reject(net, &mut conn, now, r),
        Ok(Packet::Disconnect(d)) => on_disconnect(net, &mut conn, now, d),
        Ok(Packet::Data(d)) => {
            if matches!(net.state, ConnectionState::Syncing | ConnectionState::Connected) {
                on_data(net, &mut conn, registry, world, res, now, &d);
            }
        }
        Ok(Packet::Hello(_)) => {}
        Err(e) => tracing::warn!(%e, "undecodable packet from server"),
    }
    net.conn = Some(conn);
}

fn on_welcome(
    net: &mut NetClient,
    conn: &mut Connection,
    _world: &mut World,
    res: &mut Resources,
    now: Duration,
    w: Welcome,
) {
    if net.state != ConnectionState::Handshaking {
        return;
    }
    let same_session = conn
        .welcome
        .as_ref()
        .is_some_and(|old| old.room == w.room && old.room_epoch == w.room_epoch && old.client == w.client);
    if w.resumed && same_session {
        // Keep entities; everything not re-confirmed by the next full sync is dropped.
        net.reset_seen = Some(Default::default());
        net.prediction.stop();
        net.timed_messages.clear();
        net.recv_ack = None;
    } else {
        net.clear_session_state(true);
        if w.resumed {
            net.reset_seen = Some(Default::default());
        }
    }
    if w.echo_client_time_us > 0 {
        let rtt = now.saturating_sub(Duration::from_micros(w.echo_client_time_us));
        conn.rtt.sample(rtt);
    }
    if !same_session || conn.clock.tick_hz() != f64::from(w.tick_hz) {
        conn.clock = crate::time::ServerClock::new(f64::from(w.tick_hz.max(1)));
    } else {
        conn.clock.reset();
    }
    conn.pacer.reset();
    conn.resume_token = Some(w.resume.clone());
    conn.attempt = 0;
    conn.fell_back = false;
    conn.pings_sent = 0;
    conn.next_ping_at = now;
    conn.last_input_sent = None;
    conn.input_ack = None;
    net.prediction.nominal_hz = f64::from(w.tick_hz.max(1));
    if let Some(rate) = res.get_mut::<TickRate>() {
        rate.hz = net.prediction.nominal_hz;
    }
    net.events.push(ConnectionEvent::Connected {
        client: w.client,
        player: w.player,
        room: w.room,
        resumed: w.resumed,
    });
    net.state = ConnectionState::Syncing;
    conn.welcome = Some(w);
}

fn on_reject(net: &mut NetClient, conn: &mut Connection, now: Duration, r: Reject) {
    if r.code == RejectCode::ResumeExpired
        && conn.resuming
        && matches!(conn.auth, HelloAuth::Dev { .. })
        && !conn.fell_back
    {
        // Dev credentials are reusable: join fresh once.
        conn.fell_back = true;
        conn.resume_token = None;
        if let Some(mut t) = conn.transport.take() {
            t.close(crate::transport::CloseCode::NORMAL, "resume expired");
        }
        conn.hello_sent = false;
        conn.retry_at = Some(now);
        net.state = ConnectionState::Reconnecting { attempt: conn.attempt };
        return;
    }
    // Transient refusals (server busy, rate limited) that name a retry delay are retried with
    // credentials that can be presented again: a resume token within its window, or dev auth.
    let transient = r.retry_after_ms.is_some() && matches!(r.code, RejectCode::RoomFull | RejectCode::RateLimited);
    let reusable = conn.resuming || matches!(conn.auth, HelloAuth::Dev { .. });
    let policy = net.config.reconnect;
    if transient && reusable && policy.max_attempts.is_none_or(|m| conn.attempt < m) {
        if let Some(mut t) = conn.transport.take() {
            t.close(crate::transport::CloseCode::NORMAL, "rejected (retrying)");
        }
        conn.hello_sent = false;
        let wait = Duration::from_millis(u64::from(r.retry_after_ms.unwrap_or(1_000)));
        conn.retry_at = Some(now + wait.max(policy.backoff.delay(conn.attempt)));
        conn.attempt += 1;
        tracing::debug!(code = ?r.code, attempt = conn.attempt, "transient reject; retrying");
        if conn.welcome.is_some() {
            net.state = ConnectionState::Reconnecting { attempt: conn.attempt };
            net.events.push(ConnectionEvent::Reconnecting { attempt: conn.attempt });
        } else {
            net.state = ConnectionState::Connecting { attempt: conn.attempt };
            net.events.push(ConnectionEvent::Connecting);
        }
        return;
    }
    if let Some(mut t) = conn.transport.take() {
        t.close(crate::transport::CloseCode::NORMAL, "rejected");
    }
    net.events.push(ConnectionEvent::Rejected(r.clone()));
    net.state = ConnectionState::Failed(FailReason::Rejected(r));
    net.prediction.stop();
}

fn on_disconnect(net: &mut NetClient, conn: &mut Connection, now: Duration, d: Disconnect) {
    let resume = d.resume_allowed && net.config.resume && conn.resume_token.is_some() && conn.welcome.is_some();
    if resume {
        // Reconnect path: `lost` emits Disconnected{will_reconnect: true}.
        lost(net, conn, now, &format!("disconnected ({:?}): {}", d.code, d.message));
        if let Some(ConnectionEvent::Disconnected { code, .. }) = net
            .events
            .iter_mut()
            .rev()
            .find(|e| matches!(e, ConnectionEvent::Disconnected { .. }))
        {
            *code = Some(d.code);
        }
        return;
    }
    if let Some(mut t) = conn.transport.take() {
        t.close(crate::transport::CloseCode::NORMAL, "server disconnected");
    }
    net.events.push(ConnectionEvent::Disconnected {
        code: Some(d.code),
        reason: d.message.clone(),
        will_reconnect: false,
    });
    net.state = ConnectionState::Failed(FailReason::Disconnected(d.code, d.message));
    net.prediction.stop();
}

/// Update the datagram ack window with a received `seq`.
fn record_seq(ack: &mut Option<AckField>, seq: u16) {
    match ack {
        None => *ack = Some(AckField { ack: seq, bits: 0 }),
        Some(a) => {
            let d = seq.wrapping_sub(a.ack) as i16;
            if d > 0 {
                let d = d as u32;
                a.bits = if d > 32 {
                    0
                } else {
                    (a.bits.checked_shl(d).unwrap_or(0)) | (1 << (d - 1))
                };
                a.ack = seq;
            } else if d < 0 && (-d as u32) <= 32 {
                a.bits |= 1 << ((-d) as u32 - 1);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn on_data(
    net: &mut NetClient,
    conn: &mut Connection,
    registry: &ReplicationRegistry,
    world: &mut World,
    res: &mut Resources,
    now: Duration,
    data: &DataPacket<'_>,
) {
    let tick = data.header.tick;
    if let Some(seq) = data.header.seq {
        record_seq(&mut net.recv_ack, seq);
        conn.ack_dirty = true;
    }
    for block in &data.blocks {
        match block.channel {
            ChannelId::Time => {
                if let Ok(TimeMsg::Pong(p)) = crate::wire::postcard_exact::<TimeMsg>(block.payload) {
                    let sent = Duration::from_micros(p.client_time_us);
                    let rtt = now
                        .saturating_sub(sent)
                        .saturating_sub(Duration::from_micros(u64::from(p.hold_us)));
                    conn.rtt.sample(rtt);
                    conn.clock.on_pong(&p, now);
                    net.stats.rtt = conn.rtt.srtt();
                    net.stats.jitter = conn.rtt.rttvar();
                }
            }
            ChannelId::Control => match crate::wire::postcard_exact::<ControlMsg>(block.payload) {
                Ok(ControlMsg::Notice(n)) => net.events.push(ConnectionEvent::Notice(n)),
                Ok(_) => {}
                Err(e) => tracing::warn!(%e, "bad control block from server"),
            },
            ChannelId::Snapshot => match SnapshotView::decode(block.payload) {
                Ok(view) => apply_snapshot(net, conn, registry, world, tick, &view),
                Err(e) => tracing::warn!(%e, "bad snapshot from server"),
            },
            ChannelId::MsgReliable | ChannelId::MsgUnreliable => {
                let mut r = Reader::new(block.payload);
                let Ok(kind) = r.varu16() else { continue };
                let body = &block.payload[r.position()..];
                let Some(entry) = registry.message(kind) else {
                    tracing::warn!(kind, "unknown message kind from server");
                    continue;
                };
                if entry.config.timing == crate::EventTiming::Interpolated {
                    net.timed_messages.push((tick, kind, body.to_vec()));
                } else if let Err(e) = entry.ops.deliver_from_server(res, body, tick) {
                    tracing::warn!(%e, message = entry.name, "bad message from server");
                }
            }
            ChannelId::Input => {}
        }
    }
}

fn set_marker<T: space3d_core::hecs::Component + Default>(world: &mut World, e: Entity, on: bool, was: bool) {
    if on && !was {
        let _ = world.insert_one(e, T::default());
    } else if !on && was {
        let _ = world.remove_one::<T>(e);
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_snapshot(
    net: &mut NetClient,
    conn: &mut Connection,
    registry: &ReplicationRegistry,
    world: &mut World,
    tick: Tick,
    view: &SnapshotView<'_>,
) {
    let header = view.header;
    if header.input_ack_delta != u32::MAX {
        let acked = Tick(tick.0.saturating_sub(header.input_ack_delta));
        if conn.input_ack.is_none_or(|a| acked > a) {
            conn.input_ack = Some(acked);
        }
    }
    if tick >= net.last_snapshot_tick {
        net.pending_slack = Some(header.input_slack);
    }
    if header.flags & sflags::FULL_RESET != 0 && net.reset_seen.is_none() && tick > net.last_snapshot_tick {
        net.reset_seen = Some(Default::default());
    }
    let predicted_mask = registry.predicted_mask();
    let interpolated_mask = registry.interpolated_mask();
    let prediction_on = net.config.prediction.enabled;
    let prediction_running = net.prediction.next_tick.is_some();
    let send_interval = conn.welcome.as_ref().map_or(1, |w| w.send_interval);
    for rec in &view.entities {
        let existing = net.map.entity(rec.net_id);
        let entity = match existing {
            Some(e) => {
                if net.metas.get(&rec.net_id).is_some_and(|m| m.last_applied >= tick) {
                    continue; // stale
                }
                e
            }
            None => {
                if rec.op & op::SPAWN == 0 {
                    continue; // delta for an entity we never saw (reordered datagram)
                }
                if net.tombstones.get(&rec.net_id).is_some_and(|t| *t >= tick) {
                    continue; // spawn older than a despawn we already applied (reordered datagram)
                }
                net.tombstones.remove(&rec.net_id);
                let e = world.spawn((NetEntity(rec.net_id), Remote));
                net.map.insert(rec.net_id, e);
                net.metas.insert(rec.net_id, EntityMeta::default());
                e
            }
        };
        if let Some(seen) = net.reset_seen.as_mut() {
            seen.insert(rec.net_id, ());
        }
        let mut meta = net.metas.get(&rec.net_id).copied().unwrap_or_default();
        meta.last_applied = tick;
        // A spawn carries the full state: drop registered components it does not list.
        if rec.op & op::SPAWN != 0 && existing.is_some() {
            for entry in registry.components() {
                if rec.present & entry.bit() == 0 && entry.ops.contains(world, entity) {
                    entry.ops.remove(world, entity);
                }
            }
            // Parked values of components this full state does not list are gone too.
            let present = rec.present;
            net.pending_refs
                .entries
                .retain(|(h, k, _, _)| *h != rec.net_id || present & (1u64 << *k) != 0);
        }
        let mut removed = rec.removed;
        while removed != 0 {
            let kind = removed.trailing_zeros() as u16;
            removed &= removed - 1;
            if let Some(entry) = registry.component(kind) {
                entry.ops.remove(world, entity);
            }
            net.prediction.forget_kind(rec.net_id, kind);
            // A parked (older) value must not re-insert a removed component.
            net.pending_refs.forget(rec.net_id, kind);
        }
        // Markers.
        let controlled = rec.op & op::CONTROLLED != 0;
        meta.pred_kinds = (meta.pred_kinds | rec.present) & predicted_mask & !rec.removed;
        meta.interp_kinds = (meta.interp_kinds | rec.present) & interpolated_mask & !rec.removed;
        let predicted = prediction_on && controlled && meta.pred_kinds != 0;
        let interpolated = !predicted && meta.interp_kinds != 0;
        set_marker::<Controlled>(world, entity, controlled, meta.controlled);
        if predicted != meta.predicted {
            set_marker::<Predicted>(world, entity, predicted, meta.predicted);
            set_marker::<Simulated>(world, entity, predicted, meta.predicted);
            if predicted {
                for entry in registry.components().iter().filter(|c| c.mode.is_interpolated()) {
                    entry.ops.clear_buffers(world, entity);
                }
            }
        }
        set_marker::<Interpolated>(world, entity, interpolated, meta.interpolated);
        meta.controlled = controlled;
        meta.predicted = predicted;
        meta.interpolated = interpolated;
        net.metas.insert(rec.net_id, meta);
        // Components.
        for (kind, bytes) in &rec.components {
            let Some(entry) = registry.component(*kind) else {
                tracing::warn!(kind, "unknown component kind from server");
                continue;
            };
            let mut ctx = DecodeCtx::new(&net.map);
            let result = if predicted && entry.mode.is_predicted() {
                net.prediction.confirm(rec.net_id, tick, *kind, bytes);
                let has = entry.ops.contains(world, entity);
                if !prediction_running || !has {
                    let r = entry.ops.apply(world, entity, bytes, &mut ctx);
                    if has || prediction_running {
                        net.prediction.request_rollback(tick);
                    }
                    r
                } else if !net.prediction.compares(tick) {
                    // History before a snap is stale; the confirmed value is stored above.
                    entry.ops.check(bytes, &mut ctx)
                } else {
                    // Compare with what we predicted for that tick.
                    let differs = match super::prediction::history_bytes(world, entity, tick, *kind) {
                        Some(h) => entry.ops.differs(&h, bytes, &mut DecodeCtx::new(&net.map)),
                        None => true,
                    };
                    if differs {
                        net.prediction.request_rollback(tick);
                    }
                    entry.ops.check(bytes, &mut ctx)
                }
            } else {
                match entry.mode {
                    ClientMode::Apply | ClientMode::Predict => entry.ops.apply(world, entity, bytes, &mut ctx),
                    ClientMode::Interpolate | ClientMode::PredictAndInterpolate => entry.ops.push_sample(
                        world,
                        entity,
                        tick,
                        bytes,
                        &mut ctx,
                        send_interval,
                        rec.op & op::DEFERRED != 0,
                    ),
                    ClientMode::Delayed => entry.ops.push_delayed(world, entity, tick, bytes, &mut ctx),
                }
            };
            match result {
                // A newer value supersedes any parked one (it must never be re-applied later).
                Ok(()) => net.pending_refs.forget(rec.net_id, *kind),
                Err(DecodeError::Unresolved) => {
                    net.pending_refs.forget(rec.net_id, *kind);
                    net.pending_refs.entries.push((rec.net_id, *kind, bytes.to_vec(), tick));
                }
                Err(e) => tracing::warn!(%e, component = entry.name, "bad component from server"),
            }
        }
    }
    for d in &view.despawns {
        let Some(e) = net.map.entity(d.net_id) else {
            // Despawn overtook the spawn (reordered datagrams): remember it, so the late spawn
            // does not create an entity nobody will ever despawn.
            let t = net.tombstones.entry(d.net_id).or_insert(tick);
            *t = (*t).max(tick);
            continue;
        };
        if net.metas.get(&d.net_id).is_some_and(|m| m.last_applied > tick) {
            continue; // respawned after this (reordered) packet
        }
        despawn_remote(net, d.net_id, e, d.out_of_scope, tick);
    }
    if tick > net.last_snapshot_tick {
        net.last_snapshot_tick = tick;
        // Reordering spans well under a second; keep tombstones for TOMBSTONE_SECS.
        let keep = (net.prediction.nominal_hz.max(1.0) * TOMBSTONE_SECS) as u32;
        if !net.tombstones.is_empty() {
            net.tombstones.retain(|_, t| tick.0.saturating_sub(t.0) <= keep);
        }
    }
    if header.flags & sflags::SYNC_COMPLETE != 0 {
        if let Some(seen) = net.reset_seen.take() {
            for id in net.map.net_ids() {
                if !seen.contains_key(&id)
                    && let Some(e) = net.map.entity(id)
                {
                    despawn_remote(net, id, e, false, tick);
                }
            }
        }
        if net.state == ConnectionState::Syncing {
            net.state = ConnectionState::Connected;
            net.events.push(ConnectionEvent::Synced);
        }
    }
    retry_pending(net, registry, world, tick);
}

/// How long a despawned `NetId` is remembered (reordered / duplicated spawn records older than
/// the despawn are ignored meanwhile).
const TOMBSTONE_SECS: f64 = 30.0;

fn despawn_remote(net: &mut NetClient, id: NetId, e: Entity, out_of_scope: bool, tick: Tick) {
    net.tombstones.insert(id, tick);
    net.map.remove(id);
    net.metas.remove(&id);
    net.prediction.forget(id);
    net.pending_refs.entries.retain(|(h, _, _, _)| *h != id);
    net.despawn_queue.push(e);
    net.pending_despawn_events.push(RemoteDespawned {
        net_id: id,
        entity: e,
        out_of_scope,
    });
}

fn retry_pending(net: &mut NetClient, registry: &ReplicationRegistry, world: &mut World, tick: Tick) {
    if net.pending_refs.entries.is_empty() {
        return;
    }
    let hz = net.prediction.nominal_hz.max(1.0) as u32;
    let entries = std::mem::take(&mut net.pending_refs.entries);
    for (holder, kind, bytes, first) in entries {
        let Some(e) = net.map.entity(holder) else { continue };
        if tick.0.saturating_sub(first.0) > hz {
            tracing::warn!(?holder, kind, "dropping value with an unresolved entity reference");
            continue;
        }
        let Some(entry) = registry.component(kind) else {
            continue;
        };
        let mut ctx = DecodeCtx::new(&net.map);
        match entry.ops.apply(world, e, &bytes, &mut ctx) {
            Err(DecodeError::Unresolved) => net.pending_refs.entries.push((holder, kind, bytes, first)),
            Err(err) => tracing::warn!(%err, "bad pending component"),
            Ok(()) => {}
        }
    }
}

/// Emit a `Mispredicted` event.
pub(crate) fn emit_mispredicted(res: &mut Resources, ev: Mispredicted) {
    res.init::<Events<Mispredicted>>().send(ev);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientConfig;
    use crate::transport::{ClientTransport, Connector, TransportError};
    use std::sync::Arc;

    struct NoConnect;

    impl Connector for NoConnect {
        fn connect(&self, _url: &str) -> Result<Box<dyn ClientTransport>, TransportError> {
            Err(TransportError::Connect("offline".into()))
        }
    }

    fn handshaking(auth: HelloAuth, resuming: bool) -> (NetClient, Connection) {
        let mut net = NetClient::new(ClientConfig::default());
        let mut conn = Connection::new(Arc::new(NoConnect), "mem://gs".into(), auth, &net.config);
        conn.resuming = resuming;
        if resuming {
            conn.resume_token = Some(vec![1, 2, 3]);
        }
        net.state = ConnectionState::Handshaking;
        (net, conn)
    }

    fn busy() -> Reject {
        Reject {
            code: RejectCode::RoomFull,
            message: "room busy".into(),
            retry_after_ms: Some(1_000),
        }
    }

    /// A resume refused as busy (with a retry delay) is retried, not treated as terminal.
    #[test]
    fn transient_reject_of_a_resume_retries_after_the_delay() {
        let now = Duration::from_secs(10);
        let (mut net, mut conn) = handshaking(HelloAuth::Ticket("t".into()), true);
        on_reject(&mut net, &mut conn, now, busy());
        assert!(
            matches!(
                net.state,
                ConnectionState::Connecting { .. } | ConnectionState::Reconnecting { .. }
            ),
            "{:?}",
            net.state
        );
        assert!(conn.retry_at.is_some_and(|t| t >= now + Duration::from_secs(1)));
        // Not transient: fails.
        let (mut net, mut conn) = handshaking(HelloAuth::Ticket("t".into()), true);
        on_reject(&mut net, &mut conn, now, Reject::new(RejectCode::BadTicket, "no"));
        assert!(matches!(net.state, ConnectionState::Failed(_)));
        // A single-use ticket (not a resume) cannot be presented again: fails.
        let (mut net, mut conn) = handshaking(HelloAuth::Ticket("t".into()), false);
        on_reject(&mut net, &mut conn, now, busy());
        assert!(matches!(net.state, ConnectionState::Failed(_)));
        // Dev credentials are reusable.
        let (mut net, mut conn) = handshaking(HelloAuth::Dev { name: "a".into() }, false);
        on_reject(&mut net, &mut conn, now, busy());
        assert!(!matches!(net.state, ConnectionState::Failed(_)));
    }

    #[test]
    fn ack_window() {
        let mut a = None;
        record_seq(&mut a, 10);
        record_seq(&mut a, 12);
        let f = a.unwrap();
        assert_eq!(f.ack, 12);
        assert!(f.acks(12) && f.acks(10) && !f.acks(11));
        record_seq(&mut a, 11);
        assert!(a.unwrap().acks(11));
        record_seq(&mut a, 60);
        assert_eq!(a.unwrap().bits, 0);
        // Wrap-around.
        let mut b = Some(AckField { ack: u16::MAX, bits: 0 });
        record_seq(&mut b, 1);
        assert!(b.unwrap().acks(u16::MAX) && b.unwrap().acks(1));
    }
}
