//! Errors: the bridge protocol's error table (`error.data.code`) plus local failures.

use serde_json::{Map, Value, json};
use std::fmt;

/// Stable error codes. The protocol codes mirror the error table of the bridge protocol v1.0
/// (open-apple-models `docs/PROTOCOL.md` §8, identical on Android); prefer them over the
/// numeric JSON-RPC code. [`Unsupported`](Self::Unsupported), [`Transport`](Self::Transport)
/// and [`Protocol`](Self::Protocol) are raised locally by `space3d-ai`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AiErrorCode {
    /// `-32700`: a line was not valid JSON.
    ParseError,
    /// `-32600`: not a JSON-RPC 2.0 message, or a bad id.
    InvalidMessage,
    /// `-32601`: unknown method (`data.method`).
    MethodNotFound,
    /// `-32602`: a missing or mistyped parameter.
    InvalidParams,
    /// `-32603`: a bug in the bridge.
    InternalError,
    /// `-32001`: the on-device model is unavailable (device not eligible, AI disabled, downloading).
    ModelUnavailable,
    /// `-32002`: input or output tripped the safety guardrails.
    GuardrailViolation,
    /// `-32003`: the model refused.
    Refusal,
    /// `-32004`: the conversation no longer fits the context window.
    ContextSizeExceeded,
    /// `-32005`: the system rate-limited the request (typically in the background).
    RateLimited,
    /// `-32006`: the prompt's language is not supported.
    UnsupportedLanguage,
    /// `-32007`: a schema or tool definition cannot be used.
    InvalidSchema,
    /// `-32008`: a local tool aborted the turn.
    ToolFailed,
    /// `-32009`: the turn was cancelled.
    Cancelled,
    /// `-32010`: the model session was busy.
    Busy,
    /// `-32011`: the agent rejected the input.
    InvalidRequest,
    /// `-32012`: any other model failure.
    GenerationFailed,
    /// `-32020`: unknown session (`data.session`).
    SessionNotFound,
    /// `-32021`: the session id is in use.
    SessionExists,
    /// `-32022`: too many sessions (`data.limit`).
    SessionLimit,
    /// `-32023`: the bridge was shut down.
    ShutDown,
    /// `-32024`: a blocking call timed out.
    Timeout,
    /// `-32050`: unknown NPC (`data.npc`).
    NpcNotFound,
    /// `-32051`: the NPC id is in use.
    NpcExists,
    /// `-32052`: unknown world (`data.world`).
    WorldNotFound,
    /// `-32053`: the world id is in use.
    WorldExists,
    /// `-32054`: a world path or value was rejected.
    WorldError,
    /// `-32055`: too many NPCs, worlds or subscriptions (`data.limit`).
    LimitReached,
    /// `-32056`: unknown subscription.
    SubscriptionNotFound,
    /// Local: no model backend for this platform or build (see `BackendChoice`).
    Unsupported,
    /// Local: the backend could not carry a message.
    Transport,
    /// Local: a message from the backend did not have the expected shape.
    Protocol,
    /// Any other `data.code` (extensions use -32057…-32099).
    Other(String),
}

