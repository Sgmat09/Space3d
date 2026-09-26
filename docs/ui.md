# UI

`space3d-ui` runs egui inside the engine: `UiPlugin` translates input in `Stage::PreUpdate`, UI
systems (`app.add_ui_system`, `app.add_root_ui_system`) run in `Stage::Update`, and the frame is
tessellated in `Stage::PostUpdate` and drawn by the renderer's overlay hook (so it shows up in
headless screenshots). `UiPlugin::deterministic()` gives reproducible pixels for golden tests.
Pointer, focus and egui plugin details: [platforms.md, "Mouse buttons, focus and the UI"](platforms.md#mouse-buttons-focus-and-the-ui).

## Theming: the compact density

egui's default sizes are generous for tool panels and HUDs. The engine ships a **compact
preset** — dense editor panels in the spirit of Blender or Unity that stay readable at 1 pixel
per point — and it is strictly **opt-in**:

```rust
app.insert_resource(space3d_ui::UiTheme::compact()); // space3d::ui::UiTheme with the facade
```

* **Without the resource nothing changes.** The engine never writes egui's `Style` unless a
  `UiTheme` resource exists: an app that does not opt in renders exactly as before, pixel for
  pixel.
* **Applied once, between passes.** `UiPlugin` applies the theme when the resource is inserted or
  changed, right before the next egui pass begins, to both the dark and the light style. In every
  other frame it costs one comparison and no allocation (`UiThemeSync`); there is no per-frame
  style churn.
* **Sizes only.** The theme writes the tokens below. Colors, fonts (families), shadows, animation
  time and every other `Style` field keep the values the context has, so the preset also works
  on top of a game's own look.
* **Removing the resource** leaves the style as it is (the engine does not restore anything).
  Inserting a theme again applies it again.

`UiTheme::Compact(UiDensity)` is the engine preset (`UiTheme::compact()`, possibly tweaked);
`UiTheme::Custom(UiDensity)` names a density of your own. Both are applied the same way:

```rust
use space3d_ui::{UiDensity, UiTheme};

app.insert_resource(UiTheme::Custom(UiDensity {
    body_size: 13.0,
    button_size: 13.0,
    ..UiDensity::compact()
}));
```

### Tokens

| token | egui `Style` field | compact | egui default | why |
|---|---|---|---|---|
| `item_spacing` | `spacing.item_spacing` | 6 × 2 | 8 × 3 | rows 1 pt closer, columns 2 pt |
| `button_padding` | `spacing.button_padding` | 4 × 1 | 4 × 1 | already tight |
| `window_margin` | `spacing.window_margin` | 4 | 6 | also the engine tools' panel margin |
| `menu_margin` | `spacing.menu_margin` | 3 | 6 | menus, popups, tooltips |
| `control_height` | `spacing.interact_size.y` | 18 | 18 | the smallest hit target stays 18 pt |
| `control_min_width` | `spacing.interact_size.x` | 32 | 40 | narrower drag values / color buttons |
| `indent` | `spacing.indent` | 14 | 18 | tree levels, collapsing bodies |
| `icon_size` | `spacing.icon_width` (inner: 4/7) | 12 | 14 | checkbox / radio / collapse icons |
| `icon_spacing` | `spacing.icon_spacing` | 4 | 4 | |
| `slider_width` | `spacing.slider_width` | 90 | 100 | |
| `combo_width` | `spacing.combo_width` | 84 | 100 | |
| `text_edit_width` | `spacing.text_edit_width` | 160 | 280 | egui's 280 pt fields waste the most room |
| `scroll_bar_width` | `spacing.scroll.bar_width` | 8 | 10 | |
| `heading_size` | `TextStyle::Heading` | 14 | 18 | window titles too |
| `body_size` | `TextStyle::Body` | 12.5 | 13 | readable at 1 ppp |
| `button_size` | `TextStyle::Button` | 12.5 | 13 | also drag values |
| `small_size` | `TextStyle::Small` | 10 | 9 | 9 pt is hard to read; 10 still small |
| `monospace_size` | `TextStyle::Monospace` | 12 | 13 | |
| `widget_radius` | `visuals.widgets.*.corner_radius` | 2 | 2 (3 hovered) | one radius for every state |
| `window_radius` | `visuals.window_corner_radius` | 4 | 6 | |
| `menu_radius` | `visuals.menu_corner_radius` | 3 | 6 | |
| `table_row_height` | — (read by tables) | 16 | — (18) | text rows; rows with controls stay 18 |
| `tooltip_delay` | `interaction.tooltip_delay` | 0.3 s | 0.5 s | snappier tool tips |

`UiDensity::egui_default()` holds the right-hand column. egui's `Style` has no table row height:
tables read it with `UiDensity::row_height(ui)` (the applied theme's `table_row_height`, or the
style's control height without a theme) or `UiDensity::of(ctx)`:

```rust
egui::Grid::new("assets")
    .striped(true)
    .min_row_height(UiDensity::row_height(ui))
    .show(ui, |ui| { /* ... */ });
```

The pure functions are public too: `compact_style(&base, &density) -> egui::Style`,
`apply_density(&mut style, &density)` and `apply_theme(&ctx, &theme)`.

### How much denser

`ThemePreview` is a representative tool panel (menu bar, toolbar, buttons, checkbox, slider,
combo box, text field, a collapsing section and a 10-row striped table). With the compact
preset it takes **338 × 374 pt instead of 486 × 420 pt — 61.9 % of the area** — with every
widget whole, no text wrapped or clipped and every control at least 18 pt tall
(`crates/space3d-ui/tests/theme.rs`). The stories `Engine/UI/Theme/Default` and
`Engine/UI/Theme/Compact` show the same panel side by side.

### Games that style egui themselves

A game that sets egui's `Style` itself (say, once at startup) needs to do nothing: without a
`UiTheme` resource the engine never touches the style. Should it want the compact sizes under
its own colors and fonts, it has two options:

* insert `UiTheme::compact()` (or a `Custom` density): the tokens are written over the game's
  style at the start of the next frame, once. A game that restyles later is not overwritten —
  the theme is applied again only when the resource changes (or call `apply_theme` yourself);
* or build its style from the preset: `compact_style(&my_style, &UiDensity::compact())`.

## In-game tools

The debug overlay (**F3**), profiler (**F4**) and console (**`**) show up inside games, so they
never read their sizes from the global style and never change it. Each applies the compact
density to its own `Ui` only, keeps the context's colors, and draws its own compact header (egui's
title bar height follows the global heading size). They look the same under egui's default, the
compact theme or a game's roomy style (story `Engine/UI/Tools/Game Style`).

* **Debug overlay** (`DebugOverlay`): a small panel at the top-left that never takes the pointer —
  FPS, average and worst frame time, a sparkline of the last 240 frames (60 / 30 Hz guides), and a
  monospace grid of label/value pairs with right-aligned numbers: frame, tick, rate, dropped
  ticks, entities, alpha, draws, triangles, drawn instances, uploaded bytes, egui primitives and
  vertices. `DebugOverlay::details` (console: `debug details`) adds the draw path, indirect / multi
  draws, batches, views, culled instances and instance bytes.
* **Profiler** (`Profiler`): header with the last frame's time, span count and a close button;
  Freeze / Timing toggles; collapsible *Timeline* (flame view) and *Systems* (striped table sorted
  by average time, numbers right-aligned) sections. `Profiler::record(FrameTimeline)` aggregates
  frames from other sources (recordings, replays, tests).
* **Console** (`Console`): level / filter / clear toolbar, one dense monospace line per entry and
  the command line. `Console::capture_logs = false` keeps only the console's own lines (command
  output and `Console::print`).

## Engine tools

`run_editor` / `editor_app` and `run_storybook` / `storybook_app` insert `UiTheme::compact()`
themselves. Their panels use property rows (a label column and the value on one line, shared by
every section so values align), collapsible component sections with header actions (remove,
edit JSON), fields that fill the row, and panel margins from the style. `EditorPlugin` added to a
game does **not** insert the theme: the game's style stays as it is (insert the theme yourself to
get the same density).

## Stories and tests

The engine storybook (`space3d_stories::engine_storybook`, feature `ui`, part of `engine`) holds
`Engine/UI/Theme/Default`, `Engine/UI/Theme/Compact`, `Engine/UI/Tools/Debug Overlay`,
`Engine/UI/Tools/Profiler`, `Engine/UI/Tools/Console` and `Engine/UI/Tools/Game Style`, with
goldens in `crates/space3d-stories/tests/__screenshots__/Engine/UI/`. The tool stories have a
`style` knob (egui default, compact theme, roomy game style).

Tests: `crates/space3d-ui/tests/theme.rs` (density, default path untouched, applied once, never
fights a game's style), `crates/space3d-ui/tests/alloc_free.rs` (no allocation while the theme is
unchanged or absent) and the editor's
`standalone_tools_are_compact_and_the_plugin_keeps_a_games_style`.
