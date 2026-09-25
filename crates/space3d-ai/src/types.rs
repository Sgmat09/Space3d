//! The protocol's data types: tool calls and outputs, turn results, NPC personas and dialogue
//! turns, decisions and model availability. They mirror open-apple-models' Swift types
//! (`ToolCall`, `ToolOutput`, `ToolRecord`, `AgentResponse`, `Persona`, `Emotion`,
//! `DialogueTurn`, `Decision`, …) and serialize to the wire shapes of protocol v1.0.

use serde::de::{self, DeserializeOwned, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt;

// ------------------------------------------------------------------------------------------
// Tool choice

/// Whether and how the model may call tools on a turn (`toolChoice`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ToolChoice {
    /// The model decides at every step (the default). On Android this behaves like
    /// [`Explicit`](Self::Explicit).
    #[default]
    Auto,
    /// Tools are disabled for the turn.
    None,
    /// The model must call a tool on the first step, then answers freely.
    Required,
    /// The model must make an explicit decision first: call a tool or answer directly. More
    /// reliable grounding than `Auto` without forcing a pointless call for small talk.
    Explicit,
    /// The model must call this tool on the first step, then answers freely.
    Tool(String),
}

impl ToolChoice {
    /// The wire form: `"auto"`, `"none"`, `"required"`, `"explicit"` or `{"tool": name}`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Auto => json!("auto"),
            Self::None => json!("none"),
            Self::Required => json!("required"),
            Self::Explicit => json!("explicit"),
            Self::Tool(name) => json!({"tool": name}),
        }
    }

    /// Parses the wire form (also OpenAI's `{"type": "function", "function": {"name"}}`).
    pub fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::String(s) => match s.as_str() {
                "auto" => Some(Self::Auto),
                "none" => Some(Self::None),
                "required" | "any" => Some(Self::Required),
                "explicit" => Some(Self::Explicit),
                _ => None,
            },
            Value::Object(o) => o
                .get("tool")
                .or_else(|| o.get("name"))
                .or_else(|| o.get("function").and_then(|f| f.get("name")))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(|name| Self::Tool(name.to_owned())),
            _ => None,
        }
    }
}

impl Serialize for ToolChoice {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(&value).ok_or_else(|| {
            de::Error::custom(format!(
                "expected \"auto\", \"none\", \"required\", \"explicit\" or {{\"tool\": name}}, got {value}"
            ))
        })
    }
}

// ------------------------------------------------------------------------------------------
// Tool calls, outputs and records

/// A tool invocation produced by the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Call id (`call_…`).
    pub id: String,
    /// The tool's name.
    pub name: String,
    /// The arguments, following the tool's `parameters` schema (an object).
    #[serde(default)]
    pub arguments: Value,
}

/// A missing or mistyped tool argument. Reported back to the model as an error output, which
/// usually makes it retry with corrected arguments.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolArgumentError(pub String);

impl ToolCall {
    fn argument(&self, key: &str, kind: &str, value: Option<&Value>) -> ToolArgumentError {
        let got = value.map_or_else(|| "nothing".to_owned(), Value::to_string);
        ToolArgumentError(format!(
            "Argument '{key}' of tool '{}' must be {kind}; got {got}.",
            self.name
        ))
    }

    /// A string argument.
    pub fn string(&self, key: &str) -> Result<&str, ToolArgumentError> {
        let value = self.arguments.get(key);
        value
            .and_then(Value::as_str)
            .ok_or_else(|| self.argument(key, "a string", value))
    }

    /// An integer argument (whole-number floats are accepted).
    pub fn i64(&self, key: &str) -> Result<i64, ToolArgumentError> {
        let value = self.arguments.get(key);
        value
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            })
            .ok_or_else(|| self.argument(key, "an integer", value))
    }

    /// A number argument.
    pub fn f64(&self, key: &str) -> Result<f64, ToolArgumentError> {
        let value = self.arguments.get(key);
        value
            .and_then(Value::as_f64)
            .ok_or_else(|| self.argument(key, "a number", value))
    }

    /// A boolean argument.
    pub fn bool(&self, key: &str) -> Result<bool, ToolArgumentError> {
        let value = self.arguments.get(key);
        value
            .and_then(Value::as_bool)
            .ok_or_else(|| self.argument(key, "a boolean", value))
    }

    /// Decodes the whole argument object.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, ToolArgumentError> {
        serde_json::from_value(self.arguments.clone())
            .map_err(|e| ToolArgumentError(format!("Invalid arguments for tool '{}': {e}.", self.name)))
    }
}

