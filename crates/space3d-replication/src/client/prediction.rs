//! Client-side prediction and reconciliation.
//!
//! Each predicted tick records, for every `Predicted` entity, the encoded bytes of its
//! `Predict*` components after the sim ran ([`PredictionHistory`]). When a snapshot for tick S
//! confirms different bytes (or `ComponentRule::rollback_when` says so), `rollback_from = S`:
//! in the next `client_tick` the client writes the confirmed values, then replays
//! `S+1 ..= predicted_tick` with the stored inputs (`SimTime::resimulating = true`), rewriting
//! history, and emits `Mispredicted`. The visual jump (old − new render value) goes into
//! [`PredictionError`] and decays with `smoothing_half_life` (snaps beyond `snap_distance`).
//! Replays longer than `max_rollback_ticks`, or S older than the history, snap instead.
//!
//! Physics bodies are not rolled back in v1: predicted characters use kinematic controllers in
//! shared sim code; replicated dynamic bodies are interpolated.

use super::{ConnectionState, Mispredicted, NetClient};
use crate::mapping::{DecodeCtx, EncodeCtx};
use crate::registry::ReplicationRegistry;
use crate::sim::{SimRole, SimTime, run_sim};
use crate::time::PaceAction;
use crate::wire::Writer;
use crate::{Controlled, FastMap, NetEntity, NetId, Predicted, Tick};
use smallvec::SmallVec;
use space3d_core::glam::{Quat, Vec3};
use space3d_core::{Entity, SystemCtx, TickRate, Time, Transform, World};
use std::collections::VecDeque;
use std::time::Duration;

/// Encoded predicted components of one entity at one tick.
pub type PredictedComps = SmallVec<[(u16, Vec<u8>); 2]>;

/// Per-entity ring of `(tick, encoded predicted components)` (component on predicted entities).
#[derive(Clone, Debug, Default)]
pub struct PredictionHistory {
    /// Oldest first.
    pub entries: VecDeque<(Tick, PredictedComps)>,
}

impl PredictionHistory {
    fn record(&mut self, tick: Tick, comps: PredictedComps, keep: usize) {
        while self.entries.back().is_some_and(|(t, _)| *t >= tick) {
            self.entries.pop_back();
        }
        self.entries.push_back((tick, comps));
        while self.entries.len() > keep {
            self.entries.pop_front();
        }
    }

    /// Encoded bytes of `kind` at `tick`.
    pub fn get(&self, tick: Tick, kind: u16) -> Option<&[u8]> {
        let i = self.entries.binary_search_by_key(&tick, |(t, _)| *t).ok()?;
        self.entries[i]
            .1
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, b)| b.as_slice())
    }
}

/// Decaying visual correction of a predicted entity (render-only; never simulated).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PredictionError {
    /// Translation offset added to the rendered transform.
    pub translation: Vec3,
    /// Rotation offset.
    pub rotation: Quat,
}

/// The correction currently added to `Transform` (removed before the next simulation step).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AppliedCorrection {
    translation: Vec3,
    rotation: Quat,
}

/// Inputs of past predicted ticks (erased; postcard bytes + value) for replay and redundancy.
#[derive(Default)]
pub struct InputHistory {
    /// `(tick, encoded input, decoded value)`, oldest first.
    pub entries: VecDeque<(Tick, Vec<u8>, Box<dyn std::any::Any + Send + Sync>)>,
}

impl InputHistory {
    /// Newest entry.
    pub fn newest(&self) -> Option<(Tick, &[u8])> {
        self.entries.back().map(|(t, b, _)| (*t, b.as_slice()))
    }

    /// Newest entry at or before `tick`.
    fn at_or_before(&self, tick: Tick) -> Option<&(Tick, Vec<u8>, Box<dyn std::any::Any + Send + Sync>)> {
        let i = match self.entries.binary_search_by_key(&tick, |(t, _, _)| *t) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        self.entries.get(i)
    }

