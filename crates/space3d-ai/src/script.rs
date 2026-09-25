//! Scripted models: deterministic model behavior for development, CI and the web.
//!
//! A [`Script`] is sent as a request's `model` (`{"type": "scripted", "steps": [...]}`), so the
//! same script plays on every backend: Apple's bridge, Android's bridge and the pure-Rust
//! [`ScriptedBackend`](crate::ScriptedBackend). Each model inference plays the next step; a tool
//! round takes one step and the answer another (protocol §10).
//!
//! ```
//! use space3d_ai::{Script, ScriptStep, Emotion};
//! use serde_json::json;
//! let script = Script::new()
//!     .step(ScriptStep::tool_call("check_inventory", json!({"item": "iron sword"})))
//!     .step(ScriptStep::npc_reply("Three swords, lad.", Emotion::Proud, ["Buy one."], false));
//! assert_eq!(script.to_json()["type"], "scripted");
//! ```

use crate::error::{AiError, AiErrorCode, AiResult};
use crate::types::Emotion;
use serde_json::{Map, Value, json};

/// A tool call in a [`ScriptStep::tool_calls`] step.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptedToolCall {
    /// The tool to call.
    pub name: String,
    /// Its arguments (an object).
    pub arguments: Value,
    /// A fixed call id (Apple honors it; Android and the scripted backend generate ids).
    pub id: Option<String>,
}

/// What a scripted step does.
#[derive(Clone, Debug, PartialEq)]
pub enum StepAction {
    /// Answer with text, streamed in about `chunks` pieces.
    Text {
        /// The answer.
        text: String,
        /// Streaming pieces (≥ 1).
        chunks: u32,
    },
    /// One tool round (the calls of a step may run in parallel).
    ToolCalls(Vec<ScriptedToolCall>),
    /// A structured answer for schema turns (NPC replies, decisions, content).
    Json(Value),
    /// An answer with `{prompt}`, `{toolOutput}` and `{toolOutputs}` substituted.
    Template(String),
    /// Fail with an agent error code (`guardrail_violation`, `refusal`, …).
    Error {
        /// One of the agent codes.
        code: AiErrorCode,
        /// The message (default `Scripted <code>.`).
        message: Option<String>,
    },
}

/// One scripted model step.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptStep {
    /// What the step does.
    pub action: StepAction,
    /// Wait this long before playing it (tests cancellation and concurrency).
    pub delay_ms: u64,
}

impl ScriptStep {
    fn of(action: StepAction) -> Self {
        Self { action, delay_ms: 0 }
    }

    /// Answer with `text` (streamed in about 3 pieces).
    pub fn text(text: impl Into<String>) -> Self {
        Self::text_in_chunks(text, 3)
    }

    /// Answer with `text`, streamed in about `chunks` pieces.
    pub fn text_in_chunks(text: impl Into<String>, chunks: u32) -> Self {
        Self::of(StepAction::Text {
            text: text.into(),
            chunks: chunks.max(1),
        })
    }

    /// Call one tool.
    pub fn tool_call(name: impl Into<String>, arguments: Value) -> Self {
        Self::tool_calls([(name.into(), arguments)])
    }

    /// Call several tools in one round.
    pub fn tool_calls(calls: impl IntoIterator<Item = (String, Value)>) -> Self {
        Self::of(StepAction::ToolCalls(
            calls
                .into_iter()
                .map(|(name, arguments)| ScriptedToolCall {
                    name,
                    arguments,
                    id: None,
                })
                .collect(),
        ))
    }

    /// A structured answer.
    pub fn json(value: Value) -> Self {
        Self::of(StepAction::Json(value))
    }

    /// An answer rendered from a template (`{prompt}`, `{toolOutput}`, `{toolOutputs}`).
    pub fn template(template: impl Into<String>) -> Self {
        Self::of(StepAction::Template(template.into()))
    }

    /// Fail with an agent error code.
    pub fn error(code: AiErrorCode) -> Self {
        Self::of(StepAction::Error { code, message: None })
    }

    /// Fail with an agent error code and message.
    pub fn error_with_message(code: AiErrorCode, message: impl Into<String>) -> Self {
        Self::of(StepAction::Error {
            code,
            message: Some(message.into()),
        })
    }

    /// A structured NPC reply (`emotion`, `line`, `player_options`, `ends_conversation`).
    pub fn npc_reply<S: Into<String>>(
        line: impl Into<String>,
        emotion: Emotion,
        player_options: impl IntoIterator<Item = S>,
        ends_conversation: bool,
    ) -> Self {
        let options: Vec<String> = player_options.into_iter().map(Into::into).collect();
        Self::json(json!({
            "emotion": emotion.as_str(),
            "line": line.into(),
            "player_options": options,
            "ends_conversation": ends_conversation,
        }))
    }

    /// A structured decision (`reasoning`, `choice`, `confidence`).
    pub fn decision(reasoning: impl Into<String>, choice: impl Into<String>, confidence: i32) -> Self {
        Self::json(json!({"reasoning": reasoning.into(), "choice": choice.into(), "confidence": confidence}))
    }

