//! Model backends: the transport that carries protocol v1.0 JSON-RPC lines between the engine
//! and a bridge (open-apple-models' C ABI, open-android-models' JNI entry point, or the pure-Rust
//! [`ScriptedBackend`]). The engine only speaks the protocol, so backends are injected per
//! platform ([`BackendChoice`]).

#[cfg(target_os = "android")]
mod android;
mod apple;
pub(crate) mod scripted;

#[cfg(target_os = "android")]
pub use android::AndroidBackend;
pub use apple::AppleBackend;
pub use scripted::{ModelRequest, RequestKind, ScriptedBackend};

use crate::error::{AiError, AiResult};
use crate::rpc;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

/// Which transport a backend is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// The pure-Rust [`ScriptedBackend`].
    Scripted,
    /// open-apple-models over its C ABI ([`AppleBackend`]).
    Apple,
    /// open-android-models over JNI (`AndroidBackend`).
    Android,
    /// No backend on this platform/build ([`UnsupportedBackend`]).
    Unsupported,
    /// A host-provided backend.
    Custom(&'static str),
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Scripted => "scripted",
            Self::Apple => "apple",
            Self::Android => "android",
            Self::Unsupported => "unsupported",
            Self::Custom(name) => name,
        })
    }
}

/// Carries JSON-RPC lines to a bridge and delivers the bridge's lines back.
///
/// Implementations must be thread-safe: [`send`](Self::send) is called from the engine thread
/// and from [`ToolResponder`](crate::ToolResponder)s on any thread, and lines may be delivered
/// to the [`Inbox`] from any thread. The engine drains the inbox once per frame
/// (`Stage::PreUpdate`), so delivery never touches engine state.
///
/// Contract (protocol v1.0 §1–2): one JSON object per line, no trailing newline; lines are
/// delivered in the order the bridge produced them; `send` never blocks on model work.
pub trait AiBackend: Send + Sync + 'static {
    /// Which transport this is.
    fn kind(&self) -> BackendKind;

    /// Connects the transport. Every line the bridge produces from now on must be passed to
    /// `inbox.deliver`. Called once, before any [`send`](Self::send).
    fn start(&self, inbox: Inbox) -> AiResult<()>;

    /// Sends one JSON-RPC message (a request, notification or `tool/call` response).
    fn send(&self, line: &str) -> AiResult<()>;

    /// Called once per frame on the engine thread before the inbox is drained. Backends that
    /// run on the engine's clock (the scripted backend's delays and tool timeouts) advance here.
    fn poll(&self) {}

    /// Closes the transport: cancels the bridge's work; nothing is delivered after it returns.
    /// Must be idempotent.
    fn shutdown(&self) {}
}

#[derive(Default)]
struct InboxShared {
    lines: Mutex<VecDeque<String>>,
}

/// Thread-safe queue of lines from a backend, drained by the engine once per frame.
#[derive(Clone, Default)]
pub struct Inbox(Arc<InboxShared>);

impl Inbox {
    /// An empty inbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one line from the bridge. Cheap and non-blocking (safe to call from a
    /// backend's delivery thread or FFI callback).
    pub fn deliver(&self, line: impl Into<String>) {
        self.0.lines.lock().push_back(line.into());
    }

    /// Takes every queued line, oldest first.
    pub fn drain(&self) -> Vec<String> {
        self.0.lines.lock().drain(..).collect()
    }

    /// Queued lines.
    pub fn len(&self) -> usize {
        self.0.lines.lock().len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for Inbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inbox").field("queued", &self.len()).finish()
    }
}

/// Answers every request with an [`Unsupported`](crate::AiErrorCode::Unsupported) error: the
/// fallback on platforms and builds without a model backend (Linux, Windows, the web,
/// Apple targets without the `apple-ffi` feature, …).
pub struct UnsupportedBackend {
    reason: String,
    inbox: Mutex<Option<Inbox>>,
}

impl UnsupportedBackend {
    /// A backend whose errors explain `reason`.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            inbox: Mutex::new(None),
        }
    }

    /// Why no backend is available.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl AiBackend for UnsupportedBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Unsupported
    }

    fn start(&self, inbox: Inbox) -> AiResult<()> {
        *self.inbox.lock() = Some(inbox);
        Ok(())
    }

    fn send(&self, line: &str) -> AiResult<()> {
        let Some(inbox) = self.inbox.lock().clone() else {
            return Err(AiError::unsupported(self.reason.clone()));
        };
        let error = AiError::unsupported(self.reason.clone());
        match rpc::parse(line) {
            Ok(rpc::Message::Request { id, .. }) => inbox.deliver(rpc::error(&id, &error)),
            Ok(_) => {}
            Err(_) => inbox.deliver(rpc::error(&Value::Null, &error)),
        }
        Ok(())
    }
}