/// `(code, data.code, JSON-RPC code)` for every named variant.
const TABLE: &[(AiErrorCode, &str, i64)] = &[
    (AiErrorCode::ParseError, "parse_error", -32700),
    (AiErrorCode::InvalidMessage, "invalid_message", -32600),
    (AiErrorCode::MethodNotFound, "method_not_found", -32601),
    (AiErrorCode::InvalidParams, "invalid_params", -32602),
    (AiErrorCode::InternalError, "internal_error", -32603),
    (AiErrorCode::ModelUnavailable, "model_unavailable", -32001),
    (AiErrorCode::GuardrailViolation, "guardrail_violation", -32002),
    (AiErrorCode::Refusal, "refusal", -32003),
    (AiErrorCode::ContextSizeExceeded, "context_size_exceeded", -32004),
    (AiErrorCode::RateLimited, "rate_limited", -32005),
    (AiErrorCode::UnsupportedLanguage, "unsupported_language", -32006),
    (AiErrorCode::InvalidSchema, "invalid_schema", -32007),
    (AiErrorCode::ToolFailed, "tool_failed", -32008),
    (AiErrorCode::Cancelled, "cancelled", -32009),
    (AiErrorCode::Busy, "busy", -32010),
    (AiErrorCode::InvalidRequest, "invalid_request", -32011),
    (AiErrorCode::GenerationFailed, "generation_failed", -32012),
    (AiErrorCode::SessionNotFound, "session_not_found", -32020),
    (AiErrorCode::SessionExists, "session_exists", -32021),
    (AiErrorCode::SessionLimit, "session_limit", -32022),
    (AiErrorCode::ShutDown, "shut_down", -32023),
    (AiErrorCode::Timeout, "timeout", -32024),
    (AiErrorCode::NpcNotFound, "npc_not_found", -32050),
    (AiErrorCode::NpcExists, "npc_exists", -32051),
    (AiErrorCode::WorldNotFound, "world_not_found", -32052),
    (AiErrorCode::WorldExists, "world_exists", -32053),
    (AiErrorCode::WorldError, "world_error", -32054),
    (AiErrorCode::LimitReached, "limit_reached", -32055),
    (AiErrorCode::SubscriptionNotFound, "subscription_not_found", -32056),
    (AiErrorCode::Unsupported, "unsupported", -32000),
    (AiErrorCode::Transport, "transport", -32000),
    (AiErrorCode::Protocol, "protocol", -32000),
];

/// The agent error codes a scripted model step may fail with (`{"error": "<code>"}`).
pub(crate) const AGENT_CODES: &[AiErrorCode] = &[
    AiErrorCode::ModelUnavailable,
    AiErrorCode::GuardrailViolation,
    AiErrorCode::Refusal,
    AiErrorCode::ContextSizeExceeded,
    AiErrorCode::RateLimited,
    AiErrorCode::UnsupportedLanguage,
    AiErrorCode::InvalidSchema,
    AiErrorCode::ToolFailed,
    AiErrorCode::Cancelled,
    AiErrorCode::Busy,
    AiErrorCode::InvalidRequest,
    AiErrorCode::GenerationFailed,
];

impl AiErrorCode {
    /// The stable string (`error.data.code`), e.g. `"guardrail_violation"`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Other(name) => name,
            code => TABLE
                .iter()
                .find(|(c, ..)| c == code)
                .map_or("unknown", |(_, name, _)| name),
        }
    }

    /// Parses a `data.code` string; unknown names become [`Other`](Self::Other).
    pub fn from_name(name: &str) -> Self {
        TABLE
            .iter()
            .find(|(_, n, _)| *n == name)
            .map_or_else(|| Self::Other(name.to_owned()), |(code, ..)| code.clone())
    }

    /// Maps a numeric JSON-RPC code (used when a response carries no `data.code`).
    pub fn from_rpc_code(code: i64) -> Self {
        TABLE
            .iter()
            .find(|(c, _, n)| *n == code && !matches!(c, Self::Unsupported | Self::Transport | Self::Protocol))
            .map_or_else(|| Self::Other(format!("rpc_{code}")), |(c, ..)| c.clone())
    }

    /// The JSON-RPC error code sent on the wire (`-32000` for local and unknown codes).
    pub fn rpc_code(&self) -> i64 {
        TABLE.iter().find(|(c, ..)| c == self).map_or(-32000, |(.., n)| *n)
    }

    /// Whether this is one of the agent error codes (the codes a model turn can fail with).
    pub fn is_agent_error(&self) -> bool {
        AGENT_CODES.contains(self)
    }
}

impl fmt::Display for AiErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error from a request: a JSON-RPC error response from the bridge, or a local failure.
///
/// Branch on [`code`](Self::code). `data` holds the error's extra fields (e.g. `session`,
/// `retryAfterSeconds`, `path`).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AiError {
    /// The stable code.
    pub code: AiErrorCode,
    /// A human-readable message (wording may differ between platforms).
    pub message: String,
    /// The JSON-RPC numeric code as received (or [`AiErrorCode::rpc_code`] for local errors).
    pub rpc_code: i64,
    /// `error.data` without its `code` member (`None` when empty).
    pub data: Option<Map<String, Value>>,
}

/// `Result<T, AiError>`.
pub type AiResult<T> = Result<T, AiError>;