    fn push(&mut self, tick: Tick, bytes: Vec<u8>, value: Box<dyn std::any::Any + Send + Sync>, keep: usize) {
        while self.entries.back().is_some_and(|(t, _, _)| *t >= tick) {
            self.entries.pop_back();
        }
        self.entries.push_back((tick, bytes, value));
        while self.entries.len() > keep {
            self.entries.pop_front();
        }
    }
}

/// Newest authoritative values of a predicted entity.
#[derive(Clone, Debug, Default)]
pub(crate) struct Confirmed {
    pub(crate) tick: Tick,
    pub(crate) comps: PredictedComps,
}

/// Prediction driver state (inside `NetClient`).
#[derive(Default)]
pub(crate) struct PredictionState {
    /// Tick the next `client_tick` simulates (`None` = not predicting yet).
    pub(crate) next_tick: Option<Tick>,
    pub(crate) inputs: InputHistory,
    pub(crate) rollback_from: Option<Tick>,
    /// A resync moved `next_tick`: the world holds the state after the *old* timeline's last
    /// tick, so the next `client_tick` must rebuild the state before `next_tick` from the newest
    /// confirmation even when the prediction for that tick was right (otherwise a forward jump
    /// skips ticks and a backward one applies their inputs a second time).
    pub(crate) rebuild: bool,
    pub(crate) confirmed: FastMap<NetId, Confirmed>,
    pub(crate) nominal_hz: f64,
    pub(crate) scale: f64,
    /// After a snap (no replay) at predicted tick T, the history of ticks before T is stale:
    /// confirmations older than this only update the confirmed values.
    pub(crate) snap_floor: Option<Tick>,
    /// After a resync, the server's slack feedback describes the old timeline until it consumed
    /// enough inputs of the new one (see [`client_pace`]): feedback is ignored until
    /// `input_ack` reaches this tick.
    pub(crate) feedback_after: Option<Tick>,
    /// After a backward resync on feedback: until then, the clock-based hard resync must not
    /// undo it (see [`client_pace`]).
    pub(crate) clock_hold_until: Option<Duration>,
    /// The server reported a missing input since that resync.
    pub(crate) missing_since_resync: bool,
    /// Debug builds: predicted component changes made outside the sim (between two predicted
    /// ticks) — they are invisible to rollback and mispredict (see [`check_sim_discipline`]).
    pub(crate) outside_sim_changes: u64,
}

impl PredictionState {
    pub(crate) fn new() -> Self {
        Self {
            nominal_hz: 60.0,
            scale: 1.0,
            ..Default::default()
        }
    }

    /// Stop predicting (link lost / reconnect); keeps confirmed values.
    pub(crate) fn stop(&mut self) {
        self.next_tick = None;
        self.rollback_from = None;
        self.rebuild = false;
        self.snap_floor = None;
        self.feedback_after = None;
        self.clock_hold_until = None;
        self.missing_since_resync = false;
        self.inputs.entries.clear();
        self.scale = 1.0;
    }

    /// Whether a confirmation for `tick` should be compared with the predicted history.
    pub(crate) fn compares(&self, tick: Tick) -> bool {
        self.snap_floor.is_none_or(|f| tick >= f)
    }

    /// Forget everything.
    pub(crate) fn clear(&mut self) {
        self.stop();
        self.confirmed.clear();
    }

    pub(crate) fn forget(&mut self, id: NetId) {
        self.confirmed.remove(&id);
    }

    pub(crate) fn forget_kind(&mut self, id: NetId, kind: u16) {
        if let Some(c) = self.confirmed.get_mut(&id) {
            c.comps.retain(|(k, _)| *k != kind);
        }
    }

