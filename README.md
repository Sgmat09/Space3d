# Space3d

**A cross-platform 3D game engine in Rust — one codebase for desktop, the web, Android and iOS,
with testing built in from the first line.**

Space3d is a small, modular engine built on [wgpu](https://wgpu.rs) and
[winit](https://github.com/rust-windowing/winit): an ECS (hecs) with a deterministic fixed
tick, a GPU-driven renderer that draws a million instances in a handful of draw calls, async
assets/networking/database access that never blocks a frame, and a test harness where input
simulation and golden screenshots are ordinary `cargo test`s. The `space3d` CLI scaffolds a
game, builds it for every platform, serves the web build, runs the storybook and publishes
releases to GitHub.

```rust
use space3d::prelude::*;

struct Spin;

fn build(app: &mut App) {
    app.add_plugin(DefaultPlugins::new().title("Hello cube"));
    app.add_system(Stage::Startup, |ctx| {
        let cube = ctx.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
        ctx.world.spawn((Transform::default(), Mesh3d(cube), Material::srgb(0.9, 0.4, 0.2), Spin));
        ctx.world.spawn((Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.3), Vec3::Y), DirectionalLight::default()));
        ctx.world.spawn((Transform::from_xyz(0.0, 1.5, 4.0).looking_at(Vec3::ZERO, Vec3::Y), Camera3d::default()));
    });
    app.add_system(Stage::FixedUpdate, |ctx| {
        let dt = ctx.time().fixed_delta.as_secs_f32();
        for (t, _) in ctx.world.query_mut::<(&mut Transform, &Spin)>() {
            t.rotation *= Quat::from_rotation_y(dt);
        }
    });
}

// `fn main` on desktop, `#[wasm_bindgen(start)]` on the web,
// `android_main` on Android, `extern "C" start_app` on iOS.
space3d::main!(build);
```

## Features

* **Everything is a plugin.** `DefaultPlugins` / `MinimalPlugins` groups; every engine crate is
  optional behind a cargo feature of the `space3d` facade.
* **Deterministic simulation.** `Stage::FixedUpdate` runs at a fixed `TickRate` (60 Hz by
  default), decoupled from the frame rate; tests drive time with `App::step_ticks(n)`.
* **Renderer (wgpu 30).** Metallic/roughness lighting, directional + point lights, textures,
  render-to-texture, MSAA; automatic instancing of ECS entities and explicit
  `InstanceBuffer<T>`s for 1M+ instances; GPU frustum culling + (multi-)indirect draws with a CPU
  fallback (WebGL2); dirty-range uploads through a staging belt; headless rendering and capture.
* **Never block the frame.** I/O runs on `space3d_tasks` (tokio natively,
  `wasm-bindgen-futures` on the web); CPU work on a compute pool; systems only poll
  `Task`s or register `AsyncQueue` callbacks.
* **Assets.** Async asset server with typed handles and hot reload; PNG/JPEG/WebP/…, glTF/GLB and
  OBJ; texture editing (resize, filters, brushes, flood fill, generators, undo/redo) and mesh
  editing (normals, weld, subdivide, extrude, UV projection, decimation, OBJ/glTF export).
* **Networking.** HTTP client and WebSocket client on every platform; HTTP/WebSocket servers
  (axum) natively; typed channels for replication.
* **Persistence.** SQLite on a worker thread (migrations, KV store, save slots with
  thumbnails, leaderboards) with a `localStorage` fallback on the web.
* **Physics, audio, scenes.** rapier rigid bodies/colliders/raycasts on the fixed tick; kira
  audio with spatial emitters; RON/JSON scenes, prefabs and world snapshots.
* **UI and tools.** egui integration with a compact debug overlay (F3), profiler (F4) and
  console; an opt-in compact density (`app.insert_resource(UiTheme::compact())`, see
  [docs/ui.md](docs/ui.md)); scene inspector, texture and model editors; a storybook for
  isolated, parameterized scenes.
* **Testing.** Headless `TestApp`, tick-accurate keyboard/mouse/touch/gamepad simulation and
  recorded input scripts, golden screenshots with diff images and an HTML report, determinism
  and performance assertions. GPU tests skip when there is no adapter.
* **Shipping.** `space3d` CLI for desktop, web (wasm-bindgen), Android (cargo-apk) and iOS
  (Xcode via XcodeGen) builds, a local web server with cross-origin isolation, GitHub Actions
  generation and GitHub releases; in-game update checker and opt-in crash reporter.

## Architecture

```text
                        ┌──────────────────────────── your game ─────────────────────────────┐
                        │  build(app) · GamePlugin · systems · stories · tests                │
                        └───────────────┬──────────────────────────────────────┬─────────────┘
                                        │ space3d::prelude / space3d::main!     │ space3d::test
┌─────────────── space3d (facade: features, DefaultPlugins, MinimalPlugins, entry points) ─────┐
│                                                                                               │
│  platform ── winit runner (desktop · web canvas · Android NativeActivity · iOS UIKit), Input  │
│     │                                                                                         │
│  render ──── wgpu: mesh arena · materials · lights · instancing · GPU culling · capture       │
│  ui ──────── egui overlay · debug overlay · profiler · console        editor ── inspectors    │
│  assets ──── asset server · loaders · hot reload · texture/mesh editing                       │
│  physics (rapier) · audio (kira) · scene (RON) · net (HTTP/WS) · db (SQLite/localStorage)     │
│  stories ─── storybook, gallery      test ── TestApp, input sim, goldens      github ── CI    │
│                                                                                               │
│  tasks ───── spawn / spawn_compute / Task::poll / AsyncQueue   (tokio · rayon · wasm futures) │
│  core ────── App · Plugin · ECS (hecs) · Resources · Events · Stages · TickRate · Transform   │
└───────────────────────────────────────────────────────────────────────────────────────────────┘

Frame:  Startup (once) → PreUpdate (task completions, input, network inbox)
        → FixedUpdate × N (deterministic simulation, physics) → Update → PostUpdate (transforms)
        → Render → Last (event buffers, cleanup)
```

| crate | role |
|---|---|
| [`space3d`](crates/space3d) | facade: feature-gated re-exports, plugin groups, prelude, `main!` |
| [`space3d-core`](crates/space3d-core) | App, Plugin, ECS, resources, events, stages, fixed tick, Transform |
| [`space3d-tasks`](crates/space3d-tasks) | async runtime bridge, `Task`, `AsyncQueue` |
| [`space3d-render`](crates/space3d-render) | wgpu renderer ([architecture](crates/space3d-render/ARCHITECTURE.md)) |
| [`space3d-platform`](crates/space3d-platform) | winit runner for all platforms, input, action maps |
| [`space3d-assets`](crates/space3d-assets) | asset server, loaders, hot reload, editing ops |
| [`space3d-net`](crates/space3d-net) | HTTP/WebSocket clients and servers |
| [`space3d-db`](crates/space3d-db) | SQLite / localStorage stores |
| [`space3d-physics`](crates/space3d-physics) | rapier integration |
| [`space3d-audio`](crates/space3d-audio) | kira integration |
| [`space3d-scene`](crates/space3d-scene) | scenes, prefabs, snapshots |
| [`space3d-ui`](crates/space3d-ui) | egui integration, debug tools |
| [`space3d-editor`](crates/space3d-editor) | scene inspector, texture and model editors |
| [`space3d-stories`](crates/space3d-stories) | storybook |
| [`space3d-test`](crates/space3d-test) | test harness |
| [`space3d-github`](crates/space3d-github) | GitHub REST, update checker, crash reporter, CI templates |
| [`space3d-cli`](crates/space3d-cli) | the `space3d` command |

## Quickstart

```sh
# install the CLI from this repository
cargo install --path crates/space3d-cli

space3d new my_game --template 3d        # basic | 3d | multiplayer
cd my_game
space3d run                              # desktop
space3d serve                            # web: builds wasm, serves http://localhost:8080
space3d test                             # input-simulation + screenshot tests
space3d stories                          # storybook viewer
space3d editor                           # editor with the game scene loaded
space3d doctor                           # what is missing for web / Android / iOS, and how to fix it
```

`space3d new` depends on Space3d via git by default; `--space3d-path ../Space3d` uses a local
checkout. The generated project contains the game library (`build` + `GamePlugin`), the
desktop/web entry point, a storybook binary, tests, `web/index.html`, a `mobile/` crate for
Android and iOS, and a GitHub Actions workflow. See [docs/getting-started.md](docs/getting-started.md).

Other commands: `space3d build --target desktop|web|android|ios [--release]`,
`space3d publish --tag v1.0.0 [--dry-run]`, `space3d ci init --targets desktop,web,android,ios`,
`space3d editor`, `space3d bench`. Every command takes `--dry-run` to print what it would run.

## Platform support

| | Windows | macOS | Linux | Web | Android | iOS |
|---|---|---|---|---|---|---|
| graphics backend | DX12 / Vulkan | Metal | Vulkan / GLES | WebGPU / WebGL2 | Vulkan / GLES | Metal |
| typical draw path | GPU multi-draw-count | GPU multi-draw | GPU multi-draw-count | GPU indirect / CPU | GPU / CPU | GPU multi-draw |
| entry point (`main!`) | `fn main` | `fn main` | `fn main` | `#[wasm_bindgen(start)]` | `android_main` | `start_app()` |
| build | `space3d build` | `space3d build` | `space3d build` | `space3d build -t web` | `space3d build -t android` | `space3d build -t ios` |
| input | keyboard, mouse | keyboard, mouse | keyboard, mouse | keyboard, mouse, touch | touch | touch |
| audio | ✓ | ✓ | ✓ | ✓ (after a user gesture) | ✓ | ✓ |
| database | SQLite | SQLite | SQLite | localStorage (KV/saves) | SQLite | SQLite |
| HTTP / WebSocket client | ✓ | ✓ | ✓ | ✓ (fetch / WebSocket) | ✓ | ✓ |
| HTTP / WebSocket server | ✓ | ✓ | ✓ | — | ✓ | ✓ |
| hot reload | ✓ | ✓ | ✓ | — | — | — |

Details and per-platform setup: [docs/platforms.md](docs/platforms.md).

## Testing and screenshots

Games test themselves with the same harness the engine uses:

```rust
use space3d::prelude::*;
use space3d::test::prelude::*;

#[test]
fn holding_right_moves_the_player() {
    let mut app = TestApp::headless(320, 240);   // no window; renderer only if a GPU exists
    app.add_plugin(my_game::GamePlugin);
    app.hold_key(KeyCode::ArrowRight, 30);        // exactly 30 fixed ticks
    assert!(app.translation(app.find_by_name("player").unwrap()).x > 1.0);
    assert_screenshot!(&mut app, "moved_right");  // golden image in tests/__screenshots__/
}
```

* The first run writes the golden PNG; later runs compare (tolerances, SSIM, per-backend
  variants) and write `*.actual.png` / `*.diff.png` on failure.
* `space3d test` sets `SPACE3D_TEST_REPORT` and prints a summary with links into an HTML report
  (expected / actual / diff side by side); `space3d test --update-snapshots` re-blesses.
* Without a GPU adapter, screenshot tests skip instead of failing (`SPACE3D_REQUIRE_GPU=1`
  makes them fail); CI uses a software Vulkan driver (lavapipe) so they run there too.
* Determinism (`assert_deterministic`), frame budgets and allocation counts are assertions too.

More: [docs/testing.md](docs/testing.md).

## Storybook

A *story* is an isolated, parameterized scene — a prop, a material, a level chunk, a UI panel —
with knobs (floats, colors, choices, …). The same story is browsed interactively, rendered into a
static gallery, and checked as a golden screenshot test:

```sh
space3d stories                                   # viewer
space3d stories --headless --out target/stories   # PNGs + index.html gallery
```

```rust
space3d::stories::storybook_tests!(my_game::stories::storybook); // one golden per story
```

More: [docs/storybook.md](docs/storybook.md).

## Performance notes

Numbers from the renderer design ([ARCHITECTURE.md](crates/space3d-render/ARCHITECTURE.md)) and
the [`instancing_million`](examples/instancing_million) example:

* **1,000,000 cubes, one draw.** An explicit `InstanceBuffer<InstancePacked>` stores each instance
  in **32 bytes** (position, uniform scale, snorm16 quaternion, unorm8 color, layer) — 2.5×
  smaller than matrix + color — so a million instances are ~31 MiB of vertex data, culled on
  the GPU by a compute pass (64 instances per workgroup, compacted in-workgroup) and drawn with
  (multi-)indirect draws.
* **Upload only what changed.** User buffers track dirty ranges (merged within 16 instances,
  at most 64 ranges; >50 % dirty → one full copy). The example rewrites a 16,384-instance band
  per frame and uploads **512 KiB** instead of 31 MiB; `--static` uploads **0 bytes** after the
  first frame.
* **Measured** (`cargo run -p instancing_million --release`, Apple M4 Max, Metal,
  `GpuMultiDraw`): 1,000,000 cubes = 12 M triangles in **1 indirect draw call**, holding the
  display's 120 Hz vsync while uploading 512 KiB per frame (0 KiB with `--static`).
* **Static scenes cost nothing.** Renderer-owned buffers keep a CPU shadow and upload only
  differing byte ranges: an unchanged scene with a still camera uploads **0 bytes** per frame;
  moving one ECS entity uploads one **64-byte** matrix.
* **No per-frame staging allocations.** All uploads go through a 4 MiB-chunk staging belt
  recycled after the GPU is done; copies ≥ 1 MiB are split across rayon workers.
* **Automatic batching.** Entities with equal mesh + material are sorted into one instanced draw,
  so 1,000 identical-looking entities are one draw call even with separate `Material` values.
* **One mesh arena.** All meshes share one vertex and one index buffer (36-byte vertices), so a
  GPU multi-draw covers many meshes without rebinding.

`RenderStats` (draw calls, instances, triangles, bytes uploaded, draw path, …) is a resource, and
the debug overlay (F3) shows it live. More: [docs/performance.md](docs/performance.md).

## Examples

| example | shows |
|---|---|
| [`hello_cube`](examples/hello_cube) | window, lit rotating cube |
| [`instancing_million`](examples/instancing_million) | 1M instances, dirty-range uploads, stats overlay |
| [`physics_playground`](examples/physics_playground) | rapier bodies, click/space to drop, raycasts |
| [`multiplayer_chat`](examples/multiplayer_chat) | WebSocket server + clients in one binary |
| [`save_game`](examples/save_game) | SQLite save slots + key/value settings |
| [`async_loading`](examples/async_loading) | assets over HTTP + compute-pool meshes while the frame keeps ticking |
| [`web_demo`](examples/web_demo) | a browser-first scene built with `space3d::main!` |

```sh
cargo run -p hello_cube
cargo run -p instancing_million --release
SPACE3D_EXIT_AFTER_FRAMES=120 cargo run -p physics_playground   # smoke test: quit after 120 frames
SPACE3D_HEADLESS=1 SPACE3D_EXIT_AFTER_FRAMES=60 cargo run -p hello_cube   # no window at all
```

## Dependency policy: latest stable only

Every dependency is pinned to its newest **stable** release (no alpha/beta/rc) in
`[workspace.dependencies]` of the root `Cargo.toml`; crates use `foo.workspace = true`. Before
adding a crate, check `cargo search --limit 1 <crate>`. (Example: winit stays on 0.30.x because
0.31 is still a beta.)

## Contributing

Read [AGENTS.md](AGENTS.md). In short: everything is a `Plugin`; never block the frame; the
simulation lives in `FixedUpdate`; runtime crates must build for `wasm32-unknown-unknown`; GPU
tests skip without an adapter; and

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

Proprietary and confidential — all rights reserved. See [LICENSE](LICENSE). Space3d is closed
source: it is not published to any package registry and its source is not distributed.
