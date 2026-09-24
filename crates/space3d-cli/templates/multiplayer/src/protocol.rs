//! Wire protocol (JSON text frames) and the movement rule shared by server and tests.

use serde::{Deserialize, Serialize};

/// Units per second.
pub const SPEED: f32 = 4.0;
/// Players stay within `±ARENA` on x and z.
pub const ARENA: f32 = 6.0;
/// The server broadcasts a snapshot every this many fixed ticks (20 Hz at 60 Hz).
pub const SNAPSHOT_EVERY: u64 = 3;

/// Client → server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message after (re)connecting.
    Hello {
        /// Display name.
        name: String,
    },
    /// Current movement input, each axis in −1..=1.
    Input {
        /// Left/right.
        x: f32,
        /// Forward/back.
        z: f32,
    },
}

/// Server → client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Your player id.
    Welcome {
        /// Id used in snapshots.
        id: u64,
    },
    /// Authoritative state of every player.
    Snapshot {
        /// Server tick.
        tick: u64,
        /// All players, sorted by id.
        players: Vec<PlayerState>,
    },
}

/// One player as seen by everybody.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    /// Server-assigned id.
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Position on the ground plane (x, z).
    pub position: [f32; 2],
}

/// Move `position` by `input` for `dt` seconds (input longer than 1 is normalized) and keep
/// it inside the arena. Deterministic: the server runs it every fixed tick.
pub fn step(position: [f32; 2], input: [f32; 2], dt: f32) -> [f32; 2] {
    let [mut x, mut z] = input.map(|v| if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 });
    let len = (x * x + z * z).sqrt();
    if len > 1.0 {
        x /= len;
        z /= len;
    }
    [
        (position[0] + x * SPEED * dt).clamp(-ARENA, ARENA),
        (position[1] + z * SPEED * dt).clamp(-ARENA, ARENA),
    ]
}

/// Where player `id` spawns.
pub fn spawn_position(id: u64) -> [f32; 2] {
    let angle = id as f32 * 2.399_963; // golden angle: spread players out
    [angle.cos() * 3.0, angle.sin() * 3.0]
}

/// Linear RGB color of player `id`.
pub fn color(id: u64) -> [f32; 3] {
    let h = (id as f32 * 0.618_034).fract() * std::f32::consts::TAU;
    [
        0.55 + 0.45 * h.cos(),
        0.55 + 0.45 * (h + 2.094).cos(),
        0.55 + 0.45 * (h + 4.189).cos(),
    ]
}

/// Serialize a message as a JSON text frame.
pub fn encode<T: Serialize>(msg: &T) -> String {
    serde_json::to_string(msg).expect("protocol messages always serialize")
}

/// Parse a JSON text frame (`None` for garbage).
pub fn decode<T: for<'de> Deserialize<'de>>(text: &str) -> Option<T> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_moves_normalizes_and_clamps() {
        assert_eq!(step([0.0, 0.0], [1.0, 0.0], 0.5), [SPEED * 0.5, 0.0]);
        let [x, z] = step([0.0, 0.0], [1.0, 1.0], 1.0);
        assert!((x * x + z * z).sqrt() - SPEED < 1e-4);
        assert_eq!(step([ARENA, 0.0], [1.0, 0.0], 1.0), [ARENA, 0.0]);
        assert_eq!(step([0.0, 0.0], [f32::NAN, 5.0], 1.0), [0.0, SPEED]);
    }

    #[test]
    fn messages_roundtrip() {
        let msg = ServerMsg::Snapshot {
            tick: 9,
            players: vec![PlayerState {
                id: 1,
                name: "ann".into(),
                position: [1.0, -2.0],
            }],
        };
        assert_eq!(decode::<ServerMsg>(&encode(&msg)), Some(msg));
        assert_eq!(
            encode(&ClientMsg::Input { x: 1.0, z: 0.0 }),
            r#"{"type":"input","x":1.0,"z":0.0}"#
        );
        assert_eq!(decode::<ClientMsg>("not json"), None);
    }
}
