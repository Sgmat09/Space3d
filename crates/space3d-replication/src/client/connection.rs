//! Connection state machine (internal to the replication builder):
//!
//! ```text
//! Idle ─connect─▶ Connecting ─open─▶ Handshaking ─Welcome─▶ Syncing ─SYNC_COMPLETE─▶ Connected
//!                     ▲                   │ Reject → Failed(Rejected)                  │ link lost / idle timeout
//!                     └──── Reconnecting{attempt} (backoff; HelloAuth::Resume if we hold a token) ◀┘
//! Disconnect{resume_allowed: false} → Failed(Disconnected)
//! ```
//!
//! Every attempt opens a **fresh** transport via the `Connector` (no buffered stale inputs).
//! A resume that fails with `ResumeExpired` falls back to the original ticket once (tickets
//! are single-use, so this only succeeds for `Dev` targets) and otherwise fails, letting the
//! game ask the player service for a new assignment (`/v1/sessions/rejoin`).

use super::{ClientConfig, ConnectionEvent, ConnectionState, FailReason, NetClient};
use crate::protocol::control::{ClientReport, ControlMsg, Ping, TimeMsg};
use crate::protocol::handshake::{ClientCaps, Hello, HelloAuth, Welcome};
use crate::protocol::input::InputWindow;
use crate::protocol::{ChannelId, DataWriter, PacketKind, block_overhead, encode_handshake};
use crate::time::{RttEstimator, ServerClock, TickPacer, as_micros};
use crate::transport::{ClientTransport, CloseCode, Connector, Lane, LinkState};
use crate::wire::Writer;
use crate::{ReplicationRegistry, Tick};
use space3d_core::SystemCtx;
use std::sync::Arc;
use std::time::Duration;

/// Initial ping burst after `Welcome` (count, interval).
const PING_BURST: (u32, Duration) = (8, Duration::from_millis(100));
/// Client statistics report period.
const REPORT_EVERY: Duration = Duration::from_secs(5);
/// The last resume attempt starts this long before the resume window ends.
const RESUME_DEADLINE_MARGIN: Duration = Duration::from_millis(100);

/// Live connection data.
pub struct Connection {
    /// Transport of the current attempt.
    pub transport: Option<Box<dyn ClientTransport>>,
    /// Original credentials.
    pub auth: HelloAuth,
    /// URL.
    pub url: String,
    /// Last Welcome.
    pub welcome: Option<Welcome>,
    /// RTT.
    pub rtt: RttEstimator,
    /// Server tick estimate.
    pub clock: ServerClock,
    /// Pacing.
    pub pacer: TickPacer,
    /// Next reconnect attempt time.
    pub retry_at: Option<Duration>,
    /// Last packet received.
    pub last_rx: Duration,
    pub(crate) connector: Arc<dyn Connector>,
    pub(crate) attempt: u32,
    pub(crate) hello_sent: bool,
    /// Connect / handshake deadline of the current attempt.
    pub(crate) deadline: Duration,
    pub(crate) resume_token: Option<Vec<u8>>,
    /// The current attempt uses `HelloAuth::Resume`.
    pub(crate) resuming: bool,
    /// The one-time Dev fallback after `ResumeExpired` was used.
    pub(crate) fell_back: bool,
    /// When the session's link was lost (cleared by the next `Welcome`): resume attempts stop
    /// once the server's resume window (`Welcome::resume_window_s`) has passed since.
    pub(crate) lost_at: Option<Duration>,
    /// Clock time of the current frame.
    pub(crate) now: Duration,
    pub(crate) next_ping_at: Duration,
    pub(crate) pings_sent: u32,
    pub(crate) ping_id: u16,
    /// Newest input tick handed to the transport.
    pub(crate) last_input_sent: Option<Tick>,
    /// Newest input tick the server consumed (from `input_ack_delta`).
    pub(crate) input_ack: Option<Tick>,
    /// A new datagram seq arrived since the last ack we sent.
    pub(crate) ack_dirty: bool,
}