/// What a tool returns to the model.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutput {
    /// Plain text.
    Text(String),
    /// Structured data (the model sees compact JSON).
    Json(Value),
    /// A failure the model sees as `Error: …` and may recover from. It never fails the turn.
    Error(String),
}

impl ToolOutput {
    /// A text output.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// A JSON output from anything serializable (serialization failures become errors).
    pub fn json(value: impl Serialize) -> Self {
        match serde_json::to_value(value) {
            Ok(value) => Self::Json(value),
            Err(e) => Self::Error(format!("The tool's output could not be encoded: {e}")),
        }
    }

    /// An error output.
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    /// Whether this is an error output.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    /// The text the model reads: the text, compact JSON, or `Error: message`.
    pub fn model_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Json(value) => value.to_string(),
            Self::Error(message) => format!("Error: {message}"),
        }
    }

    /// The `output` field as sent on the wire: a string for text and errors, JSON otherwise.
    pub fn wire_output(&self) -> Value {
        match self {
            Self::Text(text) | Self::Error(text) => Value::String(text.clone()),
            Self::Json(value) => value.clone(),
        }
    }

    /// The `result` of a `tool/call` response: `{"output": …}` plus `"isError": true` for errors.
    pub fn to_tool_result(&self) -> Value {
        match self {
            Self::Error(message) => json!({"output": message, "isError": true}),
            other => json!({"output": other.wire_output()}),
        }
    }

    /// Reads a `tool/call` result leniently: `{"output": "text"}` → text, `{"output": <JSON>}`
    /// → JSON, `"isError": true` → error, and a result without `output` is itself the output.
    pub fn from_tool_result(result: &Value) -> Self {
        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let output = result.get("output").unwrap_or(result);
        match (is_error, output) {
            (true, Value::String(s)) => Self::Error(s.clone()),
            (true, other) => Self::Error(other.to_string()),
            (false, Value::String(s)) => Self::Text(s.clone()),
            (false, other) => Self::Json(other.clone()),
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<Value> for ToolOutput {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

/// A completed tool call: the call, its output and how long it took.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolRecord {
    /// The call.
    pub call: ToolCall,
    /// Its output (errors the model saw are [`ToolOutput::Error`]).
    pub output: ToolOutput,
    /// Wall-clock seconds, including waiting for the engine.
    pub duration_seconds: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRecord {
    call: ToolCall,
    #[serde(default)]
    output: Value,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    duration_seconds: f64,
}

impl Serialize for ToolRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireRecord {
            call: self.call.clone(),
            output: self.output.wire_output(),
            is_error: self.output.is_error(),
            duration_seconds: self.duration_seconds,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WireRecord::deserialize(deserializer)?;
        let output = match (wire.is_error, wire.output) {
            (true, Value::String(s)) => ToolOutput::Error(s),
            (true, other) => ToolOutput::Error(other.to_string()),
            (false, Value::String(s)) => ToolOutput::Text(s),
            (false, other) => ToolOutput::Json(other),
        };
        Ok(Self {
            call: wire.call,
            output,
            duration_seconds: wire.duration_seconds,
        })
    }
}

/// Where a tool runs: in the engine (`client`, via `tool/call`) or inside the bridge (`local`,
/// e.g. the built-in NPC world and memory tools).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecution {
    /// Executed by the engine through a `tool/call` request.
    #[default]
    Client,
    /// Executed inside the bridge.
    Local,
}

// ------------------------------------------------------------------------------------------
// Turn results

/// Token usage of a turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenUsage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Prompt tokens served from the prefix cache.
    pub cached_input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// `input_tokens + output_tokens`.
    pub total_tokens: u64,
}

