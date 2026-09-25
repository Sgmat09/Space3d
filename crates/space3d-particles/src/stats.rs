//! Counters ([`ParticleStats`]) and read-only runtime state ([`ParticleRuntime`]).

use crate::ParticleBackend;
use space3d_core::Entity;

/// Counters of the last rendered frame (resource).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParticleStats {
    /// Resolved backend (`Gpu`/`Cpu`); `None` before a renderer exists.
    pub backend: Option<ParticleBackend>,
    /// Live effect instances.
    pub effects: u32,
    /// Live layer instances.
    pub layer_instances: u32,
    /// Particle pool capacity.
    pub pool_capacity: u32,
    /// Pool slots reserved by live layer instances.
    pub pool_reserved: u32,
    /// Alive particles (CPU: exact, this frame; GPU: read back 1–3 frames late).
    pub alive: u32,
    /// Frame `alive` was measured in.
    pub alive_frame: Option<u64>,
    /// Particles spawned since the start.
    pub spawned: u64,
    /// Layer instances refused for lack of budget this frame.
    pub refused: u32,
    /// Layer instances shrunk to fit the budget this frame.
    pub shrunk: u32,
    /// Ticks simulated this frame.
    pub ticks_simulated: u32,
    /// Catch-up windows exceeded (simulation reset instead of replayed).
    pub catchup_resets: u32,
    /// Compute dispatches this frame.
    pub dispatches: u32,
    /// Draw calls this frame.
    pub draw_calls: u32,
    /// Ribbon segments drawn this frame.
    pub ribbon_segments: u32,
    /// Bytes uploaded this frame.
    pub bytes_uploaded: u64,
    /// Scratch vectors that grew this frame (0 in the steady state).
    pub cpu_reallocations: u32,
}

/// Read-only view of the particle runtime (resource), for gameplay and UI queries.
#[derive(Debug, Default)]
pub struct ParticleRuntime {
    _private: (),
}

impl ParticleRuntime {
    /// State of the effect instance playing on `entity`.
    pub fn state(&self, entity: Entity) -> Option<EffectState> {
        let _ = entity; // STUB(P-PART): the runtime (PA0)
        None
    }
}

/// State of one effect instance.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct EffectState {
    /// Seconds since the (re)start.
    pub time: f32,
    /// Still emitting.
    pub emitting: bool,
    /// Fully played.
    pub finished: bool,
    /// Pool slots reserved.
    pub reserved: u32,
}
