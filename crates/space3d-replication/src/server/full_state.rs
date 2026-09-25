//! Full-state encoding of a room, used for **checkpoints** (crash recovery, graceful restart)
//! and for tests. Reuses the replication codecs, so it is compact and fast enough to run on the
//! room thread (bytes are then compressed/uploaded off-thread by `space3d-server`).
//!
//! Server-only state (AI, timers in resources) is out of scope here: the server crate adds it
//! with `space3d_scene::ComponentRegistry` and a game resource hook.

use super::NetServer;
use crate::mapping::{DecodeCtx, EncodeCtx, NetIdMap};
use crate::registry::ReplicationRegistry;
use crate::wire::{DecodeError, EncodeError, Writer};
use crate::{
    ControlledBy, InterestSource, NetId, NetPriority, NetRelevance, NetTeam, Owner, Replicated, Simulated, Tick,
};
use serde::{Deserialize, Serialize};
use space3d_core::{App, Entity, World};

/// One entity in a [`FullState`]: its `NetId` and `(registered component name, codec bytes)`.
pub type EncodedEntity = (NetId, Vec<(String, Vec<u8>)>);

/// Replicated state of a room at a tick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullState {
    /// Schema hash of the registry that wrote it (restore refuses a mismatch).
    pub schema_hash: u64,
    /// Room tick.
    pub tick: Tick,
    /// Room epoch at capture (restore bumps it).
    pub epoch: u32,
    /// Next `NetId` to allocate (ids are kept across restore).
    pub next_net_id: u32,
    /// `(NetId, [(kind name, codec bytes)])` for every `Replicated` entity.
    pub entities: Vec<EncodedEntity>,
}

/// Reserved component names for the room-side control components (always checkpointed).
pub const META_CONTROLLED_BY: &str = "net.controlled_by";
/// See [`META_CONTROLLED_BY`].
pub const META_OWNER: &str = "net.owner";
/// See [`META_CONTROLLED_BY`].
pub const META_TEAM: &str = "net.team";
/// See [`META_CONTROLLED_BY`].
pub const META_PRIORITY: &str = "net.priority";
/// See [`META_CONTROLLED_BY`].
pub const META_RELEVANCE: &str = "net.relevance";
/// See [`META_CONTROLLED_BY`].
pub const META_INTEREST: &str = "net.interest_source";
/// See [`META_CONTROLLED_BY`] (the `Simulated` marker; no payload). Restored entities keep
/// being advanced by the room's sim systems.
pub const META_SIMULATED: &str = "net.simulated";

fn meta<T: serde::Serialize + Copy + Send + Sync + 'static>(
    world: &World,
    e: Entity,
    name: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), EncodeError> {
    if let Ok(v) = world.get::<&T>(e) {
        out.push((name.to_owned(), crate::wire::to_postcard(&*v)?));
    }
    Ok(())
}

fn restore_meta(world: &mut World, e: Entity, name: &str, bytes: &[u8]) -> Result<bool, DecodeError> {
    use crate::wire::postcard_exact as p;
    let res = match name {
        META_CONTROLLED_BY => world.insert_one(e, p::<ControlledBy>(bytes)?),
        META_OWNER => world.insert_one(e, p::<Owner>(bytes)?),
        META_TEAM => world.insert_one(e, p::<NetTeam>(bytes)?),
        META_PRIORITY => world.insert_one(e, p::<NetPriority>(bytes)?),
        META_RELEVANCE => world.insert_one(e, p::<NetRelevance>(bytes)?),
        META_INTEREST => world.insert_one(e, p::<InterestSource>(bytes)?),
        META_SIMULATED => world.insert_one(e, Simulated),
        _ => return Ok(false),
    };
    res.map_err(|_| DecodeError::Invalid("entity vanished during restore"))?;
    Ok(true)
}

/// Capture every `Replicated` entity of a room (call on the room thread, e.g. from a system via
/// `AsyncQueue` bookkeeping in `space3d-server`). Entities that never went through a send tick
/// get the `NetId`s they will receive next.
pub fn capture(app: &App) -> Result<FullState, EncodeError> {
    let registry = app
        .resources
        .get::<ReplicationRegistry>()
        .filter(|r| r.is_frozen())
        .ok_or_else(|| EncodeError("replication registry missing or not frozen".into()))?;
    let server = app
        .resources
        .get::<NetServer>()
        .ok_or_else(|| EncodeError("NetServer missing".into()))?;
    let world = &app.world;
    let mut ids: NetIdMap = server.ids.clone();
    let mut entities: Vec<Entity> = world.query::<(Entity, &Replicated)>().iter().map(|(e, _)| e).collect();
    // Same numbering rule as `ChangeCache::collect` (entity-index order for unmapped entities).
    entities.sort_unstable_by_key(|e| (e.id(), e.to_bits()));
    for &e in &entities {
        if ids.net_id(e).is_none() {
            ids.allocate(e);
        }
    }
    entities.sort_by_key(|e| ids.net_id(*e));
    let ctx = EncodeCtx::new(&ids);
    let mut w = Writer::new();
    let mut out = Vec::with_capacity(entities.len());
    for e in entities {
        let id = ids.net_id(e).expect("allocated");
        let mut comps = Vec::new();
        for entry in registry.components() {
            w.clear();
            if entry.ops.encode(world, e, &mut w, &ctx) {
                comps.push((entry.name.to_owned(), w.as_slice().to_vec()));
            }
        }
        meta::<ControlledBy>(world, e, META_CONTROLLED_BY, &mut comps)?;
        meta::<Owner>(world, e, META_OWNER, &mut comps)?;
        meta::<NetTeam>(world, e, META_TEAM, &mut comps)?;
        meta::<NetPriority>(world, e, META_PRIORITY, &mut comps)?;
        meta::<NetRelevance>(world, e, META_RELEVANCE, &mut comps)?;
        meta::<InterestSource>(world, e, META_INTEREST, &mut comps)?;
        if world.satisfies::<&Simulated>(e) {
            comps.push((META_SIMULATED.to_owned(), Vec::new()));
        }
        out.push((id, comps));
    }
    Ok(FullState {
        schema_hash: registry.schema_hash(),
        tick: server.tick(),
        epoch: server.config().room_epoch,
        next_net_id: ids.next_id(),
        entities: out,
    })
}

