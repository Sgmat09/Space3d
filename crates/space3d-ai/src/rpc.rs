//! JSON-RPC 2.0 framing: one JSON object per line, as the bridge protocol v1.0 requires.

use crate::error::{AiError, AiResult};
use serde_json::{Map, Value, json};
use std::fmt;

/// The id of a request `space3d-ai` sent (always a number on the wire). Streaming events and
/// `tool/call` requests carry it as `requestId`, so they can be matched to the request that
/// caused them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

impl RequestId {
    /// Parses a wire id; `None` for ids `space3d-ai` did not produce (strings, fractions, …).
    pub fn from_json(value: &Value) -> Option<Self> {
        value.as_u64().map(Self)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One incoming message, classified.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Message {
    /// A response to one of our requests (or to a request we could not parse: `id` null).
    Response { id: Value, outcome: AiResult<Value> },
    /// A request from the other side (`tool/call`): it must be answered.
    Request { id: Value, method: String, params: Value },
    /// A notification (`session/event`, `npc/event`, `tool/cancel`, `world/changed`, …).
    Notification { method: String, params: Value },
}

/// Parses one line. Errors describe lines that are not JSON-RPC 2.0 messages.
pub(crate) fn parse(line: &str) -> AiResult<Message> {
    let value: Value = serde_json::from_str(line.trim())
        .map_err(|e| AiError::protocol(format!("The backend sent invalid JSON ({e}): {}", clip(line))))?;
    let Value::Object(mut object) = value else {
        return Err(AiError::protocol(format!(
            "The backend sent a non-object message: {}",
            clip(line)
        )));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(AiError::protocol(format!(
            "The backend sent a message without \"jsonrpc\": \"2.0\": {}",
            clip(line)
        )));
    }
    let params = object.remove("params").unwrap_or(Value::Object(Map::new()));
    match (object.remove("method"), object.remove("id")) {
        (Some(Value::String(method)), Some(id)) if !id.is_null() => Ok(Message::Request { id, method, params }),
        (Some(Value::String(method)), _) => Ok(Message::Notification { method, params }),
        (None, Some(id)) => {
            let outcome = match (object.remove("result"), object.remove("error")) {
                (_, Some(error)) if !error.is_null() => Err(AiError::from_rpc(&error)),
                (Some(result), _) => Ok(result),
                _ => Err(AiError::protocol(format!(
                    "A response has neither result nor error: {}",
                    clip(line)
                ))),
            };
            Ok(Message::Response { id, outcome })
        }
        _ => Err(AiError::protocol(format!("Unrecognized message: {}", clip(line)))),
    }
}

/// `{"jsonrpc":"2.0","id":…,"method":…,"params":…}`.
pub(crate) fn request(id: &Value, method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// `{"jsonrpc":"2.0","method":…,"params":…}`.
pub(crate) fn notification(method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// `{"jsonrpc":"2.0","id":…,"result":…}`.
pub(crate) fn result(id: &Value, result: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// `{"jsonrpc":"2.0","id":…,"error":{…}}`.
pub(crate) fn error(id: &Value, error: &AiError) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": error.to_rpc()}).to_string()
}

/// The first 200 characters of a line, for error messages.
pub(crate) fn clip(line: &str) -> String {
    const MAX: usize = 200;
    match line.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AiErrorCode;

    #[test]
    fn classifies_messages() {
        let response = parse(r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#).unwrap();
        assert_eq!(
            response,
            Message::Response {
                id: json!(3),
                outcome: Ok(json!({"ok": true}))
            }
        );

        let failure =
            parse(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32009,"message":"x","data":{"code":"cancelled"}}}"#);
        let Ok(Message::Response {
            outcome: Err(error), ..
        }) = failure
        else {
            panic!("{failure:?}")
        };
        assert_eq!(error.code, AiErrorCode::Cancelled);

        let call = parse(r#"{"jsonrpc":"2.0","id":"t-1","method":"tool/call","params":{"requestId":3}}"#).unwrap();
        assert!(matches!(call, Message::Request { ref method, .. } if method == "tool/call"));

        let event = parse(r#"{"jsonrpc":"2.0","method":"session/event","params":{"session":"a"}}"#).unwrap();
        assert!(matches!(event, Message::Notification { ref method, .. } if method == "session/event"));

        assert!(parse("not json").is_err());
        assert!(parse(r#"{"id":1,"result":{}}"#).is_err());
        assert!(parse(r#"[1,2]"#).is_err());
    }

    #[test]
    fn encodes_messages() {
        let line = request(&json!(7), "ping", &json!({}));
        assert_eq!(
            parse(&line).unwrap(),
            Message::Request {
                id: json!(7),
                method: "ping".into(),
                params: json!({})
            }
        );
        assert!(!line.contains('\n'));
        let err = error(&json!("t-2"), &AiError::cancelled("gone"));
        assert!(err.contains("\"cancelled\""));
        assert_eq!(RequestId::from_json(&json!(12)), Some(RequestId(12)));
        assert_eq!(RequestId::from_json(&json!("t-1")), None);
        assert_eq!(clip(&"é".repeat(300)).chars().count(), 201);
    }
}
