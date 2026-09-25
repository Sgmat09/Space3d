//! Test hooks (not part of the public API; hidden from the docs): read the simulated pool back
//! for the backend parity tests, and pretend a smaller storage binding limit (the renderer's
//! `TEST_LIMITS` pattern) for the budget tests.

use crate::ParticleBackend;
use crate::render::ParticlesNode;
use space3d_core::App;
use space3d_core::glam::Vec3;
use std::cell::Cell;
use std::sync::{Arc, Mutex};

/// The plugin's render node (resource).
pub(crate) struct NodeHandle(pub Arc<Mutex<ParticlesNode>>);

thread_local! {
    static STORAGE_LIMIT: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Pretend the device's storage bindings (and buffers) hold at most `bytes` on this thread
/// (`None`: the device's limits). The GPU pool budget follows; the device itself is unchanged.
pub fn set_storage_limit(bytes: Option<u64>) {
    STORAGE_LIMIT.set(bytes);
}

pub(crate) fn storage_limit() -> Option<u64> {
    STORAGE_LIMIT.get()
}

/// A live pool slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotState {
    /// Pool slot.
    pub slot: u32,
    /// Position (simulation space).
    pub pos: Vec3,
    /// Velocity.
    pub vel: Vec3,
    /// Age in seconds (negative: delayed).
    pub age: f32,
    /// Lifetime in seconds.
    pub life: f32,
    /// Particle seed.
    pub seed: u32,
    /// Layer instance.
    pub entry: u32,
}

/// The backend of the last frame and every live slot of its pool (the GPU pool is copied and
/// waited for: tests only). `None` before the first frame or without a renderer.
pub fn read_pool(app: &App) -> Option<(ParticleBackend, Vec<SlotState>)> {
    let node = app.resources.get::<NodeHandle>()?.0.clone();
    let renderer = app.resources.get::<space3d_render::Renderer>()?;
    let node = node.lock().unwrap_or_else(|e| e.into_inner());
    let (backend, pool) = node.read_pool(renderer.device(), renderer.queue())?;
    let slots = pool
        .iter()
        .enumerate()
        .filter(|(_, p)| p.vel_life[3] != 0.0)
        .map(|(i, p)| SlotState {
            slot: i as u32,
            pos: p.pos(),
            vel: p.vel(),
            age: p.pos_age[3],
            life: p.vel_life[3],
            seed: p.misc[1],
            entry: p.misc[2],
        })
        .collect();
    Some((backend, slots))
}

/// Size in bytes of the GPU backend's pool buffer (`None` before it exists).
pub fn gpu_pool_bytes(app: &App) -> Option<u64> {
    let node = app.resources.get::<NodeHandle>()?.0.clone();
    let mut node = node.lock().unwrap_or_else(|e| e.into_inner());
    node.gpu_sim()?.pool_buffer().map(|b| b.size())
}
