# Android

The game ships as the `cdylib` of the `mobile/` crate, packaged by
[cargo-apk](https://github.com/rust-mobile/cargo-apk) as a NativeActivity. Packaging settings
(package id `{{android_package}}`, SDK levels, orientation, permissions, bundled assets) live
under `[package.metadata.android]` in `mobile/Cargo.toml`.

## One-time setup

1. Install Android Studio (or the command-line tools) and, in *SDK Manager › SDK Tools*, the
   **NDK (Side by side)**.
2. Point the tools at them (add to your shell profile):

   ```sh
   export ANDROID_HOME="$HOME/Library/Android/sdk"          # Linux: $HOME/Android/Sdk
   export ANDROID_NDK_ROOT="$ANDROID_HOME/ndk/<version>"
   ```
3. Java 17+ on `PATH` (Android Studio bundles one).
4. `cargo install cargo-apk --locked` and `rustup target add aarch64-linux-android`.

`space3d doctor --platforms android` checks all of this.

## Build and run

```sh
space3d build --target android            # debug APK in dist/android/
space3d build --target android --release  # release APK (signed with the debug key)
adb install -r dist/android/{{name}}.apk
adb logcat -s RustStdoutStderr            # logs
```

For Play Store releases configure `[package.metadata.android.signing.release]` in
`mobile/Cargo.toml` with your keystore.

Touch input arrives as multi-touch `Touch`es; `Binding::TouchRegion` turns screen areas into
virtual buttons (see `bind_controls` in `src/lib.rs`). When the app is backgrounded the
surface is dropped and recreated on resume — GPU resources survive.
