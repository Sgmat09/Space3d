//! Localhost-only integration tests (no external network). One test binary so the
//! heavy dependency graph is linked once.
#![cfg(not(target_arch = "wasm32"))]

mod channel;
mod http;
mod ws;

use space3d_core::App;
use std::time::{Duration, Instant};

/// Generous deadline: only reached when something is broken.
const DEADLINE: Duration = Duration::from_secs(5);

/// Spin (with 1 ms naps) until `cond` holds. Panics after [`DEADLINE`].
pub fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Drive `app` one fixed tick per frame (plus a 1 ms nap so I/O can progress)
/// until `cond` holds. Returns the number of frames stepped.
pub fn pump_until(app: &mut App, what: &str, mut cond: impl FnMut(&mut App) -> bool) -> u64 {
    let deadline = Instant::now() + DEADLINE;
    let mut frames = 0;
    while !cond(app) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} after {frames} frames"
        );
        app.step_ticks(1);
        frames += 1;
        std::thread::sleep(Duration::from_millis(1));
    }
    frames
}
