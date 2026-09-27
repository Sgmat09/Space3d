//! Android and iOS entry points.
//!
//! # Android
//!
//! Build the game as a `cdylib` (`space3d build --target android`: cargo-ndk + Gradle,
//! NativeActivity) and export `android_main`; winit's `android-activity` backend calls it on
//! its own thread once the activity is created. `space3d::main!` generates it; by hand:
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
//! [`android_main_entry`] routes stdout/stderr (and so the `tracing` output) and panics to
//! logcat under the tag [`LOGCAT_TAG`] (`adb logcat -s space3d`), takes environment variables
//! from `debug.space3d.*` system properties ([`ENV_PROPERTIES`]: `adb shell setprop
//! debug.space3d.log debug`), and makes the app's private
//! storage (`internal_data_path`) the working directory, so relative paths such as
//! `DbPlugin`'s `space3d.db` are writable.
//!
//! On suspend the native window is destroyed: the runner drops the wgpu surface
//! ([`space3d_render::Renderer::remove_surface`]) and recreates it on resume; the device,
//! meshes and textures survive. Touch input arrives as multi-touch [`crate::Touch`]es, the
//! Back button as `KeyCode::BrowserBack`.
//!
//! winit creates one event loop per process, while Android may keep the process and create
//! the activity again. So once the app exits (`AppExit`) the process ends too, and the next
//! launch starts a fresh one.
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

/// Logcat tag of everything a Space3d app prints on Android (stdout, stderr, `tracing`,
/// panics): `adb logcat -s space3d`.
pub const LOGCAT_TAG: &str = "space3d";

#[cfg(target_os = "android")]
static ANDROID_APP: std::sync::Mutex<Option<AndroidApp>> = std::sync::Mutex::new(None);

/// Android entry: send stdout/stderr and panics to logcat, stash the [`AndroidApp`] for the
/// event loop, build the [`space3d_core::App`] with `build`, run it — and end the process
/// when it exits (winit cannot create a second event loop in it).
#[cfg(target_os = "android")]
pub fn android_main_entry(android_app: AndroidApp, build: impl FnOnce(&mut space3d_core::App)) {
    // Before any thread of ours runs: `set_var` must not race with readers.
    properties::import_env();
    logcat::init();
    {
        let mut slot = ANDROID_APP.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            // A new activity was created while the previous one's event loop still runs (a task
            // relaunched with CLEAR_TASK): winit cannot start another loop in this process, so
            // end it; Android then starts the activity in a fresh process.
            logcat::write(
                logcat::ERROR,
                "the activity was created again while the app still runs; restarting the process",
            );
            end_process();
        }
        // Relative paths (save files, `DbPlugin`'s database) go to the app's private storage:
        // the process starts in `/`, which apps cannot write.
        if let Some(dir) = android_app.internal_data_path() {
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = std::env::set_current_dir(&dir) {
                logcat::write(logcat::ERROR, &format!("cannot enter {}: {e}", dir.display()));
            }
        }
        *slot = Some(android_app);
    }
    let mut app = space3d_core::App::new();
    build(&mut app);
    app.run();
    logcat::write(logcat::INFO, "app exited; ending the process");
    end_process();
}

/// End the process right away. Not `std::process::exit`: its `atexit` handlers and static
/// destructors run while the runtime's binder threads still use them (a `SIGABRT` in
/// `pthread_mutex_lock called on a destroyed mutex`).
#[cfg(target_os = "android")]
fn end_process() -> ! {
    unsafe extern "C" {
        fn _exit(status: std::ffi::c_int) -> !;
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Let the logcat thread forward the last lines from the pipe.
    std::thread::sleep(std::time::Duration::from_millis(100));
    // SAFETY: terminates the process; nothing runs afterwards.
    unsafe { _exit(0) }
}

/// The [`AndroidApp`] the app was started with (asset manager, JNI, configuration), once
/// [`android_main_entry`] runs.
#[cfg(target_os = "android")]
pub fn android_app() -> Option<AndroidApp> {
    ANDROID_APP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Whether Android is destroying the activity (its task removed from Recents, the system
/// reclaiming it, "Don't keep activities"):
/// `Activity.isDestroyed()` through JNI. winit 0.30 drops the Destroy event, while
/// android-activity's `onDestroy` waits for `android_main` to return, so the runner checks this
/// while suspended and ends the event loop (and [`android_main_entry`] the process).
#[cfg(target_os = "android")]
pub(crate) fn activity_destroyed() -> bool {
    use jni::objects::JObject;
    use jni::refs::Global;
    let Some(app) = android_app() else {
        return false;
    };
    // SAFETY: android-activity hands out the process' JavaVM pointer.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let raw = app.activity_as_ptr() as jni::sys::jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<bool> {
        // SAFETY: the global reference stays valid while `app` lives; `Cast` never deletes it.
        let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw)? };
        env.call_method(&*activity, jni::jni_str!("isDestroyed"), jni::jni_sig!("()Z"), &[])?
            .z()
    })
    .unwrap_or(false)
}

/// Environment variables Android apps cannot be started with, read from system properties
/// instead: `adb shell setprop debug.space3d.<name> <value>` before launching sets the variable
/// (see [`ENV_PROPERTIES`]).
#[cfg(target_os = "android")]
mod properties {
    use std::ffi::{CStr, CString, c_char, c_void};

    #[repr(C)]
    struct PropInfo {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        fn __system_property_find(name: *const c_char) -> *const PropInfo;
        fn __system_property_read_callback(
            pi: *const PropInfo,
            callback: unsafe extern "C" fn(cookie: *mut c_void, name: *const c_char, value: *const c_char, serial: u32),
            cookie: *mut c_void,
        );
    }

