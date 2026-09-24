//! Performance assertions: tick throughput, render statistics budgets and allocation counting.
//!
//! Wall-clock is only *measured* here — simulation time is still advanced with fixed ticks.
//! Budgets are multiplied by `SPACE3D_PERF_BUDGET_SCALE` (default 1.0) so slow CI machines
//! can relax them without code changes. Debug builds are much slower than release builds;
//! pick budgets accordingly (e.g. `if cfg!(debug_assertions) { .. }`).
//!
//! # Allocation counting
//! Opt in per test binary with [`install_counting_allocator!`](crate::install_counting_allocator)
//! (it installs [`CountingAllocator`] as the `#[global_allocator]`), then use
//! [`count_allocations`] / [`assert_max_allocations`]. Counts are per thread, so parallel tests
//! do not disturb each other; work done on other threads (rayon, the GPU driver) is not counted.

use crate::app::TestApp;
use crate::env;
use crate::report::{ReportEntry, ReportImages, ReportKind, ReportStatus, TestReport, scoped_check_name};
use serde::{Deserialize, Serialize};
use space3d_render::RenderStats;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use web_time::Instant;

/// Frame time statistics of [`TestApp::measure_ticks`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameTimings {
    /// Ticks measured.
    pub ticks: u64,
    /// Total wall time.
    pub total: Duration,
    /// Mean time per tick (frame).
    pub mean: Duration,
    /// Median.
    pub p50: Duration,
    /// 95th percentile.
    pub p95: Duration,
    /// Slowest tick.
    pub max: Duration,
    /// Throughput.
    pub ticks_per_second: f64,
}

impl FrameTimings {
    fn from_samples(mut samples: Vec<Duration>) -> Self {
        let total: Duration = samples.iter().sum();
        samples.sort_unstable();
        let n = samples.len();
        let pick = |q: f64| {
            if n == 0 {
                Duration::ZERO
            } else {
                samples[((n - 1) as f64 * q).round() as usize]
            }
        };
        Self {
            ticks: n as u64,
            total,
            mean: if n == 0 { Duration::ZERO } else { total / n as u32 },
            p50: pick(0.5),
            p95: pick(0.95),
            max: samples.last().copied().unwrap_or_default(),
            ticks_per_second: if total.is_zero() {
                f64::INFINITY
            } else {
                n as f64 / total.as_secs_f64()
            },
        }
    }
}

impl std::fmt::Display for FrameTimings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ticks in {:.2?}: mean {:.3?}, p50 {:.3?}, p95 {:.3?}, max {:.3?} ({:.0} ticks/s)",
            self.ticks, self.total, self.mean, self.p50, self.p95, self.max, self.ticks_per_second
        )
    }
}

impl TestApp {
    /// Step `ticks` ticks, timing every frame (including rendering when a renderer is in
    /// the frame loop).
    pub fn measure_ticks(&mut self, ticks: u64) -> FrameTimings {
        let mut samples = Vec::with_capacity(ticks as usize);
        for _ in 0..ticks {
            let start = Instant::now();
            self.step();
            samples.push(start.elapsed());
        }
        FrameTimings::from_samples(samples)
    }
}

fn scaled(d: Duration) -> Duration {
    d.mul_f64(env::perf_budget_scale())
}

/// Record a performance result in the `SPACE3D_TEST_REPORT` report (if enabled) under
/// `<current test>::<check>`, so assertions of different tests do not overwrite each other.
#[track_caller]
fn report_perf(check: &str, passed: bool, message: Option<String>, details: serde_json::Value) {
    if let Some(r) = TestReport::from_env() {
        let status = if passed {
            ReportStatus::Passed
        } else {
            ReportStatus::Failed
        };
        r.record_or_warn(
            ReportEntry::new(
                &std::env::var("CARGO_PKG_NAME").unwrap_or_default(),
                &scoped_check_name(check),
                ReportKind::Performance,
                status,
            )
            .message(message)
            .details(details),
            ReportImages::default(),
        );
    }
}

