//! Process lifecycle: `Starting → Ready → Draining → Stopping → Stopped`.
//!
//! SIGTERM / SIGINT (tokio `signal`), `POST /admin/v1/drain`, or `LifecycleHandle::drain()`:
//! 1. **Draining**: `/readyz` → 503; fleet heartbeat `draining = true` (no new allocations);
//!    new tickets get `Reject(ServerDraining)` (resumes still accepted); rooms get
//!    `RoomCommand::Drain { deadline }` → `Notice::ShuttingDown` to clients.
//! 2. Match rooms run to their natural end until `drain_timeout`; persistent rooms checkpoint
//!    immediately and send `Notice::Reconnect` (clients rejoin via the player service and are
//!    restored elsewhere).
//! 3. At the deadline: checkpoint every room, await uploads + outbox flush (≤ 20 s),
//!    `Disconnect(ServerShutdown)`, stop listeners, deregister, drop `Db` (drains its queue),
//!    exit 0.
//!
//! A second signal checkpoints to local storage only and exits 1.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Lifecycle phase.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Binding, migrating, registering.
    Starting = 0,
    /// Serving.
    Ready = 1,
    /// Graceful drain in progress.
    Draining = 2,
    /// Final flushes.
    Stopping = 3,
    /// Done; the runner exits.
    Stopped = 4,
}

/// Shared lifecycle control (clone = shared; usable from signal handlers and HTTP handlers).
#[derive(Clone, Debug, Default)]
pub struct LifecycleHandle {
    phase: Arc<AtomicU8>,
    signals: Arc<AtomicU8>,
}

impl LifecycleHandle {
    /// Current phase.
    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Acquire) {
            0 => Phase::Starting,
            1 => Phase::Ready,
            2 => Phase::Draining,
            3 => Phase::Stopping,
            _ => Phase::Stopped,
        }
    }

    /// Set the phase (supervisor only).
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    /// Request a graceful drain (first call) or a forced stop (second call).
    pub fn drain(&self) {
        self.signals.fetch_add(1, Ordering::AcqRel);
    }

    /// Number of drain/stop requests so far.
    pub fn requests(&self) -> u8 {
        self.signals.load(Ordering::Acquire)
    }
}

/// Spawn the SIGTERM/SIGINT listener on the I/O runtime (each signal → `handle.drain()`).
pub fn install_signal_handlers(handle: LifecycleHandle) {
    todo!()
}

/// Supervisor plugin: drives phases, drain deadlines and exit (`AppExit` when `Stopped`).
pub struct LifecyclePlugin {
    /// Shared handle.
    pub handle: LifecycleHandle,
    /// Drain budget.
    pub drain_timeout: std::time::Duration,
}

impl space3d_core::Plugin for LifecyclePlugin {
    fn build(&self, app: &mut space3d_core::App) {
        todo!()
    }
}
