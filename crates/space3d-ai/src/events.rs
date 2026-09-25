//! Engine events: streaming notifications from the bridge, as [`AiEvent`]s in
//! `Events<AiEvent>` (sent in `Stage::PreUpdate`, readable for two frames).
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_ai::{AiEvent, NpcEvent};
//! fn show_lines(ctx: &mut SystemCtx, reader: &mut EventReader, shown: &mut String) {
//!     for event in reader.read(ctx.resources.expect::<Events<AiEvent>>()) {
//!         match event {
//!             AiEvent::Npc { event: NpcEvent::LineDelta(delta), .. } => shown.push_str(delta),
//!             AiEvent::Npc { event: NpcEvent::LineReset(line), .. } => *shown = line.clone(),
//!             _ => {}
//!         }
//!     }
//! }
//! ```

use crate::error::AiError;
use crate::rpc::RequestId;
use crate::tools::ToolOwner;
use crate::types::{Emotion, ModelStep, ServerInfo, ToolCall, ToolExecution, ToolRecord};
use serde::Deserialize;
use serde_json::Value;

/// An event of a streaming `session/respond` turn (`session/event`).
#[derive(Clone, Debug, PartialEq)]
pub enum TurnEvent {
    /// The model is about to run an inference step.
    ModelStep(ModelStep),
    /// New text: `delta` to append, `text` the whole answer so far. With `is_reset` the model
    /// rewrote earlier text and `delta` is the whole new text.
    Text {
        /// New text.
        delta: String,
        /// The answer so far.
        text: String,
        /// Whether earlier text was replaced.
        is_reset: bool,
    },
    /// Partially generated structured output (schema turns).
    Partial(Value),
    /// A tool call began. Client tools run when their `tool/call` arrives, not on this event.
    ToolCallStarted {
        /// The call.
        call: ToolCall,
        /// Where it runs.
        execution: ToolExecution,
    },
    /// A tool finished.
    ToolCallCompleted(ToolRecord),
    /// An event type this version does not know.
    Other(Value),
}

/// An event of a streaming `npc/talk` turn (`npc/event`).
#[derive(Clone, Debug, PartialEq)]
pub enum NpcEvent {
    /// The NPC's emotion, sent early so a portrait can react; a later one replaces it.
    Emotion(Emotion),
    /// Text to append to the displayed line (typewriter effect).
    LineDelta(String),
    /// Replace the displayed line (rewritten, or a fallback line replaced a blocked one).
    LineReset(String),
    /// A tool call began (`Local`: built-in world and memory tools).
    ToolCallStarted {
        /// The call.
        call: ToolCall,
        /// Where it runs.
        execution: ToolExecution,
    },
    /// A tool finished.
    ToolCallCompleted(ToolRecord),
    /// An event type this version does not know.
    Other(Value),
}

/// A `world/changed` notification.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorldChange {
    /// The world id.
    pub world: String,
    /// The subscription that matched.
    pub subscription: String,
    /// The changed dot path.
    pub path: String,
    /// The previous value (`None` when the value was created).
    pub old_value: Option<Value>,
    /// The new value (`None` when the value was removed).
    pub new_value: Option<Value>,
}

/// Everything the AI layer tells the game, as `Events<AiEvent>`.
#[derive(Clone, Debug, PartialEq)]
pub enum AiEvent {
    /// The bridge answered `initialize` (sent once at startup).
    Ready(Box<ServerInfo>),
    /// `initialize` failed: the backend is unsupported or broken; every request will fail.
    Unavailable(AiError),
    /// A streaming session turn's event.
    Session {
        /// The session id.
        session: String,
        /// The `session/respond` request.
        request: Option<RequestId>,
        /// The event.
        event: TurnEvent,
    },
    /// A streaming NPC turn's event.
    Npc {
        /// The NPC id.
        npc: String,
        /// The `npc/talk` request.
        request: Option<RequestId>,
        /// The event.
        event: NpcEvent,
    },
    /// The bridge no longer needs a tool call's output (timeout, cancelled or finished turn).
    ToolCancelled {
        /// The `tool/call` request id (`"t-3"`).
        rpc_id: String,
        /// The model's call id.
        call_id: String,
        /// Who had asked.
        owner: ToolOwner,
        /// The request (turn) that caused the call.
        request: Option<RequestId>,
        /// Why (`Error: Tool 'x' timed out after 120s.`, …).
        reason: String,
    },
    /// A watched world value changed.
    WorldChanged(WorldChange),
    /// A notification this version does not model (extension events such as `quest/event`).
    Notification {
        /// The method.
        method: String,
        /// Its params.
        params: Value,
    },
    /// A line from the backend could not be understood (logged too).
    BackendError(AiError),
}