    /// Wait `delay_ms` milliseconds before playing this step.
    pub fn delayed(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    /// The wire form (`{"text": …}`, `{"toolCalls": […]}`, …).
    pub fn to_json(&self) -> Value {
        let mut object = match &self.action {
            StepAction::Text { text, chunks } => json!({"text": text, "chunks": chunks}),
            StepAction::ToolCalls(calls) => {
                let calls: Vec<Value> = calls
                    .iter()
                    .map(|call| {
                        let mut v = json!({"name": call.name, "arguments": call.arguments});
                        if let Some(id) = &call.id {
                            v["id"] = json!(id);
                        }
                        v
                    })
                    .collect();
                json!({"toolCalls": calls})
            }
            StepAction::Json(value) => json!({"json": value}),
            StepAction::Template(template) => json!({"template": template}),
            StepAction::Error { code, message } => {
                let mut v = json!({"error": code.as_str()});
                if let Some(message) = message {
                    v["message"] = json!(message);
                }
                v
            }
        };
        if self.delay_ms > 0 {
            object["delayMs"] = json!(self.delay_ms);
        }
        object
    }

    /// Parses the wire form; `path` names the step in error messages (`model.steps[0]`).
    pub fn from_json(value: &Value, path: &str) -> AiResult<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| AiError::invalid_params(format!("'{path}' must be an object.")))?;
        let delay_ms = match object.get("delayMs") {
            None | Some(Value::Null) => 0,
            Some(v) => v
                .as_u64()
                .or_else(|| v.as_f64().filter(|f| *f >= 0.0 && f.fract() == 0.0).map(|f| f as u64))
                .ok_or_else(|| AiError::invalid_params(format!("'{path}.delayMs' must be an integer >= 0.")))?,
        };
        let action = if let Some(text) = object.get("text").filter(|v| !v.is_null()) {
            let text = text
                .as_str()
                .ok_or_else(|| AiError::invalid_params(format!("'{path}.text' must be a string.")))?;
            let chunks = match object.get("chunks") {
                None | Some(Value::Null) => 3,
                Some(v) => v
                    .as_u64()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| AiError::invalid_params(format!("'{path}.chunks' must be an integer >= 1.")))?,
            };
            StepAction::Text {
                text: text.to_owned(),
                chunks: u32::try_from(chunks).unwrap_or(u32::MAX),
            }
        } else if let Some(calls) = object.get("toolCalls").filter(|v| !v.is_null()) {
            let calls = calls
                .as_array()
                .ok_or_else(|| AiError::invalid_params(format!("'{path}.toolCalls' must be an array.")))?;
            if calls.is_empty() {
                return Err(AiError::invalid_params(format!(
                    "'{path}.toolCalls' must not be empty."
                )));
            }
            let mut parsed = Vec::with_capacity(calls.len());
            for (index, call) in calls.iter().enumerate() {
                let call_path = format!("{path}.toolCalls[{index}]");
                let call = call
                    .as_object()
                    .ok_or_else(|| AiError::invalid_params(format!("'{call_path}' must be an object.")))?;
                let name = call.get("name").and_then(Value::as_str).ok_or_else(|| {
                    AiError::invalid_params(format!("Missing required parameter '{call_path}.name'."))
                })?;
                let arguments = match call.get("arguments") {
                    None | Some(Value::Null) => Value::Object(Map::new()),
                    Some(Value::Object(o)) => Value::Object(o.clone()),
                    Some(_) => {
                        return Err(AiError::invalid_params(format!(
                            "'{call_path}.arguments' must be an object."
                        )));
                    }
                };
                parsed.push(ScriptedToolCall {
                    name: name.to_owned(),
                    arguments,
                    id: call.get("id").and_then(Value::as_str).map(str::to_owned),
                });
            }
            StepAction::ToolCalls(parsed)
        } else if let Some(json) = object.get("json") {
            StepAction::Json(json.clone())
        } else if let Some(template) = object.get("template").filter(|v| !v.is_null()) {
            let template = template
                .as_str()
                .ok_or_else(|| AiError::invalid_params(format!("'{path}.template' must be a string.")))?;
            StepAction::Template(template.to_owned())
        } else if let Some(code) = object.get("error").filter(|v| !v.is_null()) {
            let code = code
                .as_str()
                .ok_or_else(|| AiError::invalid_params(format!("'{path}.error' must be a string.")))?;
            let parsed = AiErrorCode::from_name(code);
            if !parsed.is_agent_error() {
                let known: Vec<&str> = crate::error::AGENT_CODES.iter().map(AiErrorCode::as_str).collect();
                return Err(AiError::invalid_params(format!(
                    "'{path}.error' must be one of: {}; got '{code}'.",
                    known.join(", ")
                )));
            }
            StepAction::Error {
                code: parsed,
                message: object.get("message").and_then(Value::as_str).map(str::to_owned),
            }
        } else {
            return Err(AiError::invalid_params(format!(
                "'{path}' must contain one of 'text', 'toolCalls', 'json', 'template' or 'error'."
            )));
        };
        Ok(Self { action, delay_ms })
    }
}

