# <game name> — agent guide

A Space3d game (Rust, https://space3d.dev). Everything below is how to work in this repo.

## Commands
- `space3d run` / `space3d run --headless --frames 120` — run (use headless/frames for smoke tests; never leave a window open).
- `space3d test` — unit, input-simulation and golden-screenshot tests. Must pass before you say done.
- `space3d serve` — web build on http://localhost:8080.
- `space3d stories` — storybook of UI/effects. `space3d editor` — scene editor.
- `cargo fmt && cargo clippy --all-targets -- -D warnings`.

## Rules of the engine
1. Gameplay is `GamePlugin` in `src/lib.rs`; `build(app)` adds `DefaultPlugins` + `GamePlugin`.
2. Simulation goes in `Stage::FixedUpdate` (60 Hz, deterministic, use `ctx.time().fixed_delta`). Rendering/animation in `Stage::Update`.
3. Never block the frame: I/O via `space3d_tasks::spawn`, CPU work via `spawn_compute`; poll `Task`s in systems.
4. Tests drive time with `App::step_ticks(n)` and input with the `space3d-test` harness; never wall-clock.
5. Keep it wasm-clean: native-only code behind `#[cfg(not(target_arch = "wasm32"))]`.
6. Runs must not steal focus: set `SPACE3D_HEADLESS=1` or `SPACE3D_EXIT_AFTER_FRAMES=n`.

## Workflow
Change → `space3d test` → look at golden-screenshot diffs (`tests/__screenshots__/**/*.actual.png`) → only then update goldens deliberately.
Docs: https://space3d.dev/docs · full text for LLMs: https://space3d.dev/llms-full.txt
