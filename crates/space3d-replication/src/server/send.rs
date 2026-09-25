//! `replication::server_send` (Render, send ticks): one Data packet per client.
//!
//! Per client: skip if `backlog_bytes > backlog_skip_bytes` (halve its snapshot rate until it
//! drains; kick `SlowConsumer` after 5 s above `backlog_kick_bytes`) → interest diff →
//! priority/budget selection → `SnapshotWriter` (copying cached component bytes, applying
//! `Audience` per component) → queued reliable/unreliable messages, Control, Pongs →
//! `LinkTx::send`. On datagram transports the snapshot is split at entity granularity into
//! packets ≤ `caps.max_unreliable`, each recorded in the `SentRecord` ring for acks; on
//! streams the entity baselines advance optimistically when the packet is handed to the link.

use super::change::{ChangeCache, EntityRecord};
use super::interest::{Viewer, dist_xz, relevant_set};
use super::priority::{ByteBudget, distance_falloff};
use super::slots::{OutBlock, Slot};
use super::{InterestPolicy, NetServer, ServerNetConfig, SlotState};
use crate::protocol::control::{Pong, TimeMsg};
use crate::protocol::handshake::DisconnectCode;
use crate::protocol::snapshot::{DespawnRecord, SnapshotHeader, SnapshotWriter, flags as sflags, op};
use crate::protocol::{AckField, ChannelId, DataWriter, block_overhead};
use crate::registry::{Audience, ReplicationRegistry};
use crate::stats::RoomNetStats;
use crate::transport::{Lane, LinkTx};
use crate::wire::Writer;
use crate::{FastMap, NetClientId, NetId, PlayerId, Tick};
use space3d_core::glam::Vec3;
use space3d_core::{SystemCtx, World};
use std::sync::Arc;
use std::time::Duration;

/// Per-client, per-entity replication state.
#[derive(Clone, Debug, Default)]
pub struct ClientEntity {
    /// Spawn delivered (acked / sent on a stream).
    pub spawned: bool,
    /// Client holds every component change up to this tick.
    pub ack_tick: Tick,
    /// Last tick included in a packet (a datagram spawn in flight is not resent before its
    /// ack or loss).
    pub last_sent_tick: Tick,
    /// Priority accumulator.
    pub acc: f32,
    /// Out of the leave radius since (interest hysteresis).
    pub leaving_since: Option<Tick>,
    /// Component presence the client is known to have (acked / sent on a stream).
    pub known_present: u64,
    /// Tick of the record that set `known_present`.
    pub known_tick: Tick,
    /// Consecutive snapshots in which the entity had due changes that the budget deferred.
    pub misses: u16,
    /// Controlled entity: snapshots since its unchanged predicted components were last
    /// re-confirmed (see `priority::CONFIRM_EVERY`).
    pub confirm_skipped: u16,
}

impl ClientEntity {
    /// Datagram ack of a record written at `tick` against `baseline`. Records are written
    /// against the acked baseline (every change after `ack_tick`), so a record acked while
    /// its baseline is still covered advances `ack_tick` to its tick.
    pub(crate) fn on_record_acked(&mut self, baseline: Tick, tick: Tick) {
        if tick > self.ack_tick && baseline <= self.ack_tick {
            self.ack_tick = tick;
        }
    }
}

/// Entities in one sent datagram packet (acks advance their `ack_tick`).
#[derive(Clone, Debug, Default)]
pub struct SentRecord {
    /// Packet sequence.
    pub seq: u16,
    /// Snapshot tick.
    pub tick: Tick,
    /// Entities written.
    pub entities: Vec<NetId>,
    /// Despawns written.
    pub despawns: Vec<NetId>,
    /// Presence written per entity (parallel to `entities`).
    pub present: Vec<u64>,
    /// Whether the entity was written as a spawn (parallel to `entities`).
    pub spawns: Vec<bool>,
    /// Baseline each entity was written against (parallel to `entities`).
    pub baselines: Vec<Tick>,
    /// Send time (loss detection, RTT).
    pub sent_at: Duration,
    /// Presumed lost (its entities became due again); a late ack still credits it and samples
    /// the RTT, so a long RTT raises the RTO instead of looking like permanent loss.
    pub lost: bool,
}

