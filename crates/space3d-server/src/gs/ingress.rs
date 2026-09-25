//! The ingress task (tokio): `WsServerTransport::next_event().await` → `Gatekeeper::handle`
//! (per-link packet budget: one client cannot fill the shared room inbox) →
//! `RoomManager::get_or_create` → `RoomSender::try_send`. On `InboxFull`: unreliable-only
//! packets are dropped (metric), anything else closes the link (`1008`; the client resumes, and
//! a resume refused as busy carries a retry delay it honours). Also runs `Gatekeeper::sweep`
//! and IP-ban enforcement every 250 ms and applies the drain state.
//!
//! [`dispatch`] is the sans-IO half, shared with `TestCluster` (which feeds memory-network
//! events synchronously).

use super::gatekeeper::{GateOutcome, Gatekeeper, reject_label};
use super::rooms::{RoomError, RoomManager};
use super::transport_ws::WsServerTransport;
use crate::clock::UnixClock;
use crate::metrics::GameServerMetrics;
use parking_lot::Mutex;
use space3d_replication::LinkId;
use space3d_replication::protocol::handshake::{HelloAuth, RejectCode};
use space3d_replication::protocol::{PacketKind, flags, peek};
use space3d_replication::server::RoomInbound;
use space3d_replication::server::handshake::reject_packet;
use space3d_replication::time::NetClockHandle;
use space3d_replication::transport::{CloseCode, Lane, LinkEvent, LinkTx};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Handles needed by the ingress task.
pub struct Ingress {
    /// Transport.
    pub transport: WsServerTransport,
    /// Gatekeeper (shared with the supervisor for drain / room close).
    pub gate: Arc<Mutex<Gatekeeper>>,
    /// Rooms.
    pub rooms: RoomManager,
    /// Drain flag (set by the supervisor's lifecycle handling).
    pub draining: Arc<AtomicBool>,
    /// Metrics.
    pub metrics: GameServerMetrics,
    /// Monotonic clock (handshake deadlines, rate limits).
    pub clock: NetClockHandle,
    /// Wall clock (token expiry, bans).
    pub unix: Arc<dyn UnixClock>,
}

/// Send a `Reject` and close the link.
pub fn refuse(tx: &Arc<dyn LinkTx>, code: RejectCode, message: &str, metrics: &GameServerMetrics) {
    let retry = matches!(
        code,
        RejectCode::RateLimited | RejectCode::ServerDraining | RejectCode::RoomFull
    )
    .then_some(1_000);
    let _ = tx.send(Lane::Reliable, reject_packet(code, message, retry));
    let close = if matches!(code, RejectCode::Malformed | RejectCode::RateLimited) {
        CloseCode::POLICY
    } else {
        CloseCode::NORMAL
    };
    tx.close(close, message);
    metrics.handshake(reject_label(code));
}

