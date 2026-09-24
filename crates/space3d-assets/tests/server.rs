//! Asset server behaviour driven through a real `App` with `step_ticks`.

mod common;

use common::*;
use space3d_assets::edit::texture::encode_png;
use space3d_assets::*;
use space3d_core::{App, EventReader, ImageData, MeshData};
use std::time::Duration;

const T: Duration = Duration::from_secs(20);

fn png(color: [u8; 4]) -> Vec<u8> {
    encode_png(&ImageData::filled(2, 2, color)).unwrap()
}

fn mem_app() -> (App, MemorySource) {
    let mem = MemorySource::new();
    let mut app = App::new();
    app.add_plugin(AssetsPlugin {
        root: "does-not-exist".into(),
        hot_reload: false,
    });
    app.resources.expect_mut::<AssetServer>().add_source("mem", mem.clone());
    (app, mem)
}

#[test]
fn async_load_completes_while_engine_ticks() {
    let (mut app, mem) = mem_app();
    mem.insert("red.png", png([255, 0, 0, 255]));
    let mut reader = EventReader::default();
    let handle: Handle<ImageData> = app.resources.expect_mut::<AssetServer>().load("mem://red.png");
    assert!(handle.is_strong());
    assert_eq!(
        app.resources.expect::<AssetServer>().load_state(&handle),
        LoadState::Loading
    );
    assert!(app.resources.expect::<Assets<ImageData>>().get(&handle).is_none());

    let mut created = false;
    let frames = run_until(&mut app, T, |app| {
        created |= read_events::<ImageData>(app, &mut reader)
            .iter()
            .any(|e| e.is_created(&handle));
        app.resources.expect::<AssetServer>().is_loaded(&handle) && created
    });
    assert!(frames >= 1);
    let img = app.resources.expect::<Assets<ImageData>>().get(&handle).unwrap();
    assert_eq!(img.get(1, 1), [255, 0, 0, 255]);
    assert_eq!(
        app.resources.expect::<AssetServer>().path(&handle).unwrap().to_string(),
        "mem://red.png"
    );
}

#[test]
fn filesystem_source_loads_fixture() {
    let mut app = app_with_root(&fixtures());
    let h: Handle<ImageData> = app.resources.expect_mut::<AssetServer>().load("checker.png");
    run_until(&mut app, T, |app| {
        !app.resources.expect::<AssetServer>().load_state(&h).is_loading()
    });
    assert_eq!(app.resources.expect::<AssetServer>().load_state(&h), LoadState::Loaded);
    assert_eq!(app.resources.expect::<Assets<ImageData>>().get(&h).unwrap().width, 4);
}

#[test]
fn same_path_same_handle_and_types_are_separate() {
    let (mut app, mem) = mem_app();
    mem.insert("a.png", png([1, 2, 3, 4]));
    let server = app.resources.expect_mut::<AssetServer>();
    let a: Handle<ImageData> = server.load("mem://a.png");
    let b: Handle<ImageData> = server.load("mem://./a.png");
    assert_eq!(a, b);
    assert_eq!(a.strong_count(), 2);
    assert!(server.get_handle::<ImageData>("mem://a.png").is_some());
    wait_idle(&mut app);
    assert_eq!(app.resources.expect::<Assets<ImageData>>().len(), 1);
}

#[test]
fn dropping_all_handles_unloads() {
    let (mut app, mem) = mem_app();
    mem.insert("a.png", png([9, 9, 9, 255]));
    let mut reader = EventReader::default();
    let h: Handle<ImageData> = app.resources.expect_mut::<AssetServer>().load("mem://a.png");
    let weak = h.clone_weak();
    wait_idle(&mut app);
    app.step_ticks(1);
    assert!(app.resources.expect::<Assets<ImageData>>().contains(&weak));
    read_events::<ImageData>(&app, &mut reader);

    drop(h);
    app.step_ticks(1);
    let events = read_events::<ImageData>(&app, &mut reader);
    assert!(events.iter().any(|e| e.is_removed(&weak)), "{events:?}");
    assert!(!app.resources.expect::<Assets<ImageData>>().contains(&weak));
    let server = app.resources.expect_mut::<AssetServer>();
    assert_eq!(server.load_state(&weak), LoadState::NotLoaded);
    assert!(server.get_handle::<ImageData>("mem://a.png").is_none());

    // Loading again allocates a fresh generation.
    let again: Handle<ImageData> = server.load("mem://a.png");
    assert_ne!(again.id(), weak.id());
    wait_idle(&mut app);
    assert!(app.resources.expect::<Assets<ImageData>>().contains(&again));
}

#[test]
fn dropping_during_load_discards_result() {
    let (mut app, mem) = mem_app();
    mem.insert("a.png", png([9, 9, 9, 255]));
    let h: Handle<ImageData> = app.resources.expect_mut::<AssetServer>().load("mem://a.png");
    let id = h.id();
    drop(h);
    app.step_ticks(3);
    std::thread::sleep(Duration::from_millis(20));
    app.step_ticks(3);
    assert!(app.resources.expect::<AssetServer>().is_idle());
    assert!(app.resources.expect::<Assets<ImageData>>().get(id).is_none());
    assert!(app.resources.expect::<Assets<ImageData>>().is_empty());
}