impl Connection {
    pub(crate) fn new(connector: Arc<dyn Connector>, url: String, auth: HelloAuth, config: &ClientConfig) -> Self {
        Self {
            transport: None,
            auth,
            url,
            welcome: None,
            rtt: RttEstimator::default(),
            clock: ServerClock::new(60.0),
            pacer: TickPacer::new(config.pacer),
            retry_at: None,
            last_rx: Duration::ZERO,
            connector,
            attempt: 0,
            hello_sent: false,
            deadline: Duration::MAX,
            resume_token: None,
            resuming: false,
            fell_back: false,
            lost_at: None,
            now: Duration::ZERO,
            next_ping_at: Duration::ZERO,
            pings_sent: 0,
            ping_id: 0,
            last_input_sent: None,
            input_ack: None,
            ack_dirty: false,
        }
    }
}

/// Advance the state machine (called at the start of `client_receive`).
pub(crate) fn drive(net: &mut NetClient, now: Duration, registry: &ReplicationRegistry) {
    let Some(mut conn) = net.conn.take() else { return };
    conn.now = now;
    if matches!(net.state, ConnectionState::Idle | ConnectionState::Failed(_)) {
        net.conn = Some(conn);
        return;
    }
    if conn.transport.is_none() && conn.retry_at.is_none_or(|t| now >= t) {
        match conn.connector.connect(&conn.url) {
            Ok(t) => {
                conn.transport = Some(t);
                conn.hello_sent = false;
                conn.retry_at = None;
                conn.deadline = now + net.config.handshake_timeout;
            }
            Err(e) => lost(net, &mut conn, now, &e.to_string()),
        }
    }
    let link_state = conn.transport.as_ref().map(|t| t.state());
    match link_state {
        Some(LinkState::Open) if !conn.hello_sent => send_hello(net, &mut conn, now, registry),
        Some(LinkState::Closed(reason)) => lost(net, &mut conn, now, &reason),
        Some(LinkState::Connecting) if now > conn.deadline => lost(net, &mut conn, now, "connect timeout"),
        _ => {}
    }
    match net.state {
        ConnectionState::Handshaking if now > conn.deadline => lost(net, &mut conn, now, "handshake timeout"),
        ConnectionState::Syncing | ConnectionState::Connected
            if now.saturating_sub(conn.last_rx) > net.config.idle_timeout =>
        {
            lost(net, &mut conn, now, "idle timeout")
        }
        _ => {}
    }
    net.conn = Some(conn);
}

fn send_hello(net: &mut NetClient, conn: &mut Connection, now: Duration, registry: &ReplicationRegistry) {
    let resume = net.config.resume && conn.welcome.is_some() && !conn.fell_back;
    let auth = match (&conn.resume_token, resume) {
        (Some(token), true) => {
            conn.resuming = true;
            HelloAuth::Resume(token.clone())
        }
        _ => {
            conn.resuming = false;
            conn.auth.clone()
        }
    };
    let mut hello = Hello::new(
        &net.config.game_id,
        &net.config.game_version,
        registry.schema_hash(),
        auth,
        ClientCaps {
            max_down_bytes_per_sec: net.config.max_down_bytes_per_sec,
            platform: net.config.platform,
        },
    );
    hello.client_time_us = as_micros(now).max(1);
    let pkt = encode_handshake(PacketKind::Hello, &hello);
    if let Some(t) = conn.transport.as_mut() {
        net.stats.bytes_out += pkt.len() as u64;
        if t.send(Lane::Reliable, pkt).is_err() {
            return lost(net, conn, now, "send failed");
        }
    }
    conn.hello_sent = true;
    conn.deadline = now + net.config.handshake_timeout;
    net.state = ConnectionState::Handshaking;
}

