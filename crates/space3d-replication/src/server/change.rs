//! Encode-once change detection (`replication::server_collect`, send ticks only).
//!
//! hecs has no change ticks (and `hecs::ChangeTracker` allows one tracker per world per type),
//! so every replicated component of every `Replicated` entity is encoded with its codec once
//! per send tick and compared with the cached bytes; a difference sets `changed_tick`.
//! Comparing *quantized* bytes suppresses sub-quantum jitter for free, and the cached bytes are
//! copied into every client's packet (no per-client re-encoding). Presence differences become
//! insert/remove; entities that disappeared (or lost `Replicated`) become despawn tombstones.
//!
//! Every tick (not only send ticks) components with `quantize_authoritative` are rounded through
//! their codec first, so the server simulates exactly the values clients predict from.

use super::NetServer;
use crate::mapping::{DecodeCtx, EncodeCtx, NetIdMap};
use crate::registry::{ComponentKind, ReplicationRegistry, SendRule};
use crate::wire::Writer;
use crate::{
    ControlledBy, FastMap, InterestSource, NetId, NetPriority, NetRelevance, NetTeam, Owner, PlayerId, Replicated, Tick,
};
use smallvec::SmallVec;
use space3d_core::{Entity, GlobalTransform, Parent, SystemCtx, Transform, World};

/// Cached encoding of one component of one entity.
#[derive(Clone, Debug, Default)]
pub struct CompSlot {
    /// Kind.
    pub kind: u16,
    /// Last tick the encoded bytes changed.
    pub changed_tick: Tick,
    /// Encoded bytes (codec output).
    pub bytes: SmallVec<[u8; 24]>,
}

/// One replicated entity.
#[derive(Clone, Debug)]
pub struct EntityRecord {
    /// ECS entity.
    pub entity: Entity,
    /// Wire id.
    pub net_id: NetId,
    /// Tick the record was created.
    pub spawn_tick: Tick,
    /// Presence bitmask over `ComponentKind`s.
    pub present: u64,
    /// Last insert/remove tick.
    pub structure_tick: Tick,
    /// Kinds removed at `structure_tick` (for `HAS_REMOVED`).
    pub removed: u64,
    /// Encoded components (ascending kind).
    pub comps: SmallVec<[CompSlot; 6]>,
    /// Owner / controller (for audience + priority).
    pub owner: Option<crate::PlayerId>,
    /// Controller.
    pub controller: Option<crate::PlayerId>,
    /// Team.
    pub team: Option<u16>,
    /// World position for spatial interest (from `Transform`).
    pub pos: Option<space3d_core::glam::Vec3>,
    /// Priority multiplier.
    pub priority: f32,
    /// Interest override.
    pub relevance: Option<NetRelevance>,
    /// Replicated parent (`Parent`), if any.
    pub parent: Option<NetId>,
    /// Hierarchy root (itself for roots): children inherit its interest.
    pub root: NetId,
    /// `InterestSource` radius when the entity is a viewer.
    pub interest_radius: Option<f32>,
    /// Collect pass that last saw the entity.
    pub(crate) seen: u64,
}

impl EntityRecord {
    /// Cached slot of a kind.
    pub fn comp(&self, kind: u16) -> Option<&CompSlot> {
        self.comps
            .binary_search_by_key(&kind, |c| c.kind)
            .ok()
            .map(|i| &self.comps[i])
    }

    /// Whether `player` owns or controls it.
    pub fn is_owned_by(&self, player: PlayerId) -> bool {
        self.owner == Some(player) || self.controller == Some(player)
    }
}