/// Recreate entities (with `Replicated` + all registered components) and continue at
/// `state.tick`, epoch + 1. Call after plugins are added and before the first `update`.
/// Returns the restored `(NetId, Entity)` pairs.
pub fn restore(app: &mut App, state: &FullState) -> Result<Vec<(NetId, space3d_core::Entity)>, DecodeError> {
    app.resources.init::<ReplicationRegistry>().freeze();
    let hash = app.resources.expect::<ReplicationRegistry>().schema_hash();
    if hash != state.schema_hash {
        return Err(DecodeError::Invalid("checkpoint schema hash mismatch"));
    }
    if !app.resources.contains::<NetServer>() {
        return Err(DecodeError::Invalid(
            "NetServer missing (add ServerReplicationPlugin first)",
        ));
    }
    let world = &mut app.world;
    app.resources.scope::<NetServer, _>(|res, server| {
        let registry = res.expect::<ReplicationRegistry>();
        let mut pairs = Vec::with_capacity(state.entities.len());
        for (id, _) in &state.entities {
            let e = world.spawn((Replicated,));
            server.ids.insert(*id, e);
            pairs.push((*id, e));
        }
        for ((_, comps), (_, e)) in state.entities.iter().zip(&pairs) {
            for (name, bytes) in comps {
                if restore_meta(world, *e, name, bytes)? {
                    continue;
                }
                let Some(kind) = registry.kind_by_name(name) else {
                    tracing::warn!(component = %name, "restore: unknown component skipped");
                    continue;
                };
                let entry = registry.component(kind.0).expect("kind");
                let mut ctx = DecodeCtx::server(&server.ids);
                match entry.ops.apply(world, *e, bytes, &mut ctx) {
                    Ok(()) => {}
                    Err(DecodeError::Unresolved) => {
                        tracing::warn!(component = %name, "restore: unresolved entity reference dropped")
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        server.ids.set_next_id(state.next_net_id);
        server.set_tick(state.tick);
        server.config.room_epoch = state.epoch.wrapping_add(1);
        Ok(pairs)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ServerReplicationPlugin;
    use crate::{AppReplicationExt, ReplicationPlugin};
    use space3d_core::{Name, Parent, Transform};

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Hp(u32);

    fn room() -> App {
        let mut app = App::new();
        app.add_plugin(ServerReplicationPlugin::default())
            .add_plugin(ReplicationPlugin::default())
            .replicate::<Hp>("t.hp");
        app
    }

    #[test]
    fn checkpoint_roundtrip() {
        let mut a = room();
        let parent = a
            .world
            .spawn((Replicated, Transform::from_xyz(1.0, 2.0, 3.0), Name("p".into())));
        let child = a.world.spawn((
            Replicated,
            Simulated,
            Parent(parent),
            Hp(9),
            ControlledBy(crate::PlayerId(5)),
        ));
        a.step_ticks(4);
        let state = capture(&a).unwrap();
        assert_eq!(state.entities.len(), 2);
        assert_eq!(state.tick, Tick(4));
        let bytes = postcard::to_stdvec(&state).unwrap();
        let back: FullState = postcard::from_bytes(&bytes).unwrap();
        let mut b = room();
        let pairs = restore(&mut b, &back).unwrap();
        assert_eq!(pairs.len(), 2);
        let server = b.resources.expect::<NetServer>();
        assert_eq!(server.tick(), Tick(4));
        assert_eq!(server.config().room_epoch, 2);
        let (pid, pe) = pairs[0];
        let (_, ce) = pairs[1];
        assert_eq!(pid, a.resources.expect::<NetServer>().net_id(parent).unwrap());
        assert_eq!(b.world.get::<&Parent>(ce).unwrap().0, pe);
        assert_eq!(*b.world.get::<&Hp>(ce).unwrap(), Hp(9));
        assert_eq!(b.world.get::<&ControlledBy>(ce).unwrap().0, crate::PlayerId(5));
        assert!(b.world.satisfies::<&Simulated>(ce), "sim marker restored");
        assert!(!b.world.satisfies::<&Simulated>(pe));
        assert_eq!(b.world.get::<&Transform>(pe).unwrap().translation.y, 2.0);
        let _ = child;
        // New ids continue after the restored ones; schema mismatch is refused.
        let mut c = App::new();
        c.add_plugin(ServerReplicationPlugin::default());
        assert!(restore(&mut c, &back).is_err());
    }
}