#[test]
fn failures_are_reported() {
    let (mut app, mem) = mem_app();
    mem.insert("bad.png", b"definitely not a png".to_vec());
    mem.insert("x.unknown", b"?".to_vec());
    let server = app.resources.expect_mut::<AssetServer>();
    let missing: Handle<ImageData> = server.load("mem://missing.png");
    let bad: Handle<ImageData> = server.load("mem://bad.png");
    let unknown: Handle<ImageData> = server.load("mem://x.unknown");
    let no_source: Handle<ImageData> = server.load("nope://a.png");
    let escape: Handle<ImageData> = server.load("../outside.png");
    // Loader / source problems are detected synchronously.
    assert!(matches!(
        server.load_state(&unknown),
        LoadState::Failed(AssetError::NoLoader { .. })
    ));
    assert!(matches!(
        server.load_state(&no_source),
        LoadState::Failed(AssetError::UnknownSource(_))
    ));
    wait_idle(&mut app);
    let server = app.resources.expect::<AssetServer>();
    assert!(matches!(
        server.load_state(&missing),
        LoadState::Failed(AssetError::NotFound(_))
    ));
    assert!(matches!(
        server.load_state(&bad),
        LoadState::Failed(AssetError::Decode { .. })
    ));
    assert!(matches!(
        server.load_state(&escape),
        LoadState::Failed(AssetError::InvalidPath(_))
    ));
    assert!(app.resources.expect::<Assets<ImageData>>().is_empty());
}

struct TextLoader;
impl AssetLoader for TextLoader {
    type Asset = String;
    fn extensions(&self) -> &[&str] {
        &["txt", "text"]
    }
    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<String, AssetError> {
        String::from_utf8(bytes.to_vec()).map_err(|e| AssetError::decode(ctx.path(), e))
    }
}

#[test]
fn custom_loader_and_manual_reload() {
    let (mut app, mem) = mem_app();
    app.init_asset::<String>().register_asset_loader(TextLoader);
    assert!(app.resources.expect::<AssetServer>().has_loader::<String>("TXT"));
    mem.insert("greeting.txt", b"hello".to_vec());
    let mut reader = EventReader::default();
    let h: Handle<String> = app.resources.expect_mut::<AssetServer>().load("mem://greeting.txt");
    wait_idle(&mut app);
    assert_eq!(app.resources.expect::<Assets<String>>().get(&h).unwrap(), "hello");

    mem.insert("greeting.txt", b"bonjour".to_vec());
    assert_eq!(
        app.resources.expect_mut::<AssetServer>().reload("mem://greeting.txt"),
        1
    );
    // Stays `Loaded` (old data visible) while the reload is in flight.
    assert!(app.resources.expect::<AssetServer>().is_loaded(&h));
    let mut modified = false;
    run_until(&mut app, T, |app| {
        modified |= read_events::<String>(app, &mut reader)
            .iter()
            .any(|e| e.is_modified(&h));
        modified
    });
    assert_eq!(app.resources.expect::<Assets<String>>().get(&h).unwrap(), "bonjour");

    // A reload that fails keeps the previous data.
    mem.insert("greeting.txt", vec![0xff, 0xfe]);
    app.resources.expect_mut::<AssetServer>().reload("mem://greeting.txt");
    wait_idle(&mut app);
    assert_eq!(app.resources.expect::<Assets<String>>().get(&h).unwrap(), "bonjour");
    assert!(app.resources.expect::<AssetServer>().is_loaded(&h));
}

#[test]
fn dependency_change_reloads_owner() {
    let (mut app, mem) = mem_app();
    let fx = fixtures();
    mem.insert("cube.obj", std::fs::read(fx.join("cube.obj")).unwrap());
    mem.insert("cube.mtl", std::fs::read(fx.join("cube.mtl")).unwrap());
    mem.insert("checker.png", png([255, 0, 0, 255]));
    let mut reader = EventReader::default();
    let h: Handle<ModelData> = app.resources.expect_mut::<AssetServer>().load("mem://cube.obj");
    wait_idle(&mut app);
    let img = |app: &App| app.resources.expect::<Assets<ModelData>>().get(&h).unwrap().images[0].get(0, 0);
    assert_eq!(img(&app), [255, 0, 0, 255]);

    mem.insert("checker.png", png([0, 0, 255, 255]));
    assert_eq!(app.resources.expect_mut::<AssetServer>().reload("mem://checker.png"), 1);
    let mut modified = false;
    run_until(&mut app, T, |app| {
        modified |= read_events::<ModelData>(app, &mut reader)
            .iter()
            .any(|e| e.is_modified(&h));
        modified
    });
    assert_eq!(img(&app), [0, 0, 255, 255]);
}

#[test]
fn runtime_assets_emit_events() {
    let (mut app, _mem) = mem_app();
    let mut reader = EventReader::default();
    let h = app.resources.expect_mut::<Assets<MeshData>>().add(MeshData::cube(1.0));
    app.step_ticks(1);
    let ev = read_events::<MeshData>(&app, &mut reader);
    assert_eq!(ev, vec![AssetEvent::Created { id: h.id() }]);
    app.resources
        .expect_mut::<Assets<MeshData>>()
        .get_mut(&h)
        .unwrap()
        .indices
        .clear();
    app.step_ticks(1);
    assert_eq!(
        read_events::<MeshData>(&app, &mut reader),
        vec![AssetEvent::Modified { id: h.id() }]
    );
    let id = h.id();
    drop(h);
    app.step_ticks(1);
    assert_eq!(
        read_events::<MeshData>(&app, &mut reader),
        vec![AssetEvent::Removed { id }]
    );
}

#[test]
#[should_panic(expected = "not registered")]
fn loading_unregistered_type_panics() {
    let (mut app, _mem) = mem_app();
    let _: Handle<u64> = app.resources.expect_mut::<AssetServer>().load("mem://x.bin");
}
