//! Installing the profiler/console `tracing` layers.

use crate::console::ConsoleLayer;
use crate::profiler::ProfilerLayer;
use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};
use tracing::Subscriber;
use tracing::dispatcher::Dispatch;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, filter_fn};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;

/// Default log directives (stderr / browser console and the in-game console).
/// (`egui_wgpu=error`: it warns about sRGB targets, which the engine always uses and egui handles.)
pub(crate) const DEFAULT_FILTER: &str = "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,egui_wgpu=error";

/// How the UI `tracing` layers are installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TracingInstall {
    /// Not installed.
    None,
    /// [`install_tracing`] installed the global subscriber: log output (stderr natively, the
    /// browser console on the web) + [`ProfilerLayer`] + [`ConsoleLayer`] on every thread.
    Global,
    /// Another global subscriber already existed (e.g. `space3d::LogPlugin`): the layers run in
    /// a thread-local subscriber on the threads that installed them, which forwards events to
    /// the global subscriber. Profiling works for the app thread; the console only sees logs
    /// emitted on that thread.
    ThreadLocal,
}

const STATE_NONE: u8 = 0;
const STATE_GLOBAL: u8 = 1;
static GLOBAL_STATE: AtomicU8 = AtomicU8::new(STATE_NONE);
static INSTALL_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

thread_local! {
    static LOCAL_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// How the layers are installed as seen from the calling thread.
pub fn tracing_install_state() -> TracingInstall {
    if GLOBAL_STATE.load(Ordering::Acquire) == STATE_GLOBAL {
        TracingInstall::Global
    } else if LOCAL_INSTALLED.with(Cell::get) {
        TracingInstall::ThreadLocal
    } else {
        TracingInstall::None
    }
}

fn env_filter(default: &str) -> EnvFilter {
    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(f) = EnvFilter::try_from_default_env() {
        return f;
    }
    EnvFilter::try_new(default).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Filter selecting the spans the profiler aggregates.
fn profiler_filter<S: Subscriber>() -> impl tracing_subscriber::layer::Filter<S> {
    filter_fn(|m| m.is_span() && (m.name() == "system" || m.name() == "frame") && crate::profiler::is_enabled())
}

/// Install the profiler and console layers (idempotent; [`crate::UiPlugin`] calls it).
///
/// Without an existing global subscriber, a global one is installed with log output filtered
/// by `RUST_LOG` or `filter` (stderr natively, the browser console on the web) and `log`
/// records bridged. Otherwise the layers are installed thread-locally on the calling thread
/// (see [`TracingInstall::ThreadLocal`]). Apps with their own subscriber can instead add
/// [`ProfilerLayer`] and [`ConsoleLayer`] to it before adding the plugin.
pub fn install_tracing(filter: &str) -> TracingInstall {
    let state = tracing_install_state();
    if state != TracingInstall::None {
        return state;
    }
    let _guard = INSTALL_LOCK.lock();
    if GLOBAL_STATE.load(Ordering::Acquire) == STATE_GLOBAL {
        return TracingInstall::Global;
    }
    if !tracing::dispatcher::has_been_set() {
        #[cfg(not(target_arch = "wasm32"))]
        let output = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(env_filter(filter));
        // Each event at its console level (`console.error`, `.warn`, …).
        #[cfg(target_arch = "wasm32")]
        let output = space3d_platform::ConsoleLayer.with_filter(env_filter(filter));
        let subscriber = Registry::default()
            .with(output)
            .with(ProfilerLayer::new().with_filter(profiler_filter()))
            .with(ConsoleLayer::new().with_filter(env_filter(filter)));
        // `try_init` also bridges `log` records; it may fail on that part only.
        let _ = subscriber.try_init();
        if tracing::dispatcher::has_been_set() {
            GLOBAL_STATE.store(STATE_GLOBAL, Ordering::Release);
            return TracingInstall::Global;
        }
    }

    // Someone else owns the global subscriber: run ours on this thread and forward events.
    let global = tracing::dispatcher::get_default(Dispatch::clone);
    let subscriber = Registry::default()
        .with(ProfilerLayer::new().with_filter(profiler_filter()))
        .with(ConsoleLayer::new().with_filter(env_filter(filter)))
        .with(ForwardLayer { target: global }.with_filter(filter_fn(|m| m.is_event())));
    let guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
    // Keep the thread-local default for the lifetime of the thread.
    std::mem::forget(guard);
    LOCAL_INSTALLED.with(|c| c.set(true));
    TracingInstall::ThreadLocal
}

/// Forwards events to another dispatcher (the pre-existing global subscriber).
struct ForwardLayer {
    target: Dispatch,
}

impl<S: Subscriber> Layer<S> for ForwardLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if self.target.enabled(event.metadata()) {
            self.target.event(event);
        }
    }
}