/// Run one warm-up tick, then `ticks` measured ticks; panic if the **mean** frame time exceeds
/// `budget` (scaled by `SPACE3D_PERF_BUDGET_SCALE`).
#[track_caller]
pub fn assert_frame_budget(app: &mut TestApp, ticks: u64, budget: Duration) -> FrameTimings {
    app.step();
    let t = app.measure_ticks(ticks);
    let limit = scaled(budget);
    let passed = t.mean <= limit;
    let msg = format!("frame budget: mean {:.3?} vs budget {limit:.3?} — {t}", t.mean);
    report_perf(
        "frame_budget",
        passed,
        (!passed).then(|| msg.clone()),
        serde_json::to_value(&t).unwrap_or_default(),
    );
    assert!(passed, "{msg}");
    t
}

/// Like [`assert_frame_budget`] but expressed as a minimum throughput.
#[track_caller]
pub fn assert_min_ticks_per_second(app: &mut TestApp, ticks: u64, min_ticks_per_second: f64) -> FrameTimings {
    app.step();
    let t = app.measure_ticks(ticks);
    let min = min_ticks_per_second / env::perf_budget_scale();
    let passed = t.ticks_per_second >= min;
    let msg = format!("throughput {:.0} ticks/s < required {min:.0} — {t}", t.ticks_per_second);
    report_perf(
        "ticks_per_second",
        passed,
        (!passed).then(|| msg.clone()),
        serde_json::to_value(&t).unwrap_or_default(),
    );
    assert!(passed, "{msg}");
    t
}

/// Upper (and a few lower) bounds on [`RenderStats`] of one frame.
///
/// ```
/// use space3d_render::RenderStats;
/// use space3d_test::RenderBudget;
/// let stats = RenderStats { draw_calls: 3, triangles: 900, ..Default::default() };
/// RenderBudget::new().max_draw_calls(4).max_triangles(1_000).assert_stats(&stats);
/// assert!(RenderBudget::new().max_draw_calls(2).check(&stats).is_err());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RenderBudget {
    /// Max `draw_calls`.
    pub max_draw_calls: Option<u32>,
    /// Max `batches`.
    pub max_batches: Option<u32>,
    /// Max `triangles`.
    pub max_triangles: Option<u64>,
    /// Max `instances_drawn`.
    pub max_instances_drawn: Option<u64>,
    /// Max `bytes_uploaded`.
    pub max_bytes_uploaded: Option<u64>,
    /// Max `buffer_reallocations`.
    pub max_buffer_reallocations: Option<u32>,
    /// Max `cpu_reallocations`.
    pub max_cpu_reallocations: Option<u32>,
    /// Max `compute_dispatches`.
    pub max_compute_dispatches: Option<u32>,
    /// Min `instances` (guards against accidentally empty scenes).
    pub min_instances: Option<u64>,
}

macro_rules! budget_setters {
    ($($field:ident: $ty:ty),* $(,)?) => {$(
        #[doc = concat!("Set `", stringify!($field), "`.")]
        pub fn $field(mut self, v: $ty) -> Self {
            self.$field = Some(v);
            self
        }
    )*};
}

impl RenderBudget {
    /// No bounds.
    pub fn new() -> Self {
        Self::default()
    }

    budget_setters!(
        max_draw_calls: u32,
        max_batches: u32,
        max_triangles: u64,
        max_instances_drawn: u64,
        max_bytes_uploaded: u64,
        max_buffer_reallocations: u32,
        max_cpu_reallocations: u32,
        max_compute_dispatches: u32,
        min_instances: u64,
    );