/// How [`AiPlugin`](crate::AiPlugin) picks its backend.
#[derive(Default)]
pub enum BackendChoice {
    /// The platform's backend: [`AppleBackend`] on Apple platforms (with the `apple-ffi`
    /// feature), `AndroidBackend` on Android (from the Android context that
    /// `android-activity` publishes), otherwise [`UnsupportedBackend`].
    #[default]
    Auto,
    /// Like [`Auto`](Self::Auto), but uses this backend where `Auto` would be unsupported (for
    /// example a [`ScriptedBackend`] so the web build still has NPCs).
    AutoOr(Box<dyn AiBackend>),
    /// Exactly this backend (dependency injection: tests, custom transports).
    Backend(Box<dyn AiBackend>),
}

impl BackendChoice {
    /// Injects `backend`.
    pub fn backend(backend: impl AiBackend) -> Self {
        Self::Backend(Box::new(backend))
    }

    /// `Auto`, falling back to `backend`.
    pub fn auto_or(backend: impl AiBackend) -> Self {
        Self::AutoOr(Box::new(backend))
    }

    /// Resolves the choice to a backend.
    pub fn resolve(self) -> Box<dyn AiBackend> {
        match self {
            Self::Backend(backend) => backend,
            Self::Auto => platform_backend().unwrap_or_else(|reason| Box::new(UnsupportedBackend::new(reason))),
            Self::AutoOr(fallback) => platform_backend().unwrap_or_else(|reason| {
                tracing::info!(%reason, fallback = %fallback.kind(), "space3d-ai: using the fallback backend");
                fallback
            }),
        }
    }
}

impl<B: AiBackend> From<B> for BackendChoice {
    fn from(backend: B) -> Self {
        Self::backend(backend)
    }
}

impl fmt::Debug for BackendChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("Auto"),
            Self::AutoOr(b) => write!(f, "AutoOr({})", b.kind()),
            Self::Backend(b) => write!(f, "Backend({})", b.kind()),
        }
    }
}

/// The platform's own backend, or why there is none.
fn platform_backend() -> Result<Box<dyn AiBackend>, String> {
    #[cfg(target_os = "android")]
    {
        AndroidBackend::from_android_context()
            .map(|b| Box::new(b) as Box<dyn AiBackend>)
            .map_err(|e| e.message)
    }
    #[cfg(not(target_os = "android"))]
    {
        if AppleBackend::is_linked() {
            Ok(Box::new(AppleBackend::new()))
        } else if cfg!(target_vendor = "apple") {
            Err(
                "space3d-ai was built without the `apple-ffi` feature, so Apple's on-device model is not \
                 linked (see docs/ai.md)."
                    .into(),
            )
        } else if cfg!(target_arch = "wasm32") {
            Err("There is no on-device model backend on the web; use a ScriptedBackend.".into())
        } else {
            Err("There is no on-device model backend on this platform (Apple platforms and Android only).".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AiErrorCode;

    #[test]
    fn inbox_is_fifo() {
        let inbox = Inbox::new();
        inbox.deliver("a");
        inbox.clone().deliver(String::from("b"));
        assert_eq!(inbox.len(), 2);
        assert_eq!(inbox.drain(), vec!["a", "b"]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn unsupported_backend_answers_requests_with_errors() {
        let backend = UnsupportedBackend::new("no model here");
        let inbox = Inbox::new();
        backend.start(inbox.clone()).unwrap();
        backend
            .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap();
        backend
            .send(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#)
            .unwrap();
        let lines = inbox.drain();
        assert_eq!(lines.len(), 1);
        let rpc::Message::Response {
            id,
            outcome: Err(error),
        } = rpc::parse(&lines[0]).unwrap()
        else {
            panic!("{lines:?}")
        };
        assert_eq!(id, 1);
        assert_eq!(error.code, AiErrorCode::Unsupported);
        assert_eq!(error.message, "no model here");
    }

    #[test]
    fn choices_resolve() {
        assert_eq!(
            BackendChoice::backend(ScriptedBackend::new()).resolve().kind(),
            BackendKind::Scripted
        );
        #[cfg(not(any(target_os = "android", target_vendor = "apple")))]
        {
            assert_eq!(BackendChoice::Auto.resolve().kind(), BackendKind::Unsupported);
            let fallback = BackendChoice::auto_or(ScriptedBackend::new()).resolve();
            assert_eq!(fallback.kind(), BackendKind::Scripted);
        }
        assert_eq!(
            format!("{:?}", BackendChoice::from(ScriptedBackend::new())),
            "Backend(scripted)"
        );
    }
}
