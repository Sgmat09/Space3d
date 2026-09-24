//! Identifiers shared by the wire protocol, the room server and the player service.
//!
//! 64-bit opaque ids ([`PlayerId`], [`RoomId`], [`MatchId`]) print and parse as 16 lowercase
//! hex digits, and serialize as that string in human-readable formats (JSON/TOML: safe for
//! JavaScript tooling) and as a plain `u64` in binary formats (postcard on the wire).

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Error parsing a 16-hex-digit id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected 16 hex digits")]
pub struct ParseIdError;

macro_rules! opaque_id64 {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name(pub u64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:016x}", self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:016x})"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                if s.len() != 16 {
                    return Err(ParseIdError);
                }
                u64::from_str_radix(s, 16).map(Self).map_err(|_| ParseIdError)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    s.collect_str(self)
                } else {
                    s.serialize_u64(self.0)
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                if d.is_human_readable() {
                    let s = String::deserialize(d)?;
                    s.parse().map_err(serde::de::Error::custom)
                } else {
                    u64::deserialize(d).map(Self)
                }
            }
        }
    };
}

opaque_id64!(
    /// A player account (issued by the player service; random, non-enumerable, never 0).
    PlayerId
);
opaque_id64!(
    /// A room (one isolated `World` on a game server).
    RoomId
);
opaque_id64!(
    /// A match (a room's competitive session; results are keyed by it).
    MatchId
);

/// Simulation tick of a room. Never wraps in practice (`u32` at 60 Hz = 2.2 years); restored
/// rooms continue from their checkpointed tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Tick(pub u32);

impl Tick {
    /// Tick zero.
    pub const ZERO: Tick = Tick(0);

    /// The following tick.
    pub fn next(self) -> Tick {
        Tick(self.0 + 1)
    }

    /// `self + n`.
    pub fn after(self, n: u32) -> Tick {
        Tick(self.0 + n)
    }

    /// `self - n`, saturating at zero.
    pub fn saturating_sub(self, n: u32) -> Tick {
        Tick(self.0.saturating_sub(n))
    }

    /// Signed distance `self - earlier` in ticks.
    pub fn since(self, earlier: Tick) -> i64 {
        i64::from(self.0) - i64::from(earlier.0)
    }
}

/// Transient per-room wire identity of a replicated entity (never reused within a room epoch).
/// Persistent identity is `space3d_scene::StableId`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NetId(pub u32);

/// Room-local connection slot of a player (stable across resume within the resume window).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NetClientId(pub u16);

/// Server-side id of one transport link (one WebSocket / memory link). Unique per process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LinkId(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_ids_roundtrip_hex_and_binary() {
        let id = PlayerId(0xdead_beef_0000_0001);
        assert_eq!(id.to_string(), "deadbeef00000001");
        assert_eq!("deadbeef00000001".parse::<PlayerId>(), Ok(id));
        assert!("xyz".parse::<PlayerId>().is_err());
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"deadbeef00000001\"");
        assert_eq!(serde_json::from_str::<PlayerId>("\"deadbeef00000001\"").unwrap(), id);
        let bin = postcard::to_stdvec(&id).unwrap();
        assert_eq!(postcard::from_bytes::<PlayerId>(&bin).unwrap(), id);
    }

    #[test]
    fn tick_math() {
        assert_eq!(Tick(5).next(), Tick(6));
        assert_eq!(Tick(1).saturating_sub(3), Tick(0));
        assert_eq!(Tick(3).since(Tick(5)), -2);
    }
}