/// One model inference step of a turn.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelStep {
    /// Step number within the turn.
    pub index: u32,
    /// Tool rounds completed before this step.
    pub completed_tool_rounds: u32,
    /// `allowed`, `required` or `disallowed`.
    pub tool_calling_mode: String,
    /// Tools the model could see.
    pub enabled_tools: Vec<String>,
    /// History entries hidden to fit the context.
    pub trimmed_entries: u32,
    /// Android only: `decide`, `toolArguments`, `respond` or `structured`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Android only: whether the step re-asked after invalid output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_repair: Option<bool>,
}

/// The result of a `session/respond` turn.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentResponse {
    /// The final text (for schema turns, the JSON text of `structured`).
    pub text: String,
    /// The structured answer of a schema turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    /// Tools called during the turn, in completion order.
    pub tool_calls: Vec<ToolRecord>,
    /// Token usage.
    pub usage: TokenUsage,
    /// Model steps taken.
    pub steps: Vec<ModelStep>,
    /// Warnings (unknown parameters, …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

// ------------------------------------------------------------------------------------------
// Game types

/// How a character feels while speaking a line. Unknown values decode as `Neutral`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[allow(missing_docs)]
pub enum Emotion {
    #[default]
    Neutral,
    Happy,
    Sad,
    Angry,
    Afraid,
    Surprised,
    Suspicious,
    Amused,
    Disgusted,
    Excited,
    Curious,
    Confused,
    Worried,
    Grateful,
    Annoyed,
    Proud,
}

impl Emotion {
    /// Every emotion, in protocol order.
    pub const ALL: [Emotion; 16] = [
        Self::Neutral,
        Self::Happy,
        Self::Sad,
        Self::Angry,
        Self::Afraid,
        Self::Surprised,
        Self::Suspicious,
        Self::Amused,
        Self::Disgusted,
        Self::Excited,
        Self::Curious,
        Self::Confused,
        Self::Worried,
        Self::Grateful,
        Self::Annoyed,
        Self::Proud,
    ];

    /// The lowercase wire name (`"angry"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Happy => "happy",
            Self::Sad => "sad",
            Self::Angry => "angry",
            Self::Afraid => "afraid",
            Self::Surprised => "surprised",
            Self::Suspicious => "suspicious",
            Self::Amused => "amused",
            Self::Disgusted => "disgusted",
            Self::Excited => "excited",
            Self::Curious => "curious",
            Self::Confused => "confused",
            Self::Worried => "worried",
            Self::Grateful => "grateful",
            Self::Annoyed => "annoyed",
            Self::Proud => "proud",
        }
    }

    /// Parses model output leniently: case-insensitive, surrounding punctuation ignored and
    /// common synonyms mapped (`"mad"` → `Angry`, `"scared"` → `Afraid`), as the libraries do.
    pub fn matching(text: &str) -> Option<Self> {
        let key = text.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase();
        if let Some(exact) = Self::ALL.into_iter().find(|e| e.as_str() == key) {
            return Some(exact);
        }
        let synonym = match key.as_str() {
            "calm" | "content" | "indifferent" | "bored" => Self::Neutral,
            "joyful" | "glad" | "cheerful" | "pleased" | "friendly" | "warm" => Self::Happy,
            "unhappy" | "sorrowful" | "melancholy" | "grieving" => Self::Sad,
            "mad" | "furious" | "enraged" | "hostile" => Self::Angry,
            "scared" | "fearful" | "frightened" | "terrified" => Self::Afraid,
            "nervous" | "anxious" | "concerned" => Self::Worried,
            "shocked" | "astonished" | "startled" => Self::Surprised,
            "wary" | "distrustful" | "skeptical" | "sceptical" => Self::Suspicious,
            "amusement" | "playful" | "laughing" => Self::Amused,
            "disgust" | "revolted" => Self::Disgusted,
            "eager" | "thrilled" | "enthusiastic" => Self::Excited,
            "interested" | "intrigued" => Self::Curious,
            "puzzled" | "uncertain" => Self::Confused,
            "thankful" => Self::Grateful,
            "irritated" | "grumpy" | "impatient" | "gruff" => Self::Annoyed,
            "confident" | "smug" => Self::Proud,
            _ => return None,
        };
        Some(synonym)
    }
}

