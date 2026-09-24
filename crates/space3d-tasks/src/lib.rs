//! Async without blocking the engine.
//!
//! * [`spawn`] runs a future on the background runtime (tokio on native,
//!   the browser microtask queue on web) and returns a [`Task`] you can
//!   poll every frame with [`Task::poll`] — it never blocks.
//! * [`spawn_compute`] runs CPU-heavy closures on the rayon pool (native) so
//!   work like mesh building or texture decoding stays off the main thread.
//! * [`AsyncQueue`] + [`TasksPlugin`] let you attach a completion callback that
//!   runs on the main thread in `Stage::PreUpdate` with full world access.
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_tasks::{TasksPlugin, AsyncQueue};
//! let mut app = App::new();
//! app.add_plugin(TasksPlugin);
//! app.resources.expect_mut::<AsyncQueue>().spawn_then(async { 21 * 2 }, |ctx, v| {
//!     ctx.resources.insert(v);
//! });
//! for _ in 0..100 { app.step_ticks(1); if app.resources.contains::<i32>() { break } std::thread::sleep(std::time::Duration::from_millis(1)); }
//! assert_eq!(*app.resources.expect::<i32>(), 42);
//! ```

use futures::channel::oneshot;
use space3d_core::{App, Plugin, Stage, SystemCtx};
use std::future::Future;

/// `Send` on native, nothing on wasm (browser futures are single-threaded).
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send> MaybeSend for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSend for T {}

/// Handle to a background computation.
pub struct Task<T> {
    rx: oneshot::Receiver<T>,
    done: Option<T>,
    cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Ready,
    Cancelled,
}

impl<T> Task<T> {
    /// Non-blocking check; returns the value once (subsequent calls return `None`).
    pub fn poll(&mut self) -> Option<T> {
        self.check();
        self.done.take()
    }

    pub fn state(&mut self) -> TaskState {
        self.check();
        if self.done.is_some() {
            TaskState::Ready
        } else if self.cancelled {
            TaskState::Cancelled
        } else {
            TaskState::Pending
        }
    }

    fn check(&mut self) {
        if self.done.is_none() && !self.cancelled {
            match self.rx.try_recv() {
                Ok(Some(v)) => self.done = Some(v),
                Ok(None) => {}
                Err(_) => self.cancelled = true,
            }
        }
    }

    /// Block until complete. Only for tests / tools; never call from a system.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn block(mut self) -> Option<T> {
        if let Some(v) = self.done.take() {
            return Some(v);
        }
        futures::executor::block_on(self.rx).ok()
    }

    /// Await from other async code.
    pub async fn wait(mut self) -> Option<T> {
        if let Some(v) = self.done.take() {
            return Some(v);
        }
        self.rx.await.ok()
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .thread_name("space3d-io")
            .enable_all()
            .build()
            .expect("failed to start tokio runtime")
    })
}

/// Spawn an async future in the background.
pub fn spawn<T, F>(fut: F) -> Task<T>
where
    T: MaybeSend + 'static,
    F: Future<Output = T> + MaybeSend + 'static,
{
    let (tx, rx) = oneshot::channel();
    let wrapped = async move {
        let _ = tx.send(fut.await);
    };
    #[cfg(not(target_arch = "wasm32"))]
    runtime().spawn(wrapped);
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(wrapped);
    Task { rx, done: None, cancelled: false }
}

/// Run a CPU-bound closure off the main thread (rayon on native; inline on web).
pub fn spawn_compute<T, F>(f: F) -> Task<T>
where
    T: MaybeSend + 'static,
    F: FnOnce() -> T + MaybeSend + 'static,
{
    let (tx, rx) = oneshot::channel();
    #[cfg(not(target_arch = "wasm32"))]
    rayon::spawn(move || {
        let _ = tx.send(f());
    });
    #[cfg(target_arch = "wasm32")]
    {
        let _ = tx.send(f());
    }
    Task { rx, done: None, cancelled: false }
}

type Callback = Box<dyn FnMut(&mut SystemCtx) -> bool + Send>;

/// Main-thread completion queue. Callbacks run in `PreUpdate` once their task finishes.
#[derive(Default)]
pub struct AsyncQueue {
    // Mutex only to satisfy `Resources: Sync`; always accessed through `&mut`.
    pending: parking_lot::Mutex<Vec<Callback>>,
}

impl AsyncQueue {
    pub fn spawn_then<T, F>(&mut self, fut: F, then: impl FnOnce(&mut SystemCtx, T) + Send + 'static)
    where
        T: Send + 'static,
        F: Future<Output = T> + MaybeSend + 'static,
    {
        self.track(spawn(fut), then);
    }

    pub fn compute_then<T, F>(&mut self, f: F, then: impl FnOnce(&mut SystemCtx, T) + Send + 'static)
    where
        T: Send + 'static,
        F: FnOnce() -> T + MaybeSend + 'static,
    {
        self.track(spawn_compute(f), then);
    }

    /// Attach a main-thread callback to an existing task.
    pub fn track<T: Send + 'static>(&mut self, mut task: Task<T>, then: impl FnOnce(&mut SystemCtx, T) + Send + 'static) {
        let mut then = Some(then);
        self.pending.get_mut().push(Box::new(move |ctx| match task.state() {
            TaskState::Pending => false,
            TaskState::Cancelled => true,
            TaskState::Ready => {
                let v = task.poll().expect("ready");
                (then.take().expect("called once"))(ctx, v);
                true
            }
        }));
    }

    pub fn len(&self) -> usize {
        self.pending.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.pending.lock().is_empty()
    }
}

pub struct TasksPlugin;

impl Plugin for TasksPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AsyncQueue>();
        app.add_system_named(Stage::PreUpdate, "tasks::drain_completed", |ctx| {
            let mut pending = std::mem::take(ctx.resources.expect_mut::<AsyncQueue>().pending.get_mut());
            pending.retain_mut(|cb| !cb(ctx));
            // Callbacks may have queued more work; keep both.
            let q = ctx.resources.expect_mut::<AsyncQueue>();
            let q = q.pending.get_mut();
            pending.append(q);
            *q = pending;
        });
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn poll_is_non_blocking() {
        let mut t = spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            7
        });
        assert_eq!(t.poll(), None);
        assert_eq!(t.block(), Some(7));
    }

    #[test]
    fn engine_keeps_ticking_while_task_runs() {
        let mut app = App::new();
        app.add_plugin(TasksPlugin);
        app.resources.expect_mut::<AsyncQueue>().spawn_then(
            async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                "done"
            },
            |ctx, v: &'static str| {
                ctx.resources.insert(v);
            },
        );
        let mut frames = 0;
        while !app.resources.contains::<&'static str>() {
            app.step_ticks(1);
            frames += 1;
            std::thread::sleep(Duration::from_millis(1));
            assert!(frames < 10_000);
        }
        assert!(frames > 1, "engine must tick while the task is pending");
    }

    #[test]
    fn compute_runs_off_thread() {
        let main = std::thread::current().id();
        let t = spawn_compute(move || std::thread::current().id() != main);
        assert_eq!(t.block(), Some(true));
    }
}