/// Record table + tombstones for the room (internal to the replication builder).
#[derive(Default)]
pub struct ChangeCache {
    /// Records in `NetId` order.
    pub records: Vec<EntityRecord>,
    /// `(net_id, despawn tick)`; pruned after the resume window (informational: per-client
    /// views notice despawns by the record disappearing).
    pub tombstones: Vec<(NetId, Tick)>,
    /// `NetId` → index in `records`.
    pub(crate) index: FastMap<NetId, usize>,
    /// Tick of the last collect.
    pub last_collect: Tick,
    /// Spatial grid of positioned records (XZ).
    pub(crate) grid: super::interest::SpatialGrid,
    /// Entities whose records the last collect retired (despawned / no longer replicated).
    pub(crate) retired: Vec<Entity>,
    generation: u64,
    scratch: Writer,
}

impl ChangeCache {
    /// Run collection for `tick` over the room world (see the module docs).
    pub fn collect(&mut self, tick: Tick, world: &mut World, registry: &ReplicationRegistry, ids: &mut NetIdMap) {
        self.generation += 1;
        let generation = self.generation;
        self.last_collect = tick;
        let entities: Vec<Entity> = world.query::<(Entity, &Replicated)>().iter().map(|(e, _)| e).collect();
        // 1. NetIds first, so entity references resolve within this pass. New entities are
        //    numbered in entity-index order (≈ spawn order, independent of archetype layout),
        //    so parents spawned before their children get the smaller id and arrive first.
        let mut fresh: Vec<Entity> = entities.iter().copied().filter(|e| ids.net_id(*e).is_none()).collect();
        fresh.sort_unstable_by_key(|e| (e.id(), e.to_bits()));
        // Entities mapped before their first collect (checkpoint restore pre-assigns the
        // `NetId`s it captured) are adopted: they need records like fresh ones.
        let mut adopted: Vec<NetId> = entities
            .iter()
            .filter_map(|e| ids.net_id(*e))
            .filter(|id| !self.index.contains_key(id))
            .collect();
        adopted.sort_unstable();
        let unsorted = match (adopted.first(), self.records.last()) {
            (Some(first), Some(last)) => *first < last.net_id,
            _ => false,
        };
        let new_ids: Vec<NetId> = fresh.into_iter().map(|e| ids.allocate(e)).collect();
        // 2. Records (new ids are the largest, so appending keeps `records` sorted; adopted ids
        //    are older than every allocated one and only need a re-sort when records exist).
        for id in adopted.into_iter().chain(new_ids) {
            let entity = ids.entity(id).expect("just allocated");
            self.index.insert(id, self.records.len());
            self.records.push(EntityRecord {
                entity,
                net_id: id,
                spawn_tick: tick,
                present: 0,
                structure_tick: tick,
                removed: 0,
                comps: SmallVec::new(),
                owner: None,
                controller: None,
                team: None,
                pos: None,
                priority: 1.0,
                relevance: None,
                parent: None,
                root: id,
                interest_radius: None,
                seen: 0,
            });
        }
        if unsorted {
            self.records.sort_unstable_by_key(|r| r.net_id);
            self.reindex();
        }
        let ctx = EncodeCtx::new(ids);
        for &e in &entities {
            let Some(id) = ids.net_id(e) else { continue };
            let Some(&idx) = self.index.get(&id) else { continue };
            let rec = &mut self.records[idx];
            rec.seen = generation;
            // Metadata.
            let controller = world.get::<&ControlledBy>(e).ok().map(|c| c.0);
            rec.controller = controller;
            rec.owner = world.get::<&Owner>(e).ok().map(|o| o.0).or(controller);
            rec.team = world.get::<&NetTeam>(e).ok().map(|t| t.0);
            rec.priority = world.get::<&NetPriority>(e).ok().map_or(1.0, |p| p.0.max(0.0));
            rec.relevance = world.get::<&NetRelevance>(e).ok().map(|r| *r);
            rec.interest_radius = world.get::<&InterestSource>(e).ok().map(|s| s.radius);
            rec.parent = world.get::<&Parent>(e).ok().and_then(|p| ids.net_id(p.0));
            let old_pos = rec.pos;
            rec.pos = world
                .get::<&GlobalTransform>(e)
                .ok()
                .map(|g| g.0.w_axis.truncate())
                .or_else(|| world.get::<&Transform>(e).ok().map(|t| t.translation));
            if old_pos != rec.pos {
                self.grid.update(id.0, old_pos, rec.pos);
            }
            // Components.
            let mut present = 0u64;
            for entry in registry.components() {
                let kind = entry.kind.expect("frozen").0;
                let bit = 1u64 << kind;
                let pos = rec.comps.binary_search_by_key(&kind, |c| c.kind);
                if let (SendRule::Interval(n), Ok(_)) = (entry.send, pos)
                    && n > 1
                    && !tick.0.is_multiple_of(u32::from(n))
                {
                    if entry.ops.contains(world, e) {
                        present |= bit;
                        continue;
                    }
                } else {
                    self.scratch.clear();
                    if entry.ops.encode(world, e, &mut self.scratch, &ctx) {
                        present |= bit;
                        let bytes = self.scratch.as_slice();
                        match pos {
                            Ok(i) => {
                                let slot = &mut rec.comps[i];
                                if slot.bytes.as_slice() != bytes || entry.send == SendRule::Always {
                                    slot.bytes.clear();
                                    slot.bytes.extend_from_slice(bytes);
                                    slot.changed_tick = tick;
                                }
                            }
                            Err(i) => rec.comps.insert(
                                i,
                                CompSlot {
                                    kind,
                                    changed_tick: tick,
                                    bytes: SmallVec::from_slice(bytes),
                                },
                            ),
                        }
                        continue;
                    }
                }
                // Absent now.
                if let Ok(i) = pos {
                    rec.comps.remove(i);
                }
            }
            if present != rec.present {
                rec.removed = rec.present & !present;
                rec.structure_tick = tick;
                rec.present = present;
            }
        }
        // 3. Despawns.
        let before = self.records.len();
        let mut gone = Vec::new();
        self.records.retain(|r| {
            let keep = r.seen == generation;
            if !keep {
                gone.push((r.net_id, r.entity, r.pos));
            }
            keep
        });
        if self.records.len() != before {
            self.reindex();
        }
        self.retired.clear();
        for (id, entity, pos) in gone {
            self.retired.push(entity);
            ids.remove(entity);
            self.grid.update(id.0, pos, None);
            self.tombstones.push((id, tick));
        }
        // Pre-mapped (restored) entities that were despawned before their first collect.
        if ids.len() != self.records.len() {
            let index = &self.index;
            ids.retain(|id| index.contains_key(&id));
        }
        // 4. Hierarchy roots.
        for i in 0..self.records.len() {
            let mut root = self.records[i].net_id;
            let mut cur = self.records[i].parent;
            for _ in 0..32 {
                let Some(p) = cur else { break };
                let Some(&pi) = self.index.get(&p) else { break };
                root = p;
                cur = self.records[pi].parent;
            }
            self.records[i].root = root;
        }
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (i, r) in self.records.iter().enumerate() {
            self.index.insert(r.net_id, i);
        }
    }

