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

    /// Allocate a fresh id for `entity` (never reused within the epoch). Returns the existing id
    /// if the entity is already mapped. Ids start at 1.
    pub fn allocate(&mut self, entity: Entity) -> NetId {
        if let Some(id) = self.to_net.get(&entity) {
            return *id;
        }
        self.next = self.next.max(1);
        let id = NetId(self.next);
        self.next += 1;
        self.to_net.insert(entity, id);
        self.to_entity.insert(id, entity);
        id
    }

    /// Map `entity` to a specific id (checkpoint restore). Bumps the counter past `id`.
    pub fn insert(&mut self, id: NetId, entity: Entity) {
        if let Some(old) = self.to_entity.insert(id, entity) {
            self.to_net.remove(&old);
        }
        if let Some(old) = self.to_net.insert(entity, id)
            && old != id
        {
            self.to_entity.remove(&old);
        }
        self.next = self.next.max(id.0.saturating_add(1));
    }

    /// Forget a despawned entity.
    pub fn remove(&mut self, entity: Entity) -> Option<NetId> {
        let id = self.to_net.remove(&entity)?;
        self.to_entity.remove(&id);
        Some(id)
    }

    /// Next id [`Self::allocate`] would hand out.
    pub fn next_id(&self) -> u32 {
        self.next.max(1)
    }

    /// Continue allocating from `next` (checkpoint restore).
    pub fn set_next_id(&mut self, next: u32) {
        self.next = self.next.max(next);
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
        if let Some(old) = self.to_local.insert(id, entity) {
            self.to_net.remove(&old);
        }
        self.to_net.insert(entity, id);
    }

    /// Forget a despawn; returns the local entity.
    pub fn remove(&mut self, id: NetId) -> Option<Entity> {
        let e = self.to_local.remove(&id)?;
        self.to_net.remove(&e);
        Some(e)
    }

    /// Drop everything (full reset after a new room epoch).
    pub fn clear(&mut self) {
        self.to_local.clear();
        self.to_net.clear();
    }

    /// All mapped `NetId`s in ascending order.
    pub fn net_ids(&self) -> Vec<NetId> {
        let mut v: Vec<NetId> = self.to_local.keys().copied().collect();
        v.sort_unstable();
        v
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

#[derive(Clone, Copy)]
enum EncodeMap<'a> {
    None,
    Server(&'a NetIdMap),
    Client(&'a EntityMap),
}

/// Encode-side context: `Entity` → `NetId` for entity references.
#[derive(Clone, Copy)]
pub struct EncodeCtx<'a> {
    ids: EncodeMap<'a>,
}

impl<'a> EncodeCtx<'a> {
    /// Context over a server map.
    pub fn new(ids: &'a NetIdMap) -> Self {
        Self {
            ids: EncodeMap::Server(ids),
        }
    }

    /// Context over a client map (prediction history, client-to-server messages).
    pub fn client(map: &'a EntityMap) -> Self {
        Self {
            ids: EncodeMap::Client(map),
        }
    }

    /// Context without mappings (every reference encodes as `None`).
    pub fn empty() -> EncodeCtx<'static> {
        EncodeCtx { ids: EncodeMap::None }
    }

    /// Wire id of a referenced entity (`None` if it is not replicated).
    pub fn net_id(&self, entity: Entity) -> Option<NetId> {
        match self.ids {
            EncodeMap::None => None,
            EncodeMap::Server(m) => m.net_id(entity),
            EncodeMap::Client(m) => m.net_id(entity),
        }
    }
}

#[derive(Clone, Copy)]
enum DecodeMap<'a> {
    None,
    Client(&'a EntityMap),
    Server(&'a NetIdMap),
}

/// Decode-side context: `NetId` → local `Entity`; records unresolved references.
pub struct DecodeCtx<'a> {
    map: DecodeMap<'a>,
    unresolved: bool,
}

impl<'a> DecodeCtx<'a> {
    /// Context over a client map.
    pub fn new(map: &'a EntityMap) -> Self {
        Self {
            map: DecodeMap::Client(map),
            unresolved: false,
        }
    }

    /// Context over a server map (authoritative quantization, checkpoint restore).
    pub fn server(map: &'a NetIdMap) -> Self {
        Self {
            map: DecodeMap::Server(map),
            unresolved: false,
        }
    }

    /// Context without mappings (every reference is unresolved).
    pub fn empty() -> DecodeCtx<'static> {
        DecodeCtx {
            map: DecodeMap::None,
            unresolved: false,
        }
    }

    /// Local entity for `id`; marks the decode as unresolved when unknown.
    pub fn entity(&mut self, id: NetId) -> Option<Entity> {
        let e = match self.map {
            DecodeMap::None => None,
            DecodeMap::Client(m) => m.entity(id),
            DecodeMap::Server(m) => m.entity(id),
        };
        if e.is_none() {
            self.unresolved = true;
        }
        e
    }

    /// Whether any reference failed to resolve.
    pub fn has_unresolved(&self) -> bool {
        self.unresolved
    }

    /// Clear the unresolved flag (reuse the context for the next value).
    pub fn reset(&mut self) {
        self.unresolved = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::World;

    #[test]
    fn net_ids_are_never_reused() {
        let mut w = World::new();
        let a = w.spawn(());
        let b = w.spawn(());
        let mut m = NetIdMap::default();
        let ia = m.allocate(a);
        assert_eq!(ia, NetId(1));
        assert_eq!(m.allocate(a), ia);
        m.remove(a);
        let ib = m.allocate(b);
        assert_eq!(ib, NetId(2));
        assert_eq!(m.entity(ib), Some(b));
        assert_eq!(m.entity(ia), None);
        m.insert(NetId(10), a);
        assert_eq!(m.allocate(w.spawn(())), NetId(11));
    }

    #[test]
    fn decode_ctx_marks_unresolved() {
        let mut w = World::new();
        let e = w.spawn(());
        let mut map = EntityMap::default();
        map.insert(NetId(3), e);
        let mut ctx = DecodeCtx::new(&map);
        assert_eq!(ctx.entity(NetId(3)), Some(e));
        assert!(!ctx.has_unresolved());
        assert_eq!(ctx.entity(NetId(4)), None);
        assert!(ctx.has_unresolved());
        assert_eq!(EncodeCtx::client(&map).net_id(e), Some(NetId(3)));
        assert_eq!(map.remove(NetId(3)), Some(e));
        assert!(map.is_empty());
    }
}