/// Per-client view (internal to the replication builder).
#[derive(Default)]
pub struct ClientView {
    /// Visible entities.
    pub entities: crate::FastMap<NetId, ClientEntity>,
    /// Datagram send history (256 entries).
    pub sent: Vec<SentRecord>,
    /// Next datagram sequence.
    pub next_seq: u16,
    /// Budget.
    pub budget: Option<super::priority::ByteBudget>,
    /// Every in-scope entity delivered once.
    pub sync_complete: bool,
    /// Next snapshot must carry `FULL_RESET`.
    pub full_reset: bool,
    /// Relevant set of the last interest evaluation (ascending).
    pub relevant: Vec<NetId>,
    /// Send ticks since the last interest evaluation.
    pub interest_age: u16,
    /// Datagram: despawns sent but not acked yet (`NetId` → out of scope).
    pub despawning: FastMap<NetId, bool>,
    /// Links bound so far (a second link means a resume → `FULL_RESET`).
    pub links_bound: u32,
}

impl ClientView {
    /// A new link replaces the old one: everything is resent (`FULL_RESET` after a resume).
    pub(crate) fn reset_for_new_link(&mut self) {
        self.entities.clear();
        self.sent.clear();
        self.despawning.clear();
        self.relevant.clear();
        self.interest_age = 0;
        self.budget = None;
        self.sync_complete = false;
        self.full_reset = self.links_bound > 0;
        self.links_bound += 1;
    }
}

/// Maximum `SentRecord`s kept per client.
const SENT_RING: usize = 256;
/// Stream packets (and snapshot parts) are cut at this size; a single entity record larger
/// than this still goes out alone, up to the link's `max_reliable`.
pub const STREAM_PACKET_BYTES: usize = 16 * 1024;
/// A top-priority candidate deferred this many snapshots in a row reserves the budget: lower
/// priorities wait until it fits (or the bucket is full and it goes out anyway), so an entity
/// larger than what the budget refills per snapshot is never starved by smaller ones.
const STARVE_RESERVE: u16 = 3;
/// Priority boost of the client's own (not controlled) entities.
const OWNED_WEIGHT: f32 = 4.0;
/// Datagram packet header allowance (header byte, seq, tick) plus block header.
const DATAGRAM_OVERHEAD: usize = 16;
/// Backlog above `backlog_kick_bytes` for this long → `SlowConsumer`.
const SLOW_CONSUMER_AFTER: Duration = Duration::from_secs(5);
/// Distance at which priority halves (m).
const PRIORITY_REFERENCE_DISTANCE: f32 = 25.0;

/// The `replication::server_send` system.
pub(crate) fn server_send(ctx: &mut SystemCtx) {
    let now = ctx.resources.init::<crate::time::NetClockHandle>().now();
    let world = &*ctx.world;
    ctx.resources.scope::<NetServer, _>(|res, server| {
        let Some(registry) = res.get::<ReplicationRegistry>() else {
            return;
        };
        if !registry.is_frozen() {
            return;
        }
        distribute_messages(server, registry);
        let interval = u32::from(server.config.send_interval.max(1));
        let send_tick = server.tick.0.is_multiple_of(interval);
        let mut kicks = Vec::new();
        let clients = server.slots.ids();
        for c in clients {
            if let Some(kick) = send_client(server, registry, world, c, send_tick, now) {
                kicks.push(kick);
            }
        }
        for (player, code, reason, resume) in kicks {
            server.kick(player, code, reason, resume);
        }
        server.flush_events(res);
    });
}

fn distribute_messages(server: &mut NetServer, registry: &ReplicationRegistry) {
    let queued = std::mem::take(&mut server.messages);
    for m in queued {
        let Some(entry) = registry.message_by_type(m.type_id) else {
            tracing::error!(message = m.type_name, "NetServer::send: message type not registered");
            continue;
        };
        if !entry.server_may_send() {
            tracing::error!(
                message = m.type_name,
                "NetServer::send: message is client-to-server only"
            );
            continue;
        }
        let mut w = Writer::with_capacity(m.bytes.len() + 3);
        w.varu16(entry.kind);
        w.raw(&m.bytes);
        let payload: Arc<[u8]> = w.into_vec().into();
        let reliable = entry.config.lane == Lane::Reliable;
        for c in server.recipients(&m.target) {
            if let Some(slot) = server.slots.get_mut(c) {
                if reliable {
                    slot.queue_reliable(ChannelId::MsgReliable, payload.clone());
                } else {
                    slot.queue_unreliable(ChannelId::MsgUnreliable, payload.clone());
                }
            }
        }
    }
}

