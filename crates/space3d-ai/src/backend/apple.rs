//! [`AppleBackend`]: open-apple-models' C ABI (`bindings/c/open_apple_models.h`).
//!
//! Linked only with the `apple-ffi` feature on Apple targets (see `build.rs` and docs/ai.md);
//! otherwise every method reports [`Unsupported`](crate::AiErrorCode::Unsupported), so the
//! type exists (and code using it compiles) on every platform.

use super::{AiBackend, BackendKind, Inbox};
use crate::error::{AiError, AiResult};

/// Drives Apple's on-device Foundation Models through `libOpenAppleModelsFFI`
/// (`oam_bridge_create` / `oam_bridge_send` / `oam_bridge_destroy`).
///
/// The bridge runs its own threads: [`send`](AiBackend::send) only parses and queues the line,
/// and the bridge's callback copies each outgoing line into the engine's [`Inbox`]. Requires
/// macOS / iOS / visionOS 27 with Apple Intelligence for the system model; scripted models
/// work on any machine that can load the library.
pub struct AppleBackend {
    #[cfg(oam_apple_linked)]
    bridge: parking_lot::Mutex<Option<ffi::Bridge>>,
}

impl Default for AppleBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleBackend {
    /// A backend that connects in [`start`](AiBackend::start).
    pub fn new() -> Self {
        Self {
            #[cfg(oam_apple_linked)]
            bridge: parking_lot::Mutex::new(None),
        }
    }

    /// Whether this build links open-apple-models (`apple-ffi` feature on an Apple target).
    pub const fn is_linked() -> bool {
        cfg!(oam_apple_linked)
    }

    /// The linked library's version (`oam_version()`), if linked.
    pub fn library_version() -> Option<String> {
        #[cfg(oam_apple_linked)]
        {
            Some(ffi::version())
        }
        #[cfg(not(oam_apple_linked))]
        {
            None
        }
    }

    #[cfg(not(oam_apple_linked))]
    fn not_linked() -> AiError {
        AiError::unsupported(
            "AppleBackend is not linked: build space3d-ai with the `apple-ffi` feature on an Apple target, \
             with OAM_APPLE_LIB_DIR set to the folder of libOpenAppleModelsFFI.dylib on macOS (see docs/ai.md).",
        )
    }
}

impl AiBackend for AppleBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Apple
    }

    fn start(&self, inbox: Inbox) -> AiResult<()> {
        #[cfg(oam_apple_linked)]
        {
            let mut bridge = self.bridge.lock();
            if bridge.is_some() {
                return Err(AiError::transport("AppleBackend is already started."));
            }
            *bridge = Some(ffi::Bridge::create(inbox)?);
            Ok(())
        }
        #[cfg(not(oam_apple_linked))]
        {
            let _ = inbox;
            Err(Self::not_linked())
        }
    }

    fn send(&self, line: &str) -> AiResult<()> {
        #[cfg(oam_apple_linked)]
        {
            // Holding the lock keeps `shutdown` from destroying the bridge mid-send (UB per the header).
            match self.bridge.lock().as_ref() {
                Some(bridge) => bridge.send(line),
                None => Err(AiError::transport("AppleBackend is not running.")),
            }
        }
        #[cfg(not(oam_apple_linked))]
        {
            let _ = line;
            Err(Self::not_linked())
        }
    }

    fn shutdown(&self) {
        #[cfg(oam_apple_linked)]
        {
            // Dropping destroys the bridge (waits for an in-flight callback).
            let bridge = self.bridge.lock().take();
            drop(bridge);
        }
    }
}

impl Drop for AppleBackend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(oam_apple_linked)]
mod ffi {
    //! Raw bindings and an owning wrapper. Memory rules from the header: strings passed in are
    //! borrowed for the call; the callback's line is valid only during the callback;
    //! `user_data` must outlive `oam_bridge_destroy`.

    use super::super::Inbox;
    use crate::error::{AiError, AiResult};
    use std::ffi::{CStr, CString, c_char, c_int, c_void};

    #[repr(C)]
    struct OamBridge {
        _opaque: [u8; 0],
    }

    type Callback = unsafe extern "C" fn(json_line: *const c_char, user_data: *mut c_void);

