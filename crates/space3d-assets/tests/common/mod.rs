#![allow(dead_code)]

use space3d_assets::{AssetEvent, AssetServer, AssetsPlugin};
use space3d_core::{App, EventReader, Events};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// App with the assets plugin rooted at `root`, hot reload off.
pub fn app_with_root(root: &std::path::Path) -> App {
    let mut app = App::new();
    app.add_plugin(AssetsPlugin {
        root: root.to_string_lossy().into_owned(),
        hot_reload: false,
    });
    app
}

/// Step the app one fixed tick at a time (sleeping briefly between frames so background
/// I/O can progress) until `done` holds. Returns the number of frames stepped.
pub fn run_until(app: &mut App, timeout: Duration, mut done: impl FnMut(&mut App) -> bool) -> u32 {
    let start = Instant::now();
    let mut frames = 0;
    loop {
        app.step_ticks(1);
        frames += 1;
        if done(app) {
            return frames;
        }
        assert!(start.elapsed() < timeout, "timed out after {frames} frames");
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub fn wait_idle(app: &mut App) -> u32 {
    run_until(app, Duration::from_secs(20), |app| {
        app.resources.expect::<AssetServer>().is_idle()
    })
}

/// Read new `AssetEvent<T>`s with a persistent reader.
pub fn read_events<T: Send + Sync + 'static>(app: &App, reader: &mut EventReader) -> Vec<AssetEvent<T>> {
    reader
        .read(app.resources.expect::<Events<AssetEvent<T>>>())
        .copied()
        .collect()
}