type Kick = (PlayerId, DisconnectCode, &'static str, bool);

/// One entity chosen for this snapshot.
struct Pick {
    net_id: NetId,
    op: u8,
    comps: u64,
    removed: u64,
    allowed: u64,
    spawn: bool,
    /// The record holds every due change after this tick.
    baseline: Tick,
}

/// One snapshot part: encoded block, its entities, its despawns.
type Part = (Vec<u8>, Vec<Pick>, Vec<DespawnRecord>);

fn send_client(
    server: &mut NetServer,
    registry: &ReplicationRegistry,
    world: &World,
    client: NetClientId,
    send_tick: bool,
    now: Duration,
) -> Option<Kick> {
    let NetServer {
        slots,
        cache,
        config,
        visibility,
        rules,
        stats,
        tick,
        tick_started_at,
        ..
    } = server;
    let tick = *tick;
    let hz = slots.tick_hz.max(1.0);
    let slot = slots.get_mut(client)?;
    let (_, tx) = slot.link.clone()?;
    slot.rates.update(&mut slot.stats, now);
    let caps = tx.caps();
    let backlog = tx.backlog_bytes();
    let mut kick = None;
    let mut snapshot_due = send_tick;
    let interval = u32::from(config.send_interval.max(1));
    if send_tick {
        if backlog > config.backlog_kick_bytes {
            let since = *slot.backlog_high_since.get_or_insert(now);
            if now.saturating_sub(since) >= SLOW_CONSUMER_AFTER {
                kick = Some((
                    slot.identity.player,
                    DisconnectCode::SlowConsumer,
                    "downstream backlog",
                    true,
                ));
            }
        } else {
            slot.backlog_high_since = None;
        }
        if backlog > config.backlog_skip_bytes {
            snapshot_due = false;
            stats.snapshots_skipped += 1;
            slot.stats.snapshots_skipped += 1;
            slot.divider = (slot.divider * 2).min(8);
        } else {
            if backlog < config.backlog_skip_bytes / 2 {
                slot.divider = 1;
            }
            if !(tick.0 / interval).is_multiple_of(slot.divider.max(1)) {
                snapshot_due = false;
            }
        }
    }
    if slot.datagram {
        detect_losses(slot, now);
    }
    // Pongs.
    let step = 1.0 / hz;
    let frac = (now.saturating_sub(*tick_started_at).as_secs_f64() / step).clamp(0.0, 0.999_99);
    let mut unreliable: Vec<OutBlock> = Vec::new();
    for (id, t_us, at) in slot.pongs.drain(..) {
        let pong = TimeMsg::Pong(Pong {
            id,
            client_time_us: t_us,
            server_tick: tick,
            tick_frac: (frac * 65536.0) as u16,
            hold_us: now.saturating_sub(at).as_micros().min(u128::from(u32::MAX)) as u32,
        });
        if let Ok(b) = postcard::to_stdvec(&pong) {
            unreliable.push((ChannelId::Time, b.into()));
        }
    }
    if backlog <= config.backlog_skip_bytes {
        unreliable.append(&mut slot.out_unreliable);
    } else {
        slot.out_unreliable.clear();
    }
    let reliable: Vec<OutBlock> = std::mem::take(&mut slot.out_reliable);
    let mut snapshots: Vec<Part> = Vec::new();
    if snapshot_due {
        let max_block = if slot.datagram {
            caps.max_unreliable.saturating_sub(DATAGRAM_OVERHEAD).max(64)
        } else {
            caps.max_reliable
                .min(STREAM_PACKET_BYTES)
                .saturating_sub(DATAGRAM_OVERHEAD)
                .max(64)
        };
        let other = reliable
            .iter()
            .chain(unreliable.iter())
            .map(|b| b.1.len() + 4)
            .sum::<usize>();
        snapshots = build_snapshot(
            slot,
            cache,
            config,
            registry,
            world,
            visibility,
            rules,
            tick,
            interval * slot.divider.max(1),
            hz,
            max_block,
            other,
        );
        stats.snapshots_sent += 1;
    }
    if reliable.is_empty() && unreliable.is_empty() && snapshots.is_empty() {
        return kick;
    }
    let sent_bytes = if slot.datagram {
        send_datagram(slot, &*tx, tick, reliable, unreliable, snapshots, now, stats)
    } else {
        send_stream(slot, &*tx, tick, reliable, unreliable, snapshots, stats)
    };
    slot.stats.bytes_out += sent_bytes as u64;
    if slot.view.sync_complete && slot.state == SlotState::Syncing {
        slot.state = SlotState::Connected;
    }
    kick
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    slot: &mut Slot,
    cache: &ChangeCache,
    config: &ServerNetConfig,
    registry: &ReplicationRegistry,
    world: &World,
    visibility: &super::interest::VisibilityOverrides,
    rules: &[Box<dyn super::interest::VisibilityRule>],
    tick: Tick,
    dticks: u32,
    hz: f64,
    max_block: usize,
    other_bytes: usize,
) -> Vec<Part> {
    let player = slot.identity.player;
    let team = slot.identity.team;
    let datagram = slot.datagram;
    // Interest sources.
    let default_radius = match config.interest {
        InterestPolicy::Spatial { radius, .. } => radius,
        InterestPolicy::RoomWide => 0.0,
    };
    let mut sources: Vec<(Vec3, f32)> = cache
        .records
        .iter()
        .filter(|r| r.is_owned_by(player))
        .filter_map(|r| {
            let radius = r.interest_radius?;
            Some((r.pos?, if radius > 0.0 { radius } else { default_radius }))
        })
        .collect();
    if let Some((c, r)) = slot.interest_hint {
        sources.push((Vec3::from_array(c), r));
    }
    // Relevance (spatial policies re-evaluate every `interest_every` send ticks).
    let recompute = matches!(config.interest, InterestPolicy::RoomWide)
        || slot.view.interest_age == 0
        || slot.view.relevant.is_empty();
    if recompute {
        let view = &slot.view;
        let current = |id: NetId| {
            view.entities
                .get(&id)
                .is_some_and(|e| e.spawned || e.last_sent_tick > Tick::ZERO)
        };
        let viewer = Viewer {
            player,
            sources: &sources,
            current: &current,
        };
        slot.view.relevant = relevant_set(cache, config.interest, &viewer, visibility, rules, world);
    } else {
        let mut rel: Vec<NetId> = slot
            .view
            .relevant
            .iter()
            .copied()
            .filter(|id| cache.get(*id).is_some())
            .collect();
        for r in &cache.records {
            if r.is_owned_by(player) {
                rel.push(r.net_id);
            }
        }
        rel.sort_unstable();
        rel.dedup();
        slot.view.relevant = rel;
    }
    slot.view.interest_age = (slot.view.interest_age + 1) % config.interest_every.max(1);
    let relevant = slot.view.relevant.clone();
    // Despawns: in the view but no longer relevant / gone.
    let mut despawns: Vec<DespawnRecord> = Vec::new();
    let mut gone: Vec<NetId> = slot
        .view
        .entities
        .keys()
        .copied()
        .filter(|id| relevant.binary_search(id).is_err())
        .collect();
    gone.sort_unstable();
    for id in gone {
        let ce = slot.view.entities.remove(&id).expect("key");
        let ever_sent = ce.spawned || ce.last_sent_tick > Tick::ZERO;
        if ever_sent {
            let out_of_scope = cache.get(id).is_some();
            if datagram {
                slot.view.despawning.insert(id, out_of_scope);
            } else {
                despawns.push(DespawnRecord {
                    net_id: id,
                    out_of_scope,
                });
            }
        }
    }
    if datagram {
        let mut pending: Vec<(NetId, bool)> = slot.view.despawning.iter().map(|(k, v)| (*k, *v)).collect();
        pending.sort_unstable();
        for (id, oos) in pending {
            if relevant.binary_search(&id).is_ok() {
                // Back in scope before the despawn was acked: it will be respawned.
                slot.view.despawning.remove(&id);
                continue;
            }
            despawns.push(DespawnRecord {
                net_id: id,
                out_of_scope: oos,
            });
        }
    }
    // Due components per relevant entity.
    let predicted_mask = registry.predicted_mask();
    // Overspent last time (e.g. many controlled entities): thin out re-confirmations.
    let in_debt = slot
        .view
        .budget
        .as_ref()
        .is_some_and(super::priority::ByteBudget::in_debt);
    let mut always: Vec<Pick> = Vec::new();
    let mut candidates: Vec<(f32, Pick, usize)> = Vec::new();
    let mut all_spawned = true;
    for &id in &relevant {
        let Some(rec) = cache.get(id) else { continue };
        let ce = slot.view.entities.entry(id).or_default();
        let allowed = audience_mask(rec, registry, player, team, world);
        let own = rec.is_owned_by(player);
        let controlled = rec.controller == Some(player);
        // Always against the acked state: a datagram record then holds every change after
        // the ack, so it supersedes any older record still in flight (the client may skip a
        // reordered older record without losing anything).
        let baseline = ce.ack_tick;
        let (comps, removed, spawn) = if !ce.spawned {
            all_spawned = false;
            if datagram && ce.last_sent_tick > ce.ack_tick {
                continue; // spawn in flight
            }
            (allowed, 0, true)
        } else {
            let mut due = 0u64;
            for slot_c in &rec.comps {
                let bit = 1u64 << slot_c.kind;
                if allowed & bit != 0 && slot_c.changed_tick > baseline {
                    due |= bit;
                }
            }
            due |= allowed & !ce.known_present;
            if controlled {
                // Predicted components are confirmed every snapshot (the client compares them
                // with its prediction), changed or not — except while the budget is in debt,
                // when unchanged ones are re-confirmed every CONFIRM_EVERY snapshots.
                let predicted = allowed & predicted_mask;
                if !in_debt || due & predicted != 0 || ce.confirm_skipped + 1 >= super::priority::CONFIRM_EVERY {
                    due |= predicted;
                    ce.confirm_skipped = 0;
                } else {
                    ce.confirm_skipped += 1;
                }
            }
            (due, ce.known_present & !allowed, false)
        };
        if comps == 0 && removed == 0 && !spawn {
            continue;
        }
        let mut op_bits = if spawn { op::SPAWN } else { 0 };
        if controlled {
            op_bits |= op::CONTROLLED;
        }
        if own {
            op_bits |= op::OWNED;
        }
        let pick = Pick {
            net_id: id,
            op: op_bits,
            comps,
            removed,
            allowed,
            spawn,
            baseline: if spawn { Tick::ZERO } else { baseline },
        };
        let size = entity_size(rec, comps, removed);
        // Outside the budget: the controlled (predicted) entities, which need a confirmation
        // every snapshot, and bare removals. Everything else, the client's other own entities
        // included (boosted), competes for the budget.
        if controlled || (removed != 0 && comps == 0) {
            always.push(pick);
        } else {
            let weight = rec
                .comps
                .iter()
                .filter(|c| comps & (1u64 << c.kind) != 0)
                .filter_map(|c| registry.component(c.kind))
                .map(|e| e.priority)
                .fold(if spawn { 1.0f32 } else { 0.0 }, f32::max)
                * if own { OWNED_WEIGHT } else { 1.0 };
            let falloff = match (rec.pos, sources.is_empty()) {
                (Some(p), false) => sources
                    .iter()
                    .map(|(s, _)| distance_falloff(dist_xz(*s, p), PRIORITY_REFERENCE_DISTANCE))
                    .fold(0.0f32, f32::max),
                _ => 1.0,
            };
            ce.acc += weight.max(0.01) * rec.priority.max(0.01) * falloff * dticks as f32;
            candidates.push((ce.acc, pick, size));
        }
    }
    // Budget.
    let per_snapshot = f64::from(slot.down_rate) * f64::from(dticks) / hz;
    let mult = if slot.view.sync_complete { 2.0 } else { 4.0 };
    let budget = slot
        .view
        .budget
        .get_or_insert_with(|| ByteBudget::new(slot.down_rate, (per_snapshot * mult) as u32));
    budget.set_rate(slot.down_rate, (per_snapshot * mult).max(256.0) as u32);
    budget.refill(Duration::from_secs_f64(f64::from(dticks) / hz));
    let available = budget.available();
    // A full bucket lets the top candidate through even if it does not fit (the budget goes
    // into debt and the next refills pay it back): nothing is too large to ever be sent.
    let full = budget.is_full();
    let mut used = other_bytes + 8 + despawns.len() * 3;
    let mut picks: Vec<Pick> = Vec::new();
    for p in always {
        let rec = cache.get(p.net_id).expect("relevant");
        used += entity_size(rec, p.comps, p.removed);
        picks.push(p);
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.net_id.cmp(&b.1.net_id)));
    let mut misses = 0;
    let mut written_candidates = 0usize;
    let total_candidates = candidates.len();
    let mut rest = candidates.into_iter();
    for (_, mut pick, size) in rest.by_ref() {
        let top = written_candidates == 0 && misses == 0;
        let fits = used + size <= available;
        let Some(ce) = slot.view.entities.get_mut(&pick.net_id) else {
            continue;
        };
        // Guaranteed progress: however much the always-sent set takes, a top candidate that
        // waited STARVE_FORCE snapshots goes out.
        let forced = top && (full || (picks.is_empty() && available > 0) || ce.misses >= super::priority::STARVE_FORCE);
        if fits || forced {
            used += size;
            written_candidates += 1;
            ce.acc = 0.0;
            if ce.misses > 0 && !pick.spawn {
                // Changes were held back by the budget: the client must not treat the gap as
                // the entity having been at rest.
                pick.op |= op::DEFERRED;
            }
            ce.misses = 0;
            picks.push(pick);
        } else {
            ce.misses = ce.misses.saturating_add(1);
            let reserve = top && ce.misses > STARVE_RESERVE;
            misses += 1;
            if misses > 8 || reserve {
                break;
            }
        }
    }
    for (_, pick, _) in rest {
        if let Some(ce) = slot.view.entities.get_mut(&pick.net_id) {
            ce.misses = ce.misses.saturating_add(1);
        }
    }
    budget.consume(used);
    slot.stats.starved_entities = (total_candidates - written_candidates) as u32;
    slot.stats.budget_utilization = if available > 0 {
        (used as f32 / available as f32).min(4.0)
    } else {
        1.0
    };
    picks.sort_by_key(|p| p.net_id);
    // Sync completes when everything relevant is spawned (after this snapshot on streams).
    if !slot.view.sync_complete {
        let done = if datagram {
            all_spawned
        } else {
            relevant.iter().all(|id| {
                slot.view.entities.get(id).is_some_and(|e| e.spawned)
                    || picks
                        .binary_search_by_key(id, |p| p.net_id)
                        .is_ok_and(|i| picks[i].spawn)
            })
        };
        if done {
            slot.view.sync_complete = true;
        }
    }
    // Header.
    let mut hflags = 0u8;
    if slot.view.full_reset {
        hflags |= sflags::FULL_RESET;
    }
    if slot.view.sync_complete {
        hflags |= sflags::SYNC_COMPLETE;
        slot.view.full_reset = false;
    }
    let input_ack_delta = slot
        .inputs
        .consumed_real()
        .map_or(u32::MAX, |t| tick.0.saturating_sub(t.0));
    let header = SnapshotHeader {
        flags: hflags,
        input_ack_delta,
        input_slack: slot.inputs.take_slack_q2(),
    };
    // Encode (split at entity granularity).
    let mut out = Vec::new();
    let mut sw = SnapshotWriter::new();
    let mut part: Vec<Pick> = Vec::new();
    let mut first_limit = max_block.saturating_sub(if datagram { other_bytes } else { 0 }).max(64);
    let mut desp = std::mem::take(&mut despawns);
    for d in &desp {
        sw.despawn(*d);
    }
    for p in picks {
        let rec = cache.get(p.net_id).expect("picked");
        let cp = sw.begin_entity(p.net_id, p.op, p.comps, p.removed);
        for c in &rec.comps {
            if p.comps & (1u64 << c.kind) != 0 {
                sw.component(&c.bytes);
            }
        }
        if sw.len() > first_limit && sw.entity_count() > 1 {
            sw.rollback(cp);
            let mut w = Writer::new();
            // Only the last part completes the sync (a resumed client drops every entity not
            // re-confirmed by then).
            let part_header = SnapshotHeader {
                flags: header.flags & !sflags::SYNC_COMPLETE,
                ..header
            };
            std::mem::take(&mut sw).finish(part_header, &mut w);
            out.push((w.into_vec(), std::mem::take(&mut part), std::mem::take(&mut desp)));
            first_limit = max_block;
            sw.begin_entity(p.net_id, p.op, p.comps, p.removed);
            for c in &rec.comps {
                if p.comps & (1u64 << c.kind) != 0 {
                    sw.component(&c.bytes);
                }
            }
        }
        part.push(p);
    }
    let mut w = Writer::new();
    sw.finish(header, &mut w);
    out.push((w.into_vec(), part, desp));
    out
}

