# On-device AI on Android: the Gradle host

`space3d-ai`'s `AndroidBackend` runs Gemini Nano through [open-android-models]. It reaches the library's JNI entry
point, `com.spacecorps.oam.jni.OamJni`, which runs a `BridgeEngine` speaking the same JSON-RPC protocol as Apple's
bridge, over `GeminiNanoModel` (the ML Kit GenAI Prompt API on AICore).

Space3d normally packages Android games with `cargo apk` as a NativeActivity (`docs/platforms.md`). **cargo-apk cannot
include AARs.** The open-android-models AAR carries Kotlin code (dex), and ML Kit brings its own code and manifest
entries: AICore's permission and package query. So a game that uses the Android backend is packaged by a small
**Gradle host app**:

- `android.app.NativeActivity`, as with cargo-apk (android-activity and winit run the game unchanged)
- the game's Rust `cdylib`, built with cargo-ndk into `jniLibs`
- the open-android-models AAR, which brings ML Kit, with the manifests merged by Gradle

A working template is **`examples/ai_tavern/android`**. It packages the tavern example; copy it for your game.

## Layout

```text
examples/ai_tavern/
  src/lib.rs                  the game (`pub fn build(app)`), shared by every platform
  src/main.rs                 desktop/web binary: space3d::main!(ai_tavern::build)
  android/
    native/                   the Android library: crate-type cdylib, space3d::main!(ai_tavern::build)
      Cargo.toml              its own [workspace], so the engine workspace never links it
    app/build.gradle.kts      com.android.application + oam-mlkit + the cargoNdk task
    app/src/main/AndroidManifest.xml   NativeActivity, android.app.lib_name = ai_tavern_android
    settings.gradle.kts       includeBuild(open-android-models) or mavenLocal()
    build.gradle.kts, gradle.properties, gradlew + gradle/wrapper (Gradle 9.7.1)
```

This mirrors the `space3d new` layout: the game in a library, and a thin crate per final artifact.

## Prerequisites

- JDK 21 (`JAVA_HOME`)
- The Android SDK (`ANDROID_HOME`) with platform `android-37.1`, the same `compileSdk` as open-android-models
- NDK 30 (`ANDROID_NDK_HOME`, or `$ANDROID_HOME/ndk/<version>`)
- `cargo install cargo-ndk`
- `rustup target add aarch64-linux-android` for the toolchain the build uses. Space3d pins one in
  `rust-toolchain.toml`. To use another toolchain that has the target, pass `-PrustToolchain=stable`.
- A checkout of [open-android-models]. By default it is found next to the Space3d checkout, at
  `../../../../open-android-models` relative to the host.

## Build and install

```sh
cd examples/ai_tavern/android
./gradlew :app:assembleDebug                      # cargo-ndk (release) + open-android-models from source + APK
./gradlew :app:assembleDebug -PcargoProfile=dev   # faster, unoptimized Rust
./gradlew :app:assembleDebug -PoamDir=/path/to/open-android-models
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb logcat -s space3d OamJni GeminiNano AICore    # watch it run
```

`preBuild` depends on the `cargoNdk` task, which runs
`cargo ndk -t arm64-v8a --platform 26 -o app/src/main/jniLibs build --release` in `android/native`. AGP then packages
`libai_tavern_android.so`. Only `arm64-v8a` is built: every AICore device is arm64.

**open-android-models as a dependency.** `app/build.gradle.kts` depends on
`com.spacecorps.oam:oam-mlkit:0.1.0-SNAPSHOT`. With `oamDir` set in `gradle.properties` (the default), the settings
`includeBuild()` that checkout, and Gradle substitutes its modules: they publish under the same `group:name`. The
host uses the same Android Gradle Plugin version (9.4.1), because a composite build shares one plugin version.
To use published artifacts instead, run `./gradlew publishToMavenLocal` in open-android-models and set `oamDir=`
(empty). The host already lists `mavenLocal()`.

## What happens at runtime

1. NativeActivity loads `libai_tavern_android.so` and android-activity calls `android_main`, which `space3d::main!`
   generates. android-activity publishes the JavaVM and the Application context through ndk-context.
2. `AiPlugin` with `BackendChoice::Auto` (or `auto_or(scripted)`) creates `AndroidBackend::from_android_context()`.
   `start` then does the following, following the host checklist in `OamJni`'s KDoc:
   - It loads `com.spacecorps.oam.jni.OamJni` through `context.getClassLoader()`. `FindClass` on a native thread
     sees only system classes.
   - It registers `nativeDeliver` with **`RegisterNatives`**, because libraries that NativeActivity loads are not
     searched for `Java_…` symbols. The symbol `Java_com_spacecorps_oam_jni_OamJni_nativeDeliver` is exported too,
     for hosts that call `System.loadLibrary`.
   - It calls `OamJni.create(context, handle)`. The handle is a key in a registry of inboxes, never a raw pointer, so
     a stale handle can never reach freed memory.
