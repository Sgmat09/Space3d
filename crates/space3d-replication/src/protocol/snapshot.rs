//! SNAPSHOT block (S→C, unreliable lane, latest-wins). The tick is the Data packet's tick.
//!
//! ```text
//! Snapshot   := SnapshotHeader · varu32 entity_count · Entity* · [HAS_DESPAWNS] Despawns
//! SnapshotHeader := u8 flags · varu32 input_ack_delta · i8 input_slack
//!     flags: bit0 FULL_RESET     client drops every replicated entity not listed (resume / new epoch)
//!            bit1 SYNC_COMPLETE  every in-scope entity has been delivered at least once (loading gate)
//!            bit2 HAS_DESPAWNS
//!     input_ack_delta: tick − newest input tick consumed for this client (u32::MAX = none yet)
//!     input_slack: how many ticks early this client's input for `tick` arrived (i8::MIN = missing)
//! Entity     := varu32 net_id_delta (ascending NetIds; first is absolute)
//!               · u8 op   bit0 SPAWN (full audience-filtered state follows)
//!                         bit1 HAS_REMOVED
//!                         bit2 CONTROLLED (ControlledBy == receiving player)
//!                         bit3 OWNED (Owner/ControlledBy == receiving player)
//!               · varu64 present_mask (bit i = ComponentKind(i) payload follows)
//!               · [HAS_REMOVED] varu64 removed_mask
//!               · for each set bit of present_mask, ascending: varu32 len · codec bytes
//! Despawns   := varu32 n · n × varu32 (net_id_delta << 1 | out_of_scope)
//! ```
//!
//! **Invariant** (makes loss and reordering harmless): whenever an entity is written for a
//! client, *every* component whose `changed_tick` is newer than that client's acked tick for
//! the entity is included. Locally predicted entities are written to their controller in every
//! snapshot with all `Predict*` components (reconciliation needs a confirmation each time).

use crate::NetId;
use crate::wire::{DecodeError, Reader, Writer};

/// Snapshot flag bits.
pub mod flags {
    /// Drop all replicated entities not listed.
    pub const FULL_RESET: u8 = 0x01;
    /// Initial sync finished.
    pub const SYNC_COMPLETE: u8 = 0x02;
    /// Despawn list present.
    pub const HAS_DESPAWNS: u8 = 0x04;
}

/// Entity op bits.
pub mod op {
    /// Spawn: full state follows.
    pub const SPAWN: u8 = 0x01;
    /// Removed-components mask present.
    pub const HAS_REMOVED: u8 = 0x02;
    /// Controlled by the receiving player (predict it).
    pub const CONTROLLED: u8 = 0x04;
    /// Owned by the receiving player.
    pub const OWNED: u8 = 0x08;
}

/// Parsed snapshot header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// [`flags`] bits.
    pub flags: u8,
    /// `tick − newest consumed input tick` (`u32::MAX` = none).
    pub input_ack_delta: u32,
    /// Input slack feedback for the tick pacer.
    pub input_slack: i8,
}

impl SnapshotHeader {
    /// Encode.
    pub fn encode(&self, w: &mut Writer) {
        w.u8(self.flags);
        w.varu32(self.input_ack_delta);
        w.u8(self.input_slack as u8);
    }

    /// Decode (unknown flag bits are rejected).
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let flags = r.u8()?;
        if flags & !(flags::FULL_RESET | flags::SYNC_COMPLETE | flags::HAS_DESPAWNS) != 0 {
            return Err(DecodeError::Invalid("snapshot flags"));
        }
        Ok(Self {
            flags,
            input_ack_delta: r.varu32()?,
            input_slack: r.u8()? as i8,
        })
    }
}

/// One decoded entity record (component payloads borrowed from the packet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRecord<'a> {
    /// Entity.
    pub net_id: NetId,
    /// [`op`] bits.
    pub op: u8,
    /// Components present in this record.
    pub present: u64,
    /// Components removed since the client's baseline.
    pub removed: u64,
    /// `(kind index, codec bytes)` in ascending kind order.
    pub components: smallvec::SmallVec<[(u16, &'a [u8]); 8]>,
}

/// A despawned / out-of-scope entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DespawnRecord {
    /// Entity.
    pub net_id: NetId,
    /// Left the client's interest set (vs. destroyed on the server).
    pub out_of_scope: bool,
}