pub(crate) fn owner_of(params: &Value) -> ToolOwner {
    if let Some(session) = params.get("session").and_then(Value::as_str) {
        ToolOwner::Session(session.to_owned())
    } else if let Some(npc) = params.get("npc").and_then(Value::as_str) {
        ToolOwner::Npc(npc.to_owned())
    } else {
        ToolOwner::Request
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Option<&Value>) -> Option<T> {
    serde_json::from_value(value?.clone()).ok()
}

fn execution(event: &Value) -> ToolExecution {
    decode(event.get("execution")).unwrap_or_default()
}

pub(crate) fn turn_event(event: &Value) -> TurnEvent {
    let parsed = match event.get("type").and_then(Value::as_str) {
        Some("modelStep") => decode(event.get("step")).map(TurnEvent::ModelStep),
        Some("text") => Some(TurnEvent::Text {
            delta: event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            text: event.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
            is_reset: event.get("isReset").and_then(Value::as_bool).unwrap_or(false),
        }),
        Some("partial") => Some(TurnEvent::Partial(event.get("value").cloned().unwrap_or(Value::Null))),
        Some("toolCallStarted") => decode(event.get("call")).map(|call| TurnEvent::ToolCallStarted {
            call,
            execution: execution(event),
        }),
        Some("toolCallCompleted") => decode(event.get("record")).map(TurnEvent::ToolCallCompleted),
        _ => None,
    };
    parsed.unwrap_or_else(|| TurnEvent::Other(event.clone()))
}

pub(crate) fn npc_event(event: &Value) -> NpcEvent {
    let parsed = match event.get("type").and_then(Value::as_str) {
        Some("emotion") => decode(event.get("emotion")).map(NpcEvent::Emotion),
        Some("lineDelta") => event
            .get("delta")
            .and_then(Value::as_str)
            .map(|d| NpcEvent::LineDelta(d.to_owned())),
        Some("lineReset") => event
            .get("line")
            .and_then(Value::as_str)
            .map(|l| NpcEvent::LineReset(l.to_owned())),
        Some("toolCallStarted") => decode(event.get("call")).map(|call| NpcEvent::ToolCallStarted {
            call,
            execution: execution(event),
        }),
        Some("toolCallCompleted") => decode(event.get("record")).map(NpcEvent::ToolCallCompleted),
        _ => None,
    };
    parsed.unwrap_or_else(|| NpcEvent::Other(event.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolOutput;
    use serde_json::json;

    #[test]
    fn decodes_turn_events() {
        let step = turn_event(
            &json!({"type": "modelStep", "step": {"index": 1, "completedToolRounds": 1,
            "toolCallingMode": "allowed", "enabledTools": ["a"], "trimmedEntries": 0, "kind": "decide"}}),
        );
        let TurnEvent::ModelStep(step) = step else { panic!() };
        assert_eq!(step.kind.as_deref(), Some("decide"));
        assert_eq!(
            turn_event(&json!({"type": "text", "delta": " gate", "text": "The gate", "isReset": false})),
            TurnEvent::Text {
                delta: " gate".into(),
                text: "The gate".into(),
                is_reset: false
            }
        );
        let started = turn_event(&json!({"type": "toolCallStarted", "execution": "client",
            "call": {"id": "c1", "name": "open_gate", "arguments": {"gate": "north"}}}));
        assert!(matches!(
            started,
            TurnEvent::ToolCallStarted {
                execution: ToolExecution::Client,
                ..
            }
        ));
        let done = turn_event(
            &json!({"type": "toolCallCompleted", "record": {"call": {"id": "c1", "name": "x"},
            "output": {"opened": true}, "isError": false, "durationSeconds": 0.1}}),
        );
        let TurnEvent::ToolCallCompleted(record) = done else {
            panic!()
        };
        assert_eq!(record.output, ToolOutput::Json(json!({"opened": true})));
        assert!(matches!(turn_event(&json!({"type": "future"})), TurnEvent::Other(_)));
    }

    #[test]
    fn decodes_npc_events() {
        assert_eq!(
            npc_event(&json!({"type": "emotion", "emotion": "amused"})),
            NpcEvent::Emotion(Emotion::Amused)
        );
        assert_eq!(
            npc_event(&json!({"type": "lineDelta", "delta": "Aye"})),
            NpcEvent::LineDelta("Aye".into())
        );
        assert_eq!(
            npc_event(&json!({"type": "lineReset", "line": "No."})),
            NpcEvent::LineReset("No.".into())
        );
        assert!(matches!(npc_event(&json!({"type": "lineDelta"})), NpcEvent::Other(_)));
        assert_eq!(owner_of(&json!({"npc": "gorm"})), ToolOwner::Npc("gorm".into()));
        assert_eq!(owner_of(&json!({"requestId": 3})), ToolOwner::Request);
    }
}
