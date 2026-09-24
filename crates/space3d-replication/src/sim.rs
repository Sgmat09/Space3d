//! The shared deterministic simulation schedule.
//!
//! `space3d-core` runs systems in registration order and a system cannot re-run
//! `FixedUpdate`, so gameplay that must be predicted is registered once into [`SimSchedule`]
//! (`app.add_sim_system(..)`) and executed by the replication drivers:
//!
//! * server: once per tick inside `replication::server_tick` (after inputs are applied),
//! * client: once per predicted tick inside `replication::client_tick`, and again for each
//!   replayed tick after a misprediction (`SimTime::resimulating == true`).
//!
//! Rules for sim systems: query `With<&Simulated>`; read [`SimTime`] (its `dt` is the nominal
//! step even while the client dilates time); never consume network data via `Events` inside
//! the sim (use `PlayerInput<I>`); guard cosmetic side effects with `!resimulating`.
//! `space3d_physics::physics_step` may be added as a sim system on the server (rapier state is
//! not rolled back on clients in v1: predicted characters use kinematic controllers).

use crate::Tick;
use space3d_core::SystemCtx;
use space3d_core::schedule::BoxedSystem;
use std::time::Duration;

/// Which side runs the simulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimRole {
    /// Authoritative room.
    Server,
    /// Predicting client.
    Client,
}

/// Simulation clock for the tick being simulated (resource, set by the drivers).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimTime {
    /// The tick being simulated; state after the sim run is "state at `tick`".
    pub tick: Tick,
    /// Nominal fixed step (never dilated).
    pub dt: Duration,
    /// Server or client.
    pub role: SimRole,
    /// `true` while a client replays ticks after a misprediction.
    pub resimulating: bool,
}

impl SimTime {
    /// `dt` in seconds.
    pub fn dt_secs(&self) -> f32 {
        self.dt.as_secs_f32()
    }
}

/// Ordered list of sim systems (resource). `Mutex` only to make the boxed `FnMut`s `Sync`.
#[derive(Default)]
pub struct SimSchedule {
    systems: parking_lot::Mutex<Vec<(&'static str, BoxedSystem)>>,
}

impl SimSchedule {
    /// Append a system (runs in insertion order).
    pub fn add(&mut self, name: &'static str, system: impl FnMut(&mut SystemCtx) + Send + 'static) {
        self.systems.get_mut().push((name, Box::new(system)));
    }

    /// System names in run order.
    pub fn names(&self) -> Vec<&'static str> {
        self.systems.lock().iter().map(|(n, _)| *n).collect()
    }

    /// Number of systems.
    pub fn len(&self) -> usize {
        self.systems.lock().len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.systems.lock().is_empty()
    }
}

/// Run the whole [`SimSchedule`] once for `time`: inserts `time` as the [`SimTime`] resource,
/// pins `Time::fixed_delta` to `time.dt` for the duration (so `physics_step` and other
/// `fixed_delta` readers stay deterministic while the client dilates `TickRate`), runs every
/// sim system in order, then restores `fixed_delta`. Used by both drivers.
pub fn run_sim(ctx: &mut SystemCtx, time: SimTime) {
    todo!()
}
