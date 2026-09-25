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

impl Ban {
    /// Whether the ban is in force at `now_ms`.
    pub fn is_active(&self, now_ms: i64) -> bool {
        self.until_ms.is_none_or(|until| now_ms < until)
    }
}

/// Thread-safe ban list (clone = shared).
#[derive(Clone, Debug, Default)]
pub struct BanList {
    inner: Arc<RwLock<Vec<Ban>>>,
    /// Bumped whenever a ban is added or replaced (the gatekeeper then re-checks connected
    /// links against IP bans).
    generation: Arc<std::sync::atomic::AtomicU64>,
}

impl BanList {
    /// Active ban of a player at `now_ms`.
    pub fn player(&self, player: PlayerId, now_ms: i64) -> Option<Ban> {
        self.inner
            .read()
            .iter()
            .find(|b| b.target == BanTarget::Player(player) && b.is_active(now_ms))
            .cloned()
    }

    /// Active ban covering `ip`.
    pub fn ip(&self, ip: IpAddr, now_ms: i64) -> Option<Ban> {
        self.inner
            .read()
            .iter()
            .find(|b| matches!(&b.target, BanTarget::Ip(c) if c.contains(ip)) && b.is_active(now_ms))
            .cloned()
    }

    /// Add or replace (by id).
    pub fn upsert(&self, ban: Ban) {
        let mut bans = self.inner.write();
        match bans.iter_mut().find(|b| b.id == ban.id) {
            Some(slot) => *slot = ban,
            None => bans.push(ban),
        }
        self.bump();
    }

    fn bump(&self) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// Changes whenever bans are added or replaced.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether any IP/CIDR ban exists (cheap pre-check).
    pub fn has_ip_bans(&self) -> bool {
        self.inner.read().iter().any(|b| matches!(b.target, BanTarget::Ip(_)))
    }

    /// Remove by id.
    pub fn remove(&self, id: u64) -> bool {
        let mut bans = self.inner.write();
        let before = bans.len();
        bans.retain(|b| b.id != id);
        bans.len() != before
    }

    /// Drop bans that expired before `now_ms`.
    pub fn prune(&self, now_ms: i64) {
        self.inner.write().retain(|b| b.is_active(now_ms));
    }

    /// Replace everything (fleet `Registered { bans }`).
    pub fn replace_all(&self, bans: Vec<Ban>) {
        *self.inner.write() = bans;
        self.bump();
    }

    /// Copy of all bans.
    pub fn snapshot(&self) -> Vec<Ban> {
        self.inner.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_and_ip_bans_with_expiry() {
        let list = BanList::default();
        list.upsert(Ban {
            id: 1,
            target: BanTarget::Player(PlayerId(7)),
            until_ms: Some(1_000),
            reason: "cheating".into(),
        });
        list.upsert(Ban {
            id: 2,
            target: BanTarget::Ip(Cidr::parse("198.51.100.0/24").unwrap()),
            until_ms: None,
            reason: "abuse".into(),
        });
        assert_eq!(list.player(PlayerId(7), 500).unwrap().id, 1);
        assert!(list.player(PlayerId(7), 1_000).is_none(), "expired");
        assert!(list.player(PlayerId(8), 500).is_none());
        assert_eq!(list.ip("198.51.100.20".parse().unwrap(), 5).unwrap().id, 2);
        assert!(list.ip("198.51.101.20".parse().unwrap(), 5).is_none());
        list.upsert(Ban {
            id: 1,
            target: BanTarget::Player(PlayerId(7)),
            until_ms: None,
            reason: "permanent".into(),
        });
        assert_eq!(list.snapshot().len(), 2);
        assert_eq!(list.player(PlayerId(7), 10_000).unwrap().reason, "permanent");
        assert!(list.remove(1));
        assert!(!list.remove(1));
        list.prune(i64::MAX);
        assert_eq!(list.snapshot().len(), 1, "permanent bans survive pruning");
    }
}
