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
use space3d_core::{Entity, SystemCtx, TickRate, Transform, World};
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

    fn get(&self, tick: Tick) -> Option<&(Tick, Vec<u8>, Box<dyn std::any::Any + Send + Sync>)> {
        let i = self.entries.binary_search_by_key(&tick, |(t, _, _)| *t).ok()?;
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
    pub(crate) confirmed: FastMap<NetId, Confirmed>,
    pub(crate) nominal_hz: f64,
    pub(crate) scale: f64,
    /// After a snap (no replay) at predicted tick T, the history of ticks before T is stale:
    /// confirmations older than this only update the confirmed values.
    pub(crate) snap_floor: Option<Tick>,
    /// After a resync to tick T, the server's slack feedback describes the old timeline until
    /// it consumed an input for T or later: feedback is ignored until then.
    pub(crate) feedback_after: Option<Tick>,
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
        self.snap_floor = None;
        self.feedback_after = None;
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

/// The `replication::client_pace` system (PreUpdate): tick pacing and prediction start.
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
    let jitter_ticks = conn.rtt.rttvar().as_secs_f64() * hz;
    let Some(est) = conn.clock.estimate(now) else { return };
    let target = est + conn.rtt.srtt().as_secs_f64() / 2.0 * hz + conn.pacer.target(jitter_ticks);
    let hard = conn.pacer.config().hard_resync_ticks;
    let prediction = &mut net.prediction;
    prediction.nominal_hz = hz;
    match prediction.next_tick {
        None => {
            prediction.next_tick = Some(Tick(target.round().max(1.0) as u32));
            prediction.scale = 1.0;
            conn.pacer.reset();
        }
        Some(next) => {
            if let Some(slack) = net.pending_slack.take() {
                let fresh = prediction
                    .feedback_after
                    .is_none_or(|t| conn.input_ack.is_some_and(|a| a >= t));
                if fresh {
                    prediction.feedback_after = None;
                    match conn.pacer.on_feedback(slack, jitter_ticks) {
                        PaceAction::Scale(s) => prediction.scale = s,
                        PaceAction::Resync(n) => resync(prediction, Tick((i64::from(next.0) + n).max(1) as u32)),
                    }
                }
            }
            let next = prediction.next_tick.unwrap_or(next);
            if (target - f64::from(next.0)).abs() > hard * 1.5 {
                // The clock says we are far off (stall, clock jump): jump.
                resync(prediction, Tick(target.round().max(1.0) as u32));
                conn.pacer.reset();
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
}

fn resync(p: &mut PredictionState, to: Tick) {
    tracing::debug!(?p.next_tick, ?to, "prediction resync");
    // Ticks skipped by a forward jump get the server's repeated input (they are "missing"
    // on the server, which the pacer answers with a speed-up, never another jump).
    p.next_tick = Some(to);
    p.feedback_after = Some(to);
    p.inputs.entries.clear();
    p.scale = 1.0;
    // Rebuild from the newest confirmed state.
    let newest = p.confirmed.values().map(|c| c.tick).max();
    p.rollback_from = newest;
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
        let (tick, rollback, dt) = {
            let net = res.expect_mut::<NetClient>();
            if !net.config.prediction.enabled
                || !matches!(net.state, ConnectionState::Syncing | ConnectionState::Connected)
            {
                return;
            }
            let Some(tick) = net.prediction.next_tick else { return };
            let dt = Duration::from_secs_f64(1.0 / net.prediction.nominal_hz.max(1.0));
            (tick, net.prediction.rollback_from.take(), dt)
        };
        if let Some(from) = rollback {
            rollback_and_replay(world, res, registry, from, tick, dt);
        }
        // Sample and simulate the next tick.
        let value = match registry.input() {
            Some(input) => {
                let mut inner = SystemCtx { world, resources: res };
                Some(input.ops.sample(&mut inner))
            }
            None => None,
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
        if let (Some(v), Some(input)) = (value, registry.input()) {
            let keep = usize::from(net.config.prediction.max_rollback_ticks) + 64;
            net.prediction.inputs.push(tick, input.ops.encode(&v), v, keep);
        }
        net.prediction.next_tick = Some(tick.next());
    });
}

fn rollback_and_replay(
    world: &mut World,
    res: &mut space3d_core::Resources,
    registry: &ReplicationRegistry,
    from: Tick,
    next: Tick,
    dt: Duration,
) {
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
        let have_inputs =
            registry.input().is_none() || (from.0 + 1..=last).all(|t| net.prediction.inputs.get(Tick(t)).is_some());
        let max = u32::from(net.config.prediction.max_rollback_ticks);
        (n > 0 && n <= max && have_inputs).then_some((from.0 + 1, last))
    };
    if replay.is_none() {
        // Snap: the confirmed state stands in for ticks `from+1 .. next`, whose recorded
        // history no longer describes this timeline. Compare again from `next` on (a single
        // follow-up rollback then catches the lead up, if it is within `max_rollback_ticks`).
        for (e, _) in predicted_entities(world) {
            if let Ok(mut h) = world.get::<&mut PredictionHistory>(e) {
                h.entries.clear();
            }
        }
        res.expect_mut::<NetClient>().prediction.snap_floor = Some(next);
    }
    let mut replayed = 0u32;
    if let Some((a, b)) = replay {
        for t in a..=b {
            let tick = Tick(t);
            let value = {
                let net = res.expect::<NetClient>();
                match (registry.input(), net.prediction.inputs.get(tick)) {
                    (Some(input), Some((_, _, v))) => Some(input.ops.clone_value(v)),
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
