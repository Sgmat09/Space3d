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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    space3d_tasks::runtime().spawn(async move {
        #[cfg(unix)]
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                None
            }
        };
        loop {
            #[cfg(unix)]
            let got = match term.as_mut() {
                Some(term) => tokio::select! {
                    r = tokio::signal::ctrl_c() => r.is_ok().then_some("SIGINT"),
                    s = term.recv() => s.map(|_| "SIGTERM"),
                },
                None => tokio::signal::ctrl_c().await.ok().map(|_| "SIGINT"),
            };
            #[cfg(not(unix))]
            let got = tokio::signal::ctrl_c().await.ok().map(|_| "SIGINT");
            let Some(name) = got else { break };
            handle.drain();
            if handle.requests() >= 2 {
                tracing::warn!(signal = name, "second signal: forced stop");
            } else {
                tracing::info!(signal = name, "signal received: draining");
            }
        }
    });
}

/// How far a drain participant (game-server or player-service role) has got. Participants
/// update their entry every supervisor tick; the lifecycle waits for all of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Draining: nothing left that needs the drain window (no occupied match rooms).
    pub idle: bool,
    /// Stopping: everything persisted / acknowledged (rooms closed, outbox flushed).
    pub flushed: bool,
}

/// Resource: reports of all drain participants.
#[derive(Clone, Debug, Default)]
pub struct DrainGates {
    reports: std::collections::BTreeMap<&'static str, DrainReport>,
}

impl DrainGates {
    /// Update a participant's report.
    pub fn set(&mut self, name: &'static str, report: DrainReport) {
        self.reports.insert(name, report);
    }

    /// Every participant is idle.
    pub fn all_idle(&self) -> bool {
        self.reports.values().all(|r| r.idle)
    }

    /// Every participant is flushed.
    pub fn all_flushed(&self) -> bool {
        self.reports.values().all(|r| r.flushed)
    }
}

/// Resource: process exit code once `Stopped` (0 clean, 1 forced).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExitStatus(pub u8);

/// Longest wait for final checkpoints + outbox flush after the drain deadline.
pub const STOP_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Supervisor plugin: drives phases, drain deadlines and exit (`AppExit` when `Stopped`).
pub struct LifecyclePlugin {
    /// Shared handle.
    pub handle: LifecycleHandle,
    /// Drain budget.
    pub drain_timeout: std::time::Duration,
    /// Wall clock (deadlines; injectable for tests).
    pub clock: std::sync::Arc<dyn crate::clock::UnixClock>,
}

#[derive(Default)]
struct LifecycleState {
    drain_started_ms: Option<i64>,
    stop_started_ms: Option<i64>,
}

impl space3d_core::Plugin for LifecyclePlugin {
    fn build(&self, app: &mut space3d_core::App) {
        app.init_resource::<DrainGates>();
        app.init_resource::<ExitStatus>();
        let handle = self.handle.clone();
        let drain_ms = self.drain_timeout.as_millis() as i64;
        let flush_ms = STOP_FLUSH_TIMEOUT.as_millis() as i64;
        let clock = self.clock.clone();
        let mut state = LifecycleState::default();
        app.add_system_named(space3d_core::Stage::Last, "lifecycle::drive", move |ctx| {
            let now = clock.now_ms();
            let requests = handle.requests();
            match handle.phase() {
                Phase::Starting => handle.set_phase(Phase::Ready),
                Phase::Ready if requests >= 1 => {
                    tracing::info!(timeout_ms = drain_ms, "draining");
                    state.drain_started_ms = Some(now);
                    handle.set_phase(Phase::Draining);
                }
                Phase::Ready => {}
                Phase::Draining | Phase::Stopping if requests >= 2 => {
                    tracing::warn!("forced stop");
                    ctx.resources.insert(ExitStatus(1));
                    handle.set_phase(Phase::Stopped);
                }
                Phase::Draining => {
                    let started = *state.drain_started_ms.get_or_insert(now);
                    let gates = ctx.resources.expect::<DrainGates>();
                    if gates.all_idle() || now.saturating_sub(started) >= drain_ms {
                        tracing::info!("drain finished; stopping");
                        state.stop_started_ms = Some(now);
                        handle.set_phase(Phase::Stopping);
                    }
                }
                Phase::Stopping => {
                    let started = *state.stop_started_ms.get_or_insert(now);
                    let gates = ctx.resources.expect::<DrainGates>();
                    if gates.all_flushed() || now.saturating_sub(started) >= flush_ms {
                        if !gates.all_flushed() {
                            tracing::warn!("stop flush timed out");
                        }
                        handle.set_phase(Phase::Stopped);
                    }
                }
                Phase::Stopped => ctx.send(space3d_core::AppExit),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualUnixClock;
    use space3d_core::{App, Plugin};
    use std::sync::Arc;
    use std::time::Duration;

    fn app(handle: &LifecycleHandle, clock: &ManualUnixClock) -> App {
        let mut app = App::new().with_tickrate(10.0);
        LifecyclePlugin {
            handle: handle.clone(),
            drain_timeout: Duration::from_secs(60),
            clock: Arc::new(clock.clone()),
        }
        .build(&mut app);
        app
    }

    #[test]
    fn drain_waits_for_participants_then_deadline() {
        let handle = LifecycleHandle::default();
        let clock = ManualUnixClock::at(0);
        let mut a = app(&handle, &clock);
        a.step_ticks(1);
        assert_eq!(handle.phase(), Phase::Ready);
        a.resources.expect_mut::<DrainGates>().set("gs", DrainReport::default());
        handle.drain();
        a.step_ticks(2);
        assert_eq!(handle.phase(), Phase::Draining);
        clock.advance_ms(30_000);
        a.step_ticks(1);
        assert_eq!(
            handle.phase(),
            Phase::Draining,
            "participant busy, deadline not reached"
        );
        clock.advance_ms(31_000);
        a.step_ticks(1);
        assert_eq!(handle.phase(), Phase::Stopping, "deadline");
        a.resources.expect_mut::<DrainGates>().set(
            "gs",
            DrainReport {
                idle: true,
                flushed: true,
            },
        );
        a.step_ticks(2);
        assert_eq!(handle.phase(), Phase::Stopped);
        a.step_ticks(1);
        assert!(a.should_exit());
        assert_eq!(*a.resources.expect::<ExitStatus>(), ExitStatus(0));
    }

    #[test]
    fn idle_participants_stop_early_and_second_signal_forces() {
        let handle = LifecycleHandle::default();
        let clock = ManualUnixClock::at(0);
        let mut a = app(&handle, &clock);
        a.resources.expect_mut::<DrainGates>().set(
            "ps",
            DrainReport {
                idle: true,
                flushed: false,
            },
        );
        a.step_ticks(1);
        handle.drain();
        a.step_ticks(2);
        assert_eq!(handle.phase(), Phase::Stopping, "idle → stop immediately");
        clock.advance_ms(21_000);
        a.step_ticks(1);
        assert_eq!(handle.phase(), Phase::Stopped, "flush timeout");

        let handle = LifecycleHandle::default();
        let mut b = app(&handle, &clock);
        b.resources.expect_mut::<DrainGates>().set("gs", DrainReport::default());
        b.step_ticks(1);
        handle.drain();
        b.step_ticks(1);
        handle.drain();
        b.step_ticks(2);
        assert_eq!(handle.phase(), Phase::Stopped);
        assert_eq!(*b.resources.expect::<ExitStatus>(), ExitStatus(1));
    }
}