/// Incremental snapshot encoder used by the server's per-client packet builder: it supports
/// rolling back a partially written entity when the byte budget is exceeded.
#[derive(Debug, Default)]
pub struct SnapshotWriter {
    last_id: Option<u32>,
    count: u32,
    body: Writer,
    despawns: Vec<DespawnRecord>,
    open: Option<(usize, Option<u32>)>,
}

impl SnapshotWriter {
    /// Empty snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start an entity; returns a checkpoint for [`Self::rollback`]. Ids must ascend.
    /// `op` must not contain [`op::HAS_REMOVED`]; it is derived from `removed != 0`.
    pub fn begin_entity(&mut self, net_id: NetId, op: u8, present: u64, removed: u64) -> usize {
        let checkpoint = self.body.len();
        let prev = self.last_id;
        let delta = match prev {
            None => net_id.0,
            Some(last) => {
                assert!(net_id.0 > last, "SnapshotWriter: NetIds must ascend");
                net_id.0 - last
            }
        };
        self.body.varu32(delta);
        let op = (op & !op::HAS_REMOVED) | if removed != 0 { op::HAS_REMOVED } else { 0 };
        self.body.u8(op);
        self.body.varu64(present);
        if removed != 0 {
            self.body.varu64(removed);
        }
        self.last_id = Some(net_id.0);
        self.count += 1;
        self.open = Some((checkpoint, prev));
        checkpoint
    }

    /// Append one component payload (kinds in ascending order, matching `present`).
    pub fn component(&mut self, bytes: &[u8]) {
        self.body.bytes(bytes);
    }

    /// Undo the entity started at `checkpoint` (only the most recent entity can be undone).
    pub fn rollback(&mut self, checkpoint: usize) {
        if let Some((cp, prev)) = self.open.take() {
            if cp == checkpoint {
                self.body.truncate(cp);
                self.last_id = prev;
                self.count -= 1;
            } else {
                self.open = Some((cp, prev));
            }
        }
    }

    /// Queue a despawn record.
    pub fn despawn(&mut self, rec: DespawnRecord) {
        self.despawns.push(rec);
    }

    /// Current encoded size estimate (for budgeting).
    pub fn len(&self) -> usize {
        self.body.len() + self.despawns.len() * 3 + 8
    }

    /// Whether no entity or despawn was written.
    pub fn is_empty(&self) -> bool {
        self.count == 0 && self.despawns.is_empty()
    }

    /// Entities written.
    pub fn entity_count(&self) -> u32 {
        self.count
    }

    /// Write header + entities + despawns into `w`.
    pub fn finish(mut self, mut header: SnapshotHeader, w: &mut Writer) {
        self.despawns.sort_by_key(|d| d.net_id);
        self.despawns.dedup_by_key(|d| d.net_id);
        if self.despawns.is_empty() {
            header.flags &= !flags::HAS_DESPAWNS;
        } else {
            header.flags |= flags::HAS_DESPAWNS;
        }
        header.encode(w);
        w.varu32(self.count);
        w.raw(self.body.as_slice());
        if !self.despawns.is_empty() {
            w.varu32(self.despawns.len() as u32);
            let mut last = 0u64;
            for (i, d) in self.despawns.iter().enumerate() {
                let id = u64::from(d.net_id.0);
                let delta = if i == 0 { id } else { id - last };
                last = id;
                w.varu64((delta << 1) | u64::from(d.out_of_scope));
            }
        }
    }
}

/// Decoded snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotView<'a> {
    /// Header.
    pub header: SnapshotHeader,
    /// Entities in ascending `NetId` order.
    pub entities: Vec<EntityRecord<'a>>,
    /// Despawns.
    pub despawns: Vec<DespawnRecord>,
}

