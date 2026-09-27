//! Android and iOS (Xcode project) entry points for {{title}}.
//!
//! `space3d::main!` generates `android_main` (called by the NativeActivity that `cargo apk`
//! packages) and `extern "C" fn start_app()` (called from `ios/main.m` in the app that
//! `space3d build --target ios --xcode` builds). Both run the same `build` function as the
//! desktop and web builds; plain `space3d build --target ios` bundles `src/main.rs` instead.

space3d::main!({{crate_name}}::build);