/// Exact-ish encoded size of an entity record (id varint upper bound).
fn entity_size(rec: &EntityRecord, comps: u64, removed: u64) -> usize {
    let mut n = crate::protocol::varint_len(u64::from(rec.net_id.0)) + 1 + crate::protocol::varint_len(comps);
    if removed != 0 {
        n += crate::protocol::varint_len(removed);
    }
    for c in &rec.comps {
        if comps & (1u64 << c.kind) != 0 {
            n += crate::protocol::varint_len(c.bytes.len() as u64) + c.bytes.len();
        }
    }
    n
}

/// Components of `rec` that `player` may receive.
pub(crate) fn audience_mask(
    rec: &EntityRecord,
    registry: &ReplicationRegistry,
    player: PlayerId,
    team: Option<u16>,
    world: &World,
) -> u64 {
    let mut mask = 0u64;
    for c in &rec.comps {
        let Some(entry) = registry.component(c.kind) else {
            continue;
        };
        let ok = match entry.audience {
            Audience::All => true,
            Audience::Owner => rec.owner == Some(player),
            Audience::ExceptOwner => rec.owner != Some(player),
            Audience::Team => rec.team.is_some() && rec.team == team,
            Audience::Custom(f) => f(world, rec.entity, player),
        };
        if ok {
            mask |= 1u64 << c.kind;
        }
    }
    mask
}

