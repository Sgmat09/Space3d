#![warn(missing_docs)]
//! Space3d test harness — for the engine **and** for games built with it.
//!
//! * [`TestApp`] — deterministic headless [`App`](space3d_core::App): exactly one fixed tick per
//!   step, `InputPlugin`, `RenderPlugin` + headless renderer when a GPU exists, world and
//!   resource helpers, [`run_until`](TestApp::run_until).
//! * **Input simulation** — keys, mouse clicks/drags, touch taps/swipes/pinches, gamepads,
//!   all tick-accurate; [`InputScript`] (RON) can be recorded with [`InputRecorder`] and
//!   replayed.
//! * **Screenshot testing** — [`assert_screenshot!`] against golden PNGs in
//!   `<crate>/tests/__screenshots__/`, with tolerances, SSIM, diff images and per-backend
//!   golden variants ([`screenshot`] module docs describe the workflow and env switches).
//! * **Determinism** — [`assert_deterministic`] runs two apps in lockstep and compares world
//!   fingerprints every tick plus the final frames.
//! * **Performance** — [`assert_frame_budget`], [`RenderBudget`], allocation counting
//!   ([`install_counting_allocator!`], [`assert_max_allocations`]).
//! * **Reports** — `SPACE3D_TEST_REPORT=dir` writes JSON + an HTML report ([`report`]).
//! * **GPU tests skip** without an adapter: [`require_gpu!`], [`gpu_available`].
//!
//! Environment switches are listed in [`env`](mod@env).
//!
//! # A game testing itself
//! ```no_run
//! use space3d_core::prelude::*;
//! use space3d_platform::{ActionMap, ActionState, Binding, KeyCode};
//! use space3d_test::prelude::*;
//!
//! struct Player;
//!
//! fn game(app: &mut App) {
//!     app.resources.expect_mut::<ActionMap>().bind("right", Binding::Key(KeyCode::ArrowRight));
//!     app.world.spawn((Player, Transform::default()));
//!     app.add_system(Stage::FixedUpdate, |ctx| {
//!         let dx = ctx.resources.expect::<ActionState>().value("right") * 0.1;
//!         for t in ctx.world.query_mut::<&mut Transform>().with::<&Player>() {
//!             t.translation.x += dx;
//!         }
//!     });
//! }
//!
//! let mut app = TestApp::headless(128, 128);
//! game(&mut app);
//! app.play(&InputScript::new().hold_key(0, KeyCode::ArrowRight, 10));
//! let (_, t) = app.single::<Transform>();
//! assert!((t.translation.x - 1.0).abs() < 1e-5);
//! assert_screenshot!(&mut app, "player_moved_right");
//! ```

mod app;
pub mod compare;
pub mod determinism;
pub mod env;
pub mod gpu;
pub mod image_io;
pub mod input;
mod keys;
pub mod perf;
pub mod report;
pub mod screenshot;

pub use app::{RenderMode, RunUntilTimeout, TestApp, TestAppBuilder};
pub use compare::{CompareOptions, DiffStats, compare_images, diff_image, diff_image_with, ssim};
pub use determinism::{
    DeterminismCheck, DeterminismReport, EntityFingerprint, Fingerprinter, WorldFingerprint, assert_deterministic,
    world_hash,
};
pub use gpu::{GpuInfo, gpu_available, gpu_info};
pub use image_io::{ImageIoError, decode_png, encode_png, load_png, save_png};
pub use input::{InputEvent, InputRecorder, InputRecorderPlugin, InputScript, ScriptError, TimedInput};
pub use perf::{
    AllocStats, CountingAllocator, FrameTimings, RenderBudget, allocation_counting_enabled, assert_frame_budget,
    assert_max_allocations, assert_min_ticks_per_second, count_allocations,
};
pub use report::{ReportEntry, ReportImages, ReportKind, ReportStatus, TestReport};
pub use screenshot::{
    Captured, FailureKind, GoldenDir, GoldenPolicy, IntoScreenshot, NoScreenshot, ScreenshotFailure, ScreenshotOutcome,
    VariantPolicy, assert_screenshot_in, check_screenshot,
};

/// Common imports for tests.
pub mod prelude {
    pub use crate::{
        CompareOptions, DeterminismCheck, Fingerprinter, InputEvent, InputScript, RenderBudget, RenderMode, TestApp,
        assert_deterministic, assert_frame_budget, assert_screenshot, gpu_available, require_gpu,
    };
}