impl<'a> SnapshotView<'a> {
    /// Decode a SNAPSHOT block (bounded by the block length; component count ≤ 64).
    pub fn decode(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        let header = SnapshotHeader::decode(&mut r)?;
        let count = r.varu32()? as usize;
        // Every entity takes at least 3 bytes (id, op, mask): bound before allocating.
        if count > r.remaining() / 3 {
            return Err(DecodeError::Invalid("entity count exceeds block"));
        }
        let mut entities = Vec::with_capacity(count);
        let mut last: Option<u32> = None;
        for _ in 0..count {
            let delta = r.varu32()?;
            let id = match last {
                None => delta,
                Some(l) => {
                    if delta == 0 {
                        return Err(DecodeError::Invalid("entity ids must ascend"));
                    }
                    l.checked_add(delta).ok_or(DecodeError::Invalid("entity id overflow"))?
                }
            };
            last = Some(id);
            let op_bits = r.u8()?;
            if op_bits & !(op::SPAWN | op::HAS_REMOVED | op::CONTROLLED | op::OWNED) != 0 {
                return Err(DecodeError::Invalid("entity op bits"));
            }
            let present = r.varu64()?;
            let removed = if op_bits & op::HAS_REMOVED != 0 {
                let m = r.varu64()?;
                if m == 0 {
                    return Err(DecodeError::Invalid("empty removed mask"));
                }
                m
            } else {
                0
            };
            if present & removed != 0 {
                return Err(DecodeError::Invalid("component both present and removed"));
            }
            let mut components = smallvec::SmallVec::new();
            let mut mask = present;
            while mask != 0 {
                let kind = mask.trailing_zeros() as u16;
                mask &= mask - 1;
                let payload = r.bytes(r.remaining())?;
                components.push((kind, payload));
            }
            entities.push(EntityRecord {
                net_id: NetId(id),
                op: op_bits,
                present,
                removed,
                components,
            });
        }
        let mut despawns = Vec::new();
        if header.flags & flags::HAS_DESPAWNS != 0 {
            let n = r.varu32()? as usize;
            if n == 0 || n > r.remaining() {
                return Err(DecodeError::Invalid("despawn count"));
            }
            despawns.reserve(n);
            let mut last = 0u64;
            for i in 0..n {
                let v = r.varu64()?;
                let delta = v >> 1;
                if i > 0 && delta == 0 {
                    return Err(DecodeError::Invalid("despawn ids must ascend"));
                }
                let id = if i == 0 { delta } else { last + delta };
                let id32 = u32::try_from(id).map_err(|_| DecodeError::Invalid("despawn id overflow"))?;
                last = id;
                despawns.push(DespawnRecord {
                    net_id: NetId(id32),
                    out_of_scope: v & 1 != 0,
                });
            }
        }
        if !r.is_empty() {
            return Err(DecodeError::Invalid("trailing snapshot bytes"));
        }
        Ok(Self {
            header,
            entities,
            despawns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn writer_rollback_restores_state() {
        let mut s = SnapshotWriter::new();
        s.begin_entity(NetId(5), op::SPAWN, 0b1, 0);
        s.component(&[1, 2, 3]);
        let cp = s.begin_entity(NetId(9), 0, 0b11, 0b100);
        s.component(&[4]);
        s.component(&[5]);
        s.rollback(cp);
        // The next entity may reuse an id above 5 again.
        s.begin_entity(NetId(7), op::CONTROLLED, 0, 0b10);
        s.despawn(DespawnRecord {
            net_id: NetId(40),
            out_of_scope: true,
        });
        s.despawn(DespawnRecord {
            net_id: NetId(3),
            out_of_scope: false,
        });
        let mut w = Writer::new();
        s.finish(
            SnapshotHeader {
                flags: flags::SYNC_COMPLETE,
                input_ack_delta: 2,
                input_slack: -3,
            },
            &mut w,
        );
        let view = SnapshotView::decode(w.as_slice()).unwrap();
        assert_eq!(view.header.flags, flags::SYNC_COMPLETE | flags::HAS_DESPAWNS);
        assert_eq!(view.header.input_slack, -3);
        assert_eq!(view.entities.len(), 2);
        assert_eq!(view.entities[0].net_id, NetId(5));
        assert_eq!(view.entities[0].components.as_slice(), &[(0u16, &[1u8, 2, 3][..])]);
        assert_eq!(view.entities[1].net_id, NetId(7));
        assert_eq!(view.entities[1].op, op::CONTROLLED | op::HAS_REMOVED);
        assert_eq!(view.entities[1].removed, 0b10);
        assert_eq!(
            view.despawns,
            vec![
                DespawnRecord {
                    net_id: NetId(3),
                    out_of_scope: false
                },
                DespawnRecord {
                    net_id: NetId(40),
                    out_of_scope: true
                }
            ]
        );
    }

    #[test]
    fn hostile_snapshots_are_rejected() {
        // Huge entity count with no data.
        let mut w = Writer::new();
        SnapshotHeader::default().encode(&mut w);
        w.varu32(1_000_000);
        assert!(SnapshotView::decode(w.as_slice()).is_err());
        // Unknown flag.
        assert!(SnapshotView::decode(&[0x80, 0, 0, 0]).is_err());
        // Present and removed overlap.
        let mut w = Writer::new();
        SnapshotHeader::default().encode(&mut w);
        w.varu32(1);
        w.varu32(1);
        w.u8(op::HAS_REMOVED);
        w.varu64(1);
        w.varu64(1);
        w.bytes(&[0]);
        assert!(SnapshotView::decode(w.as_slice()).is_err());
    }

    #[derive(Debug, Clone)]
    struct Ent {
        id_delta: u32,
        op: u8,
        present: u64,
        removed: u64,
        payload_len: u8,
    }

    fn ent() -> impl Strategy<Value = Ent> {
        (1u32..1000, 0u8..16, any::<u64>(), any::<u64>(), 0u8..20).prop_map(|(d, op, p, r, l)| Ent {
            id_delta: d,
            op: op & (op::SPAWN | op::CONTROLLED | op::OWNED),
            present: p & 0xFFFF,
            removed: r & 0xFFFF & !(p & 0xFFFF),
            payload_len: l,
        })
    }

    proptest! {
        /// Delta snapshot encode → decode roundtrip over arbitrary entity sets.
        #[test]
        fn prop_snapshot_roundtrip(ents in proptest::collection::vec(ent(), 0..40), despawns in proptest::collection::btree_set(1u32..100_000, 0..20), fl in 0u8..4, ack in any::<u32>(), slack in any::<i8>()) {
            let mut s = SnapshotWriter::new();
            let mut id = 0u32;
            for e in &ents {
                id += e.id_delta;
                s.begin_entity(NetId(id), e.op, e.present, e.removed);
                for k in 0..e.present.count_ones() {
                    s.component(&vec![k as u8; usize::from(e.payload_len)]);
                }
            }
            for d in &despawns {
                s.despawn(DespawnRecord { net_id: NetId(*d), out_of_scope: d % 3 == 0 });
            }
            let mut w = Writer::new();
            s.finish(SnapshotHeader { flags: fl, input_ack_delta: ack, input_slack: slack }, &mut w);
            let view = SnapshotView::decode(w.as_slice()).unwrap();
            prop_assert_eq!(view.entities.len(), ents.len());
            let mut id = 0u32;
            for (e, rec) in ents.iter().zip(&view.entities) {
                id += e.id_delta;
                prop_assert_eq!(rec.net_id, NetId(id));
                prop_assert_eq!(rec.present, e.present);
                prop_assert_eq!(rec.removed, e.removed);
                prop_assert_eq!(rec.op & !op::HAS_REMOVED, e.op);
                prop_assert_eq!(rec.components.len() as u32, e.present.count_ones());
                for (i, (_, bytes)) in rec.components.iter().enumerate() {
                    prop_assert_eq!(*bytes, &vec![i as u8; usize::from(e.payload_len)][..]);
                }
            }
            let got: Vec<u32> = view.despawns.iter().map(|d| d.net_id.0).collect();
            let want: Vec<u32> = despawns.iter().copied().collect();
            prop_assert_eq!(got, want);
            prop_assert_eq!(view.header.input_ack_delta, ack);
            prop_assert_eq!(view.header.input_slack, slack);
        }

        #[test]
        fn prop_snapshot_decoder_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = SnapshotView::decode(&bytes);
        }
    }
}