impl Connection {
    /// End of the resume window of the session lost at `lost_at` (the server keeps the slot
    /// that long after the link went; later resumes cannot succeed). `None` without a session.
    pub(crate) fn resume_deadline(&self) -> Option<Duration> {
        let window = Duration::from_secs(u64::from(self.welcome.as_ref()?.resume_window_s));
        Some(self.lost_at? + window)
    }

    /// Whether a resume can still start at `at` and find the slot (see
    /// [`Self::resume_deadline`]; the last one starts [`RESUME_DEADLINE_MARGIN`] before it).
    pub(crate) fn resume_open(&self, at: Duration) -> bool {
        self.resume_deadline()
            .is_none_or(|end| at + RESUME_DEADLINE_MARGIN < end)
    }
}

/// The link of the current attempt is gone: reconnect (with resume when possible) or give up.
///
/// Resuming stops with the server's resume window (`Welcome::resume_window_s`, counted from
/// the loss): the last attempt is made just before it ends, and then the session fails
/// (`FailReason::Unreachable`) — a slot kept no longer cannot be resumed, and a game server
/// that was lost with it (its resume state died with the process) is never coming back, so
/// the game should ask the player service where the room is now (rejoin) instead of retrying
/// for the whole reconnect schedule.
pub(crate) fn lost(net: &mut NetClient, conn: &mut Connection, now: Duration, reason: &str) {
    if let Some(mut t) = conn.transport.take() {
        t.close(CloseCode::NORMAL, reason);
    }
    conn.hello_sent = false;
    let in_session = matches!(net.state, ConnectionState::Syncing | ConnectionState::Connected);
    if in_session {
        conn.lost_at = Some(now);
    }
    let policy = net.config.reconnect;
    let allowed = policy.max_attempts.is_none_or(|m| conn.attempt < m);
    let can_resume = net.config.resume
        && conn.resume_token.is_some()
        && conn.welcome.is_some()
        && !conn.fell_back
        && conn.resume_open(now);
    let can_reauth = conn.welcome.is_none() || matches!(conn.auth, HelloAuth::Dev { .. });
    if allowed && (can_resume || can_reauth) {
        let mut delay = if in_session {
            Duration::ZERO
        } else {
            policy.backoff.delay(conn.attempt)
        };
        if !can_reauth && let Some(end) = conn.resume_deadline() {
            // The last resume goes just before the window closes, not after it.
            delay = delay.min(end.saturating_sub(now + RESUME_DEADLINE_MARGIN));
        }
        conn.retry_at = Some(now + delay);
        conn.attempt += 1;
        if in_session {
            net.events.push(ConnectionEvent::Disconnected {
                code: None,
                reason: reason.to_owned(),
                will_reconnect: true,
            });
        }
        if conn.welcome.is_some() {
            net.state = ConnectionState::Reconnecting { attempt: conn.attempt };
            net.events.push(ConnectionEvent::Reconnecting { attempt: conn.attempt });
        } else {
            net.state = ConnectionState::Connecting { attempt: conn.attempt };
            net.events.push(ConnectionEvent::Connecting);
        }
        tracing::debug!(reason, attempt = conn.attempt, "connection lost; retrying");
    } else {
        let reason = if allowed && conn.welcome.is_some() && !conn.resume_open(now) {
            format!("{reason} (resume window over)")
        } else {
            reason.to_owned()
        };
        tracing::info!(reason, "connection lost; giving up");
        net.state = ConnectionState::Failed(FailReason::Unreachable(reason.clone()));
        net.events.push(ConnectionEvent::Disconnected {
            code: None,
            reason,
            will_reconnect: false,
        });
    }
    net.prediction.stop();
}

