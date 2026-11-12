# Platforms

One `build(app: &mut App)` function runs everywhere; `space3d::main!(build)` generates the
platform entry points:

| target | generated | what runs |
|---|---|---|
| Windows / macOS / Linux | `pub fn main()` | `App::new()` → `build` → `app.run()` (winit event loop) |
| Web (`wasm32-unknown-unknown`) | `#[wasm_bindgen(start)]` fn (+ a no-op `main`) | panic hook + console logging, then the same; `run()` returns and the browser drives frames |
| Android | `#[no_mangle] fn android_main(AndroidApp)` | `space3d_platform::android_main_entry` (NativeActivity) |
| iOS | `#[no_mangle] extern "C" fn start_app()` | called from the Xcode project's `main.m`; never returns |

Android and iOS load a *library*, so projects created by `space3d new` keep the game in
`src/lib.rs` and have two thin entry crates: `src/main.rs` (desktop + web binary) and `mobile/`
(`crate-type = ["cdylib", "staticlib"]`). Desktop and web builds therefore never link the mobile
library types.

`space3d doctor` checks everything below and prints the fix for anything missing.

## Desktop

```sh
space3d run [--release]
space3d build --release            # binary + assets/ in dist/desktop/
```

Linux needs `libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev` (audio, input,
keyboard, Wayland). Graphics: DX12/Vulkan (Windows), Metal (macOS), Vulkan with GLES fallback
(Linux).

## Web

```sh
rustup target add wasm32-unknown-unknown
space3d serve                      # build + serve http://localhost:8080 (COOP/COEP, no-cache)
space3d serve --watch              # rebuild on changes to src/, assets/, web/, Cargo.toml
space3d serve --host               # listen on 0.0.0.0 (open it on a phone on the same network)
space3d build --target web --release   # dist/web/
```

What `space3d build --target web` does:

1. `cargo build --target wasm32-unknown-unknown --bin <game>`.
2. Reads the `wasm-bindgen` version from `Cargo.lock` and checks the installed
   `wasm-bindgen` CLI. They must match exactly; if not it prints
   `cargo install wasm-bindgen-cli --version <x.y.z> --locked` (or installs it with
   `--install-tools`).
3. `wasm-bindgen --target web --no-typescript --out-name <game> --out-dir dist/web …`.
4. Release builds: `wasm-opt -Oz` when binaryen is installed.
5. Copies `web/` (your `index.html`, icons, …) and `assets/` into `dist/web`.

The page loads the bundle with `import init from "./<game>.js"; init();`. Set
`WindowPlugin::canvas_id` to render into an existing `<canvas id="…">` (the templates use
`space3d`); otherwise a canvas is appended to `<body>`. winit gives the canvas an inline CSS
size from `WindowPlugin::width`/`height`; to let the page decide (e.g. fill the viewport), use
`!important` in your stylesheet as the templates do
(`canvas#space3d { width: 100vw !important; height: 100vh !important; }`) — the surface follows
the canvas' real size.

Web specifics: the renderer is created asynchronously (WebGPU when available, WebGL2 otherwise —
WebGL2 uses the CPU draw path); assets are fetched over HTTP relative to the page; the database
uses `localStorage` (KV, save slots, leaderboards; raw SQL returns `Unsupported`); WebSocket and
HTTP clients use the browser APIs; servers are native-only; audio starts after a user gesture.