#[allow(clippy::too_many_arguments)]
fn send_stream(
    slot: &mut Slot,
    tx: &dyn LinkTx,
    tick: Tick,
    reliable: Vec<OutBlock>,
    unreliable: Vec<OutBlock>,
    snapshots: Vec<Part>,
    stats: &mut RoomNetStats,
) -> usize {
    let max = tx.caps().max_reliable;
    // Packets are cut at `STREAM_PACKET_BYTES`; only a single block larger than that (one big
    // entity) makes a bigger packet, up to the link's cap.
    let cut = max.min(STREAM_PACKET_BYTES);
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut dw = DataWriter::new(None, None, tick);
    let mut dropped_parts: Vec<usize> = Vec::new();
    let blocks = reliable
        .iter()
        .map(|(c, b)| (*c, &b[..], None))
        .chain(unreliable.iter().map(|(c, b)| (*c, &b[..], None)))
        .chain(
            snapshots
                .iter()
                .enumerate()
                .map(|(i, s)| (ChannelId::Snapshot, &s.0[..], Some(i))),
        );
    for (ch, payload, part) in blocks {
        let need = block_overhead(payload.len()) + payload.len();
        if !dw.is_empty() && dw.len() + need > cut {
            packets.push(std::mem::replace(&mut dw, DataWriter::new(None, None, tick)).finish());
        }
        if dw.len() + need > max {
            tracing::error!(
                room_client = ?slot.client,
                len = payload.len(),
                max,
                "dropping a block larger than the link's packet cap"
            );
            stats.oversize_dropped += 1;
            if let Some(i) = part {
                dropped_parts.push(i);
            }
            continue;
        }
        dw.block_bytes(ch, payload);
    }
    if !dw.is_empty() {
        packets.push(dw.finish());
    }
    let mut total = 0;
    let mut ok = true;
    for p in packets {
        total += p.len();
        stats.packets_out += 1;
        stats.bytes_out += p.len() as u64;
        if tx.send(Lane::Reliable, p).is_err() {
            ok = false;
        }
    }
    // Optimistic acks: the stream delivers everything or the link dies (then resume resends).
    // Entities of a dropped part were never sent: they stay due (and unspawned).
    if ok {
        for (i, (bytes, picks, _)) in snapshots.iter().enumerate() {
            if dropped_parts.contains(&i) {
                if picks.iter().any(|p| p.spawn) {
                    slot.view.sync_complete = false;
                }
                continue;
            }
            stats.snapshot_bytes += bytes.len() as u64;
            for p in picks {
                if let Some(ce) = slot.view.entities.get_mut(&p.net_id) {
                    ce.spawned = true;
                    ce.ack_tick = tick;
                    ce.last_sent_tick = tick;
                    ce.known_present = p.allowed;
                    ce.known_tick = tick;
                }
            }
        }
    }
    total
}