/// The `replication::client_send` system (PostUpdate): one packet per frame.
pub(crate) fn client_send(ctx: &mut SystemCtx) {
    let now = ctx.resources.init::<crate::time::NetClockHandle>().now();
    let fps = {
        let t = ctx.resources.get::<space3d_core::Time>();
        t.map_or(0.0, |t| if t.delta.is_zero() { 0.0 } else { 1.0 / t.delta_secs() })
    };
    let Some(net) = ctx.resources.get_mut::<NetClient>() else {
        return;
    };
    if !matches!(net.state, ConnectionState::Syncing | ConnectionState::Connected) {
        return;
    }
    let Some(mut conn) = net.conn.take() else { return };
    send_frame(net, &mut conn, now, fps);
    net.conn = Some(conn);
    let NetClient { rates, stats, .. } = net;
    rates.update(stats, now);
}

fn send_frame(net: &mut NetClient, conn: &mut Connection, now: Duration, fps: f32) {
    let Some(transport) = conn.transport.as_mut() else {
        return;
    };
    let caps = transport.caps();
    let datagram = caps.true_datagrams;
    // Periodic report.
    if now >= net.report_at {
        if !net.report_at.is_zero() {
            let report = ClientReport {
                fps,
                interp_delay_ms: (net.interp.delay_ticks() * 1000.0 / conn.clock.tick_hz().max(1.0)) as f32,
                rollbacks: (net.stats.rollbacks - net.report_rollbacks) as u32,
                resim_ticks: (net.stats.resim_ticks - net.report_resims) as u32,
                rtt_ms: net.stats.rtt.as_secs_f32() * 1000.0,
            };
            if let Ok(b) = postcard::to_stdvec(&ControlMsg::Report(report)) {
                net.out_reliable.push((ChannelId::Control, b));
            }
            net.report_rollbacks = net.stats.rollbacks;
            net.report_resims = net.stats.resim_ticks;
        }
        net.report_at = now + REPORT_EVERY;
    }
    let mut unreliable: Vec<(ChannelId, Vec<u8>)> = Vec::new();
    // Pings.
    if now >= conn.next_ping_at {
        conn.ping_id = conn.ping_id.wrapping_add(1);
        let ping = TimeMsg::Ping(Ping {
            id: conn.ping_id,
            t_us: as_micros(now).max(1),
        });
        if let Ok(b) = postcard::to_stdvec(&ping) {
            unreliable.push((ChannelId::Time, b));
        }
        conn.pings_sent += 1;
        conn.next_ping_at = now
            + if conn.pings_sent < PING_BURST.0 {
                PING_BURST.1
            } else {
                net.config.ping_interval
            };
    }
    // Inputs.
    let mut input_block: Option<Vec<u8>> = None;
    let mut newest_input: Option<Tick> = None;
    if let Some((newest, _)) = net.prediction.inputs.newest()
        && conn.last_input_sent.is_none_or(|s| newest > s)
    {
        let floor = if datagram {
            let redundancy = u32::from(net.config.input_redundancy.max(1));
            let by_ack = conn.input_ack.map_or(0, |a| a.0 + 1);
            by_ack.max(newest.0 + 1 - redundancy.min(newest.0 + 1))
        } else {
            conn.last_input_sent.map_or(0, |s| s.0 + 1)
        };
        let inputs: Vec<(Tick, &[u8])> = net
            .prediction
            .inputs
            .entries
            .iter()
            .rev()
            .take_while(|(t, _, _)| t.0 >= floor)
            .map(|(t, b, _)| (*t, b.as_slice()))
            .collect();
        // Consecutive only (a resync may leave gaps).
        let mut n = 1;
        while n < inputs.len() && inputs[n - 1].0.0 == inputs[n].0.0 + 1 && n < 255 {
            n += 1;
        }
        if !inputs.is_empty() {
            let render = net.interp.render_tick().unwrap_or(f64::from(newest.0));
            let view_delay_q8 = ((f64::from(newest.0) - render).max(0.0) * 256.0).min(f64::from(u32::MAX)) as u32;
            let mut w = Writer::new();
            InputWindow::encode(&mut w, &inputs[..n], view_delay_q8);
            input_block = Some(w.into_vec());
            newest_input = Some(newest);
        }
    }
    let reliable: Vec<(ChannelId, Vec<u8>)> = std::mem::take(&mut net.out_reliable);
    unreliable.append(&mut net.out_unreliable);
    let ack = if datagram { net.recv_ack } else { None };
    let header_tick = net.prediction.next_tick.map_or(Tick::ZERO, |t| t.saturating_sub(1));
    let has_payload = !reliable.is_empty() || !unreliable.is_empty() || input_block.is_some();
    if !has_payload && !(datagram && conn.ack_dirty) {
        return;
    }
    let mut sent = 0usize;
    let mut ok = true;
    if datagram {
        if !reliable.is_empty() {
            let mut dw = DataWriter::new(None, ack, header_tick);
            for (ch, b) in &reliable {
                dw.block_bytes(*ch, b);
            }
            let p = dw.finish();
            sent += p.len();
            ok &= transport.send(Lane::Reliable, p).is_ok();
        }
        let mut dw = DataWriter::new(None, ack, header_tick);
        if let Some(b) = &input_block {
            dw.block_bytes(ChannelId::Input, b);
        }
        for (ch, b) in &unreliable {
            if dw.len() + block_overhead(b.len()) + b.len() <= caps.max_unreliable {
                dw.block_bytes(*ch, b);
            }
        }
        if !dw.is_empty() || ack.is_some() {
            let p = dw.finish();
            sent += p.len();
            ok &= transport.send(Lane::Unreliable, p).is_ok();
        }
        conn.ack_dirty = false;
    } else {
        let mut dw = DataWriter::new(None, None, header_tick);
        for (ch, b) in &reliable {
            dw.block_bytes(*ch, b);
        }
        if let Some(b) = &input_block {
            dw.block_bytes(ChannelId::Input, b);
        }
        for (ch, b) in &unreliable {
            dw.block_bytes(*ch, b);
        }
        // Inputs are sent once on streams, so only a ping-only packet may be skipped.
        let lane = if reliable.is_empty() && input_block.is_none() && unreliable.iter().all(|b| b.0 == ChannelId::Time)
        {
            Lane::Unreliable
        } else {
            Lane::Reliable
        };
        let p = dw.finish();
        sent += p.len();
        ok &= transport.send(lane, p).is_ok();
    }
    net.stats.bytes_out += sent as u64;
    if ok && let Some(t) = newest_input {
        conn.last_input_sent = Some(t);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::WireLimits;
    use crate::transport::TransportError;
    use crate::{NetClientId, PlayerId, RoomId};

    /// A game server that is gone: every connect is refused at once (and counted).
    #[derive(Default)]
    struct Refused(std::sync::atomic::AtomicUsize);

    impl Connector for Refused {
        fn connect(&self, _url: &str) -> Result<Box<dyn ClientTransport>, TransportError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(TransportError::Connect("Connection refused (os error 61)".into()))
        }
    }

    pub(crate) fn welcome(resume_window_s: u16) -> Welcome {
        Welcome {
            client: NetClientId(0),
            player: PlayerId(7),
            room: RoomId(9),
            room_epoch: 1,
            tick_hz: 60,
            send_interval: 2,
            server_tick: Tick(0),
            resume: vec![1, 2, 3],
            resume_window_s,
            limits: WireLimits::default(),
            down_bytes_per_sec: 0,
            resumed: false,
            echo_client_time_us: 0,
        }
    }

    /// A session (joined with `auth`) whose link drops at `at`; its server is gone.
    fn dropped(auth: HelloAuth, resume_window_s: u16, at: Duration) -> (NetClient, Arc<Refused>) {
        let mut net = NetClient::new(ClientConfig::default());
        let server = Arc::new(Refused::default());
        let mut conn = Connection::new(server.clone(), "ws://gs-1/play".into(), auth, &net.config);
        conn.welcome = Some(welcome(resume_window_s));
        conn.resume_token = Some(vec![1, 2, 3]);
        net.state = ConnectionState::Connected;
        lost(&mut net, &mut conn, at, "Connection reset without closing handshake");
        net.conn = Some(conn);
        (net, server)
    }

    /// Drive frames every 50 ms from `from` for at most `limit`: when the client tried to
    /// connect, and when it gave up.
    fn run(
        net: &mut NetClient,
        server: &Refused,
        from: Duration,
        limit: Duration,
    ) -> (Vec<Duration>, Option<Duration>) {
        let registry = ReplicationRegistry::default();
        let mut attempts = Vec::new();
        let mut t = from;
        while t < from + limit {
            t += Duration::from_millis(50);
            let before = server.0.load(std::sync::atomic::Ordering::Relaxed);
            drive(net, t, &registry);
            if server.0.load(std::sync::atomic::Ordering::Relaxed) > before {
                attempts.push(t - from);
            }
            if matches!(net.state, ConnectionState::Failed(_)) {
                return (attempts, Some(t - from));
            }
        }
        (attempts, None)
    }

    /// Regression: after its game server was lost (every reconnect refused at once), a client
    /// kept resuming for its whole reconnect schedule — 10 attempts over ~56 s — although the
    /// server keeps a slot only for the resume window (20 s) and a lost server's resume state
    /// died with it. The player service had the room restorable after ~10 s, but the client
    /// asked it only once the schedule ran out. Resuming now stops with the resume window: the
    /// last attempt goes just before it ends, then the session fails and the game rejoins.
    #[test]
    fn resuming_stops_when_the_resume_window_is_over() {
        let t0 = Duration::from_secs(100);
        let (mut net, gs) = dropped(HelloAuth::Ticket("spent".into()), 20, t0);
        assert!(matches!(net.state, ConnectionState::Reconnecting { .. }));
        let (attempts, failed) = run(&mut net, &gs, t0, Duration::from_secs(70));
        let failed = failed.expect("gave up");
        assert!(
            failed >= Duration::from_secs(19) && failed < Duration::from_secs(20),
            "gave up {failed:?} after the loss (attempts at {attempts:?})"
        );
        let last = *attempts.last().unwrap();
        assert!(
            last >= Duration::from_millis(19_800) && last < Duration::from_secs(20),
            "the last resume goes just before the window ends: {attempts:?}"
        );
        assert!(attempts.len() < 10, "{attempts:?}");
        let ConnectionState::Failed(FailReason::Unreachable(why)) = &net.state else {
            panic!("{:?}", net.state)
        };
        assert!(why.contains("resume window"), "{why}");

        // A short window ends the attempts sooner; the reconnect schedule still bounds a long one.
        let (mut net, gs) = dropped(HelloAuth::Ticket("spent".into()), 2, t0);
        let (_, failed) = run(&mut net, &gs, t0, Duration::from_secs(70));
        assert!(failed.is_some_and(|f| f < Duration::from_secs(2)), "{failed:?}");
        let (mut net, gs) = dropped(HelloAuth::Ticket("spent".into()), u16::MAX, t0);
        let (attempts, failed) = run(&mut net, &gs, t0, Duration::from_secs(70));
        assert!(failed.is_some(), "the reconnect policy's 10 attempts still apply");
        assert_eq!(attempts.len(), 10);

        // Dev credentials can join afresh once the slot is gone: not bound by the window.
        let (mut net, gs) = dropped(HelloAuth::Dev { name: "a".into() }, 2, t0);
        let (attempts, _) = run(&mut net, &gs, t0, Duration::from_secs(10));
        assert!(
            attempts.last().is_some_and(|t| *t > Duration::from_secs(2)),
            "{attempts:?}"
        );
    }
}
