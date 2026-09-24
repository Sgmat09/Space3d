//! Streaming assets over HTTP while the frame loop keeps running.
//!
//! The example starts a tiny [`HttpServer`] in-process that serves generated PNG textures and
//! an OBJ model with **artificial latency** (0.2 – 1.6 s per file). A custom
//! [`AssetSource`] (`NetSource`) fetches them through [`Http::fetch`]; the [`AssetServer`]
//! decodes them on the compute pool and inserts them during `PreUpdate`. A heavy procedural
//! mesh is built with [`AsyncQueue::compute_then`] at the same time.
//!
//! Meanwhile a ring of cubes keeps spinning and a watchdog records the longest frame. When
//! everything has arrived it prints something like:
//!
//! ```text
//! all 13 assets ready after 1.62 s: 188 frames rendered meanwhile, longest frame 9.4 ms
//! ```
//!
//! ```text
//! cargo run -p async_loading
//! SPACE3D_EXIT_AFTER_SECS=4 cargo run -p async_loading     # smoke test
//! cargo run -p async_loading -- --exit-when-done
//! ```
//!
//! Native only (the web build of this pattern simply uses the default `HttpSource`, which
//! already reads via `fetch`).

use space3d::assets::{AssetError, AssetPath, AssetSource, BoxedFuture};
use space3d::net::server::axum::{
    extract::Path,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use space3d::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const TEXTURES: usize = 12;

/// Reads asset bytes over HTTP with the engine's [`Http`] client (native: reqwest on the
/// background runtime). Registered as the `net://` source.
struct NetSource {
    http: Http,
    base: String,
}

impl AssetSource for NetSource {
    fn read(&self, path: &AssetPath) -> BoxedFuture<Result<Vec<u8>, AssetError>> {
        let url = format!("{}/{}", self.base.trim_end_matches('/'), path.path());
        let request = self.http.fetch(HttpRequest::get(&url));
        Box::pin(async move {
            match request.await {
                Ok(response) if response.is_success() => Ok(response.body),
                Ok(response) if response.status == 404 => Err(AssetError::NotFound(url)),
                Ok(response) => Err(AssetError::Other(format!("GET {url}: HTTP {}", response.status))),
                Err(e) => Err(AssetError::Other(format!("GET {url}: {e}"))),
            }
        })
    }
}

/// What the server lists at `/manifest.json`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct Manifest {
    textures: Vec<String>,
    model: String,
}

/// Generate the files the demo server hands out.
fn generate_files() -> HashMap<String, Vec<u8>> {
    use space3d::assets::edit::{mesh, texture};
    let mut files = HashMap::new();
    for i in 0..TEXTURES {
        let hue = i as f32 / TEXTURES as f32 * 360.0;
        let mut img = match i % 3 {
            0 => texture::checker(256, 256, 32, [240, 240, 240, 255], [60, 60, 60, 255]),
            1 => texture::value_noise(256, 256, i as u64, 24.0, 4),
            _ => texture::radial_gradient(256, 256, [255, 255, 255, 255], [40, 40, 40, 255]),
        };
        texture::tint(&mut img, hue_to_rgba(hue), 0.6);
        files.insert(format!("tex{i:02}.png"), texture::encode_png(&img).expect("png"));
    }
    let mut knot = MeshData::sphere(1.0, 48, 32);
    mesh::scale(&mut knot, Vec3::new(1.0, 0.6, 1.0));
    files.insert("model.obj".into(), mesh::to_obj(&knot).into_bytes());
    files
}

fn hue_to_rgba(hue: f32) -> [u8; 4] {
    let h = hue.to_radians();
    let c = |o: f32| ((0.5 + 0.5 * (h + o).cos()) * 255.0) as u8;
    [c(0.0), c(2.094), c(4.188), 255]
}

/// Deterministic latency per file: 200 ms … 1.6 s.
fn latency(name: &str) -> Duration {
    let h = name
        .bytes()
        .fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
    Duration::from_millis(200 + h % 1400)
}

/// Start the slow file server; returns its handle (dropping it stops the server).
fn start_server() -> ServerHandle {
    serve(generate_files(), &[])
}

