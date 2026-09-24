# Storybook

A **story** is an isolated, parameterized setup — a prop, a material, a camera rig, a level
chunk, a UI panel — that you can look at without playing the game to get there. Stories come
from `space3d-stories` (facade feature `stories`) and are used three ways:

1. **Interactively** — `space3d stories` opens the storybook viewer from `space3d-editor`
   (story tree with search, live viewport, knob widgets, tick controls, screenshot capture).
2. **As a gallery** — `space3d stories --headless --out target/stories` renders every story to
   PNG plus `index.html` and `manifest.json` (knobs, backend, draw calls, timings).
3. **As tests** — every story is a golden screenshot test.

## Writing stories

```rust
use space3d::prelude::*;

pub fn storybook() -> Storybook {
    let mut book = Storybook::new();
    book.add(
        Story::new("Props/Crate stack", |app, knobs| {
            let height = knobs.i64("height");
            let tint = knobs.color("tint");
            app.add_plugin(PhysicsPlugin::default());
            app.add_system(Stage::Startup, move |ctx| { /* spawn crates, light, camera */ });
        })
        .description("A physics-stacked tower of crates")
        .knob_i64("height", 4, 1..=8)
        .knob_color("tint", [0.8, 0.55, 0.3, 1.0])
        .ticks(30)            // simulate 30 fixed ticks before capturing
        .size(256, 256),      // capture size
    );
    book
}
```

* The id `"Group/Sub/Name"` organizes the viewer and gallery.
* A story's setup only **adds content** (entities, resources, game plugins). The host adds
  window/render/input plugins — so the same story runs in the viewer, headless and in tests.
* Knobs: `knob_bool`, `knob_f32`, `knob_i64`, `knob_color` (linear RGBA), `knob_text`,
  `knob_choice`; read them with `knobs.f32("name")`, … Defaults are filled in for missing
  values; `Knob::clamp` sanitizes UI input.
* Tags: `.tag("slow")`; the `no-screenshot` tag excludes a story from galleries and golden
  tests (e.g. for stories that only make sense interactively).
* `.tolerance(0.02)` loosens the screenshot comparison for one story.

The engine ships its own book (`space3d::stories::engine_storybook()`: lighting, materials,
instancing, textures, cameras, hierarchies, physics) — `book.extend(engine_storybook())` to
browse both.

## The storybook binary

Projects created by `space3d new` have `src/bin/stories.rs`; `space3d stories [args]` runs it as
`cargo run --bin stories -- [args]`:

| command | does |
|---|---|
| `space3d stories` | opens the viewer (`space3d::editor::run_storybook_with`) |
| `space3d stories --filter crate` | opens the viewer on the first story whose id or tags contain "crate" |
| `space3d stories --list` | prints ids and descriptions |
| `space3d stories --headless --out shots` | renders the gallery into `shots/` (exit code 1 if a story failed) |

The headless gallery uses `space3d::stories::screenshot_all(&book, out_dir, filter)`; each story
runs in a fresh `TestApp` with the story's size and tick count, inside `catch_unwind`, so one
broken story does not stop the rest. Without a GPU, stories are reported as skipped.

## Stories as golden tests

```rust
// tests/screenshots.rs
space3d::stories::storybook_tests!(my_game::stories::storybook);
```

This generates one `#[test]` that renders every story (except `no-screenshot`) and compares it
against `tests/__screenshots__/<Group>/<Name>.png` using the usual golden workflow (first run
writes, `space3d test --update-snapshots` re-blesses, diffs + HTML report on failure). For more
control use `check_storybook` / `assert_storybook_in` with `StorybookTestOptions`.
