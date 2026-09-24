//! The single error type shared by every API in this crate.

/// Errors produced by the HTTP/WebSocket clients, servers and codecs.
///
/// All variants carry owned, cloneable data so errors can travel through
/// [`space3d_core::Events`] and be stored in resources.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetError {
    /// The URL could not be parsed or resolved against a base URL.
    #[error("invalid url `{0}`")]
    InvalidUrl(String),
    /// The request or connection attempt did not finish within its timeout.
    #[error("timed out")]
    Timeout,
    /// A connection could not be established (DNS, refused, TLS handshake, ...).
    #[error("connection failed: {0}")]
    Connect(String),
    /// The server answered with a non-success status (see [`crate::HttpResponse::error_for_status`]).
    #[error("http status {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body (lossy UTF-8, truncated to 512 bytes).
        body: String,
    },
    /// Transport failure after the connection was established.
    #[error("transport error: {0}")]
    Transport(String),
    /// A value could not be serialized.
    #[error("encode error: {0}")]
    Encode(String),
    /// A payload could not be deserialized.
    #[error("decode error: {0}")]
    Decode(String),
    /// The connection (or server) is closed.
    #[error("connection closed")]
    Closed,
    /// No connection / client with this id exists.
    #[error("unknown peer {0}")]
    UnknownPeer(u64),
    /// Local I/O failure, e.g. binding a port.
    #[error("io error: {0}")]
    Io(String),
    /// The operation is not available on this platform.
    #[error("unsupported on this platform: {0}")]
    Unsupported(&'static str),
}

impl NetError {
    /// Whether retrying the same operation might succeed (timeouts, connection and transport errors).
    pub fn is_retryable(&self) -> bool {
        matches!(self, NetError::Timeout | NetError::Connect(_) | NetError::Transport(_))
    }
}

impl From<std::io::Error> for NetError {
    fn from(e: std::io::Error) -> Self {
        NetError::Io(e.to_string())
    }
}

/// Render an error together with its `source()` chain (reqwest/tungstenite hide the useful bit there).
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let msg = s.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        cur = s.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(NetError::Timeout.is_retryable());
        assert!(NetError::Connect("refused".into()).is_retryable());
        assert!(!NetError::Decode("bad".into()).is_retryable());
        assert!(
            !NetError::Status {
                status: 500,
                body: String::new()
            }
            .is_retryable()
        );
    }

    #[test]
    fn chain_includes_sources() {
        let io = std::io::Error::other("inner cause");
        let outer = std::io::Error::other(io);
        assert!(error_chain(&outer).contains("inner cause"));
    }
}