    /// Record of a `NetId`.
    pub fn get(&self, id: NetId) -> Option<&EntityRecord> {
        self.index.get(&id).map(|&i| &self.records[i])
    }

    /// Kind helper.
    pub fn has(&self, id: NetId, kind: ComponentKind) -> bool {
        self.get(id).is_some_and(|r| r.present & (1u64 << kind.0) != 0)
    }

    /// Drop tombstones older than `horizon`.
    pub fn prune_tombstones(&mut self, horizon: Tick) {
        self.tombstones.retain(|(_, t)| *t >= horizon);
    }
}

/// Round every `quantize_authoritative` component of every replicated entity through its codec.
pub(crate) fn quantize_all(world: &mut World, registry: &ReplicationRegistry, ids: &NetIdMap) {
    let mask = registry.quantized_mask();
    if mask == 0 {
        return;
    }
    let entities: Vec<Entity> = world.query::<(Entity, &Replicated)>().iter().map(|(e, _)| e).collect();
    let enc = EncodeCtx::new(ids);
    let mut dec = DecodeCtx::server(ids);
    let mut scratch = Writer::new();
    for entry in registry.components().iter().filter(|c| c.quantize_authoritative) {
        for &e in &entities {
            entry.ops.quantize(world, e, &enc, &mut dec, &mut scratch);
        }
    }
}