/// Serve `files` (with latency); `missing` names are listed in the manifest but answer 404.
fn serve(files: HashMap<String, Vec<u8>>, missing: &[&str]) -> ServerHandle {
    let files = Arc::new(files);
    let mut textures: Vec<String> = files.keys().filter(|k| k.ends_with(".png")).cloned().collect();
    textures.extend(missing.iter().map(|m| m.to_string()));
    textures.sort();
    let manifest = Manifest {
        textures,
        model: "model.obj".into(),
    };
    HttpServer::new()
        .get_json("/manifest.json", move || Manifest {
            textures: manifest.textures.clone(),
            model: manifest.model.clone(),
        })
        .route(
            "/files/{name}",
            get(move |Path(name): Path<String>| {
                let files = files.clone();
                async move {
                    tokio::time::sleep(latency(&name)).await;
                    match files.get(&name) {
                        Some(bytes) => {
                            let kind = if name.ends_with(".png") {
                                "image/png"
                            } else {
                                "text/plain"
                            };
                            ([(header::CONTENT_TYPE, kind)], bytes.clone()).into_response()
                        }
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        )
        .start()
        .expect("start demo server")
}

/// A cube waiting for its texture.
struct Slot {
    entity: Entity,
    handle: Handle<ImageData>,
    /// Finished: loaded, or failed (then the placeholder stays).
    done: bool,
}

/// Loading progress + frame-time watchdog.
struct Loading {
    _server: ServerHandle,
    server_url: String,
    slots: Vec<Slot>,
    model: Option<Handle<MeshData>>,
    model_done: bool,
    procedural_done: bool,
    /// Loads that failed (each is logged once and counts as finished).
    failed: usize,
    /// The manifest request failed: nothing else will arrive.
    manifest_failed: bool,
    started: Option<f64>,
    frames_while_loading: u64,
    longest_frame: Duration,
    finished: bool,
    exit_when_done: bool,
}

impl Loading {
    fn total(&self) -> usize {
        self.slots.len() + 2
    }
    fn ready(&self) -> usize {
        self.slots.iter().filter(|s| s.done).count() + self.model_done as usize + self.procedural_done as usize
    }
}

/// Spins the ring.
struct Spin(f32);

fn build(app: &mut App) {
    let exit_when_done = std::env::args().any(|a| a == "--exit-when-done");
    app.add_plugin(
        DefaultPlugins::new()
            .title("Space3d — async loading")
            .without_physics()
            .without_audio(),
    );
    add_game(app, start_server(), exit_when_done);
}

/// The demo on top of an app with the engine plugins (the tests use a headless app).
fn add_game(app: &mut App, server: ServerHandle, exit_when_done: bool) {
    let server_url = server.url();
    println!("demo asset server on {server_url} (every file is delayed on purpose)");
    let http = app.resources.expect::<Http>().clone();
    app.resources.expect_mut::<AssetServer>().add_source(
        "net",
        NetSource {
            http,
            base: format!("{server_url}/files"),
        },
    );
    app.insert_resource(Loading {
        _server: server,
        server_url,
        slots: Vec::new(),
        model: None,
        model_done: false,
        procedural_done: false,
        failed: 0,
        manifest_failed: false,
        started: None,
        frames_while_loading: 0,
        longest_frame: Duration::ZERO,
        finished: false,
        exit_when_done,
    });
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::Startup, request_manifest);
    app.add_system(Stage::Update, spin);
    app.add_system(Stage::Update, apply_loaded);
    app.add_system(Stage::Last, watchdog);
}

fn setup(ctx: &mut SystemCtx) {
    let cube = ctx.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.9));
    let mut entities = Vec::new();
    for i in 0..TEXTURES {
        let angle = i as f32 / TEXTURES as f32 * std::f32::consts::TAU;
        entities.push(ctx.world.spawn((
            Transform::from_xyz(angle.cos() * 4.0, 0.0, angle.sin() * 4.0),
            Mesh3d(cube),
            // Grey placeholder until the texture arrives.
            Material::srgb(0.35, 0.35, 0.38),
            Spin(0.6 + i as f32 * 0.07),
        )));
    }
    ctx.resources.insert(entities);
    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.3), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.5,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.7, 0.8, 1.0),
        intensity: 0.2,
    });
    ctx.world.spawn((
        Transform::from_xyz(0.0, 5.0, 9.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.03, 0.03, 0.05, 1.0)),
    ));

    // CPU-heavy work on the compute pool: a finely subdivided, displaced sphere.
    ctx.resources.expect_mut::<AsyncQueue>().compute_then(
        || {
            let mut mesh = MeshData::sphere(1.0, 32, 24);
            edit::mesh::subdivide(&mut mesh);
            edit::mesh::subdivide(&mut mesh);
            for p in &mut mesh.positions {
                let v = Vec3::from_array(*p);
                let bump = 1.0 + 0.12 * (v.x * 9.0).sin() * (v.y * 7.0).cos() * (v.z * 8.0).sin();
                *p = (v * bump).to_array();
            }
            edit::mesh::recalculate_smooth_normals(&mut mesh);
            mesh
        },
        |ctx, mesh: MeshData| {
            let triangles = mesh.triangle_count();
            let handle = ctx.resources.expect_mut::<RenderAssets>().add_mesh(mesh);
            ctx.world.spawn((
                Transform::from_xyz(0.0, 2.2, 0.0).with_scale(Vec3::splat(0.8)),
                Mesh3d(handle),
                Material::srgb(0.9, 0.5, 0.3).with_metallic_roughness(0.6, 0.3),
                Spin(0.3),
            ));
            ctx.resources.expect_mut::<Loading>().procedural_done = true;
            println!("procedural mesh ready ({triangles} triangles, built on the compute pool)");
        },
    );
}