    pub(crate) fn confirm(&mut self, id: NetId, tick: Tick, kind: u16, bytes: &[u8]) {
        let c = self.confirmed.entry(id).or_default();
        if tick > c.tick {
            c.tick = tick;
        } else if tick < c.tick {
            return;
        }
        match c.comps.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, b)) => {
                b.clear();
                b.extend_from_slice(bytes);
            }
            None => c.comps.push((kind, bytes.to_vec())),
        }
    }

    pub(crate) fn request_rollback(&mut self, tick: Tick) {
        if self.next_tick.is_some() {
            self.rollback_from = Some(self.rollback_from.map_or(tick, |t| t.min(tick)));
        }
    }
}

/// Encoded bytes of `kind` predicted for `entity` at `tick`.
pub(crate) fn history_bytes(world: &World, entity: Entity, tick: Tick, kind: u16) -> Option<Vec<u8>> {
    world
        .get::<&PredictionHistory>(entity)
        .ok()
        .and_then(|h| h.get(tick, kind).map(<[u8]>::to_vec))
}

/// Server slack samples to wait for after a backward feedback resync before its feedback is
/// trusted again: the server smooths the slack (EWMA, α = 0.1), so right after the first input
/// of the new timeline its report still mostly describes the old one (0.9²⁴ ≈ 8 % left).
pub const SLACK_SETTLE_TICKS: u32 = 24;

/// The `replication::client_pace` system (PreUpdate): tick pacing and prediction start.
///
/// Two sources can move the predicted tick: the server's slack feedback (the pacer) and the
/// clock-based check (`|target − next| > 1.5 × hard_resync_ticks`, for when feedback cannot
/// tell: stalls, hitches, missing inputs). The clock's target lags a path change by the time
/// its path RTT and offset need to follow (≈ 1.25 s); after a large drop of the uplink
/// latency the feedback reacts first, with a backward resync. After such a resync (1) feedback
/// is ignored until the server consumed [`SLACK_SETTLE_TICKS`] inputs of the new timeline —
/// the first new input is the one after the newest already sent, and the server's smoothed
/// slack needs that many samples to forget the old timeline (acting on it again made further
/// backward jumps) — and (2) the clock-based check does not undo it for a hold (8 ping
/// intervals, at least 2 s) unless the server reports missing inputs meanwhile (then the clock
/// is right). Other resyncs gate feedback until the server consumed an input for the new tick.
pub(crate) fn client_pace(ctx: &mut SystemCtx) {
    let now = ctx.resources.init::<crate::time::NetClockHandle>().now();
    let Some(net) = ctx.resources.get_mut::<NetClient>() else {
        return;
    };
    if !matches!(net.state, ConnectionState::Syncing | ConnectionState::Connected) {
        return;
    }
    let Some(conn) = net.conn.as_mut() else { return };
    let Some(welcome) = conn.welcome.as_ref() else { return };
    let hz = f64::from(welcome.tick_hz.max(1));
    // Stall-resistant path RTT / jitter (a head-of-line stall must not move the target).
    let jitter_ticks = conn.clock.jitter().as_secs_f64() * hz;
    let rtt = conn.clock.path_rtt().unwrap_or_else(|| conn.rtt.srtt());
    let Some(est) = conn.clock.estimate(now) else { return };
    let target = est + rtt.as_secs_f64() / 2.0 * hz + conn.pacer.target(jitter_ticks);
    let hard = conn.pacer.config().hard_resync_ticks;
    let prediction = &mut net.prediction;
    prediction.nominal_hz = hz;
    let mut realigned = false;
    match prediction.next_tick {
        None => {
            prediction.next_tick = Some(Tick(target.round().max(1.0) as u32));
            prediction.scale = 1.0;
            conn.pacer.reset();
        }
        Some(next) => {
            if let Some(slack) = net.pending_slack.take() {
                prediction.missing_since_resync |= slack == i8::MIN;
                let fresh = prediction
                    .feedback_after
                    .is_none_or(|t| conn.input_ack.is_some_and(|a| a >= t));
                if fresh {
                    prediction.feedback_after = None;
                    match conn.pacer.on_feedback(slack, jitter_ticks) {
                        PaceAction::Scale(s) => prediction.scale = s,
                        PaceAction::Resync(n) => {
                            let to = Tick((i64::from(next.0) + n).max(1) as u32);
                            resync(prediction, to);
                            if n < 0 && prediction.next_tick == Some(to) {
                                // Inputs arrive too early: the path got faster, which the clock
                                // only sees ≈ 1.25 s later. The server's slack describes the new
                                // timeline once it consumed enough of its inputs — the first one
                                // is after the newest input already sent (nothing new is sent
                                // before the prediction passes it). (A forward jump on late
                                // inputs, after a stall, keeps the short gate and gets no hold:
                                // there the clock is right and may undo it at once.)
                                let first_new = conn.last_input_sent.map_or(to, |s| s.next().max(to));
                                prediction.feedback_after = Some(Tick(first_new.0.saturating_add(SLACK_SETTLE_TICKS)));
                                let hold = (net.config.ping_interval * 8).max(Duration::from_secs(2));
                                prediction.clock_hold_until = Some(now + hold);
                                prediction.missing_since_resync = false;
                            }
                        }
                    }
                }
            }
            let next = prediction.next_tick.unwrap_or(next);
            let held = prediction.clock_hold_until.is_some_and(|until| now < until) && !prediction.missing_since_resync;
            if !held && (target - f64::from(next.0)).abs() > hard * 1.5 {
                // The clock says we are far off (stall, clock jump): jump.
                resync(prediction, Tick(target.round().max(1.0) as u32));
                conn.pacer.reset();
                prediction.clock_hold_until = None;
                realigned = true;
            }
        }
    }
    let scale = prediction.scale;
    net.stats.tick_scale = scale as f32;
    net.stats.input_slack = conn.pacer.target(jitter_ticks) as f32;
    if let Some(rate) = ctx.resources.get_mut::<TickRate>() {
        let want = hz * scale;
        if (rate.hz - want).abs() > 1e-9 {
            rate.hz = want;
        }
    }
    if realigned && let Some(time) = ctx.resources.get_mut::<Time>() {
        // The jump already covers the time a long frame (a hitch) left in the fixed-step
        // backlog: running that backlog on top of it would put the prediction up to
        // `max_steps_per_frame` ticks too far ahead (and a later backward resync would replay
        // ticks the player already saw). This frame runs at most one step, as usual.
        time.discard_backlog(1);
    }
}