    /// Every violated bound, or `Ok`.
    pub fn check(&self, s: &RenderStats) -> Result<(), Vec<String>> {
        let mut v = Vec::new();
        let mut max = |name: &str, limit: Option<u64>, value: u64| {
            if let Some(l) = limit
                && value > l
            {
                v.push(format!("{name} = {value} > max {l}"));
            }
        };
        max("draw_calls", self.max_draw_calls.map(u64::from), s.draw_calls as u64);
        max("batches", self.max_batches.map(u64::from), s.batches as u64);
        max("triangles", self.max_triangles, s.triangles);
        max("instances_drawn", self.max_instances_drawn, s.instances_drawn);
        max("bytes_uploaded", self.max_bytes_uploaded, s.bytes_uploaded);
        max(
            "buffer_reallocations",
            self.max_buffer_reallocations.map(u64::from),
            s.buffer_reallocations as u64,
        );
        max(
            "cpu_reallocations",
            self.max_cpu_reallocations.map(u64::from),
            s.cpu_reallocations as u64,
        );
        max(
            "compute_dispatches",
            self.max_compute_dispatches.map(u64::from),
            s.compute_dispatches as u64,
        );
        if let Some(min) = self.min_instances
            && s.instances < min
        {
            v.push(format!("instances = {} < min {min}", s.instances));
        }
        if v.is_empty() { Ok(()) } else { Err(v) }
    }

    /// Panic listing every violated bound.
    #[track_caller]
    pub fn assert_stats(&self, stats: &RenderStats) {
        let result = self.check(stats);
        report_perf(
            "render_budget",
            result.is_ok(),
            result.as_ref().err().map(|v| v.join("\n")),
            serde_json::json!({
                "draw_calls": stats.draw_calls,
                "batches": stats.batches,
                "triangles": stats.triangles,
                "instances": stats.instances,
                "bytes_uploaded": stats.bytes_uploaded,
                "draw_path": format!("{:?}", stats.draw_path),
            }),
        );
        if let Err(v) = result {
            panic!("render budget exceeded:\n  - {}\n  stats: {stats:?}", v.join("\n  - "));
        }
    }

    /// Render one frame of `app` (without advancing time) and check its stats. Returns the
    /// stats, or `None` (skipped with a notice) when there is no GPU adapter.
    ///
    /// Panics when a GPU adapter exists but `app` has no renderer (built with
    /// [`RenderMode::Disabled`](crate::RenderMode::Disabled), or renderer creation failed).
    #[track_caller]
    pub fn assert_frame(&self, app: &mut TestApp) -> Option<RenderStats> {
        let Some(stats) = app.render_frame() else {
            if let Err(reason) = crate::gpu::gpu_info() {
                crate::gpu::skip_notice("render budget", reason);
                return None;
            }
            panic!(
                "render budget: the app has no renderer although a GPU adapter is available: {}",
                app.renderer_error().unwrap_or("frame rendering failed")
            );
        };
        self.assert_stats(&stats);
        Some(stats)
    }
}

// ---- allocation counting ------------------------------------------------------------------

static INSTALLED: AtomicBool = AtomicBool::new(false);
static GLOBAL_ALLOCS: AtomicU64 = AtomicU64::new(0);
static GLOBAL_BYTES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static DEALLOCS: Cell<u64> = const { Cell::new(0) };
    static REALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

fn bump(key: &'static std::thread::LocalKey<Cell<u64>>, by: u64) {
    let _ = key.try_with(|c| c.set(c.get().wrapping_add(by)));
}

/// Global allocator wrapper around [`System`] that counts allocations per thread (and
/// globally). Install it with [`install_counting_allocator!`](crate::install_counting_allocator).
#[derive(Clone, Copy, Debug, Default)]
pub struct CountingAllocator;