/// Ask the server what to load, then start every load at once.
fn request_manifest(ctx: &mut SystemCtx) {
    let url = format!("{}/manifest.json", ctx.resources.expect::<Loading>().server_url);
    ctx.resources.expect_mut::<Loading>().started = Some(ctx.time().elapsed_secs());
    ctx.resources
        .expect::<Http>()
        .get_json_then(url, |ctx, manifest: Result<Manifest, NetError>| {
            let manifest = match manifest {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("manifest request failed: {e}");
                    ctx.resources.expect_mut::<Loading>().manifest_failed = true;
                    return;
                }
            };
            println!("manifest: {} textures + {}", manifest.textures.len(), manifest.model);
            let entities = ctx.resources.expect::<Vec<Entity>>().clone();
            let server = ctx.resources.expect_mut::<AssetServer>();
            let slots: Vec<Slot> = manifest
                .textures
                .iter()
                .zip(entities)
                .map(|(name, entity)| Slot {
                    entity,
                    handle: server.load(format!("net://{name}")),
                    done: false,
                })
                .collect();
            let model = server.load(format!("net://{}", manifest.model));
            let loading = ctx.resources.expect_mut::<Loading>();
            loading.slots = slots;
            loading.model = Some(model);
        });
}

/// Upload whatever finished since last frame. A failed load is logged once and counts as
/// finished (its cube keeps the placeholder material).
fn apply_loaded(ctx: &mut SystemCtx) {
    let mut textures = Vec::new();
    let mut failed_slots = Vec::new();
    let mut model = None;
    let mut model_failed = false;
    {
        let loading = ctx.resources.expect::<Loading>();
        let server = ctx.resources.expect::<AssetServer>();
        let images = ctx.resources.expect::<Assets<ImageData>>();
        for (i, slot) in loading.slots.iter().enumerate().filter(|(_, s)| !s.done) {
            match server.load_state(&slot.handle) {
                LoadState::Loaded => {
                    if let Some(img) = images.get(&slot.handle) {
                        textures.push((i, slot.entity, img.clone()));
                    }
                }
                LoadState::Failed(e) => {
                    eprintln!("texture {i} failed: {e}");
                    failed_slots.push(i);
                }
                _ => {}
            }
        }
        if let Some(handle) = loading.model.as_ref().filter(|_| !loading.model_done) {
            match server.load_state(handle) {
                LoadState::Loaded => model = ctx.resources.expect::<Assets<MeshData>>().get(handle).cloned(),
                LoadState::Failed(e) => {
                    eprintln!("model failed: {e}");
                    model_failed = true;
                }
                _ => {}
            }
        }
    }
    {
        let loading = ctx.resources.expect_mut::<Loading>();
        for i in failed_slots {
            loading.slots[i].done = true;
            loading.failed += 1;
        }
        if model_failed {
            loading.model_done = true;
            loading.failed += 1;
        }
    }
    for (i, entity, image) in textures {
        let texture = ctx.resources.expect_mut::<RenderAssets>().add_image(image);
        if let Ok(mut material) = ctx.world.get::<&mut Material>(entity) {
            *material = Material::default().with_texture(texture);
        }
        ctx.resources.expect_mut::<Loading>().slots[i].done = true;
    }
    if let Some(mesh) = model {
        let handle = ctx.resources.expect_mut::<RenderAssets>().add_mesh(mesh);
        ctx.world.spawn((
            Transform::from_xyz(0.0, -0.4, 0.0),
            Mesh3d(handle),
            Material::srgb(0.3, 0.8, 0.5).with_metallic_roughness(0.2, 0.5),
            Spin(-0.4),
        ));
        ctx.resources.expect_mut::<Loading>().model_done = true;
        println!("model.obj ready");
    }
}

