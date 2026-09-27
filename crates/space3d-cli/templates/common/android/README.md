# Android

The game ships as the `cdylib` of the `mobile/` crate, loaded by Android's NativeActivity: no
Java or Kotlin code, `space3d::main!` generates the `android_main` it calls. This directory is
the Gradle project (Android Gradle Plugin) that packages it — edit it freely:

| file | what |
|---|---|
| `build.gradle.kts` | application id, SDK levels, version, signing |
| `src/main/AndroidManifest.xml` | app name, orientation, permissions, the NativeActivity |
| `src/main/res/` | add launcher icons here (`mipmap-*/ic_launcher.png`, then `android:icon`) |
| `settings.gradle.kts`, `gradle.properties` | Gradle settings |

`space3d build --target android`:

1. `cargo ndk -t arm64-v8a -P 28 build -p {{name}}-mobile --lib` builds
   `lib{{crate_name}}_mobile.so` with the NDK and copies it to
   `target/android/{{name}}/jniLibs/arm64-v8a/`;
2. `gradle -p android assembleDebug` (or `assembleRelease`, `bundleRelease` with `--aab`)
   packages it with `../assets/` into `dist/android/{{name}}.apk`. All Gradle output goes to
   `target/android/{{name}}/`.

## One-time setup

`space3d doctor --platforms android` checks all of this and prints the fixes.

1. The Android SDK and NDK: Android Studio's SDK Manager, or the command-line tools
   (`brew install --cask android-commandlinetools` on macOS) and
   `sdkmanager --sdk_root="$HOME/Library/Android/sdk" "platform-tools" "platforms;android-37.0" "build-tools;37.0.0" "ndk;30.0.16248370"`.
   The SDK is found in `ANDROID_HOME` or the default location (`~/Library/Android/sdk`,
   `~/Android/Sdk`), the newest NDK inside it.
2. Java 17+ (`JAVA_HOME`, or found automatically: Android Studio's, Homebrew's `openjdk`).
3. Gradle 9 (`brew install gradle`, or add a Gradle wrapper here: `gradle wrapper`).
4. `cargo install cargo-ndk --locked` and `rustup target add aarch64-linux-android`.

## Build and run

```sh
space3d build --target android            # debug APK in dist/android/
space3d build --target android --release  # release APK (debug key unless configured below)
space3d build --target android --release --aab   # App Bundle for Google Play
adb install -r dist/android/{{name}}.apk
adb shell am start -n {{android_package}}/android.app.NativeActivity
adb logcat -s space3d                     # the game's log: stdout, tracing, panics
```

Environment variables reach the app through system properties: set them before launching,
e.g. `adb shell setprop debug.space3d.log debug` (`RUST_LOG`),
`debug.space3d.wgpu_backend gl` (`WGPU_BACKEND`), `debug.space3d.exit_after_frames 600`.

Release signing: set `SPACE3D_ANDROID_KEYSTORE` (path), `SPACE3D_ANDROID_KEYSTORE_PASSWORD`,
`SPACE3D_ANDROID_KEY_ALIAS` and `SPACE3D_ANDROID_KEY_PASSWORD` (see `build.gradle.kts`).

Touch input arrives as multi-touch `Touch`es; `Binding::TouchRegion` turns screen areas into
virtual buttons (see `bind_controls` in `src/lib.rs`); the Back button is
`KeyCode::BrowserBack`. When the app is backgrounded the surface is dropped and recreated on
resume — GPU resources survive. Sending `AppExit` ends the process (the next launch starts a
fresh one).