// SAFETY: every method forwards to `System` unchanged; the bookkeeping only touches atomics and
// const-initialized, destructor-free thread-locals (no allocation, no reentrancy).
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        INSTALLED.store(true, Ordering::Relaxed);
        bump(&ALLOCS, 1);
        bump(&BYTES, layout.size() as u64);
        GLOBAL_ALLOCS.fetch_add(1, Ordering::Relaxed);
        GLOBAL_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: forwarded with the caller's layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        INSTALLED.store(true, Ordering::Relaxed);
        bump(&ALLOCS, 1);
        bump(&BYTES, layout.size() as u64);
        GLOBAL_ALLOCS.fetch_add(1, Ordering::Relaxed);
        GLOBAL_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: forwarded with the caller's layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(&DEALLOCS, 1);
        // SAFETY: `ptr` was allocated by `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(&REALLOCS, 1);
        let grow = new_size.saturating_sub(layout.size()) as u64;
        bump(&BYTES, grow);
        GLOBAL_BYTES.fetch_add(grow, Ordering::Relaxed);
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Install [`CountingAllocator`] as the global allocator of the current binary (use once, at
/// the top level of an integration test file).
///
/// ```ignore
/// space3d_test::install_counting_allocator!();
///
/// #[test]
/// fn hot_loop_does_not_allocate() {
///     let mut v = Vec::with_capacity(16);
///     space3d_test::assert_max_allocations(0, || v.push(1));
/// }
/// ```
#[macro_export]
macro_rules! install_counting_allocator {
    () => {
        #[global_allocator]
        static SPACE3D_TEST_COUNTING_ALLOCATOR: $crate::perf::CountingAllocator = $crate::perf::CountingAllocator;
    };
}

/// Allocation counts of the current thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocStats {
    /// `alloc` + `alloc_zeroed` calls.
    pub allocations: u64,
    /// `dealloc` calls.
    pub deallocations: u64,
    /// `realloc` calls.
    pub reallocations: u64,
    /// Bytes requested by allocations and growing reallocations.
    pub bytes_allocated: u64,
}

impl AllocStats {
    /// Current thread's running totals (all zero unless [`CountingAllocator`] is installed).
    pub fn current_thread() -> Self {
        let get = |k: &'static std::thread::LocalKey<Cell<u64>>| k.try_with(Cell::get).unwrap_or(0);
        Self {
            allocations: get(&ALLOCS),
            deallocations: get(&DEALLOCS),
            reallocations: get(&REALLOCS),
            bytes_allocated: get(&BYTES),
        }
    }

    /// Allocations plus reallocations: every call that may hit the system allocator.
    pub fn total_calls(&self) -> u64 {
        self.allocations + self.reallocations
    }

    fn since(&self, start: &AllocStats) -> AllocStats {
        AllocStats {
            allocations: self.allocations.wrapping_sub(start.allocations),
            deallocations: self.deallocations.wrapping_sub(start.deallocations),
            reallocations: self.reallocations.wrapping_sub(start.reallocations),
            bytes_allocated: self.bytes_allocated.wrapping_sub(start.bytes_allocated),
        }
    }
}

/// Process-wide `(allocations, bytes)` since start (counting allocator only).
pub fn global_allocation_totals() -> (u64, u64) {
    (
        GLOBAL_ALLOCS.load(Ordering::Relaxed),
        GLOBAL_BYTES.load(Ordering::Relaxed),
    )
}

/// Whether [`CountingAllocator`] is the global allocator of this binary.
pub fn allocation_counting_enabled() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// Run `f` and return its result with the allocations it made on this thread.
///
/// Panics if [`CountingAllocator`] is not installed (the counts would be meaningless).
#[track_caller]
pub fn count_allocations<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
    assert!(
        allocation_counting_enabled(),
        "allocation counting needs `space3d_test::install_counting_allocator!();` in this test binary"
    );
    let start = AllocStats::current_thread();
    let out = f();
    let stats = AllocStats::current_thread().since(&start);
    (out, stats)
}

