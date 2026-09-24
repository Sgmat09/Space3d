//! Account and IP/CIDR bans, shared by the gateway (checked at handshake) and the admin API.
//! The player service is the source of truth; game servers receive bans over the fleet link.

use super::netaddr::Cidr;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use space3d_replication::PlayerId;
use std::net::IpAddr;
use std::sync::Arc;

/// What is banned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BanTarget {
    /// An account.
    Player(PlayerId),
    /// An address range.
    Ip(Cidr),
}

/// One ban.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ban {
    /// Id.
    pub id: u64,
    /// Target.
    pub target: BanTarget,
    /// Expiry (unix ms; `None` = permanent).
    pub until_ms: Option<i64>,
    /// Reason (shown to the player).
    pub reason: String,
}

/// Thread-safe ban list (clone = shared).
#[derive(Clone, Debug, Default)]
pub struct BanList {
    inner: Arc<RwLock<Vec<Ban>>>,
}

impl BanList {
    /// Active ban of a player at `now_ms`.
    pub fn player(&self, player: PlayerId, now_ms: i64) -> Option<Ban> {
        todo!()
    }

    /// Active ban covering `ip`.
    pub fn ip(&self, ip: IpAddr, now_ms: i64) -> Option<Ban> {
        todo!()
    }

    /// Add or replace (by id).
    pub fn upsert(&self, ban: Ban) {
        todo!()
    }

    /// Remove by id.
    pub fn remove(&self, id: u64) -> bool {
        todo!()
    }

    /// Replace everything (fleet `Registered { bans }`).
    pub fn replace_all(&self, bans: Vec<Ban>) {
        *self.inner.write() = bans;
    }

    /// Copy of all bans.
    pub fn snapshot(&self) -> Vec<Ban> {
        self.inner.read().clone()
    }
}
