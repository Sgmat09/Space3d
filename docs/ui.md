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

* **Without the resource the app's UI does not change.** The engine never writes egui's `Style`
  unless a `UiTheme` resource exists: the UI of an app that does not opt in renders exactly as
  before, pixel for pixel. (The engine's own F3 / F4 / console tools are not the app's UI: they
  have their own compact look in every app, see [In-game tools](#in-game-tools).)
* **Applied once, between passes.** `UiPlugin` applies the theme when the resource is inserted or
  changed, right before the next egui pass begins, to both the dark and the light style. In every
  other frame it costs one comparison and no allocation (`UiThemeSync`; tokens compare bit for
  bit, so even a NaN token counts as unchanged); there is no per-frame style churn.
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
combo box, text field, a collapsing section and a 10-row striped table). Its width is that of its
"Space [combo] Name [text field]" row at the style's default widths (`ThemePreview::panel_width`),
and every other row fills it: the slider rail, the transform fields and the table's columns
(numbers and their headers right-aligned). With the compact preset it takes **338 × 354 pt
instead of 487 × 390 pt — 63.1 % of the area** — with every widget whole, no text wrapped or
clipped and every control at least 18 pt tall (`crates/space3d-ui/tests/theme.rs`). The stories
`Engine/UI/Theme/Default` and `Engine/UI/Theme/Compact` show the same panel side by side, each in
an `egui::Window`.

### Window width

The theme holds sizes of egui's `Style`; the width of a window is not one of them. An
`egui::Window` is **340 pt wide by default whatever it holds** (its contents fill that width), so
a small game window ("Cargo hold: 12 t [Jettison]") keeps a lot of empty space under any theme.
Size game windows explicitly:

* `.auto_sized()`: the window takes the width of its contents. Give the contents a width
  (`ui.set_width(..)`, fixed-width widgets): widgets that fill the available width — separators,
  right-to-left rows, `TextEdit::desired_width(f32::INFINITY)` — stretch an auto-sized window to
  the screen edge (its first frame spans the screen, so pin it with `fixed_pos` or keep it clear
  of the edges);
* or `.default_width(w)` (with `.resizable(false)` for a fixed width).

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
never read their sizes from the global style and never change it. They are the engine's tools,
not the app's UI: they are compact **in every app**, with or without a `UiTheme`. Each applies
the compact density to its own `Ui` only (one cached copy of the style, not one per frame), keeps
the context's colors, and draws its own compact header (egui's title bar height follows the
global heading size). Their size and layout are the same under egui's default, the compact theme
or a game's roomy style; only the window fill and stroke follow the game's colors (story
`Engine/UI/Tools/Game Style`, `crates/space3d-ui/tests/tools.rs`).

The profiler docks to the top-right corner and the console to the bottom-left, 6 pt in from the
screen edges (and from each other), and never grow with their contents. The profiler is exactly as
tall as its contents, up to 45 % of the screen height (at least 300 pt) and never down into the
open console; its table scrolls by whole rows (no row is ever cut) with a scroll bar that stays
visible while rows are hidden. The console is 640 × 40 % of the screen (140–360 pt) and wraps long
lines (paths, backtraces) instead of widening.

* **Debug overlay** (`DebugOverlay`): a small panel at the top-left that never takes the pointer —
  FPS, average and worst frame time, a sparkline of the last 240 frames (60 / 30 Hz guides), and a
  monospace grid of label/value pairs with right-aligned numbers: frame, tick, rate, dropped
  ticks, entities, alpha, draws, triangles, drawn instances, uploaded bytes, egui primitives and
  vertices; the last line names the other tools' keys. `DebugOverlay::details` (console:
  `debug details`) adds the draw path, indirect / multi draws, batches, views, culled instances
  and instance bytes.
* **Profiler** (`Profiler`): header with the last frame's time, span count and a close button;
  Freeze / Timing toggles; collapsible *Timeline* (flame view) and *Systems* (striped table sorted
  by average time: a bar of the average, the name — truncated, full name on hover — and
  right-aligned avg / last / max / calls columns under a fixed header) sections.
  `Profiler::record(FrameTimeline)` aggregates frames from other sources (recordings, replays,
  tests).
* **Console** (`Console`): level / filter / clear toolbar, one dense monospace line per entry and
  the command line. `Console::capture_logs = false` keeps only the console's own lines (command
  output and `Console::print`).

## Engine tools

`run_editor` / `editor_app` and `run_storybook` / `storybook_app` insert `UiTheme::compact()`
themselves. Their panels use property rows (a label column in the normal text color and the value
on one line, shared by every section so values align), collapsible component sections with header
actions (remove, edit JSON), and panel margins from the style. Rows never ask for more than the
panel's width, under any style or density: sliders get a fixed-width value box (the same in every
row, so the boxes line up) and the rail takes the rest; x / y / z and paired fields split the row
into equal cells (axis color bars instead of "x " prefixes) and drop decimals before they would
widen it; groups of checkboxes or buttons wrap within the value column. The side panels keep
their width frame after frame and whatever is selected (hierarchy 160–400 pt, inspector 220–480
pt), their scroll bars stay visible while content is hidden, and buttons (the section "×"
included) are at least 18 pt tall. `EditorPlugin` added to a game does **not** insert the theme:
the game's style stays as it is (insert the theme yourself to get the same density), and its
panels still keep their width.

## Stories and tests

The engine storybook (`space3d_stories::engine_storybook`, feature `ui`, part of `engine`) holds
`Engine/UI/Theme/Default`, `Engine/UI/Theme/Compact`, `Engine/UI/Tools/Debug Overlay`,
`Engine/UI/Tools/Profiler`, `Engine/UI/Tools/Console` and `Engine/UI/Tools/Game Style`, with
goldens in `crates/space3d-stories/tests/__screenshots__/Engine/UI/`. The tool stories have a
`style` knob (egui default, compact theme, roomy game style).

Tests: `crates/space3d-ui/tests/theme.rs` (density, default path untouched, applied once, never
fights a game's style), `crates/space3d-ui/tests/tools.rs` (docked tools stay inside the screen and
apart at several sizes and scale factors, never grow, same geometry under a game's style),
`crates/space3d-ui/tests/alloc_free.rs` (no allocation while the theme is unchanged or absent) and
the editor's `standalone_tools_are_compact_and_the_plugin_keeps_a_games_style` and
`editor_panels_keep_their_width_under_any_style`.