    unsafe extern "C" {
        fn oam_bridge_create(callback: Option<Callback>, user_data: *mut c_void) -> *mut OamBridge;
        fn oam_bridge_send(bridge: *mut OamBridge, json_line: *const c_char) -> c_int;
        fn oam_bridge_destroy(bridge: *mut OamBridge);
        fn oam_version() -> *const c_char;
    }

    const OAM_ERROR_INVALID_ARGUMENT: c_int = -1;
    const OAM_ERROR_INVALID_UTF8: c_int = -2;

    /// The library version string.
    pub(super) fn version() -> String {
        // SAFETY: `oam_version` returns a static NUL-terminated string.
        unsafe { CStr::from_ptr(oam_version()) }.to_string_lossy().into_owned()
    }

    /// Copies each outgoing line into the inbox. Runs on the bridge's delivery thread, never
    /// concurrently with itself.
    unsafe extern "C" fn deliver(json_line: *const c_char, user_data: *mut c_void) {
        if json_line.is_null() || user_data.is_null() {
            return;
        }
        // SAFETY: `user_data` is the `Box<Inbox>` owned by `Bridge`, alive until destroy returns;
        // `json_line` is a valid C string for the duration of the callback.
        let (inbox, line) = unsafe { (&*user_data.cast::<Inbox>(), CStr::from_ptr(json_line)) };
        let line = line.to_string_lossy().into_owned();
        // Never unwind into Swift.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inbox.deliver(line)));
    }

    /// An open bridge and the inbox its callback writes to.
    pub(super) struct Bridge {
        raw: *mut OamBridge,
        inbox: *mut Inbox,
    }

    // SAFETY: the C ABI may be called from any thread; `Bridge` is only used behind a mutex that
    // serializes `send` against destruction.
    unsafe impl Send for Bridge {}

    impl Bridge {
        pub(super) fn create(inbox: Inbox) -> AiResult<Self> {
            let inbox = Box::into_raw(Box::new(inbox));
            // SAFETY: `deliver` matches `oam_message_callback`; `inbox` stays valid until `Drop`.
            let raw = unsafe { oam_bridge_create(Some(deliver), inbox.cast()) };
            if raw.is_null() {
                // SAFETY: the bridge was not created, so nothing else references `inbox`.
                drop(unsafe { Box::from_raw(inbox) });
                return Err(AiError::transport("oam_bridge_create failed."));
            }
            Ok(Self { raw, inbox })
        }

        pub(super) fn send(&self, line: &str) -> AiResult<()> {
            let line = CString::new(line).map_err(|_| AiError::transport("A JSON-RPC line contained a NUL byte."))?;
            // SAFETY: `raw` is a live bridge (destroyed only in `Drop`); `line` outlives the call.
            match unsafe { oam_bridge_send(self.raw, line.as_ptr()) } {
                0 => Ok(()),
                OAM_ERROR_INVALID_ARGUMENT => Err(AiError::transport("oam_bridge_send: invalid argument.")),
                OAM_ERROR_INVALID_UTF8 => Err(AiError::transport("oam_bridge_send: the line is not valid UTF-8.")),
                code => Err(AiError::transport(format!("oam_bridge_send failed with code {code}."))),
            }
        }
    }

    impl Drop for Bridge {
        fn drop(&mut self) {
            // SAFETY: destroy cancels all work and waits for an in-flight callback; afterwards the
            // callback never runs again, so the inbox box can be freed.
            unsafe {
                oam_bridge_destroy(self.raw);
                drop(Box::from_raw(self.inbox));
            }
        }
    }
}

#[cfg(all(test, not(oam_apple_linked)))]
mod tests {
    use super::*;
    use crate::error::AiErrorCode;

    #[test]
    fn unlinked_backend_reports_unsupported() {
        let backend = AppleBackend::new();
        assert!(!AppleBackend::is_linked());
        assert_eq!(AppleBackend::library_version(), None);
        assert_eq!(backend.start(Inbox::new()).unwrap_err().code, AiErrorCode::Unsupported);
        assert_eq!(backend.send("{}").unwrap_err().code, AiErrorCode::Unsupported);
        backend.shutdown();
        assert_eq!(backend.kind(), BackendKind::Apple);
    }
}
