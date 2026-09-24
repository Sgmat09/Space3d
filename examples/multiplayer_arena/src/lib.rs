//! `multiplayer_arena`: the smallest complete Space3d multiplayer game.
//!
//! Every player drives a cube around a square arena with WASD. The **dedicated server**
//! (`src/bin/server.rs`) owns the game state and the players; **clients**
//! (`src/bin/client.rs`, native or web) send input, predict their own cube and interpolate
//! everyone else's.
//!
//! This crate is the part both sides share:
//!
//! * [`CubesProtocol`]: the replicated components, the input type and the simulation. The
//!   registration list is hashed into the schema hash, so a client built from a different
//!   version is refused with `Reject(SchemaMismatch)`.
//! * [`drive`]: the only predicted gameplay. It runs once per tick on the server, and once per
//!   predicted (and replayed) tick on the client that controls the cube.
//! * [`CubesRules`]: the authoritative server rules (a cube per player; restored cubes are
//!   reused after a crash or a server loss). Pure logic that only runs in server rooms.
//! * [`Wander`]: a deterministic bot for `client --headless --bot`, tests and load tests.
//! * `controls` (feature `client`): WASD / arrow keys through a `space3d_platform::ActionMap`,
//!   sampled into [`CubeInput`] once per predicted tick.
//! * `server::CubesGame` (native only): the `space3d_server::GameDefinition`.
//!
//! ```text
//! cargo run -p multiplayer_arena --bin server                                  # all-in-one dev server
//! cargo run -p multiplayer_arena --bin client -- --ps http://127.0.0.1:7777   # window, via the player service
//! cargo run -p multiplayer_arena --bin client -- --headless --bot --seconds 10 # headless bot
//! ```

use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_replication::prelude::*;
use space3d_replication::{InvalidInput, Tick};

#[cfg(feature = "client")]
pub mod controls;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;

/// `Hello::game_id`.
pub const GAME_ID: &str = "multiplayer_arena";
/// Game mode that quick play and the dev config use.
pub const MODE: &str = "arena";
/// Simulation rate of rooms and client prediction.
pub const TICK_HZ: f64 = 60.0;
/// The arena is a square of `2 × HALF_EXTENT` metres centred on the origin.
pub const HALF_EXTENT: f32 = 12.0;
/// Cube speed (m/s) at full stick.
pub const SPEED: f32 = 5.0;
/// Number of cube colours.
pub const COLORS: u8 = 8;

/// Player input: movement on the XZ plane. It carries state, not edges, so repeated and
/// redundant inputs are harmless.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CubeInput {
    /// Right (+) / left (−), `-127..=127`.
    pub x: i8,
    /// Back (+) / forward (−), `-127..=127`.
    pub z: i8,
}

impl CubeInput {
    /// Input from two axes in `-1..=1`.
    pub fn from_axes(x: f32, z: f32) -> Self {
        let q = |v: f32| (v.clamp(-1.0, 1.0) * 127.0).round() as i8;
        Self { x: q(x), z: q(z) }
    }

    /// Movement direction (length ≤ 1).
    pub fn direction(&self) -> Vec3 {
        let v = Vec3::new(f32::from(self.x), 0.0, f32::from(self.z)) / 127.0;
        if v.length_squared() > 1.0 { v.normalize() } else { v }
    }
}

impl NetInput for CubeInput {
    fn sanitize(&mut self) -> Result<(), InvalidInput> {
        // i8::MIN would make the axes asymmetric; clamping is enough (not a strike).
        self.x = self.x.max(-127);
        self.z = self.z.max(-127);
        Ok(())
    }
}

/// Colour index of a cube (`0..COLORS`), replicated to everyone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CubeColor(pub u8);

/// The shared protocol: add it in every server room and on every client, after the
/// replication plugin.
#[derive(Clone, Copy, Debug, Default)]
pub struct CubesProtocol;

impl Plugin for CubesProtocol {
    fn build(&self, app: &mut App) {
        // `ReplicationPlugin` registers `Transform` (predicted for the owner, interpolated for
        // everyone else) and `Name`.
        app.add_plugin(ReplicationPlugin::default())
            .replicate::<CubeColor>("cubes.color")
            .add_input::<CubeInput>("cubes.input")
            .add_sim_system("cubes::drive", drive);
    }
}

/// The predicted movement. Deterministic: it reads only [`SimTime`] and the
/// `PlayerInput<CubeInput>` of `Simulated` entities (every cube on the server, only the
/// controlled cube on a client).
pub fn drive(ctx: &mut SystemCtx) {
    let dt = ctx.resources.expect::<SimTime>().dt_secs();
    for (t, input) in ctx
        .world
        .query_mut::<(&mut Transform, &PlayerInput<CubeInput>)>()
        .with::<&Simulated>()
    {
        let step = input.0.direction() * SPEED * dt;
        t.translation.x = (t.translation.x + step.x).clamp(-HALF_EXTENT, HALF_EXTENT);
        t.translation.z = (t.translation.z + step.z).clamp(-HALF_EXTENT, HALF_EXTENT);
    }
}

