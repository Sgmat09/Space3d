//! Arena example: the networked game protocol shared by the dedicated server
//! (`examples/arena-server`) and the native / web client (`examples/arena-client`).
//!
//! Everything both sides must agree on lives here, in one plugin ([`ArenaProtocol`]): the
//! replicated components, the input type, the messages and the deterministic simulation
//! systems. The registration list is hashed into the schema hash, so a client built from a
//! different version of this crate is refused with `Reject(SchemaMismatch)`.
//!
//! * [`ArenaInput`] carries *state*, not edges: movement axes, an absolute yaw, held buttons
//!   and a wrapping fire counter (redundant / repeated inputs are harmless).
//! * [`walk`] is the only predicted gameplay: it runs once per tick on the server and once per
//!   predicted (and replayed) tick on the controlling client.
//! * [`rules::ArenaRules`] is the authoritative server logic (spawn / despawn pawns, hit-scan
//!   shots with lag compensation, health, score, chat). It is pure logic and only runs in rooms.

pub mod rules;

use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_replication::prelude::*;
use space3d_replication::{InvalidInput, InvalidMessage};

/// `Hello::game_id` of the arena.
pub const GAME_ID: &str = "arena";
/// Simulation rate (server rooms and client prediction).
pub const TICK_HZ: f64 = 60.0;
/// The arena is a square of `2 × ARENA_HALF` metres centred on the origin.
pub const ARENA_HALF: f32 = 20.0;
/// Walking speed (m/s).
pub const SPEED: f32 = 6.0;
/// Sprint multiplier.
pub const SPRINT: f32 = 1.6;
/// Starting / maximum health.
pub const MAX_HEALTH: u16 = 100;
/// Magazine size.
pub const MAX_AMMO: u16 = 12;
/// Damage per hit.
pub const DAMAGE: u16 = 25;
/// Hit-scan range (m).
pub const RANGE: f32 = 40.0;

/// Player input (sampled every predicted tick, sent redundantly).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaInput {
    /// Right (+) / left (−), `-127..=127`.
    pub move_x: i8,
    /// Back (+) / forward (−), `-127..=127`.
    pub move_z: i8,
    /// Absolute facing, in 1/65536 of a turn.
    pub yaw: u16,
    /// Held buttons ([`ArenaInput::SPRINT`]).
    pub buttons: u8,
    /// Wrapping count of trigger presses (the server fires once per increment).
    pub fire_seq: u8,
}

impl ArenaInput {
    /// Sprint held.
    pub const SPRINT: u8 = 0b0000_0001;
    /// Every defined button bit.
    pub const BUTTONS: u8 = Self::SPRINT;

    /// Movement direction on the XZ plane (length ≤ 1).
    pub fn direction(&self) -> Vec3 {
        let v = Vec3::new(f32::from(self.move_x), 0.0, f32::from(self.move_z)) / 127.0;
        if v.length_squared() > 1.0 { v.normalize() } else { v }
    }

    /// Facing yaw in radians.
    pub fn yaw_radians(&self) -> f32 {
        f32::from(self.yaw) / 65536.0 * std::f32::consts::TAU
    }

    /// Quantize a yaw in radians.
    pub fn yaw_from_radians(radians: f32) -> u16 {
        let turns = (radians / std::f32::consts::TAU).rem_euclid(1.0);
        (turns * 65536.0).round() as u32 as u16
    }
}

impl NetInput for ArenaInput {
    fn sanitize(&mut self) -> Result<(), InvalidInput> {
        // i8::MIN would make the axes asymmetric.
        self.move_x = self.move_x.max(-127);
        self.move_z = self.move_z.max(-127);
        if self.buttons & !Self::BUTTONS != 0 {
            self.buttons &= Self::BUTTONS;
            return Err(InvalidInput("unknown button bits"));
        }
        Ok(())
    }
}

/// Health (everyone sees it; released with the render clock so it matches hit effects).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health(pub u16);

/// Rounds left (only the owner receives it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ammo(pub u16);

/// Kills.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score(pub u32);

/// Colour index of a pawn (0..8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerColor(pub u8);

/// Client → server chat line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Say(pub String);

impl Say {
    /// Longest accepted line (characters).
    pub const MAX_CHARS: usize = 120;
}

