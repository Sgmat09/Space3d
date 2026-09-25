//! Request builders: sessions, NPCs, decisions and content. Tool lists name tools registered
//! with [`Ai::register_tool`](crate::Ai::register_tool); the [`Ai`](crate::Ai) resource turns
//! them into protocol definitions.

use crate::script::ModelSpec;
use crate::types::{DecisionOption, Emotion, NpcMemory, Persona, ReplyFormat, Sampling, ToolChoice};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `session/create` / `session/respond` options (protocol §5). Unset fields use the bridge's
/// defaults; `extra` carries platform-specific options (e.g. Android's `userLabel`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOptions {
    /// Default tool choice per turn (`auto`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Model steps that may call tools (4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<u32>,
    /// Tool calls per turn (12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Restrict the visible tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Response token limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_response_tokens: Option<u32>,
    /// Sampling mode (`Greedy` makes decisions reproducible).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Sampling>,
    /// Seconds the engine has to answer a `tool/call` (120; `0` = forever).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_timeout_seconds: Option<f64>,
    /// Hide the oldest turns when the context would overflow (`true`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trim_history: Option<bool>,
    /// Other options, sent as is.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A `session/create` request: an agent with its own conversation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionConfig {
    /// System instructions (persona, rules).
    pub instructions: Option<String>,
    /// Names of registered tools the model may call.
    pub tools: Vec<String>,
    /// Options.
    pub options: SessionOptions,
    /// The model (`System` by default).
    pub model: ModelSpec,
    /// A transcript from `session/transcript` to resume.
    pub history: Option<Value>,
}

impl SessionConfig {
    /// An empty configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the instructions.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Adds a registered tool.
    pub fn tool(mut self, name: impl Into<String>) -> Self {
        self.tools.push(name.into());
        self
    }

    /// Adds registered tools.
    pub fn tools<S: Into<String>>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.tools.extend(names.into_iter().map(Into::into));
        self
    }

    /// Sets the default tool choice.
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.options.tool_choice = Some(choice);
        self
    }

    /// Sets the options.
    pub fn options(mut self, options: SessionOptions) -> Self {
        self.options = options;
        self
    }

    /// Sets the model.
    pub fn model(mut self, model: impl Into<ModelSpec>) -> Self {
        self.model = model.into();
        self
    }
}

/// Per-turn settings for `session/respond`.
#[derive(Clone, Debug, PartialEq)]
pub struct RespondOptions {
    /// Stream `session/event`s as [`AiEvent::Session`](crate::AiEvent::Session) (default `true`).
    pub stream: bool,
    /// JSON Schema for a structured answer (`AgentResponse::structured`).
    pub schema: Option<Value>,
    /// Override the session's tool choice.
    pub tool_choice: Option<ToolChoice>,
    /// Override the session's round budget.
    pub max_tool_rounds: Option<u32>,
    /// Override the session's call budget.
    pub max_tool_calls: Option<u32>,
    /// Override the visible tools.
    pub enabled_tools: Option<Vec<String>>,
}

impl Default for RespondOptions {
    fn default() -> Self {
        Self {
            stream: true,
            schema: None,
            tool_choice: None,
            max_tool_rounds: None,
            max_tool_calls: None,
            enabled_tools: None,
        }
    }
}

impl RespondOptions {
    /// Streaming, no overrides.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables or disables streaming events.
    pub fn stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Asks for a structured answer matching `schema`.
    pub fn schema(mut self, schema: Value) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Overrides the tool choice.
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }
}

/// NPC options (protocol §6, `npc/create` `options`). Unset fields use the bridge's defaults;
/// `extra` carries the rest (`worldReadable`, `memoryTools`, `compactAfterTurns`, …).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NpcOptions {
    /// `automatic` (default), `structured` or `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_format: Option<ReplyFormat>,
    /// A tool the NPC must call first on every turn (grounds facts; the small models invent
    /// facts otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grounding_tool: Option<String>,
    /// Tool choice when there is no grounding tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Tool rounds per turn (2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<u32>,
    /// Tool calls per turn (6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Seconds the engine has to answer a `tool/call` (120; `0` = forever).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_timeout_seconds: Option<f64>,
    /// Suggested player replies, 0–4 (3; structured replies only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player_option_count: Option<u32>,
    /// Whether the NPC may end the conversation (`true`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_end_conversation: Option<bool>,
    /// Answer blocked turns with a fallback line instead of an error (`true`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_on_guardrail: Option<bool>,
    /// Rotated fallback lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_lines: Option<Vec<String>>,
    /// Emotions the model may choose from (all).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emotions: Option<Vec<Emotion>>,
    /// Appended to the persona's instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_instructions: Option<String>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Other options, sent as is.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// An `npc/create` request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NpcConfig {
    /// Who the NPC is.
    pub persona: Persona,
    /// Names of registered tools the NPC may call.
    pub tools: Vec<String>,
    /// A world id (adds the built-in `read_world_state` / `update_world_state` tools).
    pub world: Option<String>,
    /// Initial memory.
    pub memory: Option<NpcMemory>,
    /// Options.
    pub options: NpcOptions,
    /// The model (`System` by default).
    pub model: ModelSpec,
}

impl NpcConfig {
    /// An NPC with this persona.
    pub fn new(persona: Persona) -> Self {
        Self {
            persona,
            ..Self::default()
        }
    }

    /// Adds a registered tool.
    pub fn tool(mut self, name: impl Into<String>) -> Self {
        self.tools.push(name.into());
        self
    }

    /// Makes `tool` (added too, if missing) the grounding tool called first on every turn.
    pub fn grounding_tool(mut self, tool: impl Into<String>) -> Self {
        let tool = tool.into();
        if !self.tools.contains(&tool) {
            self.tools.push(tool.clone());
        }
        self.options.grounding_tool = Some(tool);
        self
    }