/// Spawn point of the `index`-th cube (spread on a circle).
pub fn spawn_point(index: u32) -> Vec3 {
    let a = index as f32 * 2.399_963; // golden angle
    let r = HALF_EXTENT * 0.5;
    Vec3::new(a.cos() * r, 0.5, a.sin() * r)
}

/// Authoritative server rules: a cube per player, removed when the player leaves for good.
/// Disconnected players keep their cube during the resume window (the replication server
/// feeds it neutral input), so a reconnecting player gets the same cube back.
#[derive(Clone, Copy, Debug, Default)]
pub struct CubesRules;

/// The cube of `player` in a room world.
pub fn cube_of(world: &World, player: PlayerId) -> Option<Entity> {
    world
        .query::<(Entity, &ControlledBy)>()
        .iter()
        .find(|(_, c)| c.0 == player)
        .map(|(e, _)| e)
}

impl Plugin for CubesRules {
    fn build(&self, app: &mut App) {
        let mut joined = EventReader::default();
        let mut left = EventReader::default();
        let mut spawned = 0u32;
        app.add_system_named(Stage::Update, "cubes::rules", move |ctx| {
            let joins: Vec<PlayerJoined> = joined
                .read(ctx.resources.expect::<Events<PlayerJoined>>())
                .cloned()
                .collect();
            for j in joins {
                // A room restored from a checkpoint already has the player's cube: keep it.
                if let Some(e) = cube_of(ctx.world, j.identity.player) {
                    let _ = ctx.world.insert_one(e, Simulated);
                    continue;
                }
                let color = (j.identity.player.0 % u64::from(COLORS)) as u8;
                let at = spawn_point(spawned);
                spawned += 1;
                ctx.world.spawn((
                    Replicated,
                    Simulated,
                    ControlledBy(j.identity.player),
                    Transform::from_xyz(at.x, at.y, at.z),
                    Name(j.identity.display_name.clone()),
                    CubeColor(color),
                ));
            }
            let leaves: Vec<PlayerLeft> = left
                .read(ctx.resources.expect::<Events<PlayerLeft>>())
                .cloned()
                .collect();
            for l in leaves {
                // A closing room keeps its state for the final checkpoint.
                if matches!(l.reason, LeaveReason::RoomClosed) {
                    continue;
                }
                if let Some(e) = cube_of(ctx.world, l.player) {
                    let _ = ctx.world.despawn(e);
                }
            }
        });
    }
}

/// A wandering bot: picks a new random direction every 1–2 s, sometimes stands still.
/// Deterministic per seed (SplitMix64), so tests replay exactly.
#[derive(Clone, Debug)]
pub struct Wander {
    state: u64,
    input: CubeInput,
    next_turn: u32,
}

impl Wander {
    /// Bot with its own random stream.
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
            input: CubeInput::default(),
            next_turn: 0,
        }
    }

    /// Seed derived from a name (`bot-3` → stable seed).
    pub fn seed_of(name: &str) -> u64 {
        name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        })
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl space3d_replication::bots::Bot<CubeInput> for Wander {
    fn think(&mut self, _world: &World, tick: Tick) -> CubeInput {
        if tick.0 >= self.next_turn {
            let r = self.next();
            self.input = if r.is_multiple_of(5) {
                CubeInput::default()
            } else {
                let a = (r >> 8) as f32 / (1u64 << 56) as f32 * std::f32::consts::TAU;
                CubeInput::from_axes(a.cos(), a.sin())
            };
            self.next_turn = tick.0 + 60 + ((r >> 40) % 60) as u32;
        }
        self.input
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_replication::bots::Bot;

    #[test]
    fn input_helpers_and_sanitize() {
        let mut i = CubeInput { x: i8::MIN, z: 127 };
        assert!(i.sanitize().is_ok());
        assert_eq!(i.x, -127);
        assert!((i.direction().length() - 1.0).abs() < 1e-5);
        assert_eq!(CubeInput::from_axes(1.0, -0.5), CubeInput { x: 127, z: -64 });
        assert_eq!(CubeInput::from_axes(3.0, 0.0).x, 127);
    }

    #[test]
    fn wander_is_deterministic_and_valid() {
        let world = World::new();
        let run = |seed| {
            let mut b = Wander::new(seed);
            (0..600).map(|t| b.think(&world, Tick(t))).collect::<Vec<_>>()
        };
        let a = run(Wander::seed_of("bot-1"));
        assert_eq!(a, run(Wander::seed_of("bot-1")));
        assert_ne!(a, run(Wander::seed_of("bot-2")));
        assert!(a.iter().any(|i| *i != CubeInput::default()), "moves");
        assert!(a.iter().all(|i| i.x > i8::MIN && i.z > i8::MIN));
    }

    #[test]
    fn spawn_points_are_inside_the_arena() {
        for i in 0..64 {
            let p = spawn_point(i);
            assert!(p.x.abs() <= HALF_EXTENT && p.z.abs() <= HALF_EXTENT);
        }
    }
}
