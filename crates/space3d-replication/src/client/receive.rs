//! Packet decoding and snapshot application (internal to the replication builder).
//!
//! * Entities: `SPAWN` → spawn (or reuse the entity already mapped to that `NetId` after a
//!   resume `FULL_RESET`), insert `NetEntity`, `Remote`, `Interpolated`/`Predicted`/`Controlled`
//!   markers per op bits and component modes; `HAS_REMOVED` → remove components; despawns →
//!   `RemoteDespawned` then despawn. `FULL_RESET` → despawn every mapped entity not listed.
//! * Per-entity `last_applied_tick`: a record older than or equal to it is skipped (safe under
//!   reordering thanks to the snapshot invariant).
//! * Component values with unresolved entity references are parked in [`PendingRefs`] and
//!   retried after each snapshot (dropped after 1 s or when the holder despawns).

use crate::{NetId, Tick};

/// Parked component values waiting for referenced entities.
#[derive(Default)]
pub struct PendingRefs {
    /// `(holder, kind, bytes, first seen)`.
    pub entries: Vec<(NetId, u16, Vec<u8>, Tick)>,
}