#[allow(clippy::too_many_arguments)]
fn send_datagram(
    slot: &mut Slot,
    tx: &dyn LinkTx,
    tick: Tick,
    reliable: Vec<OutBlock>,
    unreliable: Vec<OutBlock>,
    snapshots: Vec<Part>,
    now: Duration,
    stats: &mut RoomNetStats,
) -> usize {
    let caps = tx.caps();
    let mut total = 0;
    // Reliable blocks: the reliable stream, no sequence number.
    if !reliable.is_empty() {
        let mut dw = DataWriter::new(None, None, tick);
        for (ch, b) in &reliable {
            let need = block_overhead(b.len()) + b.len();
            if !dw.is_empty() && dw.len() + need > caps.max_reliable {
                let p = std::mem::replace(&mut dw, DataWriter::new(None, None, tick)).finish();
                total += p.len();
                stats.packets_out += 1;
                stats.bytes_out += p.len() as u64;
                let _ = tx.send(Lane::Reliable, p);
            }
            dw.block_bytes(*ch, b);
        }
        let p = dw.finish();
        total += p.len();
        stats.packets_out += 1;
        stats.bytes_out += p.len() as u64;
        let _ = tx.send(Lane::Reliable, p);
    }
    // Unreliable: small blocks ride with the first snapshot part.
    let mut extra: Vec<OutBlock> = unreliable;
    let parts: Vec<_> = if snapshots.is_empty() {
        vec![None]
    } else {
        snapshots.into_iter().map(Some).collect()
    };
    for part in parts {
        let seq = slot.view.next_seq;
        slot.view.next_seq = seq.wrapping_add(1);
        let mut dw = DataWriter::new(Some(seq), None, tick);
        for (ch, b) in extra.drain(..) {
            if dw.len() + block_overhead(b.len()) + b.len() <= caps.max_unreliable {
                dw.block_bytes(ch, &b);
            }
        }
        let mut rec = SentRecord {
            seq,
            tick,
            sent_at: now,
            ..Default::default()
        };
        if let Some((bytes, picks, desp)) = &part {
            stats.snapshot_bytes += bytes.len() as u64;
            dw.block_bytes(ChannelId::Snapshot, bytes);
            for p in picks {
                rec.entities.push(p.net_id);
                rec.present.push(p.allowed);
                rec.spawns.push(p.spawn);
                rec.baselines.push(p.baseline);
                if let Some(ce) = slot.view.entities.get_mut(&p.net_id) {
                    ce.last_sent_tick = tick;
                }
            }
            rec.despawns = desp.iter().map(|d| d.net_id).collect();
        }
        if dw.is_empty() {
            continue;
        }
        let p = dw.finish();
        total += p.len();
        stats.packets_out += 1;
        stats.bytes_out += p.len() as u64;
        let lane = if p.len() > caps.max_unreliable {
            Lane::Reliable
        } else {
            Lane::Unreliable
        };
        let _ = tx.send(lane, p);
        slot.view.sent.push(rec);
        if slot.view.sent.len() > SENT_RING {
            slot.view.sent.remove(0);
        }
    }
    total
}

