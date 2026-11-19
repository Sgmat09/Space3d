---
name: space3d-game
description: Create, run, test and ship a Space3d (Rust cross-platform 3D engine) game. Use when the user wants a game, a 3D scene, or works in a project with a `space3d` dependency or the `space3d` CLI.
---

# Building games with Space3d

Docs: https://space3d.dev/docs — full text: https://space3d.dev/llms-full.txt

## Setup (once)
1. `space3d --version` — if missing: `cargo install --git https://github.com/SpaceCorps/Space3d space3d-cli`, then `space3d doctor` and run the fixes it prints.
2. New game: `space3d new <name> [--template basic|3d|multiplayer]` (names: lower_snake or kebab). Copy the AGENTS template into the project: `AGENTS.md` from this plugin's `AGENTS.template.md`.

## Loop (every change)
1. Edit `src/lib.rs` (`GamePlugin`): systems are `FnMut(&mut SystemCtx)` on a `Stage` — `Startup`, `FixedUpdate` (deterministic 60 Hz), `Update`.
2. Add or extend a test in `tests/` (input simulation with `App::step_ticks(n)`; golden screenshots).
3. `space3d test`, then `space3d run --headless --frames 120` for a smoke run. Never open a window that steals focus.
4. Look at screenshot diffs before updating goldens.
5. Web check: `space3d serve`.

## Hard rules
- Never `.block()` in engine code; I/O through `space3d_tasks`.
- Everything is a `Plugin`; keep code wasm-clean.
- Do not claim done until `space3d test` output is green — report it.

## Ship
`space3d build --target web|desktop --release`; `space3d publish --tag vX.Y.Z` makes a GitHub release (ask the user before publishing).
