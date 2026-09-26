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

One-time setup:

```sh
# Android Studio → SDK Manager → SDK Tools → NDK (Side by side)
export ANDROID_HOME="$HOME/Library/Android/sdk"        # Linux: $HOME/Android/Sdk
export ANDROID_NDK_ROOT="$ANDROID_HOME/ndk/<version>"
cargo install cargo-apk --locked
rustup target add aarch64-linux-android
```

Build and install:

```sh
space3d build --target android [--release]     # = cargo apk build -p <game>-mobile --lib --target aarch64-linux-android
adb install -r dist/android/<game>.apk
adb logcat -s RustStdoutStderr
```

Packaging metadata (application id, SDK levels, orientation, permissions, bundled `assets/`)
lives in `[package.metadata.android]` of `mobile/Cargo.toml`. Rendering uses Vulkan (GLES
fallback); touch input arrives as multi-touch `Touch`es and `Binding::TouchRegion` maps screen
areas to actions. When the app is backgrounded the surface is dropped and recreated on resume —
meshes and textures survive.

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
