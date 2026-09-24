//! Full-stack, deterministic, in-process test cluster (feature `testing`): player service +
//! gatekeeper + rooms + N clients over the memory network, one `ManualClock` +
//! `ManualUnixClock`, no threads (rooms are stepped directly, shards are bypassed), in-memory
//! `Db`. Exercises the real login → matchmake → ticket → gatekeeper → room → play → result →
//! outbox → store path.
//!
//! ```ignore
//! let mut c = TestCluster::new(ArenaGame, TestClusterConfig::default());
//! let alice = c.guest("alice");
//! let bob = c.guest("bob");
//! let [a, b] = c.matchmake(&[&alice, &bob], "duel");        // runs matchmaker ticks, returns assignments
//! let ca = c.connect(&a, |app| { app.add_plugin(ArenaProtocol); });
//! let cb = c.connect(&b, |app| { app.add_plugin(ArenaProtocol); });
//! assert!(c.run_until(600, |c| c.client_net(ca).is_connected() && c.client_net(cb).is_connected()));
//! c.crash_room(a.room_id);                                  // panic → restore from checkpoint
//! ```

use crate::clock::ManualUnixClock;
use crate::config::ServerConfig;
use crate::game::GameDefinition;
use space3d_core::App;
use space3d_replication::api::{Assignment, SessionTokens};
use space3d_replication::harness::{ClientHandle, ClientSpec};
use space3d_replication::time::ManualClock;
use space3d_replication::transport::conditioner::LinkProfile;
use space3d_replication::{RoomId, client::NetClient};

/// Cluster settings.
#[derive(Clone, Debug)]
pub struct TestClusterConfig {
    /// Server config (role forced to all-in-one, in-memory DB).
    pub server: ServerConfig,
    /// Seed.
    pub seed: u64,
    /// Default link.
    pub link: LinkProfile,
    /// Start of the wall clock (unix ms).
    pub start_ms: i64,
}

impl Default for TestClusterConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            seed: 1,
            link: LinkProfile::default(),
            start_ms: 1_800_000_000_000,
        }
    }
}

/// The cluster.
pub struct TestCluster {
    clock: ManualClock,
    unix: ManualUnixClock,
    rooms: std::collections::BTreeMap<RoomId, App>,
    clients: Vec<App>,
}

impl TestCluster {
    /// Build.
    pub fn new<G: GameDefinition>(game: G, config: TestClusterConfig) -> Self {
        todo!()
    }

    /// Guest login (in-process call; no HTTP).
    pub fn guest(&mut self, name: &str) -> SessionTokens {
        todo!()
    }

    /// Queue `players` and step the matchmaker until all have assignments.
    pub fn matchmake(&mut self, players: &[&SessionTokens], queue: &str) -> Vec<Assignment> {
        todo!()
    }

    /// Connect a client with an assignment.
    pub fn connect(&mut self, assignment: &Assignment, build: impl FnOnce(&mut App)) -> ClientHandle {
        todo!()
    }

    /// Advance one tick (network, gatekeeper, rooms, clients, supervisor systems, clocks).
    pub fn step(&mut self) {
        todo!()
    }

    /// Step until `pred` or `max_ticks`.
    pub fn run_until(&mut self, max_ticks: u64, pred: impl FnMut(&Self) -> bool) -> bool {
        todo!()
    }

    /// A room.
    pub fn room(&self, id: RoomId) -> Option<&App> {
        self.rooms.get(&id)
    }

    /// A client's `NetClient`.
    pub fn client_net(&self, c: ClientHandle) -> &NetClient {
        self.clients[c.0].resources.expect()
    }

    /// Panic a room's next tick (exercises catch_unwind + checkpoint restore).
    pub fn crash_room(&mut self, id: RoomId) {
        todo!()
    }

    /// Simulate a game-server loss (all rooms gone; lease expiry; rejoin path).
    pub fn kill_game_server(&mut self) {
        todo!()
    }

    /// Wall clock.
    pub fn unix_clock(&self) -> &ManualUnixClock {
        &self.unix
    }
}

/// Re-exported so tests can write `ClientSpec` without another import.
pub type ClientSpecAlias = ClientSpec;
