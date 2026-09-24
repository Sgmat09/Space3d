//! Android and iOS entry points.
//!
//! # Android
//!
//! Build the game as a `cdylib` (e.g. with `cargo apk`/`xbuild`, NativeActivity) and export
//! `android_main`; winit's `android-activity` backend calls it on the app's main thread:
//!
//! ```ignore
//! #[cfg(target_os = "android")]
//! #[unsafe(no_mangle)]
//! fn android_main(app: space3d_platform::AndroidApp) {
//!     space3d_platform::android_main_entry(app, |app| {
//!         app.add_plugin(space3d_platform::PlatformPlugin::default());
//!         // add game plugins/systems here
//!     });
//! }
//! ```
//!
//! On suspend the native window is destroyed: the runner drops the wgpu surface
//! ([`space3d_render::Renderer::remove_surface`]) and recreates it on resume; the device,
//! meshes and textures survive. Touch input arrives as multi-touch [`crate::Touch`]es.
//!
//! # iOS
//!
//! winit's UIKit backend needs no special entry point in Rust: build a `staticlib`, export a
//! C function and call it from the Xcode project's `main` (it never returns):
//!
//! ```ignore
//! #[unsafe(no_mangle)]
//! pub extern "C" fn start_app() {
//!     let mut app = space3d_core::App::new();
//!     app.add_plugin(space3d_platform::PlatformPlugin::default());
//!     app.run();
//! }
//! ```
//!
//! Suspend/resume behave as on Android (surface dropped while backgrounded).

#[cfg(target_os = "android")]
pub use winit::platform::android::activity::AndroidApp;

#[cfg(target_os = "android")]
static ANDROID_APP: std::sync::OnceLock<AndroidApp> = std::sync::OnceLock::new();

/// Android entry: stash the [`AndroidApp`] for the event loop, build the [`space3d_core::App`]
/// with `build`, then run it.
#[cfg(target_os = "android")]
pub fn android_main_entry(android_app: AndroidApp, build: impl FnOnce(&mut space3d_core::App)) {
    let _ = ANDROID_APP.set(android_app);
    let mut app = space3d_core::App::new();
    build(&mut app);
    app.run();
}

#[cfg(target_os = "android")]
pub(crate) fn android_app() -> Option<AndroidApp> {
    ANDROID_APP.get().cloned()
}