/// A deterministic model: steps played one per inference, then `fallback`
/// (default `{"text": "(script exhausted)"}`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Script {
    /// The steps, in order.
    pub steps: Vec<ScriptStep>,
    /// Played once the steps run out.
    pub fallback: Option<ScriptStep>,
}

impl Script {
    /// An empty script (every inference plays the fallback).
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a step.
    pub fn step(mut self, step: ScriptStep) -> Self {
        self.steps.push(step);
        self
    }

    /// Appends steps.
    pub fn steps(mut self, steps: impl IntoIterator<Item = ScriptStep>) -> Self {
        self.steps.extend(steps);
        self
    }

    /// Sets the fallback step.
    pub fn fallback(mut self, step: ScriptStep) -> Self {
        self.fallback = Some(step);
        self
    }

    /// The `model` parameter: `{"type": "scripted", "steps": […], "fallback"?: step}`.
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "type": "scripted",
            "steps": self.steps.iter().map(ScriptStep::to_json).collect::<Vec<_>>(),
        });
        if let Some(fallback) = &self.fallback {
            value["fallback"] = fallback.to_json();
        }
        value
    }

    /// Parses `{"type": "scripted", "steps"?: […], "fallback"?: step}`.
    pub fn from_json(value: &Value, path: &str) -> AiResult<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| AiError::invalid_params(format!("'{path}' must be an object.")))?;
        let steps = match object.get("steps") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(steps)) => steps
                .iter()
                .enumerate()
                .map(|(i, step)| ScriptStep::from_json(step, &format!("{path}.steps[{i}]")))
                .collect::<AiResult<_>>()?,
            Some(_) => return Err(AiError::invalid_params(format!("'{path}.steps' must be an array."))),
        };
        let fallback = match object.get("fallback") {
            None | Some(Value::Null) => None,
            Some(step) => Some(ScriptStep::from_json(step, &format!("{path}.fallback"))?),
        };
        Ok(Self { steps, fallback })
    }
}

/// The model a session, NPC or request runs on (the `model` parameter).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ModelSpec {
    /// The platform's on-device model (Apple Foundation Models, Gemini Nano).
    #[default]
    System,
    /// A deterministic [`Script`].
    Scripted(Script),
    /// A custom model type the host registered (`{"type": "<custom>", …}`).
    Custom(Value),
}

impl ModelSpec {
    /// The wire form (`"system"`, `{"type": "scripted", …}` or the custom object).
    pub fn to_json(&self) -> Value {
        match self {
            Self::System => json!("system"),
            Self::Scripted(script) => script.to_json(),
            Self::Custom(value) => value.clone(),
        }
    }
}

impl From<Script> for ModelSpec {
    fn from(script: Script) -> Self {
        Self::Scripted(script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_round_trip() {
        let steps = [
            ScriptStep::text("Hmph."),
            ScriptStep::text_in_chunks("Hello there", 5).delayed(250),
            ScriptStep::tool_call("open_gate", json!({"gate": "north"})),
            ScriptStep::json(json!({"choice": "flee"})),
            ScriptStep::template("Gate says: {toolOutput}"),
            ScriptStep::error_with_message(AiErrorCode::GuardrailViolation, "blocked"),
            ScriptStep::error(AiErrorCode::Refusal),
        ];
        for step in steps {
            assert_eq!(ScriptStep::from_json(&step.to_json(), "s").unwrap(), step);
        }
        let script = Script::new()
            .step(ScriptStep::decision("Timid.", "flee", 80))
            .fallback(ScriptStep::text("..."));
        assert_eq!(Script::from_json(&script.to_json(), "model").unwrap(), script);
        assert_eq!(ModelSpec::from(script).to_json()["type"], "scripted");
        assert_eq!(ModelSpec::System.to_json(), json!("system"));
    }

    #[test]
    fn invalid_steps_name_their_path() {
        let err = ScriptStep::from_json(&json!({"error": "session_not_found"}), "model.steps[1]").unwrap_err();
        assert_eq!(err.code, AiErrorCode::InvalidParams);
        assert!(
            err.message.starts_with("'model.steps[1].error' must be one of:"),
            "{}",
            err.message
        );
        let err = ScriptStep::from_json(&json!({"toolCalls": []}), "s").unwrap_err();
        assert_eq!(err.message, "'s.toolCalls' must not be empty.");
        let err = ScriptStep::from_json(&json!({"chunks": 2}), "s").unwrap_err();
        assert!(err.message.contains("must contain one of"));
        let err = ScriptStep::from_json(&json!({"text": "x", "chunks": 0}), "s").unwrap_err();
        assert_eq!(err.message, "'s.chunks' must be an integer >= 1.");
        assert!(Script::from_json(&json!({"type": "scripted", "steps": {}}), "model").is_err());
    }
}
