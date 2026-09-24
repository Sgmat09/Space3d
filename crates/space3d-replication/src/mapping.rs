//! Entity identity mapping: server `Entity` ↔ [`NetId`] ↔ client `Entity`.
//!
//! * Server: [`NetIdMap`] — `NetId`s come from a per-room counter and are never reused within
//!   a room epoch (a restored room starts a new epoch and clients fully resync).
//! * Client: [`EntityMap`] — filled by snapshot `Spawn` ops, emptied by `Despawn`/`Leave`.
//! * Entity references inside components/messages are encoded through [`EncodeCtx`] and
//!   decoded through [`DecodeCtx`]; a reference to a `NetId` the client does not know yet makes
//!   the value *pending* (retried after each snapshot; dropped with a warning after 1 s or when
//!   the holder despawns).

use crate::FastMap as HashMap;
use crate::NetId;
use space3d_core::Entity;

/// Server-side bidirectional map (resource owned by `NetServer`).
#[derive(Debug, Default)]
pub struct NetIdMap {
    to_net: HashMap<Entity, NetId>,
    to_entity: HashMap<NetId, Entity>,
    next: u32,
}

impl NetIdMap {
    /// `NetId` of a replicated entity.
    pub fn net_id(&self, entity: Entity) -> Option<NetId> {
        self.to_net.get(&entity).copied()
    }

    /// Entity of a `NetId`.
    pub fn entity(&self, id: NetId) -> Option<Entity> {
        self.to_entity.get(&id).copied()
    }

    /// Allocate a fresh id for `entity` (never reused within the epoch).
    pub fn allocate(&mut self, entity: Entity) -> NetId {
        todo!()
    }

    /// Forget a despawned entity.
    pub fn remove(&mut self, entity: Entity) -> Option<NetId> {
        todo!()
    }

    /// Number of mapped entities.
    pub fn len(&self) -> usize {
        self.to_net.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.to_net.is_empty()
    }
}

/// Client-side bidirectional map (resource owned by `NetClient`).
#[derive(Debug, Default)]
pub struct EntityMap {
    to_local: HashMap<NetId, Entity>,
    to_net: HashMap<Entity, NetId>,
}

impl EntityMap {
    /// Local entity of a `NetId`.
    pub fn entity(&self, id: NetId) -> Option<Entity> {
        self.to_local.get(&id).copied()
    }

    /// `NetId` of a local replicated entity.
    pub fn net_id(&self, entity: Entity) -> Option<NetId> {
        self.to_net.get(&entity).copied()
    }

    /// Record a spawn.
    pub fn insert(&mut self, id: NetId, entity: Entity) {
        todo!()
    }

    /// Forget a despawn; returns the local entity.
    pub fn remove(&mut self, id: NetId) -> Option<Entity> {
        todo!()
    }

    /// Drop everything (full reset after a new room epoch).
    pub fn clear(&mut self) {
        self.to_local.clear();
        self.to_net.clear();
    }

    /// Number of mapped entities.
    pub fn len(&self) -> usize {
        self.to_local.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.to_local.is_empty()
    }
}

/// Encode-side context: `Entity` → `NetId` for entity references.
#[derive(Clone, Copy)]
pub struct EncodeCtx<'a> {
    ids: Option<&'a NetIdMap>,
}

impl<'a> EncodeCtx<'a> {
    /// Context over a server map.
    pub fn new(ids: &'a NetIdMap) -> Self {
        Self { ids: Some(ids) }
    }

    /// Context without mappings (every reference encodes as `None`).
    pub fn empty() -> EncodeCtx<'static> {
        EncodeCtx { ids: None }
    }

    /// Wire id of a referenced entity (`None` if it is not replicated).
    pub fn net_id(&self, entity: Entity) -> Option<NetId> {
        self.ids.and_then(|m| m.net_id(entity))
    }
}

/// Decode-side context: `NetId` → local `Entity`; records unresolved references.
pub struct DecodeCtx<'a> {
    map: Option<&'a EntityMap>,
    unresolved: bool,
}

impl<'a> DecodeCtx<'a> {
    /// Context over a client map.
    pub fn new(map: &'a EntityMap) -> Self {
        Self {
            map: Some(map),
            unresolved: false,
        }
    }

    /// Context without mappings (every reference is unresolved).
    pub fn empty() -> DecodeCtx<'static> {
        DecodeCtx {
            map: None,
            unresolved: false,
        }
    }

    /// Local entity for `id`; marks the decode as unresolved when unknown.
    pub fn entity(&mut self, id: NetId) -> Option<Entity> {
        let e = self.map.and_then(|m| m.entity(id));
        if e.is_none() {
            self.unresolved = true;
        }
        e
    }

    /// Whether any reference failed to resolve.
    pub fn has_unresolved(&self) -> bool {
        self.unresolved
    }
}
