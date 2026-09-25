//! Client slots: `Syncing → Connected → Disconnected{until} → (resumed | freed)`.
//!
//! A slot outlives its link for `resume_window`: the pawn stays, input becomes
//! `NetInput::neutral()`, and a `HelloAuth::Resume` with the slot's current epoch and secret
//! rebinds a new link (epoch + 1, fresh token, `FULL_RESET` snapshot). The same `PlayerId`
//! joining with a fresh ticket supersedes the old link.

use super::{PlayerIdentity, ServerNetConfig, SlotState};
use crate::FastMap as HashMap;
use crate::protocol::ChannelId;
use crate::protocol::handshake::ResumeToken;
use crate::stats::NetStats;
use crate::time::RttEstimator;
use crate::transport::LinkTx;
use crate::{LinkId, NetClientId, PlayerId, Tick};
use std::sync::Arc;
use std::time::Duration;

/// A block queued for the next packet of a slot.
pub(crate) type OutBlock = (ChannelId, Arc<[u8]>);

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
    /// Clock time of the last packet from the link (idle timeout).
    pub(crate) last_rx: Duration,
    pub(crate) rtt: RttEstimator,
    pub(crate) stats: NetStats,
    pub(crate) view: super::send::ClientView,
    pub(crate) inputs: super::inputs::InputBuffer,
    pub(crate) limits: super::receive::LinkLimiter,
    /// Server-side bot brain (virtual clients have no link).
    pub(crate) brain: Option<Box<dyn super::inputs::VirtualBrain>>,
    /// Reliable blocks (Control, MsgReliable) for the next packet.
    pub(crate) out_reliable: Vec<OutBlock>,
    /// Unreliable message blocks for the next packet (dropped under flow control).
    pub(crate) out_unreliable: Vec<OutBlock>,
    /// Pings to answer: `(id, client time µs, received at)`.
    pub(crate) pongs: Vec<(u16, u64, Duration)>,
    /// Downstream budget in bytes/s.
    pub(crate) down_rate: u32,
    /// Snapshot every `divider`-th send tick (flow control halves the rate).
    pub(crate) divider: u32,
    /// Backlog above `backlog_kick_bytes` since.
    pub(crate) backlog_high_since: Option<Duration>,
    /// Latest `InputWindow::view_delay_q8` (lag compensation).
    pub(crate) view_delay_q8: u32,
    /// `ControlMsg::InterestHint` (centre, radius).
    pub(crate) interest_hint: Option<([f32; 3], f32)>,
    /// Last client report.
    pub(crate) report: Option<crate::protocol::control::ClientReport>,
    /// Byte-rate window of `stats`.
    pub(crate) rates: crate::stats::RateWindow,
    /// Whether the link uses datagram semantics (seq/acks).
    pub(crate) datagram: bool,
}

impl Slot {
    pub(crate) fn new(
        client: NetClientId,
        identity: PlayerIdentity,
        tick: Tick,
        config: &ServerNetConfig,
        now: Duration,
    ) -> Self {
        Self {
            client,
            identity,
            state: SlotState::Syncing,
            link: None,
            epoch: 1,
            resume_secret: [0; 16],
            joined_tick: tick,
            last_packet_tick: tick,
            last_rx: now,
            rtt: RttEstimator::default(),
            stats: NetStats::default(),
            view: Default::default(),
            inputs: Default::default(),
            limits: super::receive::LinkLimiter::new(&config.limits, 60.0, now),
            brain: None,
            out_reliable: Vec::new(),
            out_unreliable: Vec::new(),
            pongs: Vec::new(),
            down_rate: config.client_bandwidth,
            divider: 1,
            backlog_high_since: None,
            view_delay_q8: 0,
            interest_hint: None,
            report: None,
            rates: Default::default(),
            datagram: false,
        }
    }

    /// Queue a reliable block.
    pub(crate) fn queue_reliable(&mut self, channel: ChannelId, bytes: Arc<[u8]>) {
        if self.link.is_some() {
            self.out_reliable.push((channel, bytes));
        }
    }

    /// Queue an unreliable block.
    pub(crate) fn queue_unreliable(&mut self, channel: ChannelId, bytes: Arc<[u8]>) {
        if self.link.is_some() {
            self.out_unreliable.push((channel, bytes));
        }
    }

    /// Forget per-link transient state (queues, pongs, flow control) after the link dropped.
    pub(crate) fn clear_link_state(&mut self) {
        self.out_reliable.clear();
        self.out_unreliable.clear();
        self.pongs.clear();
        self.divider = 1;
        self.backlog_high_since = None;
    }
}

/// All slots of a room.
#[derive(Default)]
pub(crate) struct SlotTable {
    pub(crate) slots: Vec<Option<Slot>>,
    pub(crate) by_link: HashMap<LinkId, NetClientId>,
    pub(crate) by_player: HashMap<PlayerId, NetClientId>,
    /// Nominal room tick rate (cached from `TickRate` by the receive system).
    pub(crate) tick_hz: f64,
}

impl SlotTable {
    /// Lowest free slot id.
    pub(crate) fn allocate(&mut self, max: u16) -> Option<NetClientId> {
        for i in 0..max {
            if self.slots.get(usize::from(i)).is_none_or(|s| s.is_none()) {
                return Some(NetClientId(i));
            }
        }
        None
    }

    /// Verify a resume token against the slot (epoch, secret, window).
    pub(crate) fn verify_resume(&self, token: &ResumeToken, now: Tick) -> Option<NetClientId> {
        let slot = self.get(token.client)?;
        if slot.epoch != token.epoch || slot.brain.is_some() {
            return None;
        }
        // Constant-time secret comparison, evaluated before the window check.
        let secret_ok = super::handshake::ct_eq(&slot.resume_secret, &token.secret);
        let in_window = match slot.state {
            SlotState::Disconnected { until } => now <= until,
            // The old link may be half-dead (not noticed yet): resuming replaces it.
            SlotState::Syncing | SlotState::Connected => true,
        };
        (secret_ok && in_window).then_some(slot.client)
    }

    pub(crate) fn get(&self, client: NetClientId) -> Option<&Slot> {
        self.slots.get(usize::from(client.0))?.as_ref()
    }

    pub(crate) fn get_mut(&mut self, client: NetClientId) -> Option<&mut Slot> {
        self.slots.get_mut(usize::from(client.0))?.as_mut()
    }

    pub(crate) fn insert(&mut self, slot: Slot) {
        let idx = usize::from(slot.client.0);
        if self.slots.len() <= idx {
            self.slots.resize_with(idx + 1, || None);
        }
        self.by_player.insert(slot.identity.player, slot.client);
        if let Some((link, _)) = &slot.link {
            self.by_link.insert(*link, slot.client);
        }
        self.slots[idx] = Some(slot);
    }

    pub(crate) fn remove(&mut self, client: NetClientId) -> Option<Slot> {
        let slot = self.slots.get_mut(usize::from(client.0))?.take()?;
        self.by_player.remove(&slot.identity.player);
        if let Some((link, _)) = &slot.link {
            self.by_link.remove(link);
        }
        Some(slot)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter().flatten()
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
        self.slots.iter_mut().flatten()
    }

    pub(crate) fn ids(&self) -> Vec<NetClientId> {
        self.iter().map(|s| s.client).collect()
    }
}