    /// Sets the reply format.
    pub fn reply_format(mut self, format: ReplyFormat) -> Self {
        self.options.reply_format = Some(format);
        self
    }

    /// Sets the world.
    pub fn world(mut self, world: impl Into<String>) -> Self {
        self.world = Some(world.into());
        self
    }

    /// Sets the initial memory.
    pub fn memory(mut self, memory: NpcMemory) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Sets the options.
    pub fn options(mut self, options: NpcOptions) -> Self {
        self.options = options;
        self
    }

    /// Sets the model.
    pub fn model(mut self, model: impl Into<ModelSpec>) -> Self {
        self.model = model.into();
        self
    }
}

/// Per-turn settings for `npc/talk`.
#[derive(Clone, Debug, PartialEq)]
pub struct TalkOptions {
    /// What is happening right now (this turn only), as text or JSON.
    pub context: Option<Value>,
    /// Stream `npc/event`s as [`AiEvent::Npc`](crate::AiEvent::Npc) (default `true`).
    pub stream: bool,
    /// Override the NPC's tool choice.
    pub tool_choice: Option<ToolChoice>,
}

impl Default for TalkOptions {
    fn default() -> Self {
        Self {
            context: None,
            stream: true,
            tool_choice: None,
        }
    }
}

impl TalkOptions {
    /// Streaming, no context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the context (`"The player just paid 45 gold."`).
    pub fn context(mut self, context: impl Into<Value>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Enables or disables streaming events.
    pub fn stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }
}

/// Who makes a decision.
#[derive(Clone, Debug, PartialEq)]
pub enum Actor {
    /// A persona.
    Persona(Persona),
    /// An existing NPC, by id.
    Npc(String),
}

/// A `decision/decide` request: pick one of a fixed set of options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecisionRequest {
    /// The situation, from the actor's point of view.
    pub situation: String,
    /// The options (unique ids).
    pub options: Vec<DecisionOption>,
    /// Who decides.
    pub actor: Option<Actor>,
    /// Facts as JSON (`{"hp": 3}`).
    pub context: Option<Value>,
    /// Names of registered tools the model may call first.
    pub tools: Vec<String>,
    /// `Required` / `Tool(name)` forces a lookup first.
    pub tool_choice: Option<ToolChoice>,
    /// Returned (with `is_fallback`) when the guardrails block the decision.
    pub fallback_option_id: Option<String>,
    /// Replaces the default instructions.
    pub instructions: Option<String>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Model steps that may call tools (2).
    pub max_tool_rounds: Option<u32>,
    /// Seconds the engine has to answer a `tool/call`.
    pub tool_timeout_seconds: Option<f64>,
    /// The model.
    pub model: ModelSpec,
}

impl DecisionRequest {
    /// A decision about `situation` between `options` (ids, `(id, description)` pairs or
    /// [`DecisionOption`]s).
    pub fn new<O: Into<DecisionOption>>(situation: impl Into<String>, options: impl IntoIterator<Item = O>) -> Self {
        Self {
            situation: situation.into(),
            options: options.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// Decides as this persona.
    pub fn actor(mut self, persona: Persona) -> Self {
        self.actor = Some(Actor::Persona(persona));
        self
    }

    /// Decides as an existing NPC.
    pub fn actor_npc(mut self, npc: impl Into<String>) -> Self {
        self.actor = Some(Actor::Npc(npc.into()));
        self
    }

    /// Adds facts.
    pub fn context(mut self, context: Value) -> Self {
        self.context = Some(context);
        self
    }

    /// Adds a registered tool.
    pub fn tool(mut self, name: impl Into<String>) -> Self {
        self.tools.push(name.into());
        self
    }

    /// Sets the tool choice.
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Sets the fallback option.
    pub fn fallback(mut self, option_id: impl Into<String>) -> Self {
        self.fallback_option_id = Some(option_id.into());
        self
    }

    /// Sets the model.
    pub fn model(mut self, model: impl Into<ModelSpec>) -> Self {
        self.model = model.into();
        self
    }
}

/// A `content/generate` request: JSON matching a schema (items, quests, loot).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContentRequest {
    /// What to generate.
    pub prompt: String,
    /// The JSON Schema of the result (keep it flat; put guidance in property descriptions).
    pub schema: Value,
    /// Replaces the default instructions.
    pub instructions: Option<String>,
    /// Facts as JSON.
    pub context: Option<Value>,
    /// Names of registered tools the model may call first.
    pub tools: Vec<String>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// The model.
    pub model: ModelSpec,
}

impl ContentRequest {
    /// Generate JSON for `prompt` matching `schema`.
    pub fn new(prompt: impl Into<String>, schema: Value) -> Self {
        Self {
            prompt: prompt.into(),
            schema,
            ..Self::default()
        }
    }

    /// Adds facts.
    pub fn context(mut self, context: Value) -> Self {
        self.context = Some(context);
        self
    }

    /// Adds a registered tool.
    pub fn tool(mut self, name: impl Into<String>) -> Self {
        self.tools.push(name.into());
        self
    }

    /// Sets the model.
    pub fn model(mut self, model: impl Into<ModelSpec>) -> Self {
        self.model = model.into();
        self
    }
}

/// The result of `session/create` / `npc/create`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Created {
    /// The session or NPC id.
    pub id: String,
    /// Every tool the model sees (NPCs: client tools plus built-in world and memory tools).
    pub tools: Vec<String>,
    /// Warnings (unknown options, unenforced schema constraints, …).
    pub warnings: Vec<String>,
}