/// The `replication::server_collect` system (Render): authoritative quantization every tick,
/// change collection on send ticks.
pub(crate) fn server_collect(ctx: &mut SystemCtx) {
    let world = &mut *ctx.world;
    ctx.resources.scope::<NetServer, _>(|res, server| {
        let Some(registry) = res.get::<ReplicationRegistry>() else {
            return;
        };
        if !registry.is_frozen() {
            return;
        }
        quantize_all(world, registry, &server.ids);
        let interval = u32::from(server.config.send_interval.max(1));
        if server.tick.0.is_multiple_of(interval) {
            let tick = server.tick;
            server.cache.collect(tick, world, registry, &mut server.ids);
            for e in &server.cache.retired {
                server.visibility.forget_entity(*e);
            }
            // Overrides of entities that were never replicated (despawned before a collect).
            if server.visibility.len() > 0 && (tick.0 / interval).is_multiple_of(64) {
                server.visibility.retain_entities(|e| world.contains(e));
            }
            let window = (server.config.resume_window.as_secs_f64() * server.tick_hz().max(1.0)) as u32;
            server.cache.prune_tombstones(tick.saturating_sub(window));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ComponentRule;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Hp(u32);

    fn registry() -> ReplicationRegistry {
        let mut r = ReplicationRegistry::default();
        r.register_component::<Hp>("hp", ComponentRule::serde());
        r.register_component::<Transform>(
            "core.transform",
            ComponentRule::new(crate::quant::TransformCodec::default()).predict_and_interpolate(),
        );
        r.freeze();
        r
    }

    #[test]
    fn collect_tracks_changes_presence_and_despawns() {
        let reg = registry();
        let mut world = World::new();
        let mut ids = NetIdMap::default();
        let mut cache = ChangeCache::default();
        let a = world.spawn((Replicated, Hp(1), Transform::from_xyz(1.0, 0.0, 0.0)));
        let b = world.spawn((Replicated, Hp(2)));
        let _not_replicated = world.spawn((Hp(3),));
        // Kinds are assigned by sorted name: "core.transform" = 0, "hp" = 1.
        let (tf, hp) = (0u16, 1u16);
        assert_eq!(reg.kind_by_name("core.transform"), Some(ComponentKind(tf)));
        assert_eq!(reg.kind_by_name("hp"), Some(ComponentKind(hp)));
        cache.collect(Tick(2), &mut world, &reg, &mut ids);
        assert_eq!(cache.records.len(), 2);
        let ra = cache.get(ids.net_id(a).unwrap()).unwrap();
        assert_eq!(ra.present, 0b11);
        assert_eq!(ra.comp(tf).unwrap().changed_tick, Tick(2));
        // Sub-quantum jitter does not count as a change.
        world.get::<&mut Transform>(a).unwrap().translation.x += 0.00001;
        world.get::<&mut Hp>(b).unwrap().0 = 20;
        cache.collect(Tick(4), &mut world, &reg, &mut ids);
        let ra = cache.get(ids.net_id(a).unwrap()).unwrap();
        assert_eq!(ra.comp(tf).unwrap().changed_tick, Tick(2));
        assert_eq!(ra.comp(hp).unwrap().changed_tick, Tick(2));
        let rb = cache.get(ids.net_id(b).unwrap()).unwrap();
        assert_eq!(rb.comp(hp).unwrap().changed_tick, Tick(4));
        // A real move is a change.
        world.get::<&mut Transform>(a).unwrap().translation.x += 0.5;
        cache.collect(Tick(5), &mut world, &reg, &mut ids);
        let ra = cache.get(ids.net_id(a).unwrap()).unwrap();
        assert_eq!(ra.comp(tf).unwrap().changed_tick, Tick(5));
        // Removal.
        world.remove_one::<Hp>(a).unwrap();
        cache.collect(Tick(6), &mut world, &reg, &mut ids);
        let ra = cache.get(ids.net_id(a).unwrap()).unwrap();
        assert_eq!(ra.present, 1 << tf);
        assert_eq!(ra.removed, 1 << hp);
        assert_eq!(ra.structure_tick, Tick(6));
        assert!(ra.comp(hp).is_none());
        // Despawn → tombstone, id forgotten, never reused.
        let bid = ids.net_id(b).unwrap();
        world.despawn(b).unwrap();
        cache.collect(Tick(8), &mut world, &reg, &mut ids);
        assert!(cache.get(bid).is_none());
        assert_eq!(cache.tombstones, vec![(bid, Tick(8))]);
        let c = world.spawn((Replicated,));
        cache.collect(Tick(10), &mut world, &reg, &mut ids);
        assert!(ids.net_id(c).unwrap() > bid);
        assert!(cache.records.windows(2).all(|w| w[0].net_id < w[1].net_id));
    }

    #[test]
    fn premapped_entities_get_records() {
        // Checkpoint restore maps entities to their captured ids before the first collect.
        let reg = registry();
        let mut world = World::new();
        let mut ids = NetIdMap::default();
        let mut cache = ChangeCache::default();
        let live = world.spawn((Replicated, Hp(1)));
        let gone = world.spawn((Replicated, Hp(2)));
        let late = world.spawn((Replicated, Hp(3)));
        ids.insert(NetId(7), live);
        ids.insert(NetId(3), gone);
        ids.set_next_id(9);
        world.despawn(gone).unwrap();
        cache.collect(Tick(2), &mut world, &reg, &mut ids);
        let ids_of: Vec<NetId> = cache.records.iter().map(|r| r.net_id).collect();
        assert_eq!(ids_of, vec![NetId(7), NetId(9)], "adopted + fresh, sorted");
        assert_eq!(ids.net_id(late), Some(NetId(9)));
        assert_eq!(ids.entity(NetId(3)), None, "dead pre-mapped entity forgotten");
        assert_eq!(cache.get(NetId(7)).unwrap().comp(1).unwrap().changed_tick, Tick(2));
        // Adopting below existing records keeps `records` sorted and indexed.
        let older = world.spawn((Replicated, Hp(4)));
        ids.insert(NetId(5), older);
        cache.collect(Tick(4), &mut world, &reg, &mut ids);
        let ids_of: Vec<NetId> = cache.records.iter().map(|r| r.net_id).collect();
        assert_eq!(ids_of, vec![NetId(5), NetId(7), NetId(9)]);
        assert_eq!(cache.get(NetId(5)).unwrap().entity, older);
    }

    #[test]
    fn children_inherit_root() {
        let reg = registry();
        let mut world = World::new();
        let mut ids = NetIdMap::default();
        let mut cache = ChangeCache::default();
        let root = world.spawn((Replicated,));
        let child = world.spawn((Replicated, Parent(root)));
        let grandchild = world.spawn((Replicated, Parent(child)));
        cache.collect(Tick(2), &mut world, &reg, &mut ids);
        let rid = ids.net_id(root).unwrap();
        assert_eq!(cache.get(ids.net_id(grandchild).unwrap()).unwrap().root, rid);
        assert_eq!(cache.get(ids.net_id(child).unwrap()).unwrap().parent, Some(rid));
    }

    #[test]
    fn quantize_all_rounds_predicted_components() {
        let reg = registry();
        let mut world = World::new();
        let ids = NetIdMap::default();
        let e = world.spawn((Replicated, Transform::from_xyz(1.00049, 0.0, 0.0)));
        quantize_all(&mut world, &reg, &ids);
        assert_eq!(world.get::<&Transform>(e).unwrap().translation.x, 1.0);
    }
}