fn resync(p: &mut PredictionState, to: Tick) {
    tracing::debug!(?p.next_tick, ?to, "prediction resync");
    // Ticks skipped by a forward jump get the server's repeated input (they are "missing"
    // on the server, which the pacer answers with a speed-up, never another jump). The input
    // history is kept: the rebuild below replays the recorded ticks and repeats the last input
    // over the skipped ones, exactly like the server, instead of snapping. After a backward
    // jump the ticks up to the newest input already sent are predicted again with those sent
    // inputs (the server has them; see `client_tick`).
    if p.next_tick == Some(to) {
        return;
    }
    p.next_tick = Some(to);
    p.feedback_after = Some(to);
    p.scale = 1.0;
    // Rebuild the state before `to` from the newest confirmed state (always: the world still
    // holds the old timeline's state, whether or not its prediction was right).
    let newest = p.confirmed.values().map(|c| c.tick).max();
    if let Some(newest) = newest {
        p.rollback_from = Some(newest);
        p.rebuild = true;
    }
}

/// Predicted entities: `(entity, net id)`.
fn predicted_entities(world: &World) -> Vec<(Entity, NetId)> {
    world
        .query::<(Entity, &NetEntity)>()
        .with::<&Predicted>()
        .iter()
        .map(|(e, n)| (e, n.0))
        .collect()
}

