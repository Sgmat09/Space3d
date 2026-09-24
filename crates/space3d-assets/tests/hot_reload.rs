//! Native hot reload: editing a file under the asset root re-loads it and emits `Modified`.
#![cfg(all(feature = "hot-reload", not(target_arch = "wasm32")))]

mod common;

use common::*;
use space3d_assets::edit::texture::encode_png;
use space3d_assets::*;
use space3d_core::{App, EventReader, ImageData};
use std::time::{Duration, Instant};

fn write_png(path: &std::path::Path, color: [u8; 4]) {
    std::fs::write(path, encode_png(&ImageData::filled(3, 3, color)).unwrap()).unwrap();
}

#[test]
fn modified_file_is_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("textures")).unwrap();
    let file = dir.path().join("textures/tile.png");
    write_png(&file, [255, 0, 0, 255]);

    let mut app = App::new();
    app.add_plugin(AssetsPlugin {
        root: dir.path().to_string_lossy().into_owned(),
        hot_reload: true,
    });
    assert!(app.resources.contains::<HotReloadWatcher>(), "watcher should start");
    let h: Handle<ImageData> = app.resources.expect_mut::<AssetServer>().load("textures/tile.png");
    run_until(&mut app, Duration::from_secs(20), |app| {
        app.resources.expect::<AssetServer>().is_loaded(&h)
    });
    let pixel = |app: &App| app.resources.expect::<Assets<ImageData>>().get(&h).unwrap().get(1, 1);
    assert_eq!(pixel(&app), [255, 0, 0, 255]);

    let mut reader = EventReader::default();
    read_events::<ImageData>(&app, &mut reader);
    // Give the OS watcher a moment to settle, then edit the file. Some backends (FSEvents)
    // coalesce or delay events, so rewrite periodically until the change is observed.
    std::thread::sleep(Duration::from_millis(100));
    write_png(&file, [0, 0, 255, 255]);
    let mut last_write = Instant::now();
    let mut modified = false;
    run_until(&mut app, Duration::from_secs(30), |app| {
        modified |= read_events::<ImageData>(app, &mut reader)
            .iter()
            .any(|e| e.is_modified(&h));
        if !modified && last_write.elapsed() > Duration::from_secs(3) {
            write_png(&file, [0, 0, 255, 255]);
            last_write = Instant::now();
        }
        modified && pixel(app) == [0, 0, 255, 255]
    });
    assert!(app.resources.expect::<AssetServer>().is_loaded(&h));
}

#[test]
fn missing_root_disables_watcher_without_failing() {
    let mut app = App::new();
    app.add_plugin(AssetsPlugin {
        root: "/definitely/not/a/real/dir".into(),
        hot_reload: true,
    });
    assert!(!app.resources.contains::<HotReloadWatcher>());
    app.step_ticks(2);
    let _ = fixtures();
}