fn spin(ctx: &mut SystemCtx) {
    let dt = ctx.time().delta_secs();
    for (transform, spin) in ctx.world.query_mut::<(&mut Transform, &Spin)>() {
        transform.rotation = (Quat::from_rotation_y(spin.0 * dt) * transform.rotation).normalize();
    }
}

/// Track the longest frame while anything is still loading.
fn watchdog(ctx: &mut SystemCtx) {
    let (dt, now, frame) = {
        let t = ctx.time();
        (t.delta, t.elapsed_secs(), t.frame)
    };
    let loading = ctx.resources.expect_mut::<Loading>();
    if loading.finished || loading.started.is_none() {
        return;
    }
    // Skip the first frames (window creation, shader pipeline compilation).
    if frame > 10 {
        loading.frames_while_loading += 1;
        loading.longest_frame = loading.longest_frame.max(dt);
    }
    let (ready, total) = (loading.ready(), loading.total());
    if loading.manifest_failed {
        loading.finished = true;
        eprintln!("nothing to load: the manifest request failed");
        if loading.exit_when_done {
            ctx.send(AppExit);
        }
    } else if !loading.slots.is_empty() && ready == total {
        loading.finished = true;
        let elapsed = now - loading.started.unwrap_or(0.0);
        let outcome = match loading.failed {
            0 => "ready".to_owned(),
            n => format!("finished ({n} failed)"),
        };
        println!(
            "all {total} assets {outcome} after {elapsed:.2} s: {} frames rendered meanwhile, longest frame {:.1} ms",
            loading.frames_while_loading,
            loading.longest_frame.as_secs_f64() * 1000.0
        );
        if loading.exit_when_done {
            ctx.send(AppExit);
        }
    }
    let text = format!("Space3d — async loading | {ready}/{total} assets");
    if let Some(window) = ctx.resources.get_mut::<WindowState>() {
        window.title = text;
    }
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_is_bounded_and_stable() {
        for name in ["tex00.png", "tex11.png", "model.obj"] {
            let l = latency(name);
            assert!(l >= Duration::from_millis(200) && l < Duration::from_millis(1600));
            assert_eq!(l, latency(name));
        }
    }

    #[test]
    fn a_failed_load_is_reported_once_and_finishes_the_run() {
        let mut files = generate_files();
        files.retain(|name, _| name == "tex00.png" || name == "model.obj");
        let mut app = App::new();
        app.add_plugin(DefaultPlugins::headless(32, 32).without_physics().without_audio());
        add_game(&mut app, serve(files, &["tex99.png"]), true);
        let mut ticks = 0;
        while !app.should_exit() {
            app.step_ticks(1);
            ticks += 1;
            assert!(ticks < 20_000, "never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
        let loading = app.resources.expect::<Loading>();
        assert!(loading.finished);
        assert_eq!(loading.slots.len(), 2);
        assert_eq!(loading.failed, 1, "counted once, not once per frame");
        assert_eq!((loading.ready(), loading.total()), (4, 4));
    }

    #[test]
    fn loads_over_http_without_blocking_ticks() {
        let mut app = App::new();
        app.add_plugin(MinimalPlugins::new());
        app.add_plugin(NetPlugin::default());
        app.add_plugin(AssetsPlugin {
            hot_reload: false,
            ..Default::default()
        });
        let server = start_server();
        let http = app.resources.expect::<Http>().clone();
        app.resources.expect_mut::<AssetServer>().add_source(
            "net",
            NetSource {
                http,
                base: format!("{}/files", server.url()),
            },
        );
        let handle: Handle<ImageData> = app.resources.expect_mut::<AssetServer>().load("net://tex01.png");
        let missing: Handle<ImageData> = app.resources.expect_mut::<AssetServer>().load("net://nope.png");
        let mut ticks = 0;
        while !(app.resources.expect::<AssetServer>().is_loaded(&handle)
            && app.resources.expect::<AssetServer>().load_state(&missing).is_failed())
        {
            app.step_ticks(1);
            ticks += 1;
            assert!(ticks < 20_000, "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        // The load took ≥ 200 ms of latency, during which the app kept ticking.
        assert!(ticks > 50, "only {ticks} ticks while loading");
        let img = app.resources.expect::<Assets<ImageData>>().get(&handle).unwrap();
        assert_eq!((img.width, img.height), (256, 256));
    }
}
