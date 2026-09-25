//! `AndroidBackend`: JNI into open-android-models' `com.spacecorps.oam.jni.OamJni`.
//!
//! ```text
//! engine thread ── OamJni.send(id, line) ──▶ BridgeEngine (Kotlin, background, in order)
//! Inbox        ◀── nativeDeliver(handle, line) ── JVM delivery thread (in order, never concurrent)
//! ```
//!
//! Follows the host checklist in `OamJni`'s KDoc: `nativeDeliver` is registered with
//! `RegisterNatives` (libraries loaded by `NativeActivity` are not searched for `Java_…`
//! symbols; the symbol is exported too, for hosts that `System.loadLibrary` the game), the
//! class is loaded through the context's class loader (`FindClass` on a native thread only
//! sees system classes), strings cross as proper modified UTF-8 (the `jni` crate encodes and
//! decodes surrogate pairs, so emoji survive), and `destroy` is never called while holding a
//! lock `nativeDeliver` takes.

use super::{AiBackend, BackendKind, Inbox};
use crate::error::{AiError, AiResult};
use jni::errors::{Error as JniError, LogErrorAndDefault};
use jni::objects::{JClass, JObject, JString, JValue};
use jni::refs::Global;
use jni::sys::{jlong, jobject};
use jni::vm::JavaVM;
use jni::{Env, jni_sig, jni_str};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};

/// The JNI entry point class in the open-android-models AAR.
pub const OAM_JNI_CLASS: &str = "com.spacecorps.oam.jni.OamJni";

/// Inboxes by native handle. `nativeDeliver` looks the handle up, so a stale handle can never
/// reach freed memory.
static INBOXES: LazyLock<Mutex<HashMap<i64, Inbox>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

/// `OamJni.nativeDeliver(long, String)`: one outgoing JSON-RPC line on a JVM background thread.
const NATIVE_DELIVER: jni::NativeMethod = jni::native_method! {
    java_type = "com.spacecorps.oam.jni.OamJni",
    static fn native_deliver(handle: jlong, line: JString),
    export = "Java_com_spacecorps_oam_jni_OamJni_nativeDeliver",
    error_policy = LogErrorAndDefault,
};

fn native_deliver<'local>(
    env: &mut Env<'local>,
    _class: JClass<'local>,
    handle: jlong,
    line: JString<'local>,
) -> Result<(), JniError> {
    let line = line.try_to_string(env)?;
    // Copy the inbox out so the map lock is not held while delivering.
    let inbox = INBOXES.lock().get(&handle).cloned();
    if let Some(inbox) = inbox {
        inbox.deliver(line);
    }
    Ok(())
}

struct Connection {
    class: Global<JClass<'static>>,
    bridge: i64,
    handle: i64,
}

/// Drives Gemini Nano (AICore, ML Kit GenAI Prompt API) through open-android-models' JNI entry
/// point `OamJni` in a Gradle-built host app (see docs/ai-android.md).
///
/// `OamJni.send` only queues the line; the Kotlin bridge runs on its own coroutines and
/// delivers every outgoing line through `nativeDeliver` on a JVM background thread, which
/// copies it into the engine's [`Inbox`].
pub struct AndroidBackend {
    vm: JavaVM,
    context: Global<JObject<'static>>,
    connection: Mutex<Option<Connection>>,
}

fn jni_error(what: &str, error: JniError) -> AiError {
    AiError::transport(format!("{what}: {error}"))
}

impl AndroidBackend {
    /// Uses the process's Android context, as published by `android-activity` (Space3d's
    /// `NativeActivity` runner) through `ndk-context`.
    ///
    /// Must run after `android_main` started (for example in `AiPlugin::build`).
    pub fn from_android_context() -> AiResult<Self> {
        let context = ndk_context::android_context();
        // SAFETY: ndk-context holds the process's JavaVM and a global reference to the
        // Application for the lifetime of the process.
        unsafe { Self::from_raw(context.vm(), context.context()) }
    }