impl NetMessage for Say {
    fn validate(&self) -> Result<(), InvalidMessage> {
        if self.0.chars().count() > Self::MAX_CHARS {
            return Err(InvalidMessage("chat line too long"));
        }
        if self.0.chars().any(char::is_control) {
            return Err(InvalidMessage("control characters in chat"));
        }
        Ok(())
    }
}

/// Server → client chat line (or system announcement when `from` is empty).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    /// Sender display name.
    pub from: String,
    /// Text.
    pub text: String,
}

impl NetMessage for Chat {}

/// A shot hit someone (released at the render tick, so it lines up with interpolated poses).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    /// Shooter.
    pub shooter: PlayerId,
    /// Victim.
    pub target: PlayerId,
    /// Impact point (world space).
    pub point: [f32; 3],
    /// The victim died.
    pub kill: bool,
}

impl NetMessage for Hit {}

/// The shared protocol plugin: add it on the server (in every room) and on every client,
/// after the replication plugin.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaProtocol;

impl Plugin for ArenaProtocol {
    fn build(&self, app: &mut App) {
        app.add_plugin(ReplicationPlugin::default())
            .replicate_with::<Health>("arena.health", ComponentRule::serde().delayed())
            .replicate_with::<Ammo>("arena.ammo", ComponentRule::serde().audience(Audience::Owner))
            .replicate::<Score>("arena.score")
            .replicate::<PlayerColor>("arena.color")
            .add_input::<ArenaInput>("arena.input")
            .add_message::<Say>(
                "arena.say",
                MessageConfig::reliable(Direction::ClientToServer)
                    .rate_limit(2, 5)
                    .max_bytes(512),
            )
            .add_message::<Chat>("arena.chat", MessageConfig::reliable(Direction::ServerToClient))
            .add_message::<Hit>(
                "arena.hit",
                MessageConfig::reliable(Direction::ServerToClient).timing(EventTiming::Interpolated),
            )
            .add_sim_system("arena::walk", walk);
    }
}

/// The predicted movement: runs on the server for every pawn and on a client for its own pawn.
/// Deterministic, reads only [`SimTime`] and `PlayerInput<ArenaInput>`.
pub fn walk(ctx: &mut SystemCtx) {
    let dt = ctx.resources.expect::<SimTime>().dt_secs();
    for (t, input) in ctx
        .world
        .query_mut::<(&mut Transform, &PlayerInput<ArenaInput>)>()
        .with::<&Simulated>()
    {
        let i = input.0;
        let speed = if i.buttons & ArenaInput::SPRINT != 0 {
            SPEED * SPRINT
        } else {
            SPEED
        };
        let step = i.direction() * speed * dt;
        t.translation.x = (t.translation.x + step.x).clamp(-ARENA_HALF, ARENA_HALF);
        t.translation.z = (t.translation.z + step.z).clamp(-ARENA_HALF, ARENA_HALF);
        t.rotation = Quat::from_rotation_y(i.yaw_radians());
    }
}

/// Spawn point of the `index`-th player (spread around a circle).
pub fn spawn_point(index: u32) -> Vec3 {
    let a = index as f32 * 2.399_963; // golden angle
    let r = ARENA_HALF * 0.6;
    Vec3::new(a.cos() * r, 0.5, a.sin() * r)
}

/// Unit forward vector for a yaw (−Z at yaw 0, like `Transform::forward`).
pub fn forward(yaw_radians: f32) -> Vec3 {
    Quat::from_rotation_y(yaw_radians) * Vec3::NEG_Z
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_sanitize_and_helpers() {
        let mut i = ArenaInput {
            move_x: i8::MIN,
            move_z: 127,
            buttons: 0x80 | ArenaInput::SPRINT,
            ..Default::default()
        };
        assert!(i.sanitize().is_err());
        assert_eq!((i.move_x, i.buttons), (-127, ArenaInput::SPRINT));
        assert!((i.direction().length() - 1.0).abs() < 1e-5);
        let yaw = ArenaInput::yaw_from_radians(-std::f32::consts::FRAC_PI_2);
        assert_eq!(yaw, 49152);
        let back = ArenaInput { yaw, ..i }.yaw_radians();
        assert!((back - 3.0 * std::f32::consts::FRAC_PI_2).abs() < 1e-4);
    }

    #[test]
    fn say_validation() {
        assert!(Say("gg".into()).validate().is_ok());
        assert!(Say("x".repeat(121)).validate().is_err());
        assert!(Say("bell\u{7}".into()).validate().is_err());
    }
}