impl fmt::Display for Emotion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Emotion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Emotion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(Self::matching(&text).unwrap_or_default())
    }
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// Who an NPC is. Only `name` is required; keep every field short (the on-device models have
/// about 4000 (Android) to 8192 (Apple) tokens for everything).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Persona {
    /// `"Gorm"`.
    pub name: String,
    /// `"the village blacksmith"`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// A sentence or two.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub personality: String,
    /// How they talk.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub speaking_style: String,
    /// Their history.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub backstory: String,
    /// What they want.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub goals: Vec<String>,
    /// Facts they know.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub knowledge: Vec<String>,
    /// Kept out of the prompt until the relationship unlocks them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Used when the model gives no emotion, and for fallback lines.
    #[serde(skip_serializing_if = "is_default")]
    pub default_emotion: Emotion,
    /// Reply length limit (default 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_sentences: Option<u32>,
}

impl Persona {
    /// A persona with just a name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Sets the role.
    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = role.into();
        self
    }

    /// Sets the personality.
    pub fn personality(mut self, personality: impl Into<String>) -> Self {
        self.personality = personality.into();
        self
    }

    /// Sets the speaking style.
    pub fn speaking_style(mut self, style: impl Into<String>) -> Self {
        self.speaking_style = style.into();
        self
    }

    /// Sets the backstory.
    pub fn backstory(mut self, backstory: impl Into<String>) -> Self {
        self.backstory = backstory.into();
        self
    }

    /// Adds a goal.
    pub fn goal(mut self, goal: impl Into<String>) -> Self {
        self.goals.push(goal.into());
        self
    }

    /// Adds a known fact.
    pub fn knows(mut self, fact: impl Into<String>) -> Self {
        self.knowledge.push(fact.into());
        self
    }

    /// Adds a secret.
    pub fn secret(mut self, secret: impl Into<String>) -> Self {
        self.secrets.push(secret.into());
        self
    }

    /// Sets the default emotion.
    pub fn default_emotion(mut self, emotion: Emotion) -> Self {
        self.default_emotion = emotion;
        self
    }

    /// Sets the reply length limit.
    pub fn max_sentences(mut self, sentences: u32) -> Self {
        self.max_sentences = Some(sentences);
        self
    }
}

/// What an NPC remembers about the player.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NpcMemory {
    /// Facts learned (`"Aria likes axes."`).
    pub facts: Vec<String>,
    /// Attitude toward the player, `-100…100`.
    pub relationship: i32,
    /// A summary of older conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// How an NPC's reply is generated (`options.replyFormat`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyFormat {
    /// Structured; if blocked, retried once as plain text, then a fallback line (default).
    #[default]
    Automatic,
    /// Emotion, line, player options and end-of-conversation as JSON.
    Structured,
    /// `[emotion] line` text: blocked least, fastest; no player options.
    Text,
}

/// A dialogue turn from `npc/talk`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DialogueTurn {
    /// The NPC id.
    pub npc: String,
    /// What the NPC says.
    pub line: String,
    /// How the NPC feels saying it.
    pub emotion: Emotion,
    /// Suggested player replies (may be empty).
    pub player_options: Vec<String>,
    /// Whether the NPC ended the conversation.
    pub ends_conversation: bool,
    /// Tools executed during the turn, in completion order.
    pub tool_calls: Vec<ToolRecord>,
    /// The NPC's attitude after the turn (`-100…100`).
    pub relationship: i32,
    /// Whether a fallback line replaced a blocked (guardrail or refused) turn.
    pub is_fallback: bool,
    /// Token usage.
    pub usage: TokenUsage,
}

/// One choice offered to `decision/decide`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    /// Stable id returned as [`Decision::option_id`]; short snake_case ids work best.
    pub id: String,
    /// What choosing it means, in a short sentence.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl DecisionOption {
    /// An option with an id and a description.
    pub fn new(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
        }
    }
}