/// Quantize every predicted component of predicted entities and record history at `tick`.
fn record_history(world: &mut World, registry: &ReplicationRegistry, net: &NetClient, tick: Tick) {
    let keep = usize::from(net.config.prediction.max_rollback_ticks) + 16;
    let enc = EncodeCtx::client(&net.map);
    let mut dec = DecodeCtx::new(&net.map);
    let mut scratch = Writer::new();
    for (e, _) in predicted_entities(world) {
        let mut comps: PredictedComps = SmallVec::new();
        for entry in registry.components().iter().filter(|c| c.mode.is_predicted()) {
            let kind = entry.kind.expect("frozen").0;
            if entry.quantize_authoritative {
                if entry.ops.quantize(world, e, &enc, &mut dec, &mut scratch) {
                    comps.push((kind, scratch.as_slice().to_vec()));
                }
            } else {
                scratch.clear();
                if entry.ops.encode(world, e, &mut scratch, &enc) {
                    comps.push((kind, scratch.as_slice().to_vec()));
                }
            }
        }
        let comps = match world.get::<&mut PredictionHistory>(e) {
            Ok(mut h) => {
                h.record(tick, comps, keep);
                None
            }
            Err(_) => Some(comps),
        };
        if let Some(comps) = comps {
            let mut h = PredictionHistory::default();
            h.record(tick, comps, keep);
            let _ = world.insert_one(e, h);
        }
    }
}

fn write_inputs(world: &mut World, registry: &ReplicationRegistry, value: &Box<dyn std::any::Any + Send + Sync>) {
    let Some(input) = registry.input() else { return };
    let targets: Vec<Entity> = world.query::<(Entity, &Controlled)>().iter().map(|(e, _)| e).collect();
    for e in targets {
        input.ops.write(world, e, value);
    }
}

fn visual(world: &World) -> Vec<(Entity, Vec3, Quat)> {
    world
        .query::<(Entity, &Transform)>()
        .with::<&Predicted>()
        .iter()
        .map(|(e, t)| (e, t.translation, t.rotation))
        .collect()
}

/// The `replication::client_tick` system (FixedUpdate): rollback + replay, sample, simulate.
pub(crate) fn client_tick(ctx: &mut SystemCtx) {
    let world = &mut *ctx.world;
    let resources = &mut *ctx.resources;
    if !resources.contains::<ReplicationRegistry>() || !resources.contains::<NetClient>() {
        return;
    }
    resources.scope::<ReplicationRegistry, _>(|res, registry| {
        if !registry.is_frozen() {
            return;
        }
        let (tick, rollback, rebuild, dt) = {
            let net = res.expect_mut::<NetClient>();
            if !net.config.prediction.enabled
                || !matches!(net.state, ConnectionState::Syncing | ConnectionState::Connected)
            {
                return;
            }
            let Some(tick) = net.prediction.next_tick else { return };
            let dt = Duration::from_secs_f64(1.0 / net.prediction.nominal_hz.max(1.0));
            let rebuild = std::mem::take(&mut net.prediction.rebuild);
            (tick, net.prediction.rollback_from.take(), rebuild, dt)
        };
        let tick = match rollback {
            // A confirmation at or past `tick` moves the timeline forward (see
            // `rollback_and_replay`): simulate the tick after it instead.
            Some(from) => rollback_and_replay(world, res, registry, from, tick, rebuild, dt).unwrap_or(tick),
            None if cfg!(debug_assertions) => {
                let net = res.expect_mut::<NetClient>();
                check_sim_discipline(world, registry, net, tick);
                tick
            }
            None => tick,
        };
        // Sample and simulate the next tick. A tick predicted again after a backward resync
        // whose input was already sent keeps that input: the server simulates it with the one
        // it received, so a fresh sample (never sent) would only mispredict.
        let resent = {
            let net = res.expect::<NetClient>();
            let sent = net.conn.as_ref().and_then(|c| c.last_input_sent);
            match (registry.input(), net.prediction.inputs.at_or_before(tick)) {
                (Some(input), Some((at, _, v))) if *at == tick && sent.is_some_and(|s| tick <= s) => {
                    Some(input.ops.clone_value(v))
                }
                _ => None,
            }
        };
        let reused = resent.is_some();
        let value = match (resent, registry.input()) {
            (Some(v), _) => Some(v),
            (None, Some(input)) => {
                let mut inner = SystemCtx { world, resources: res };
                Some(input.ops.sample(&mut inner))
            }
            (None, None) => None,
        };
        if let Some(v) = &value {
            write_inputs(world, registry, v);
        }
        {
            let mut inner = SystemCtx { world, resources: res };
            run_sim(
                &mut inner,
                SimTime {
                    tick,
                    dt,
                    role: SimRole::Client,
                    resimulating: false,
                },
            );
        }
        let net = res.expect_mut::<NetClient>();
        record_history(world, registry, net, tick);
        if let (Some(v), Some(input), false) = (value, registry.input(), reused) {
            let keep = usize::from(net.config.prediction.max_rollback_ticks) + 64;
            net.prediction.inputs.push(tick, input.ops.encode(&v), v, keep);
        }
        net.prediction.next_tick = Some(tick.next());
    });
}

