# Getting started

## 1. Install

* Rust **1.98+** (`rustup update stable`).
* The CLI: `cargo install --path crates/space3d-cli` from a Space3d checkout (installs the
  `space3d` binary into `~/.cargo/bin`).
* Run `space3d doctor` — it lists what is installed for each platform and prints the exact
  command for anything missing (wasm target, `wasm-bindgen-cli`, Android NDK, Xcode, …).

## 2. Create a game

```sh
space3d new my_game                    # basic: a cube you move around
space3d new arena --template 3d        # physics: roll a ball, knock crates off the arena
space3d new party --template multiplayer   # WebSocket server + clients, server-authoritative
```

Names are lower case with single `_` or `-` separators (`my_game`, `my-game`); the Rust crate
is always the `_` form (`my_game::build`).

Options: `--space3d-path <checkout>` (use a local engine checkout instead of git),
`--git <url> [--branch b | --rev r]`, `--dir <path>`, `--targets desktop,web,android,ios`
(CI workflow), `--no-ci`, `--force`.

```text
my_game/
├── Cargo.toml            space3d dependency, profiles, workspace with mobile/
├── src/lib.rs            the game: `build(app)` + `GamePlugin`
├── src/main.rs           space3d::main!(my_game::build)
├── src/stories.rs        storybook
├── src/bin/stories.rs    storybook viewer / gallery
├── src/bin/editor.rs     editor with the game scene (`space3d editor`)
├── tests/input.rs        gameplay driven by simulated input
├── tests/screenshots.rs  golden screenshots (+ one per story)
├── assets/               textures, models, sounds
├── web/index.html        page for the web build
├── mobile/               cdylib/staticlib crate for Android and iOS
├── android/README.md     cargo-apk setup
├── ios/                  XcodeGen project.yml, main.m, README
└── .github/workflows/space3d.yml
```

## 3. Run it

```sh
space3d run                   # cargo run
space3d run --release
space3d run --frames 120      # quit after 120 frames (smoke test)
space3d run --headless        # no window (renders offscreen)
space3d serve                 # web build + http://localhost:8080 (add --watch to rebuild on save)
space3d editor                # the editor with your scene: hierarchy, inspector, gizmos, play/pause/step
```

## 4. The shape of a Space3d game

```rust
use space3d::prelude::*;

/// Everything: engine plugins + gameplay. Called by `space3d::main!`.
pub fn build(app: &mut App) {
    app.add_plugin(DefaultPlugins::new().title("My game"));
    app.add_plugin(GamePlugin);
}

/// Only gameplay — tests and stories add this to a headless app.
pub struct GamePlugin;

impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.add_system(Stage::Startup, setup);          // once
        app.add_system(Stage::FixedUpdate, simulate);   // 60 Hz, deterministic
        app.add_system(Stage::Update, animate);         // once per frame
    }
}
```

* **Systems** are `FnMut(&mut SystemCtx)`: `ctx.world` (hecs ECS) and `ctx.resources`
  (type-keyed singletons such as `Input`, `ActionState`, `RenderAssets`, `Physics`, `Time`).
* **Simulation** goes in `Stage::FixedUpdate` and uses `ctx.time().fixed_delta`, so it is
  identical for any frame rate — and tests can step it tick by tick.
* **Never block the frame**: loading, networking and database calls return `Task`s. Poll them, or
  let `AsyncQueue` / `Http::*_then` call you back on the main thread:

  ```rust
  ctx.resources.expect_mut::<AsyncQueue>().compute_then(
      || build_huge_mesh(),                      // runs on the compute pool
      |ctx, mesh| { let h = ctx.resources.expect_mut::<RenderAssets>().add_mesh(mesh); /* … */ },
  );
  ```
* **Input** via named actions: `ActionMap::bind("jump", Binding::Key(KeyCode::Space))`, then
  `ctx.resources.expect::<ActionState>().just_pressed("jump")`. Touch regions work as virtual
  buttons on phones. `KeyCode`s are physical key positions (WASD stays WASD-shaped on AZERTY);
  show players the key's label on their layout with `Input::key_cap` — see
  [Keyboard layouts](platforms.md#keyboard-layouts).

## 5. `DefaultPlugins` and `MinimalPlugins`

`DefaultPlugins` adds, depending on the facade's cargo features: logging, tasks, window +
renderer + input (`platform`), asset server (`assets`), audio, physics, scenes, HTTP/WebSocket
clients (`net`) and egui (`ui`, F3 debug overlay). The database is opt-in:
`.with_db(DbPlugin::new("save.db"))`. Builders: `.title()`, `.window(WindowPlugin { .. })`,
`.tick_rate(hz)`, `.without_audio()`, `.without_physics()`, `.without_net()`,
`.without_ui()`, `.with_debug_overlay()`, `DefaultPlugins::headless(w, h)`.

`MinimalPlugins` is tasks + auto-exit + optional logging: servers and tools.

Environment switches understood by both:

| variable | effect |
|---|---|
| `SPACE3D_EXIT_AFTER_FRAMES=N` | send `AppExit` after N frames |
| `SPACE3D_EXIT_AFTER_SECS=S` | … after S seconds |
| `SPACE3D_HEADLESS=1` / `=640x360` | no window; offscreen renderer (each axis clamped to 1–8192) |
| `RUST_LOG=debug` | log filter |

## 6. Cargo features of `space3d`

Default: `render`, `platform`, `assets`, `audio`, `physics`, `scene`, `net`, `db`, `ui`.
Optional: `editor`, `github`, `stories`, `test` (generated projects enable `stories` and
`editor`, plus `test` for their tests; `stories` holds the engine's UI stories, so it builds
egui even without `ui`). The engine crates of features you leave off stay out of your
`Cargo.lock` too. A dedicated server can use
`space3d = { …, default-features = false, features = ["net"] }` (see
`examples/multiplayer_chat`).

## 7. Updating Space3d

Space3d is consumed by git revision. After bumping the pinned `rev`, run `cargo update -p space3d`
(or `cargo update`). Space3d always tracks the latest stable release of its dependencies, so a
consumer lockfile pinned to an older version can fail to resolve, e.g. `space3d-net` needs
`hyper-util ^0.1.21` ("failed to select a version for `hyper-util`"): update that crate too —
`cargo update -p hyper-util` — or simply `cargo update`.

## Next

* [Testing](testing.md) · [Storybook](storybook.md) · [Platforms](platforms.md) ·
  [Networking](networking.md) · [Performance](performance.md)