    unsafe extern "C" fn store(cookie: *mut c_void, _name: *const c_char, value: *const c_char, _serial: u32) {
        // SAFETY: `cookie` is the `&mut String` passed below; `value` is a C string that is
        // valid for the duration of the callback.
        unsafe { *cookie.cast::<String>() = CStr::from_ptr(value).to_string_lossy().into_owned() };
    }

    /// Value of system property `name` (`None`: unset or empty).
    pub(super) fn get(name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        // SAFETY: valid C string; the returned pointer is checked before use.
        let pi = unsafe { __system_property_find(name.as_ptr()) };
        if pi.is_null() {
            return None;
        }
        let mut value = String::new();
        // SAFETY: `pi` came from `__system_property_find`; `store` writes to `value` only.
        unsafe { __system_property_read_callback(pi, store, (&raw mut value).cast()) };
        Some(value).filter(|v| !v.is_empty())
    }

    /// Set each unset variable of [`super::ENV_PROPERTIES`] from its property.
    pub(super) fn import_env() {
        for (property, var) in super::ENV_PROPERTIES {
            if std::env::var_os(var).is_none()
                && let Some(value) = get(property)
            {
                // SAFETY: called at the start of `android_main`, before the app starts threads
                // that read the environment.
                unsafe { std::env::set_var(var, value) };
            }
        }
    }
}

/// System properties that set environment variables on Android, where apps have no
/// environment of their own: `adb shell setprop debug.space3d.log debug` then launching the
/// app runs it with `RUST_LOG=debug`. Unset variables only.
pub const ENV_PROPERTIES: [(&str, &str); 7] = [
    ("debug.space3d.log", "RUST_LOG"),
    ("debug.space3d.backtrace", "RUST_BACKTRACE"),
    ("debug.space3d.wgpu_backend", "WGPU_BACKEND"),
    ("debug.space3d.exit_after_frames", "SPACE3D_EXIT_AFTER_FRAMES"),
    ("debug.space3d.exit_after_secs", "SPACE3D_EXIT_AFTER_SECS"),
    ("debug.space3d.quality", "SPACE3D_QUALITY"),
    ("debug.space3d.screenshot", "SPACE3D_SCREENSHOT"),
];

/// Logcat output: stdout/stderr through a pipe and a reader thread, panics written directly.
#[cfg(target_os = "android")]
mod logcat {
    use super::LOGCAT_TAG;
    use std::ffi::{CString, c_char, c_int};
    use std::io::BufRead;
    use std::os::fd::AsRawFd;

    pub(super) const INFO: c_int = 4;
    pub(super) const ERROR: c_int = 6;
    /// logcat truncates longer entries (~4 KiB); split before that.
    const MAX_ENTRY: usize = 3000;

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    unsafe extern "C" {
        fn dup2(old: c_int, new: c_int) -> c_int;
    }

    /// Write one message (split into logcat-sized entries) with priority `prio`.
    pub(super) fn write(prio: c_int, text: &str) {
        let tag = CString::new(LOGCAT_TAG).expect("tag has no NUL");
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(MAX_ENTRY);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let (chunk, tail) = rest.split_at(end);
            let line = CString::new(chunk.replace('\0', "\u{fffd}")).expect("NULs replaced");
            // SAFETY: both pointers are valid NUL-terminated strings for the call's duration.
            unsafe { __android_log_write(prio, tag.as_ptr(), line.as_ptr()) };
            rest = tail;
        }
    }

    /// Route stdout/stderr to logcat and log panics (once per process).
    pub(super) fn init() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::panic::set_hook(Box::new(|info| {
                let thread = std::thread::current();
                let name = thread.name().unwrap_or("<unnamed>");
                let backtrace = std::backtrace::Backtrace::capture();
                // Written directly: with `panic = "abort"` the process dies before the pipe
                // reader could forward stderr.
                write(ERROR, &format!("thread '{name}' {info}\n{backtrace}"));
            }));
            let (reader, writer) = match std::io::pipe() {
                Ok(pipe) => pipe,
                Err(e) => return write(ERROR, &format!("could not create the logcat pipe: {e}")),
            };
            // SAFETY: plain fd duplication; fds 1 and 2 stay open for the process lifetime.
            let ok = unsafe { dup2(writer.as_raw_fd(), 1) >= 0 && dup2(writer.as_raw_fd(), 2) >= 0 };
            if !ok {
                return write(ERROR, "could not redirect stdout/stderr to logcat");
            }
            drop(writer);
            let spawned = std::thread::Builder::new()
                .name("space3d-logcat".into())
                .spawn(move || {
                    let mut reader = std::io::BufReader::new(reader);
                    let mut line = Vec::new();
                    while matches!(reader.read_until(b'\n', &mut line), Ok(n) if n > 0) {
                        let text = super::strip_ansi(&String::from_utf8_lossy(&line));
                        write(INFO, text.trim_end());
                        line.clear();
                    }
                });
            if spawned.is_err() {
                write(ERROR, "could not start the logcat thread");
            }
        });
    }
}

/// Remove ANSI CSI escape sequences (`ESC [ ... final byte`), e.g. terminal colors.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, then one final byte in 0x40..=0x7e.
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn ansi_colors_are_stripped() {
        assert_eq!(
            strip_ansi("\u{1b}[2m2026-09-27\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m space3d: ready"),
            "2026-09-27  INFO space3d: ready"
        );
        assert_eq!(strip_ansi("plain ✓"), "plain ✓");
        assert_eq!(strip_ansi("cut \u{1b}["), "cut ");
    }
}