impl From<&str> for DecisionOption {
    fn from(id: &str) -> Self {
        Self::new(id, "")
    }
}

impl From<(&str, &str)> for DecisionOption {
    fn from((id, description): (&str, &str)) -> Self {
        Self::new(id, description)
    }
}

/// The outcome of `decision/decide`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Decision {
    /// The chosen option id.
    #[serde(rename = "optionID")]
    pub option_id: String,
    /// The model's one-sentence reason.
    pub reasoning: String,
    /// Self-reported confidence, `0…100` (not calibrated).
    pub confidence: i32,
    /// Tools executed while deciding.
    pub tool_calls: Vec<ToolRecord>,
    /// Token usage.
    pub usage: TokenUsage,
    /// Whether the fallback option was returned because the model was blocked.
    pub is_fallback: bool,
}

/// `model/availability` (also in `initialize`'s result).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelAvailability {
    /// Whether the system model can run now.
    pub available: bool,
    /// Why not: `device_not_eligible`, `apple_intelligence_not_enabled`, `model_not_ready`,
    /// `aicore_unavailable`, `needs_system_update`, `not_enough_disk_space`, `unknown`, …
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The token budget (Apple: shared 8192 window; Android: ~4000 input tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u64>,
    /// The model variant (`"AFM 3 Core Advanced"`, a Gemini Nano name, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Supported languages (always empty on Android).
    pub supported_languages: Vec<String>,
    /// Android: `available`, `downloadable`, `downloading` or `unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// A readable reason (Android, scripted backend).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Other platform-specific fields (`maxOutputTokens`, download progress, …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The `server` member of `initialize`'s result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerName {
    /// `"open-apple-models"`, `"open-android-models"` or `"space3d-scripted"`.
    pub name: String,
    /// The library version.
    pub version: String,
}

/// `initialize`'s result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerInfo {
    /// `"1.0"`.
    pub protocol_version: String,
    /// Which bridge answered.
    pub server: ServerName,
    /// Methods, notifications, models and limits the bridge supports.
    pub capabilities: Value,
    /// The system model's availability.
    pub model: ModelAvailability,
}

impl ServerInfo {
    /// Whether the bridge serves `method` (from `capabilities.methods`).
    pub fn supports(&self, method: &str) -> bool {
        self.capabilities
            .get("methods")
            .and_then(Value::as_array)
            .is_some_and(|methods| methods.iter().any(|m| m.as_str() == Some(method)))
    }
}

/// `"greedy"` or `{"topK": n, "seed"?}` / `{"topP": p, "seed"?}` sampling.
#[derive(Clone, Debug, PartialEq)]
pub enum Sampling {
    /// Always the most likely token (reproducible decisions).
    Greedy,
    /// Top-K sampling.
    TopK {
        /// K.
        k: u32,
        /// Optional seed.
        seed: Option<u64>,
    },
    /// Nucleus sampling (ignored with a warning on Android).
    TopP {
        /// P.
        p: f64,
        /// Optional seed.
        seed: Option<u64>,
    },
}