/// Route one gatekeeper outcome to the rooms (never blocks).
pub fn dispatch(gate: &mut Gatekeeper, rooms: &RoomManager, outcome: GateOutcome, metrics: &GameServerMetrics) {
    match outcome {
        GateOutcome::None => {}
        GateOutcome::Refuse { tx, code, message } => refuse(&tx, code, &message, metrics),
        GateOutcome::Admit { room, spec, inbound } => {
            let RoomInbound::Join(join) = &inbound else {
                return;
            };
            let (link, tx) = (join.link, join.tx.clone());
            let resume = matches!(join.hello.auth, HelloAuth::Resume(_));
            let handle = match &spec {
                Some(spec) => rooms.get_or_create(spec),
                None => rooms.get(room).ok_or(RoomError::Gone),
            };
            match handle {
                Ok(h) => {
                    if h.sender.try_send(inbound).is_ok() {
                        metrics.handshake(if resume { "resume_routed" } else { "admitted" });
                    } else {
                        gate.unroute(link);
                        refuse(&tx, RejectCode::RoomFull, "room busy", metrics);
                    }
                }
                Err(e) => {
                    gate.unroute(link);
                    let code = match e {
                        RoomError::Capacity => RejectCode::RoomFull,
                        RoomError::Draining => RejectCode::ServerDraining,
                        RoomError::InvalidSpec(_) => RejectCode::RoomNotFound,
                        RoomError::Gone if resume => RejectCode::ResumeExpired,
                        RoomError::Gone => RejectCode::RoomClosed,
                        RoomError::Build(_) => RejectCode::Internal,
                    };
                    refuse(&tx, code, &e.to_string(), metrics);
                }
            }
        }
        GateOutcome::Forward { room, inbound } => {
            let (link, droppable) = match &inbound {
                RoomInbound::Packet { link, bytes } => (
                    *link,
                    matches!(peek(bytes), Some((PacketKind::Data, f)) if f & flags::RELIABLE == 0),
                ),
                RoomInbound::Closed { link, .. } => (*link, false),
                RoomInbound::Join(j) => (j.link, false),
            };
            let closed = matches!(inbound, RoomInbound::Closed { .. });
            match rooms.get(room) {
                Some(h) => {
                    if h.sender.try_send(inbound).is_err() && !closed {
                        if droppable {
                            metrics.rate_limited("room_inbox");
                        } else if let Some(tx) = gate.link_tx(link) {
                            metrics.disconnect("room_inbox_full");
                            tx.close(CloseCode::POLICY, "room overloaded");
                        }
                    }
                }
                None => {
                    if !closed && let Some(tx) = gate.link_tx(link) {
                        tx.close(CloseCode::GOING_AWAY, "room closed");
                    }
                    gate.unroute(link);
                }
            }
        }
    }
}

/// Feed one link event through the gatekeeper and dispatch it.
pub fn handle_event(
    gate: &Mutex<Gatekeeper>,
    rooms: &RoomManager,
    event: LinkEvent,
    now: Duration,
    now_ms: i64,
    metrics: &GameServerMetrics,
) {
    let mut g = gate.lock();
    let dropped = g.dropped_packets();
    let outcome = g.handle(event, now, now_ms);
    if g.dropped_packets() > dropped {
        metrics.rate_limited("link_packets");
    }
    dispatch(&mut g, rooms, outcome, metrics);
}

/// Links closed by `sweep` are released when their `Closed` event arrives.
pub fn link_of(event: &LinkEvent) -> LinkId {
    match event {
        LinkEvent::Opened { link, .. } | LinkEvent::Packet { link, .. } | LinkEvent::Closed { link, .. } => *link,
    }
}

/// Spawn the ingress loop on `space3d_tasks::runtime()`; it ends when the transport closes.
pub fn spawn(ingress: Ingress) -> space3d_tasks::Task<()> {
    let (tx, rx) = async_channel::bounded::<()>(1);
    // Never closed: the loop runs for the life of the process.
    std::mem::forget(tx);
    spawn_until(ingress, rx)
}

/// Like [`spawn`], but also stops when every sender of `stop` is dropped.
pub fn spawn_until(ingress: Ingress, stop: async_channel::Receiver<()>) -> space3d_tasks::Task<()> {
    let span = tracing::Span::current();
    space3d_tasks::spawn(tracing::Instrument::instrument(
        async move {
            let Ingress {
                transport,
                gate,
                rooms,
                draining,
                metrics,
                clock,
                unix,
            } = ingress;
            let mut sweep = tokio::time::interval(Duration::from_millis(250));
            sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    ev = transport.next_event() => match ev {
                        Some(ev) => handle_event(&gate, &rooms, ev, clock.now(), unix.now_ms(), &metrics),
                        None => break,
                    },
                    _ = stop.recv() => break,
                    _ = sweep.tick() => {
                        let mut g = gate.lock();
                        g.set_draining(draining.load(Ordering::Acquire));
                        let closed = g.sweep(clock.now());
                        if closed > 0 {
                            metrics.handshake("timeout");
                        }
                        g.enforce_ip_bans(unix.now_ms());
                    }
                }
            }
            tracing::info!("ingress stopped");
        },
        span,
    ))
}