3. `Ai` methods call `OamJni.send(id, line)`, which only queues the line. The Kotlin bridge answers on its coroutines,
   and every outgoing line arrives through `nativeDeliver` on a JVM background thread. The line is copied into the
   engine's `Inbox`, and `ai::pump` routes it in `Stage::PreUpdate` of the next frame.
4. Strings cross JNI as proper modified UTF-8: the `jni` crate encodes and decodes surrogate pairs, so emoji survive
   in both directions.
5. When the `Ai` resource is dropped, `OamJni.destroy(id)` runs. It waits for an in-flight `nativeDeliver` and then
   stops delivery, and the handle is unregistered.

If the AAR is missing, or `OamJni.create` fails, the backend reports why. `Ai::status()` becomes
`AiStatus::Unavailable(error)`, and every request fails with `AiErrorCode::Unsupported`, carrying that message: the
game keeps running and can show scripted content.

## Adapting it to your game

1. Put the game in a library crate with `pub fn build(app: &mut App)`, and add
   `space3d = { …, features = ["ai"] }`.
2. Copy `android/` and change the following:
   - `native/Cargo.toml`: the package name and the path to your game crate
   - `AndroidManifest.xml`: `android.app.lib_name`, which is the native package name with `-` replaced by `_`
   - `app/build.gradle.kts`: `namespace` and `applicationId`
3. For a custom Activity, a GameActivity or an existing Android app, create the backend yourself and inject it:
   `AiPlugin::new(BackendChoice::backend(unsafe { AndroidBackend::from_raw(vm_ptr, context_ptr)? }))`, for example
   with `AndroidApp::vm_as_ptr()` and `activity_as_ptr()`.
4. To replace the model options, limits or extensions, set `OamJni.engineFactory` in your `Application.onCreate`.
   See open-android-models' `docs/PROTOCOL.md`.

## Device and product notes

- **Devices.** Gemini Nano runs only on AICore devices. nano-v3 and later cover Pixel 9 and 10, Galaxy S26 and
  others; nano-v4 covers Pixel 11 and Galaxy Z Flip8/Fold8. There is no emulator support and no support on unlocked
  bootloaders. Elsewhere, `model/availability` reports `available: false` with a `reason`, and scripted models still
  work.
- **Download.** A fresh device may report `status: "downloadable"` or `"downloading"` in `ModelAvailability`. Offer
  the download, or show scripted content until it is ready.
- **Foreground only.** Turns started in the background fail with `rate_limited` (`BACKGROUND_USE_BLOCKED`).
  NativeActivity is the foreground app while the game is on screen.
- **Budget.** The input budget is about 4000 tokens (`contextSize`) and output is capped at 4096. Keep personas short.
- **Behavior.** The protocol behaves the same as on Apple, apart from the Android differences in open-android-models'
  `docs/PROTOCOL.md`:
  - `auto` behaves like `explicit`.
  - A step makes at most one tool call.
  - Steps carry `kind` and `isRepair`.
  - Transcripts are platform-specific.
- **Terms.** Read the [ML Kit GenAI terms](https://developers.google.com/ml-kit/genai-terms) before shipping. They
  restrict services likely to be accessed by under-18s.

## Testing without a device

`tests/android_jvm.rs` runs `AndroidBackend` against open-android-models' **real** `OamJni` and Kotlin `BridgeEngine`
in a desktop JVM, started in-process with `-Xcheck:jni`. It covers everything the backend does on a device except
ML Kit:

- class loading through the context's class loader
- `RegisterNatives`
- `create`, `send` and `destroy`
- string conversion both ways, including emoji
- delivery on the JVM's threads
- the engine-level tool flow
- the missing-AAR error

It also runs the eight shared protocol scenarios. The `android-jvm` feature compiles the backend on desktop hosts,
and `tests/jvm` holds stubs for `android.content.Context` and `android.util.Log`.

```sh
export JAVA_HOME=/path/to/jdk-21
(cd open-android-models && ./gradlew -q --no-configuration-cache \
    -I $SPACE3D/crates/space3d-ai/tests/jvm/print-classpath.init.gradle.kts \
    :oam-bridge:jar :oam-mlkit:bundleLibRuntimeToDirDebug :oam-bridge:printOamJniClasspath) > /tmp/oam-cp
OAM_ANDROID_CLASSPATH=$(tail -1 /tmp/oam-cp) \
    cargo test -p space3d-ai --features android-jvm --test android_jvm -- --nocapture
```

## Status

- **Build.** The template builds an APK on this machine (`./gradlew :app:assembleDebug`). The APK contains:
  - `libai_tavern_android.so`, exporting `android_main` and `Java_com_spacecorps_oam_jni_OamJni_nativeDeliver`
  - `OamJni`, `BridgeEngine` and ML Kit in its dex
  - the AICore permission and package query in the merged manifest
- **Checks.** The JNI code is clippy-clean for `aarch64-linux-android`, and it passes the protocol scenarios
  against the real `OamJni` in a desktop JVM.
- **Not yet run on a device.** Nothing has run on Gemini Nano: no device was available. On a device, the new
  element is ML Kit itself; the bridge code on top of it is the part the JVM test covers.

[open-android-models]: https://github.com/SpaceCorps/open-android-models