impl Serialize for Sampling {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            Self::Greedy => json!("greedy"),
            Self::TopK { k, seed } => {
                let mut v = json!({"topK": k});
                if let Some(seed) = seed {
                    v["seed"] = json!(seed);
                }
                v
            }
            Self::TopP { p, seed } => {
                let mut v = json!({"topP": p});
                if let Some(seed) = seed {
                    v["seed"] = json!(seed);
                }
                v
            }
        };
        value.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_choice_wire_forms() {
        for choice in [
            ToolChoice::Auto,
            ToolChoice::None,
            ToolChoice::Required,
            ToolChoice::Explicit,
            ToolChoice::Tool("open_gate".into()),
        ] {
            let wire = serde_json::to_value(&choice).unwrap();
            assert_eq!(serde_json::from_value::<ToolChoice>(wire).unwrap(), choice);
        }
        let openai = json!({"type": "function", "function": {"name": "x"}});
        assert_eq!(ToolChoice::from_json(&openai), Some(ToolChoice::Tool("x".into())));
        assert!(serde_json::from_value::<ToolChoice>(json!("sometimes")).is_err());
    }

    #[test]
    fn tool_outputs_and_records() {
        assert_eq!(
            ToolOutput::from_tool_result(&json!({"output": "ok"})),
            ToolOutput::text("ok")
        );
        assert_eq!(
            ToolOutput::from_tool_result(&json!({"output": "jammed", "isError": true})),
            ToolOutput::error("jammed")
        );
        assert_eq!(
            ToolOutput::from_tool_result(&json!({"opened": true})),
            ToolOutput::Json(json!({"opened": true}))
        );
        assert_eq!(
            ToolOutput::error("x").to_tool_result(),
            json!({"output": "x", "isError": true})
        );
        assert_eq!(ToolOutput::json(json!({"a": 1})).model_text(), r#"{"a":1}"#);
        assert_eq!(ToolOutput::error("x").model_text(), "Error: x");

        let wire = json!({"call": {"id": "c1", "name": "t", "arguments": {"item": "axe"}},
                          "output": "Shop closed.", "isError": true, "durationSeconds": 0.5});
        let record: ToolRecord = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(record.output, ToolOutput::error("Shop closed."));
        assert_eq!(serde_json::to_value(&record).unwrap(), wire);
    }

    #[test]
    fn tool_call_arguments() {
        let call = ToolCall {
            id: "c".into(),
            name: "buy".into(),
            arguments: json!({"item": "sword", "count": 2.0, "price": 4.5, "gift": true}),
        };
        assert_eq!(call.string("item").unwrap(), "sword");
        assert_eq!(call.i64("count").unwrap(), 2);
        assert_eq!(call.f64("price").unwrap(), 4.5);
        assert!(call.bool("gift").unwrap());
        let err = call.string("missing").unwrap_err();
        assert_eq!(err.0, "Argument 'missing' of tool 'buy' must be a string; got nothing.");
        #[derive(Deserialize)]
        struct Args {
            item: String,
        }
        assert_eq!(call.decode::<Args>().unwrap().item, "sword");
    }

    #[test]
    fn emotions_parse_leniently() {
        assert_eq!(Emotion::matching("  [Angry]! "), Some(Emotion::Angry));
        assert_eq!(Emotion::matching("scared"), Some(Emotion::Afraid));
        assert_eq!(Emotion::matching("purple"), None);
        assert_eq!(
            serde_json::from_value::<Emotion>(json!("purple")).unwrap(),
            Emotion::Neutral
        );
        assert_eq!(serde_json::to_value(Emotion::Proud).unwrap(), json!("proud"));
    }

    #[test]
    fn game_types_decode_wire_shapes() {
        let turn: DialogueTurn = serde_json::from_value(json!({
            "npc": "gorm", "line": "Aye.", "emotion": "proud", "playerOptions": ["Buy"],
            "endsConversation": false, "toolCalls": [], "relationship": 5, "isFallback": false,
            "usage": {"inputTokens": 1, "cachedInputTokens": 0, "outputTokens": 2, "totalTokens": 3}
        }))
        .unwrap();
        assert_eq!(turn.emotion, Emotion::Proud);
        assert_eq!(turn.usage.total_tokens, 3);
        let decision: Decision =
            serde_json::from_value(json!({"optionID": "flee", "reasoning": "r", "confidence": 70})).unwrap();
        assert_eq!(decision.option_id, "flee");
        let persona = serde_json::to_value(Persona::new("Gorm").role("smith")).unwrap();
        assert_eq!(persona, json!({"name": "Gorm", "role": "smith"}));
        let availability: ModelAvailability =
            serde_json::from_value(json!({"available": false, "reason": "model_not_ready", "maxOutputTokens": 4096}))
                .unwrap();
        assert_eq!(availability.extra["maxOutputTokens"], 4096);
        assert_eq!(
            serde_json::to_value(Sampling::TopK { k: 3, seed: Some(1) }).unwrap(),
            json!({"topK": 3, "seed": 1})
        );
    }
}