impl AiError {
    /// An error with `code` and `message`.
    pub fn new(code: AiErrorCode, message: impl Into<String>) -> Self {
        Self {
            rpc_code: code.rpc_code(),
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Adds one `data` field.
    pub fn with_data(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.data.get_or_insert_with(Map::new).insert(key.into(), value.into());
        self
    }

    /// `-32009 cancelled`.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(AiErrorCode::Cancelled, message)
    }

    /// `-32602 invalid_params`.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(AiErrorCode::InvalidParams, message)
    }

    /// Local: no backend for this platform/build.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(AiErrorCode::Unsupported, message)
    }

    /// Local: a malformed message from the backend.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(AiErrorCode::Protocol, message)
    }

    /// Local: the transport failed.
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(AiErrorCode::Transport, message)
    }

    /// Whether the model cannot be used right now or on this platform
    /// ([`ModelUnavailable`](AiErrorCode::ModelUnavailable) or [`Unsupported`](AiErrorCode::Unsupported)):
    /// show scripted content instead.
    pub fn is_unavailable(&self) -> bool {
        matches!(self.code, AiErrorCode::ModelUnavailable | AiErrorCode::Unsupported)
    }

    /// Whether the turn was blocked by the guardrails or refused (use a fallback line).
    pub fn is_blocked(&self) -> bool {
        matches!(self.code, AiErrorCode::GuardrailViolation | AiErrorCode::Refusal)
    }

    /// `data.retryAfterSeconds` of a `rate_limited` error, when the platform gave one.
    pub fn retry_after_seconds(&self) -> Option<f64> {
        self.data.as_ref()?.get("retryAfterSeconds")?.as_f64()
    }

    /// Parses a JSON-RPC `error` object. `data.code` wins over the numeric code.
    pub fn from_rpc(error: &Value) -> Self {
        let rpc_code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Unknown error.")
            .to_owned();
        let mut data = error.get("data").and_then(Value::as_object).cloned();
        let name = data
            .as_mut()
            .and_then(|d| d.remove("code"))
            .and_then(|c| c.as_str().map(str::to_owned));
        let code = match name {
            Some(name) => AiErrorCode::from_name(&name),
            None => AiErrorCode::from_rpc_code(rpc_code),
        };
        Self {
            code,
            message,
            rpc_code,
            data: data.filter(|d| !d.is_empty()),
        }
    }

    /// The JSON-RPC `error` object: `{"code", "message", "data": {"code", …}}`.
    pub fn to_rpc(&self) -> Value {
        let mut data = self.data.clone().unwrap_or_default();
        data.insert("code".into(), Value::String(self.code.as_str().to_owned()));
        json!({"code": self.rpc_code, "message": self.message, "data": data})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for (code, name, number) in TABLE {
            assert_eq!(code.as_str(), *name);
            assert_eq!(AiErrorCode::from_name(name), *code);
            assert_eq!(code.rpc_code(), *number);
        }
        assert_eq!(AiErrorCode::from_rpc_code(-32009), AiErrorCode::Cancelled);
        assert_eq!(
            AiErrorCode::from_rpc_code(-32000),
            AiErrorCode::Other("rpc_-32000".into())
        );
        assert_eq!(AiErrorCode::from_name("quest_not_found").as_str(), "quest_not_found");
        assert!(AiErrorCode::GuardrailViolation.is_agent_error());
        assert!(!AiErrorCode::SessionNotFound.is_agent_error());
    }

    #[test]
    fn rpc_errors_parse_and_encode() {
        let wire = json!({"code": -32005, "message": "Slow down.", "data": {"code": "rate_limited", "retryAfterSeconds": 2.5}});
        let error = AiError::from_rpc(&wire);
        assert_eq!(error.code, AiErrorCode::RateLimited);
        assert_eq!(error.retry_after_seconds(), Some(2.5));
        assert_eq!(error.to_string(), "rate_limited: Slow down.");
        assert_eq!(error.to_rpc(), wire);

        let bare = AiError::from_rpc(&json!({"code": -32601, "message": "No such method."}));
        assert_eq!(bare.code, AiErrorCode::MethodNotFound);
        assert_eq!(bare.data, None);
        assert!(AiError::unsupported("x").is_unavailable());
        assert!(AiError::new(AiErrorCode::Refusal, "no").is_blocked());
    }
}