/// Run `f` and panic if it made more than `max` allocations + reallocations on this thread.
#[track_caller]
pub fn assert_max_allocations<R>(max: u64, f: impl FnOnce() -> R) -> R {
    let (out, stats) = count_allocations(f);
    assert!(
        stats.total_calls() <= max,
        "expected at most {max} allocations, got {} ({stats:?})",
        stats.total_calls()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timings_statistics() {
        let t = FrameTimings::from_samples((1..=100).map(Duration::from_millis).collect());
        assert_eq!(t.ticks, 100);
        assert_eq!(t.max, Duration::from_millis(100));
        assert_eq!(t.p50, Duration::from_millis(51));
        assert_eq!(t.p95, Duration::from_millis(95));
        assert_eq!(t.mean, Duration::from_micros(50_500));
        assert!((t.ticks_per_second - 100.0 / 5.05).abs() < 1e-9);
        assert!(t.to_string().contains("100 ticks"));
        let empty = FrameTimings::from_samples(Vec::new());
        assert_eq!(empty.mean, Duration::ZERO);
    }

    #[test]
    fn frame_budget_on_trivial_app() {
        let mut app = TestApp::no_render();
        let t = assert_frame_budget(&mut app, 50, Duration::from_millis(50));
        assert_eq!(t.ticks, 50);
        assert_eq!(app.tick(), 51);
        assert_min_ticks_per_second(&mut app, 20, 10.0);
    }

    #[test]
    #[should_panic(expected = "frame budget")]
    fn frame_budget_violation_panics() {
        // Fails on purpose: keep it out of a real SPACE3D_TEST_REPORT.
        crate::report::without_report(|| {
            let mut app = TestApp::no_render();
            app.add_system(space3d_core::Stage::Update, |_| {
                std::thread::sleep(Duration::from_millis(3))
            });
            assert_frame_budget(&mut app, 3, Duration::from_micros(1));
        });
    }

    #[test]
    fn render_budget_reports_all_violations() {
        let s = RenderStats {
            draw_calls: 10,
            batches: 4,
            triangles: 5,
            instances: 1,
            ..Default::default()
        };
        let err = RenderBudget::new()
            .max_draw_calls(2)
            .max_batches(3)
            .max_triangles(5)
            .min_instances(2)
            .check(&s)
            .unwrap_err();
        assert_eq!(err.len(), 3, "{err:?}");
        assert!(err[0].contains("draw_calls = 10 > max 2"));
        assert!(RenderBudget::new().check(&s).is_ok());
    }

    #[test]
    fn render_budget_without_renderer_skips_only_without_gpu() {
        let mut app = TestApp::no_render();
        let budget = RenderBudget::new().max_draw_calls(0);
        if crate::gpu::gpu_available() {
            // A GPU exists, so a missing renderer is a test bug, not a reason to skip.
            let err =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| budget.assert_frame(&mut app))).unwrap_err();
            let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(msg.contains("RenderMode::Disabled"), "{msg}");
        } else if !env::require_gpu() {
            assert!(budget.assert_frame(&mut app).is_none());
        }
    }

    #[test]
    fn reports_of_different_tests_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let report = TestReport::new(tmp.path());
        // Two tests (threads named like libtest names them) assert the same kind of budget.
        let record = |test: &str, passed: bool| {
            let report = report.clone();
            std::thread::Builder::new()
                .name(test.to_string())
                .spawn(move || {
                    let status = if passed {
                        ReportStatus::Passed
                    } else {
                        ReportStatus::Failed
                    };
                    let name = scoped_check_name("frame_budget");
                    let entry = ReportEntry::new("suite", &name, ReportKind::Performance, status);
                    report.record(entry, ReportImages::default())
                })
                .unwrap()
                .join()
                .unwrap()
                .unwrap();
        };
        record("physics::slow_test", false);
        record("render::fast_test", true);
        let entries = report.entries().unwrap();
        let names: Vec<(&str, ReportStatus)> = entries.iter().map(|e| (e.name.as_str(), e.status)).collect();
        assert_eq!(
            names,
            [
                ("physics::slow_test::frame_budget", ReportStatus::Failed),
                ("render::fast_test::frame_budget", ReportStatus::Passed)
            ]
        );
    }

    #[test]
    fn counting_requires_installation() {
        // The unit-test binary does not install the counting allocator.
        assert!(!allocation_counting_enabled());
        assert_eq!(AllocStats::current_thread(), AllocStats::default());
        let r = std::panic::catch_unwind(|| count_allocations(|| 1));
        assert!(r.is_err());
    }
}