/// Datagram: packets unacked for longer than the RTO are presumed lost; their entities become
/// due again (with current values, never stale bytes). Lost records stay in the ring (marked)
/// so a late ack still counts and samples the RTT.
fn detect_losses(slot: &mut Slot, now: Duration) {
    let rto = slot.rtt.rto().max(Duration::from_millis(50)) + Duration::from_millis(20);
    let Slot { view, .. } = slot;
    for r in view.sent.iter_mut().filter(|r| !r.lost) {
        if now.saturating_sub(r.sent_at) <= rto {
            continue;
        }
        r.lost = true;
        for id in &r.entities {
            // The lost changes are not covered by `ack_tick`: the next record carries every
            // change since then (records are written against the ack), and an unacked spawn
            // may be sent again.
            if let Some(ce) = view.entities.get_mut(id)
                && ce.ack_tick < r.tick
            {
                ce.last_sent_tick = ce.ack_tick;
            }
        }
    }
}

/// Datagram acks from a client packet header.
pub(crate) fn on_ack(server: &mut NetServer, client: NetClientId, ack: AckField, now: Duration) {
    let Some(slot) = server.slots.get_mut(client) else {
        return;
    };
    let mut newest: Option<Duration> = None;
    let mut acked = Vec::new();
    slot.view.sent.retain(|r| {
        if ack.acks(r.seq) {
            if r.seq == ack.ack {
                newest = Some(r.sent_at);
            }
            acked.push(r.clone());
            false
        } else {
            true
        }
    });
    if let Some(sent_at) = newest {
        slot.rtt.sample(now.saturating_sub(sent_at));
    }
    for r in acked {
        for (i, id) in r.entities.iter().enumerate() {
            if let Some(ce) = slot.view.entities.get_mut(id) {
                ce.on_record_acked(r.baselines[i], r.tick);
                if r.spawns[i] {
                    ce.spawned = true;
                }
                if r.tick >= ce.known_tick {
                    ce.known_present = r.present[i];
                    ce.known_tick = r.tick;
                }
            }
        }
        for id in &r.despawns {
            slot.view.despawning.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_acks_advance_from_the_acked_baseline() {
        let mut ce = ClientEntity::default();
        // Written against the ack (0): an ack advances directly, in any order.
        ce.on_record_acked(Tick(0), Tick(12));
        assert_eq!(ce.ack_tick, Tick(12));
        ce.on_record_acked(Tick(0), Tick(10));
        assert_eq!(ce.ack_tick, Tick(12), "older acks are ignored");
        ce.on_record_acked(Tick(12), Tick(20));
        assert_eq!(ce.ack_tick, Tick(20));
    }

    #[test]
    fn datagram_loss_forces_a_resend_since_the_ack() {
        use crate::server::{PlayerIdentity, ServerNetConfig};
        let config = ServerNetConfig::default();
        let mut slot = Slot::new(
            NetClientId(0),
            PlayerIdentity::dev("a"),
            Tick(0),
            &config,
            Duration::ZERO,
        );
        let id = NetId(1);
        slot.view.entities.insert(
            id,
            ClientEntity {
                spawned: true,
                ack_tick: Tick(4),
                last_sent_tick: Tick(12),
                ..Default::default()
            },
        );
        for (seq, tick) in [(1u16, 10u32), (2, 12)] {
            slot.view.sent.push(SentRecord {
                seq,
                tick: Tick(tick),
                entities: vec![id],
                present: vec![1],
                spawns: vec![false],
                baselines: vec![Tick(4)],
                sent_at: Duration::ZERO,
                ..Default::default()
            });
        }
        // Both presumed lost: everything since the ack becomes due again; the records stay
        // (marked) so a late ack still counts.
        detect_losses(&mut slot, Duration::from_secs(5));
        assert!(slot.view.sent.iter().all(|r| r.lost));
        assert_eq!(slot.view.entities[&id].last_sent_tick, Tick(4));
    }
}
