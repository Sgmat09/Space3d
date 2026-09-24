//! Client slots: `Syncing → Connected → Disconnected{until} → (resumed | freed)`.
//!
//! A slot outlives its link for `resume_window`: the pawn stays, input becomes
//! `NetInput::neutral()`, and a `HelloAuth::Resume` with the slot's current epoch and secret
//! rebinds a new link (epoch + 1, fresh token, `FULL_RESET` snapshot). The same `PlayerId`
//! joining with a fresh ticket supersedes the old link.

use super::{PlayerIdentity, SlotState};
use crate::FastMap as HashMap;
use crate::protocol::handshake::ResumeToken;
use crate::stats::NetStats;
use crate::time::RttEstimator;
use crate::transport::LinkTx;
use crate::{LinkId, NetClientId, PlayerId, Tick};
use std::sync::Arc;

/// Server-side state of one slot (internal to the replication builder).
pub(crate) struct Slot {
    pub(crate) client: NetClientId,
    pub(crate) identity: PlayerIdentity,
    pub(crate) state: SlotState,
    pub(crate) link: Option<(LinkId, Arc<dyn LinkTx>)>,
    pub(crate) epoch: u32,
    /// Current resume secret (compared with `handshake::ct_eq`; rotated on every Welcome).
    pub(crate) resume_secret: [u8; 16],
    pub(crate) joined_tick: Tick,
    pub(crate) last_packet_tick: Tick,
    pub(crate) rtt: RttEstimator,
    pub(crate) stats: NetStats,
    pub(crate) view: super::send::ClientView,
    pub(crate) inputs: super::inputs::InputBuffer,
    pub(crate) limits: super::receive::LinkLimiter,
}

/// All slots of a room.
#[derive(Default)]
pub(crate) struct SlotTable {
    pub(crate) slots: Vec<Option<Slot>>,
    pub(crate) by_link: HashMap<LinkId, NetClientId>,
    pub(crate) by_player: HashMap<PlayerId, NetClientId>,
}

impl SlotTable {
    /// Lowest free slot id.
    pub(crate) fn allocate(&mut self, max: u16) -> Option<NetClientId> {
        todo!()
    }

    /// Verify a resume token against the slot (epoch, secret, window).
    pub(crate) fn verify_resume(&self, token: &ResumeToken, now: Tick) -> Option<NetClientId> {
        todo!()
    }
}
