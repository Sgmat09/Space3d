//! What the runner asks the Android activity through JNI: winit 0.30 and `android-activity`'s
//! NativeActivity glue do not pass these on.
//!
//! * **Whether the activity is destroyed.** `onDestroy` runs on Java's UI thread and waits for
//!   `android_main` to return; winit only logs the event and its loop goes on, so the UI thread
//!   waits for ever, the process stays and the next start hangs. The runner asks
//!   `Activity.isDestroyed()` while the app is in the background and leaves the loop when it is.
//! * **Finishing the activity** when the game itself exits, so that the task ends with it.
//! * **The window's insets** (system bars and display cutout): an edge-to-edge window's content
//!   rectangle is the whole window, the insets are Java's to give.
//!
//! * **The on-screen keyboard's height** (`WindowInsets.Type.ime()`, Android 11 on): a
//!   NativeActivity's window is not resized for the keyboard and nothing tells it that one is up.
//!
//! Every call attaches the calling thread to the VM (cheap once attached) and treats a Java
//! exception as "no answer".

use crate::mobile::AndroidApp;
use crate::window::SafeArea;
use jni::objects::JObject;
use jni::refs::Global;
use jni::{JValue, JavaVM, jni_sig, jni_str};

/// Run `f` with the activity's object. `None`: no VM, or Java threw.
fn with_activity<T>(app: &AndroidApp, f: impl FnOnce(&mut jni::Env, &JObject) -> jni::errors::Result<T>) -> Option<T> {
    let vm = app.vm_as_ptr();
    let raw = app.activity_as_ptr() as jni::sys::jobject;
    if vm.is_null() || raw.is_null() {
        return None;
    }
    // SAFETY: the pointer is the process's `JavaVM`, valid for the process's life.
    let vm = unsafe { JavaVM::from_raw(vm.cast()) };
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: a global reference `app` owns and keeps while it lives; not deleted here.
        let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw)? };
        f(env, activity.as_ref())
    })
    .ok()
}

/// `Activity.isDestroyed()`: true from the start of `onDestroy` on. `false` when not known.
pub(crate) fn activity_destroyed(app: &AndroidApp) -> bool {
    with_activity(app, |env, activity| {
        env.call_method(activity, jni_str!("isDestroyed"), jni_sig!("()Z"), &[])?
            .z()
    })
    .unwrap_or(false)
}

/// `Activity.finish()`: the activity goes away as after Back (pause, stop, destroy).
pub(crate) fn finish_activity(app: &AndroidApp) -> bool {
    with_activity(app, |env, activity| {
        env.call_method(activity, jni_str!("finish"), jni_sig!("()V"), &[])?;
        Ok(())
    })
    .is_some()
}

/// What the on-screen keyboard covers of the window's bottom, in pixels
/// (`getRootWindowInsets().getInsets(ime()).bottom`; 0 while it is hidden). `None`: before
/// Android 11, no insets yet, or Java threw.
pub(crate) fn keyboard_inset(app: &AndroidApp) -> Option<u32> {
    if AndroidApp::sdk_version() < 30 {
        return None;
    }
    with_activity(app, |env, activity| {
        let window = env
            .call_method(
                activity,
                jni_str!("getWindow"),
                jni_sig!("()Landroid/view/Window;"),
                &[],
            )?
            .l()?;
        let decor = env
            .call_method(
                &window,
                jni_str!("getDecorView"),
                jni_sig!("()Landroid/view/View;"),
                &[],
            )?
            .l()?;
        let insets = env
            .call_method(
                &decor,
                jni_str!("getRootWindowInsets"),
                jni_sig!("()Landroid/view/WindowInsets;"),
                &[],
            )?
            .l()?;
        if insets.is_null() {
            return Ok(None);
        }
        let ime = env
            .call_static_method(
                jni_str!("android/view/WindowInsets$Type"),
                jni_str!("ime"),
                jni_sig!("()I"),
                &[],
            )?
            .i()?;
        let covered = env
            .call_method(
                &insets,
                jni_str!("getInsets"),
                jni_sig!("(I)Landroid/graphics/Insets;"),
                &[JValue::Int(ime)],
            )?
            .l()?;
        Ok(Some(
            env.get_field(&covered, jni_str!("bottom"), jni_sig!(jint))?.i()?.max(0) as u32,
        ))
    })
    .flatten()
}

/// What the system bars that are shown and the display cutout cover of the window, in pixels
/// (`getRootWindowInsets().getInsets(systemBars | displayCutout)`, Android 11 on; the system
/// window insets before). `None`: the window has no insets yet, or Java threw.
pub(crate) fn window_insets(app: &AndroidApp) -> Option<SafeArea> {
    with_activity(app, |env, activity| {
        let window = env
            .call_method(
                activity,
                jni_str!("getWindow"),
                jni_sig!("()Landroid/view/Window;"),
                &[],
            )?
            .l()?;
        let decor = env
            .call_method(
                &window,
                jni_str!("getDecorView"),
                jni_sig!("()Landroid/view/View;"),
                &[],
            )?
            .l()?;
        let insets = env
            .call_method(
                &decor,
                jni_str!("getRootWindowInsets"),
                jni_sig!("()Landroid/view/WindowInsets;"),
                &[],
            )?
            .l()?;
        if insets.is_null() {
            return Ok(None);
        }
        if AndroidApp::sdk_version() >= 30 {
            let class = jni_str!("android/view/WindowInsets$Type");
            let bars = env
                .call_static_method(class, jni_str!("systemBars"), jni_sig!("()I"), &[])?
                .i()?;
            let cutout = env
                .call_static_method(class, jni_str!("displayCutout"), jni_sig!("()I"), &[])?
                .i()?;
            let covered = env
                .call_method(
                    &insets,
                    jni_str!("getInsets"),
                    jni_sig!("(I)Landroid/graphics/Insets;"),
                    &[JValue::Int(bars | cutout)],
                )?
                .l()?;
            let mut side = |name| -> jni::errors::Result<u32> {
                Ok(env.get_field(&covered, name, jni_sig!(jint))?.i()?.max(0) as u32)
            };
            Ok(Some(SafeArea {
                left: side(jni_str!("left"))?,
                top: side(jni_str!("top"))?,
                right: side(jni_str!("right"))?,
                bottom: side(jni_str!("bottom"))?,
            }))
        } else {
            let mut side = |name| -> jni::errors::Result<u32> {
                Ok(env.call_method(&insets, name, jni_sig!("()I"), &[])?.i()?.max(0) as u32)
            };
            Ok(Some(SafeArea {
                left: side(jni_str!("getSystemWindowInsetLeft"))?,
                top: side(jni_str!("getSystemWindowInsetTop"))?,
                right: side(jni_str!("getSystemWindowInsetRight"))?,
                bottom: side(jni_str!("getSystemWindowInsetBottom"))?,
            }))
        }
    })
    .flatten()
}
