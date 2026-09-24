//! Android and iOS entry points for {{title}}.
//!
//! `space3d::main!` generates `android_main` (called by the NativeActivity that `cargo apk`
//! packages) and `extern "C" fn start_app()` (called from `ios/main.m`). Both run the same
//! `build` function as the desktop and web builds.

space3d::main!({{crate_name}}::build);