    /// Uses an explicit JavaVM and `android.content.Context` (an Activity or the Application),
    /// e.g. `AndroidBackend::from_raw(app.vm_as_ptr(), app.activity_as_ptr())` with an
    /// `android_activity::AndroidApp`.
    ///
    /// # Safety
    ///
    /// `vm` must be the process's `JavaVM*` and `context` a valid (global) `jobject` reference to
    /// an `android.content.Context`, both valid for this call. A global reference to the context
    /// is taken, so it may be released afterwards.
    pub unsafe fn from_raw(vm: *mut c_void, context: *mut c_void) -> AiResult<Self> {
        if vm.is_null() || context.is_null() {
            return Err(AiError::unsupported(
                "AndroidBackend needs a JavaVM and an Android context.",
            ));
        }
        // SAFETY: the caller guarantees `vm` is the process's JavaVM.
        let vm = unsafe { JavaVM::from_raw(vm.cast()) };
        let context = vm
            .attach_current_thread(|env| -> Result<_, JniError> {
                // SAFETY: the caller guarantees `context` is a valid reference.
                let object = unsafe { JObject::from_raw(env, context as jobject) };
                env.new_global_ref(&object)
            })
            .map_err(|e| jni_error("Could not reference the Android context", e))?;
        Ok(Self {
            vm,
            context,
            connection: Mutex::new(None),
        })
    }

    fn connect(&self, env: &mut Env<'_>, handle: i64) -> Result<Connection, JniError> {
        // The app's class loader: FindClass on a native thread would only see system classes.
        let loader = env
            .call_method(
                &*self.context,
                jni_str!("getClassLoader"),
                jni_sig!("()Ljava/lang/ClassLoader;"),
                &[],
            )?
            .l()?;
        let name = JString::from_str(env, OAM_JNI_CLASS)?;
        let class = env
            .call_method(
                &loader,
                jni_str!("loadClass"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
                &[JValue::Object(&name)],
            )?
            .l()?;
        let class: JClass = env.cast_local::<JClass>(class)?;
        let class = env.new_global_ref(&class)?;
        // SAFETY: NATIVE_DELIVER is generated by `native_method!` from the Java signature
        // `static native void nativeDeliver(long, String)`, which it type-checks.
        unsafe { env.register_native_methods(&class, &[NATIVE_DELIVER])? };
        let bridge = env
            .call_static_method(
                &class,
                jni_str!("create"),
                jni_sig!("(Landroid/content/Context;J)J"),
                &[JValue::Object(&self.context), JValue::Long(handle)],
            )?
            .j()?;
        Ok(Connection { class, bridge, handle })
    }
}

impl AiBackend for AndroidBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Android
    }

    fn start(&self, inbox: Inbox) -> AiResult<()> {
        let mut connection = self.connection.lock();
        if connection.is_some() {
            return Err(AiError::transport("AndroidBackend is already started."));
        }
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        INBOXES.lock().insert(handle, inbox);
        match self.vm.attach_current_thread(|env| self.connect(env, handle)) {
            Ok(opened) if opened.bridge != 0 => {
                *connection = Some(opened);
                Ok(())
            }
            Ok(_) => {
                INBOXES.lock().remove(&handle);
                Err(AiError::transport("OamJni.create returned 0; see logcat (tag OamJni)."))
            }
            Err(error) => {
                INBOXES.lock().remove(&handle);
                Err(jni_error(
                    "Could not start open-android-models' OamJni (is the AAR in the Gradle host app?)",
                    error,
                ))
            }
        }
    }

    fn send(&self, line: &str) -> AiResult<()> {
        let connection = self.connection.lock();
        let Some(open) = connection.as_ref() else {
            return Err(AiError::transport("AndroidBackend is not running."));
        };
        self.vm
            .attach_current_thread(|env| -> Result<(), JniError> {
                let line = JString::from_str(env, line)?;
                env.call_static_method(
                    &open.class,
                    jni_str!("send"),
                    jni_sig!("(JLjava/lang/String;)V"),
                    &[JValue::Long(open.bridge), JValue::Object(&line)],
                )?;
                Ok(())
            })
            .map_err(|e| jni_error("OamJni.send failed", e))
    }

    fn shutdown(&self) {
        let Some(open) = self.connection.lock().take() else {
            return;
        };
        // `destroy` waits for an in-flight `nativeDeliver`, which takes only the INBOXES lock.
        let destroyed = self.vm.attach_current_thread(|env| -> Result<(), JniError> {
            env.call_static_method(
                &open.class,
                jni_str!("destroy"),
                jni_sig!("(J)V"),
                &[JValue::Long(open.bridge)],
            )?;
            Ok(())
        });
        if let Err(error) = destroyed {
            tracing::warn!(%error, "space3d-ai: OamJni.destroy failed");
        }
        INBOXES.lock().remove(&open.handle);
    }
}

impl Drop for AndroidBackend {
    fn drop(&mut self) {
        self.shutdown();
    }
}