/// Debug check of `SimSchedule` discipline: a predicted component must only change inside the
/// sim. Before simulating `tick`, every predicted component of a predicted entity must still
/// hold what the sim recorded for `tick − 1`; anything else was written by a system outside
/// `add_sim_system` (the change is lost on the next rollback and causes mispredictions).
/// Counted in `NetClient::sim_discipline_violations`, warned once per session.
fn check_sim_discipline(world: &World, registry: &ReplicationRegistry, net: &mut NetClient, tick: Tick) {
    let Some(prev) = tick.0.checked_sub(1).map(Tick) else {
        return;
    };
    let enc = EncodeCtx::client(&net.map);
    let mut scratch = Writer::new();
    let mut found: Option<(NetId, &'static str)> = None;
    let mut n = 0u64;
    for (e, id) in predicted_entities(world) {
        let Ok(history) = world.get::<&PredictionHistory>(e) else {
            continue;
        };
        if history.entries.back().is_none_or(|(t, _)| *t != prev) {
            continue;
        }
        for entry in registry.components().iter().filter(|c| c.mode.is_predicted()) {
            let kind = entry.kind.expect("frozen").0;
            let Some(recorded) = history.get(prev, kind) else {
                continue;
            };
            scratch.clear();
            if entry.ops.encode(world, e, &mut scratch, &enc)
                && entry
                    .ops
                    .differs(recorded, scratch.as_slice(), &mut DecodeCtx::new(&net.map))
            {
                n += 1;
                found.get_or_insert((id, entry.name));
            }
        }
    }
    if n > 0 {
        if net.prediction.outside_sim_changes == 0
            && let Some((id, name)) = found
        {
            tracing::warn!(
                net_id = ?id,
                component = name,
                "a predicted component changed outside the sim: move that logic into add_sim_system \
                 (it is lost on rollback and mispredicts)"
            );
        }
        net.prediction.outside_sim_changes += n;
    }
}

/// Newest confirmed tick over the predicted entities, and whether the recorded prediction for
/// that tick already equals the confirmed values of every predicted entity.
fn newest_confirmation(world: &World, registry: &ReplicationRegistry, net: &NetClient) -> Option<(Tick, bool)> {
    let predicted = predicted_entities(world);
    let newest = predicted
        .iter()
        .filter_map(|(_, id)| net.prediction.confirmed.get(id).map(|c| c.tick))
        .max()?;
    let matches = net.prediction.compares(newest)
        && predicted.iter().all(|(e, id)| {
            let Some(c) = net.prediction.confirmed.get(id) else {
                return true;
            };
            c.tick == newest
                && c.comps.iter().all(|(kind, bytes)| {
                    let (Some(h), Some(entry)) = (history_bytes(world, *e, newest, *kind), registry.component(*kind))
                    else {
                        return false;
                    };
                    !entry.ops.differs(&h, bytes, &mut DecodeCtx::new(&net.map))
                })
        });
    Some((newest, matches))
}

/// Returns the tick `client_tick` must simulate next when the timeline moved forward: the
/// newest confirmation is at or past `next` (the client fell behind snapshots it already holds —
/// a frame hitch on a low-latency link), so the confirmed state already includes ticks
/// `next ..= from` and simulating them again would apply their inputs twice.
///
/// `rebuild` (after a resync moved `next`): the world holds the state after the old
/// timeline's last tick, not `next − 1`, so the state is rebuilt even when the prediction for
/// the newest confirmed tick was right.
#[allow(clippy::too_many_arguments)]
fn rollback_and_replay(
    world: &mut World,
    res: &mut space3d_core::Resources,
    registry: &ReplicationRegistry,
    from: Tick,
    next: Tick,
    rebuild: bool,
    dt: Duration,
) -> Option<Tick> {
    // Only the newest confirmed values are kept, so the replay starts from their tick: a
    // mismatch at an older tick (several snapshots arriving together after a stall) is
    // superseded by the newest one, and if our prediction for the newest confirmed tick was
    // right there is nothing to correct (unless a resync moved the timeline).
    let from = match newest_confirmation(world, registry, res.expect::<NetClient>()) {
        Some((_, true)) if !rebuild => return None,
        Some((newest, _)) => newest.max(from),
        None => from,
    };
    let before = visual(world);
    let replay = {
        let net = res.expect_mut::<NetClient>();
        // Write the confirmed values of every predicted entity.
        let confirmed: Vec<(NetId, Tick, PredictedComps)> = net
            .prediction
            .confirmed
            .iter()
            .map(|(id, c)| (*id, c.tick, c.comps.clone()))
            .collect();
        for (id, _, comps) in &confirmed {
            let Some(e) = net.map.entity(*id) else { continue };
            if !world.satisfies::<&Predicted>(e) {
                continue;
            }
            for (kind, bytes) in comps {
                if let Some(entry) = registry.component(*kind) {
                    let _ = entry.ops.apply(world, e, bytes, &mut DecodeCtx::new(&net.map));
                }
            }
        }
        let last = next.0.saturating_sub(1);
        let n = last.saturating_sub(from.0);
        // Ticks without a recorded input (skipped by a resync) replay the last earlier input
        // repeated, as the server did for them; only a replay that starts before the oldest
        // recorded input cannot be reconstructed.
        let have_inputs = registry.input().is_none() || net.prediction.inputs.at_or_before(Tick(from.0 + 1)).is_some();
        let max = u32::from(net.config.prediction.max_rollback_ticks);
        (n > 0 && n <= max && have_inputs).then_some((from.0 + 1, last))
    };
    // Confirmed at or past `next`: fast-forward to `from + 1` (a forward resync; the server
    // already simulated the skipped ticks with the inputs it had).
    let forward = (from >= next).then(|| from.next());
    if replay.is_none() {
        // Snap: the confirmed state stands in for ticks `from+1 .. next`, whose recorded
        // history no longer describes this timeline. Compare again from `next` on (a single
        // follow-up rollback then catches the lead up, if it is within `max_rollback_ticks`).
        for (e, _) in predicted_entities(world) {
            if let Ok(mut h) = world.get::<&mut PredictionHistory>(e) {
                h.entries.clear();
            }
        }
        let prediction = &mut res.expect_mut::<NetClient>().prediction;
        prediction.snap_floor = Some(forward.unwrap_or(next));
        if let Some(to) = forward {
            tracing::debug!(?next, ?to, "prediction behind the confirmed state; fast-forward");
            prediction.next_tick = Some(to);
            prediction.feedback_after = Some(to);
        }
    }
    let mut replayed = 0u32;
    if let Some((a, b)) = replay {
        for t in a..=b {
            let tick = Tick(t);
            let value = {
                let net = res.expect::<NetClient>();
                match (registry.input(), net.prediction.inputs.at_or_before(tick)) {
                    (Some(input), Some((at, _, v))) if *at == tick => Some(input.ops.clone_value(v)),
                    (Some(input), Some((_, _, v))) => Some(input.ops.repeat(v)),
                    _ => None,
                }
            };
            if let Some(v) = &value {
                write_inputs(world, registry, v);
            }
            {
                let mut inner = SystemCtx { world, resources: res };
                run_sim(
                    &mut inner,
                    SimTime {
                        tick,
                        dt,
                        role: SimRole::Client,
                        resimulating: true,
                    },
                );
            }
            let net = res.expect::<NetClient>();
            record_history(world, registry, net, tick);
            replayed += 1;
        }
    }
    // Visual error: keep showing where the entity was, then decay.
    let after = visual(world);
    let snap = res.expect::<NetClient>().config.prediction.snap_distance;
    for (e, t0, r0) in before {
        let Some((_, t1, r1)) = after.iter().find(|(ae, _, _)| *ae == e).copied() else {
            continue;
        };
        let dt_ = t0 - t1;
        let dr = r0 * r1.inverse();
        let mut err = world.get::<&PredictionError>(e).map(|p| *p).unwrap_or(PredictionError {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
        });
        err.translation += dt_;
        err.rotation = (dr * err.rotation).normalize();
        if err.translation.length() > snap || !err.translation.is_finite() {
            err = PredictionError {
                translation: Vec3::ZERO,
                rotation: Quat::IDENTITY,
            };
        }
        if let Ok(mut p) = world.get::<&mut PredictionError>(e) {
            *p = err;
        } else {
            let _ = world.insert_one(e, err);
        }
    }
    let net = res.expect_mut::<NetClient>();
    net.stats.rollbacks += 1;
    net.stats.resim_ticks += u64::from(replayed);
    super::receive::emit_mispredicted(res, Mispredicted { tick: from, replayed });
    forward
}

/// PreUpdate: take the visual correction off `Transform` before anything simulates.
pub(crate) fn remove_visual_offsets(world: &mut World) {
    for (t, applied) in world.query_mut::<(&mut Transform, &mut AppliedCorrection)>() {
        if applied.translation != Vec3::ZERO || applied.rotation != Quat::IDENTITY {
            t.translation -= applied.translation;
            t.rotation = (applied.rotation.inverse() * t.rotation).normalize();
            applied.translation = Vec3::ZERO;
            applied.rotation = Quat::IDENTITY;
        }
    }
}

/// Update: decay the prediction error and add it to the rendered `Transform`.
pub(crate) fn apply_visual_offsets(world: &mut World, frame_dt: Duration, half_life: Duration) {
    let factor = if half_life.is_zero() {
        0.0
    } else {
        0.5f32.powf(frame_dt.as_secs_f32() / half_life.as_secs_f32())
    };
    let mut missing = Vec::new();
    for (e, t, err, applied) in world.query_mut::<(
        Entity,
        &mut Transform,
        &mut PredictionError,
        Option<&mut AppliedCorrection>,
    )>() {
        err.translation *= factor;
        err.rotation = Quat::IDENTITY.slerp(err.rotation, factor).normalize();
        if err.translation.length_squared() < 1e-10 {
            err.translation = Vec3::ZERO;
        }
        t.translation += err.translation;
        t.rotation = (err.rotation * t.rotation).normalize();
        let a = AppliedCorrection {
            translation: err.translation,
            rotation: err.rotation,
        };
        match applied {
            Some(slot) => *slot = a,
            None => missing.push((e, a)),
        }
    }
    for (e, a) in missing {
        let _ = world.insert_one(e, a);
    }
}