Graphics on the web ([graphics.md](graphics.md#webgl2-and-other-downlevel-devices)): WebGPU
browsers get every feature as natively (`GraphicsQuality::recommended` picks Medium or High);
WebGL2 gets `GraphicsQuality::Low` — render scale 0.75, FXAA instead of MSAA, a 4-level bloom
(where `EXT_color_buffer_float` exists; without it the Ldr fallback tonemaps per fragment and has
no bloom), a 1024² PCF4 shadow, the Classic shading model with a small environment bake spread
over frames, and the CPU particle backend (8 192 particles, thinner spawns). Every feature has a
WebGL2 path (no compute, no storage buffers in vertex shaders). WebGL2 links each shader
program when a pipeline is first built (50–150 ms): prewarm on loading screens
(`GraphicsQuality::prewarm_request`). `examples/graphics_showcase` builds for the web
(`cargo run -p space3d-cli -- serve` from its directory; `web/index.html`), and
`DownlevelOverride::webgl2()` shows the WebGL2 look on a desktop GPU (the
`Engine/Showcase/WebGL2 *` stories).

## Android

One-time setup (the exact commands for a Windows PC without Android Studio are in
SpaceCorps2027's `docs/MOBILE.md`):

```sh
# SDK: platform-tools, a platform, build-tools, the NDK (sdkmanager or Android Studio)
export ANDROID_HOME="$HOME/Library/Android/sdk"        # Linux: $HOME/Android/Sdk
export ANDROID_NDK_ROOT="$ANDROID_HOME/ndk/<version>"
cargo install cargo-apk --locked
# rust-toolchain.toml already lists aarch64-linux-android (phones) and x86_64-linux-android (PC emulators)
```

Build and install:

```sh
space3d build --target android [--release]     # = cargo apk build -p <game>-mobile --lib --target aarch64-linux-android
adb install -r dist/android/<game>.apk
adb logcat -s RustStdoutStderr
```

The engine's own proof is `examples/hello_cube_mobile` (first run on a device: 2026-11-12, the
Android 16 x86_64 emulator, cargo-apk 0.10.0, NDK 30):

```sh
cargo apk build -p hello_cube_mobile --lib --target x86_64-linux-android
adb install -r target/debug/apk/hello_cube.apk
adb shell am start -n org.spacecorps.space3d.hellocube/android.app.NativeActivity
```

Packaging metadata (application id, SDK levels, orientation, permissions, bundled `assets/`)
lives in `[package.metadata.android]` of `mobile/Cargo.toml`; add `strip = "strip"` there, or a
debug build's library carries hundreds of MB of debug info into the APK. What the engine does
on Android:

- **Entry**: `space3d::main!` generates `android_main` (NativeActivity; no Java code).
  `space3d::platform::android_app()` is the activity's handle from the game's `build` on, and
  `space3d::platform::mobile::data_dir()` the folder the app may write in.
- **Assets**: the default asset source is `AndroidAssetSource`, the APK's `assets/` read through
  the activity's asset manager (they are zip entries, not files). Hot reload is off.
- **Rendering**: Vulkan (GLES fallback). wgpu's debug flags are off even in a debug build: the
  emulator's Vulkan driver crashes when objects are named (set `WGPU_DEBUG=1` to ask for them).
- **Lifecycle**: in the background the surface is dropped and the loop sleeps (no frames, no
  systems); on resume the surface is recreated, meshes and textures survive.
- **Input**: multi-touch `Touch`es, `Binding::TouchRegion`; `WindowPlugin::touch_as_mouse` makes
  the first finger the mouse too, for controls written for one. While a `space3d-ui` text field
  has the focus (`TextInput::wanted`) the on-screen keyboard is up.
- **HTTPS**: `space3d_net::http_client_builder()` checks certificates against the compiled-in
  Mozilla roots (reqwest's platform verifier needs a Java class a NativeActivity app has not).

Not done: safe-area insets (UI under cutouts and the gesture bar), moving the view when the
keyboard covers a text field, pausing audio in the background, compressed textures (ASTC), and
`space3d build --target android` still builds arm64 only.

## iOS

One-time setup (macOS only):

```sh
# Install Xcode from the App Store (the Command Line Tools alone are not enough: C dependencies
# such as SQLite and rustls' aws-lc need the iOS SDK), then:
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
brew install xcodegen
```

Build:

```sh
space3d build --target ios --sim        # simulator: cargo build -p <game>-mobile --target aarch64-apple-ios-sim,
                                        # then xcodegen + xcodebuild
space3d build --target ios --release    # device (needs signing in Xcode)
```

The Xcode project is generated from `ios/project.yml` by XcodeGen: an app target with
`main.m` (calls the Rust `start_app()`), `assets/` as a resource folder, the Rust static library
linked from `target/aarch64-apple-ios[-sim]/<profile>/`, and a pre-build phase that runs
`cargo build -p <game>-mobile` for the SDK and configuration being built — so ⌘R in Xcode
always runs current Rust code. To run on a device, open `ios/<game>.xcodeproj` and pick your
signing team.

Manual equivalent:

```sh
cargo build -p <game>-mobile --lib --target aarch64-apple-ios-sim
(cd ios && xcodegen generate)
xcodebuild -project ios/<game>.xcodeproj -scheme <game> -sdk iphonesimulator \
  -configuration Debug -derivedDataPath target/xcode CODE_SIGNING_ALLOWED=NO build
xcrun simctl install booted target/xcode/Build/Products/Debug-iphonesimulator/<game>.app
xcrun simctl launch booted <bundle id>
```

## Keyboard layouts

Players use AZERTY, QWERTZ, Dvorak, JIS and many other layouts. Every keyboard event carries
two identities, and the engine keeps both:

* **Physical key** (`KeyCode`) — the key's *position*, named after the US QWERTY key in that
  place. `Input::pressed`, `just_pressed` and `Binding::Key` use it. `KeyCode::KeyW` is the key
  right of Tab on every keyboard, labelled W on QWERTY and Z on AZERTY.
* **Logical key** (`LogicalKey`, winit's `Key`) — what the key *means* on the player's layout.
  The window runner records it on a `KeyEvent` per OS event: `physical`, `logical_key` (with
  Shift/AltGr/Option applied), `key_without_modifiers` (desktop only), `text`, `location`,
  `pressed`, `repeat`. Read this frame's events with `Input::key_events()`, or the last logical
  key of a physical key with `Input::logical_key(KeyCode)`.

**Bindings.** Bind physical keys for movement and anything position-based: WASD bound as
`KeyW/KeyA/KeyS/KeyD` is `Z/Q/S/D` on AZERTY — still an inverted T under the left hand. What
the player must see is the label on *their* keyboard, not the code name:

```rust
let input = ctx.resources.expect::<Input>();
let cap = input.key_cap(KeyCode::KeyW);           // "W" on QWERTY, "Z" on AZERTY — never empty
let label = input.key_label(KeyCode::KeyW);       // Some("z") / None while unknown
let space = input.key_label(KeyCode::Space);      // Some("Space") — English: localize it
let gen = input.keyboard_layout().generation();   // changes when labels may have changed
```

`key_label` returns what the key types **without modifiers** (dead keys: their base character,
`^`), or an English name for non-character keys (`Space`, `Enter`, `Shift`, `F1`, `Numpad 1`;
`Command`/`Option` on Apple platforms, `Windows` on Windows). `key_label_info` adds the shifted
character and the source (`Os`, `Learned`, `Named`); `key_cap` upper-cases letters as keyboards
print them (`Q`, but AZERTY's `é`/`2` key stays `é`) and falls back to the US QWERTY character
of the position while the layout's is unknown.

**UI.** egui receives the logical key (`Event::Key::key`) and the physical one
(`physical_key`), like egui-winit, so text fields and shortcuts follow the layout as in native
apps: `Ctrl/Cmd+Z/A/C/V/X` act on the keys labelled Z/A/C/V/X (on AZERTY, undo is the key in
QWERTY's W position). Layouts without Latin letters (Cyrillic, Greek, …) fall back to the
physical key, so shortcuts still work in the QWERTY positions.

Hotkeys read from egui follow the layout too: `i.key_pressed(egui::Key::M)` fires on the key
*labelled* M — `KeyCode::Semicolon` on AZERTY, `KeyU` for `G` on Dvorak — where it used to fire
on physical `KeyM`. Code that compares such a hotkey with physical-key bindings (skip the hotkey
while an action is bound to the same key) must translate it first, or the two disagree on
other layouts:

```rust
let input = ctx.resources.expect::<Input>();
// The physical key egui reads as M right now (None: the key labelled M is not known yet).
let m_key = space3d_ui::physical_key_for_egui_key(input, egui::Key::M);
let m_free = m_key.is_none_or(|k| !bound_to_an_action(k));
```

Alternatively read position-based hotkeys from `Input::just_pressed(KeyCode::KeyM)` and show
`Input::key_cap(KeyCode::KeyM)`. `physical_key_for_egui_key` is cheap enough for every frame:
it walks a reverse map (label → physical key) that the layout caches per `generation()` and
does not allocate while the labels stay the same. Use the map directly for "which key types
this character":

```rust
let layout = ctx.resources.expect::<Input>().keyboard_layout();
let m = layout.key_for_label("m");                     // Some(KeyCode::Semicolon) on AZERTY
let digit = layout.key_for_label_matching(|t| t.chars().all(|c| c.is_ascii_digit()));
let text: Option<&str> = layout.label_text(KeyCode::KeyA); // borrowed, no allocation
```

When several keys print the same text, the first in keyboard order (`writing_system_keys()`)
wins.

**Fake and sample layouts.** `KeyboardLayout::set_os_layout(id, labels)` sets a layout table
directly (custom platforms, tests, screenshots of another layout's key caps). On macOS,
Windows and the web the window runner re-reads the OS table (once per second, on focus), which
replaces such a table; pin it to keep it:

```rust
let layout = ctx.resources.expect_mut::<Input>().keyboard_layout_mut();
layout.set_os_layout(Some("sample-azerty".into()), azerty_labels);
layout.set_pinned(true);  // no OS re-reads, key events teach nothing, until unpinned
// ...
layout.set_pinned(false); // the runner re-reads the OS table on its next frame
```

While pinned, explicit calls (`set_os_layout`, `learn`, `clear_learned`, `clear_os_layout`)
still apply; `refresh_from_os` / `Input::refresh_keyboard_layout` do nothing.

**Where labels come from.** Learned labels (from key events on the current layout) win over the
OS table; the OS table is read by the window runner on the event-loop thread at start-up, on
focus gain and when the layout id changes (checked at most once per second) — never per frame.

| platform | OS table | layout switch detected | notes |
|---|---|---|---|
| macOS | `TISCopyCurrentKeyboardLayoutInputSource` + `UCKeyTranslate`, main thread only | yes (input source id, keyboard type) | first query at start-up initializes Text Input Services (~35 ms, once); IMEs report the layout they type through; ISO and JIS keys: see below |
| Windows | `MapVirtualKeyExW` + `ToUnicodeEx` for the thread's `HKL` | yes (`HKL`) | |
| Web | `navigator.keyboard.getLayoutMap()` (Chromium-based browsers), async | on focus | Firefox/Safari, or iframes without the `keyboard-map` permission: learned only; no shifted characters |
| Linux X11/Wayland | none (no hard `libxkbcommon` dependency) | no | learned from winit's `key_without_modifiers` |
| Android, iOS | none | no | learned from unmodified presses (Shift/Alt presses only teach the shifted character) |

Limitations:

* Without an OS table a character key has no label until the player presses it once; after an
  undetected layout switch the old learned labels stay until each key is pressed again (a key
  that types a new character forgets the shifted character learned on the old layout).
* Key-cap capitalization without a known shifted character is a heuristic (letter rows only).
* Caps Lock on the web and mobile: letters are learned in lower case (`"A"` → `"a"`), but where
  Caps Lock also locks Shift on other keys (the digit row of Windows AZERTY) a press teaches the
  shifted character until the key is pressed without Caps Lock.
* macOS Text Input Sources are main-thread only (macOS 14+ aborts the process otherwise): the
  runner reads them on the event-loop thread, and `Input::refresh_keyboard_layout` reads nothing
  on other threads (headless tests on libtest threads use learned labels only).
* macOS ISO keyboards (most European Macs): winit 0.30 reports both the key left of `1` (`§`
  on US/British, `@` on French, `^` on German Macs; virtual key `0x0A`) and the key right of
  left Shift (`` ` `` on US/British, `<` on French/German; `0x32`) as `KeyCode::Backquote`. The
  runner reads the virtual key of the event AppKit is dispatching (`NSApp.currentEvent`) and
  reports the key right of left Shift as `KeyCode::IntlBackslash` (its W3C position, as on
  Windows); each key has its own OS label (egui gets its character's key, else
  `egui::Key::IntlBackslash`, like egui-winit). A `Backquote` binding (such as the UI
  console's default key) now fires on the key left of `1` only. If winit delivers a key event
  late (it queues events that arrive while it is handling another), the virtual key is unknown
  and either key reads `Backquote` (a release still releases the key its press named), so on
  ISO keyboards `key_label(Backquote)` comes from the OS table only and is not learned from
  presses.
* macOS JIS keyboards: winit 0.30 has no code for the Ro key (`_`, left of right Shift; virtual
  key `0x5E`); the runner reports it as `KeyCode::IntlRo` with its OS label.
* A table set with `set_os_layout` lasts until the next OS check unless pinned (see above).

## Mouse buttons, focus and the UI

`Input` holds the mouse buttons (`mouse_pressed`, per-frame edges `mouse_just_pressed` /
`mouse_just_released`) and also records this frame's presses and releases **in order**:
`Input::mouse_button_events()` lists a `MouseButtonEvent` per change with the button, `pressed`,
the cursor `position` at the time (`None`: outside the window) and a `cause`. The edges alone
lose the order — a release and a new press in one frame (the second click of a fast double
click) show both edges with the button held; the events say it went up first. Every injection
records events (`inject_mouse_button_down` / `inject_mouse_button_up` as `Device` events), so
tests and replays drive the same path as the window runner.

**Releases without a release event.** A button normally comes up with its release event. Two
more things release it, as **cancels** (`MouseButtonEvent::cancelled()`,
`MouseButtonCause::is_cancel()`):

* `MouseButtonCause::FocusLost` — the window lost the focus (or the app was suspended): the
  runner calls `Input::release_all()`, which releases every held key and button so game state
  never sticks.
* `MouseButtonCause::OsReconciled` — the OS reports the button up although its release event
  never arrived. On macOS the release goes elsewhere when the system takes the pointer between
  press and release (Mission Control, App Exposé, a Spaces swipe, a hot corner, the screenshot
  overlay) while the window stays key; nothing else would ever release the button. Before each
  frame the runner asks the OS which buttons it holds — one call, only while a button is held —
  and releases a button the OS showed held during this press and has shown up since, on
  consecutive checks spanning at least 50 ms (two at least, with a frame's events delivered in
  between: the OS state can be ahead of the event queue, and a real release event still on its
  way stays a real release). A press the OS never showed held (a click
  shorter than a frame, a synthetic or injected press) is left to its own release. It never
  presses anything.

| platform | OS button state | a lost release |
|---|---|---|
| macOS | `+[NSEvent pressedMouseButtons]` (window server state) | released as `OsReconciled` |
| Windows | `GetAsyncKeyState` (physical buttons, swapped back for left-handed mice: `SM_SWAPBUTTON`) | released as `OsReconciled` |
| Linux, web, Android, iOS | none | waits for the next click or a focus loss |

Leaving the window releases nothing: macOS, Windows and X11 deliver the release of a drag that
ends outside, and a drag may cross the window's edge (a slider, a window, hold-to-steer).

**What the UI does with them.** `space3d-ui` sends egui the button events in order, each where
the cursor was (egui's pointer moves there first, so a release after a move beyond egui's click
distance is no click), then the cursor's current position. A button event while the cursor is
outside the window goes to the last position egui saw, and the pointer is gone again right after
(`PointerGone`), so nothing at the window's corner gets hovered. `Event::WindowFocused` is sent
when `WindowState::focused` changes (egui then forgets the keys and modifiers it held).

A cancelled release registers **neither a click nor a drop** in egui — like switching away from
a native app mid-drag. Drags end (`Response::drag_stopped`), a dragged slider keeps its value,
a button pressed before the focus loss does not fire, and egui's drag-and-drop payload is
dropped without reaching any drop zone (`dnd_release_payload` returns `None`). Game code that
implements its own drag-and-drop or "release over X" logic from `Input` should skip releases
with `cancelled()`:

```rust
for e in ctx.resources.expect::<Input>().mouse_button_events() {
    if e.button == MouseButton::Left && !e.pressed {
        if e.cancelled() {
            cancel_drag();           // focus lost / release only the OS saw: no drop
        } else {
            drop_here(e.position);   // a real release
        }
    }
}
```

**egui plugins.** egui 0.36 runs its plugins' pass hooks (`Plugin::on_begin_pass` /
`on_end_pass`) only from `Context::run_ui`; the engine's pass spans `Stage::PreUpdate` to
`Stage::PostUpdate` (`begin_pass` / `end_pass`), so the engine runs the hooks itself, in
`run_ui`'s order and with a top-level `Ui` like `run_ui`'s: the begin hooks before the UI systems,
the end hooks after them. That covers egui's built-in plugins — label text selection (a drag to
select ends with the release), drag-and-drop (a payload nobody took ends with the release or
Esc) and `egui::debug_text` — and plugins added with `EguiContext::add_plugin(plugin)` (or made
known with `EguiContext::register_pass_hooks::<P>()` when a library already added them with
`egui::Context::add_plugin`). egui cannot list a context's plugins, so hooks of plugins the engine
does not know about do not run, and `egui::Context::on_begin_pass` / `on_end_pass` callbacks
(kept in a private egui plugin) never run: register a plugin instead.

**Tests** drive all of this headless: `WindowState::focused = false` plus `Input::release_all()`
is what the runner does on a focus loss, and `Input::release_mouse_button(b,
MouseButtonCause::OsReconciled)` is what the OS check does.

## The mouse cursor and the window icon

The pointer shape over the window is the `WindowCursor` resource (inserted by `WindowPlugin`).
`space3d-ui` writes egui's cursor into it whenever egui asks for a different one (the I-beam
over text fields, resize arrows, a pointing hand where the game asks for it with
`on_hover_cursor` or egui's `interact_cursor` style); a game may set `WindowCursor::icon`
itself (a crosshair in flight, say), and that stays until egui's cursor changes. After every
frame the runner applies it with winit's `Window::set_cursor`, so it survives Windows
re-applying the window cursor on `WM_SETCURSOR` and macOS's cursor rects. `icon: None` (egui's
`CursorIcon::None`) hides the pointer, as does `WindowState::cursor_visible = false`; grabbing
(`cursor_grab`) is independent. `WindowCursor::applied()` reports what was last applied to a
real window (shape and visibility; `None` headless), for tests that run a window in background
mode.

`WindowPlugin::icon` sets the window icon (title bar, Alt-Tab) from RGBA8 pixels
(`WindowIcon { rgba, width, height }` or `WindowIcon::from_image`); on Windows it is also the
taskbar icon, which Windows 11 otherwise shows blank for an exe without an embedded icon. macOS
ignores it (the Dock shows the app bundle's icon); the web and mobile use their own manifests.

## CI and releases

`space3d ci init --targets desktop,web,android,ios` (also run by `space3d new`) writes
`.github/workflows/space3d.yml`: tests with a software GPU, a desktop matrix (Linux, Windows,
macOS), the web bundle (deployed to GitHub Pages), an APK and an unsigned `.ipa`, and a GitHub
Release with all of them on `v*` tags. The file goes to the repository root (`git rev-parse
--show-toplevel`, else the cargo workspace root), the only place GitHub reads workflows; for a
package nested inside a bigger repository the command says which `-p` / `working-directory`
to add to the jobs. The `wasm-bindgen-cli` version it installs is taken from `Cargo.lock`.

Locally, `space3d publish --tag v1.2.0 [--targets desktop,web,android] [--draft] [--dry-run]`
builds release artifacts into `dist/`, archives them (`<game>-v1.2.0-<platform>.tar.gz`, `.apk`)
in `dist/release/`, creates the GitHub release (or reuses the existing one for that tag) and
uploads the archives. It needs `GITHUB_TOKEN`/`GH_TOKEN` or `gh auth login`; the repository is
taken from `git remote get-url origin` unless `--repo owner/name` is given.
