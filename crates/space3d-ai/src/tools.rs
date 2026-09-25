//! Engine-side tools: Rust closures the model can call, run on the engine thread with full ECS
//! access.
//!
//! A `tool/call` request from the bridge is queued when its line is drained (`Stage::PreUpdate`)
//! and handled by the `ai::run_tools` system in `Stage::Update`, which calls the registered
//! [`AiTool`] handler with the [`SystemCtx`] and sends the reply. Handlers must be quick (they
//! run inside the frame); for work that takes longer — an animation, a network call — return
//! [`ToolReply::Deferred`] and answer later through a [`ToolResponder`].

use crate::backend::AiBackend;
use crate::error::{AiError, AiResult};
use crate::rpc::{self, RequestId};
use crate::types::{ToolCall, ToolOutput};
use serde_json::{Map, Value, json};
use space3d_core::SystemCtx;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A tool handler: runs on the engine thread inside `Stage::Update`.
pub type ToolHandler = Box<dyn FnMut(&mut SystemCtx, &ToolInvocation) -> ToolReply + Send>;

/// A tool the model can call: a definition (name, description, JSON Schema of the arguments)
/// plus an optional Rust handler.
///
/// ```
/// use space3d_ai::{AiTool, ToolOutput};
/// use serde_json::json;
/// struct Gold(u32);
/// let tool = AiTool::new("check_gold", "How much gold the player has.")
///     .handler(|ctx, _call| {
///         let gold = ctx.resources.get::<Gold>().map_or(0, |g| g.0);
///         ToolOutput::json(json!({"gold": gold}))
///     });
/// assert_eq!(tool.definition()["name"], "check_gold");
/// ```
pub struct AiTool {
    name: String,
    description: String,
    parameters: Option<Value>,
    timeout_seconds: Option<f64>,
    pub(crate) handler: Option<ToolHandler>,
}

impl AiTool {
    /// A tool without arguments. `description` says what it does and returns: the model decides
    /// from it.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: None,
            timeout_seconds: None,
            handler: None,
        }
    }

    /// The JSON Schema of the arguments (an object schema; keep it flat).
    pub fn parameters(mut self, schema: Value) -> Self {
        self.parameters = Some(schema);
        self
    }

    /// Seconds the bridge waits for the reply before telling the model the tool timed out
    /// (default: the session's `toolTimeoutSeconds`, 120; `0` = forever).
    pub fn timeout_seconds(mut self, seconds: f64) -> Self {
        self.timeout_seconds = Some(seconds);
        self
    }

    /// Sets the handler. It returns anything convertible into a [`ToolReply`]: a
    /// [`ToolOutput`], a `String`/`&str` (text), a `serde_json::Value` (JSON), a
    /// `Result<_, impl Display>` (errors become error outputs the model sees), or
    /// [`ToolReply::Deferred`].
    pub fn handler<F, R>(mut self, mut handler: F) -> Self
    where
        F: FnMut(&mut SystemCtx, &ToolInvocation) -> R + Send + 'static,
        R: Into<ToolReply>,
    {
        self.handler = Some(Box::new(move |ctx, invocation| handler(ctx, invocation).into()));
        self
    }

    /// The tool's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether a handler is set.
    pub fn has_handler(&self) -> bool {
        self.handler.is_some()
    }

    /// Runs the handler directly, as `ai::run_tools` does for a `tool/call` — for unit tests
    /// of tools. A deferred reply's [`ToolResponder`] is detached (replies go nowhere).
    ///
    /// ```
    /// use space3d_ai::{AiTool, ToolCall, ToolOutput, ToolReply};
    /// use space3d_core::prelude::*;
    /// use serde_json::json;
    /// let mut tool = AiTool::new("double", "Doubles n.")
    ///     .handler(|_ctx, call| call.i64("n").map(|n| ToolOutput::json(n * 2)));
    /// let mut app = App::new();
    /// let mut ctx = SystemCtx { world: &mut app.world, resources: &mut app.resources };
    /// let call = ToolCall { id: "c1".into(), name: "double".into(), arguments: json!({"n": 21}) };
    /// assert_eq!(tool.invoke(&mut ctx, call), ToolReply::Output(ToolOutput::json(42)));
    /// ```
    pub fn invoke(&mut self, ctx: &mut SystemCtx, call: ToolCall) -> ToolReply {
        let Some(handler) = self.handler.as_mut() else {
            return ToolReply::Output(ToolOutput::error(format!(
                "No handler is registered for tool '{}'.",
                self.name
            )));
        };
        let invocation = ToolInvocation {
            call,
            owner: ToolOwner::Request,
            request: None,
            context: Map::new(),
            responder: ToolResponder::new(Arc::new(Detached), Value::Null),
        };
        handler(ctx, &invocation)
    }

    /// The protocol's tool definition: `{"name", "description", "parameters"?, "timeoutSeconds"?}`.
    pub fn definition(&self) -> Value {
        let mut definition = json!({"name": self.name, "description": self.description});
        if let Some(parameters) = &self.parameters {
            definition["parameters"] = parameters.clone();
        }
        if let Some(timeout) = self.timeout_seconds {
            definition["timeoutSeconds"] = json!(timeout);
        }
        definition
    }

    /// Checks the protocol's rules: 1–64 of `A–Z a–z 0–9 _ - .`, an object schema, a finite
    /// timeout of 0…86400 s.
    pub fn validate(&self) -> AiResult<()> {
        if !is_valid_tool_name(&self.name) {
            return Err(AiError::invalid_params(format!(
                "Tool names must be 1-64 characters of letters, digits, '_', '-' or '.'; got '{}'.",
                self.name
            )));
        }
        if let Some(parameters) = &self.parameters
            && !parameters.is_object()
        {
            return Err(AiError::invalid_params(format!(
                "The parameters of tool '{}' must be a JSON Schema object.",
                self.name
            )));
        }
        if let Some(timeout) = self.timeout_seconds
            && !(0.0..=86_400.0).contains(&timeout)
        {
            return Err(AiError::invalid_params(format!(
                "The timeout of tool '{}' must be 0...86400 seconds; got {timeout}.",
                self.name
            )));
        }
        Ok(())
    }
}

impl fmt::Debug for AiTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiTool")
            .field("definition", &self.definition())
            .field("has_handler", &self.has_handler())
            .finish()
    }
}

/// The backend of [`AiTool::invoke`]'s detached responders: drops every line.
struct Detached;

impl AiBackend for Detached {
    fn kind(&self) -> crate::BackendKind {
        crate::BackendKind::Custom("detached")
    }

    fn start(&self, _inbox: crate::Inbox) -> AiResult<()> {
        Ok(())
    }

    fn send(&self, _line: &str) -> AiResult<()> {
        Ok(())
    }
}

/// Tool names: 1–64 of `A–Z a–z 0–9 _ - .`.
pub(crate) fn is_valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

/// What asked for a tool call: the `tool/call` params' context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOwner {
    /// A `session/respond` turn of this session.
    Session(String),
    /// An `npc/talk` turn of this NPC.
    Npc(String),
    /// A request without a conversation (`decision/decide`, `content/generate`, …).
    Request,
}

/// One `tool/call` from the bridge. Dereferences to its [`ToolCall`], so
/// `invocation.string("item")` reads an argument.
pub struct ToolInvocation {
    /// The call (id, tool name, arguments).
    pub call: ToolCall,
    /// Who asked.
    pub owner: ToolOwner,
    /// The request (turn) that caused the call.
    pub request: Option<RequestId>,
    /// The remaining `tool/call` params (`index` for `decision/decideMany`, extension fields).
    pub context: Map<String, Value>,
    pub(crate) responder: ToolResponder,
}

impl ToolInvocation {
    /// A handle to answer this call later (with [`ToolReply::Deferred`]). It can be stored (for
    /// example in a component) and used from any thread.
    pub fn responder(&self) -> ToolResponder {
        self.responder.clone()
    }
}

impl Deref for ToolInvocation {
    type Target = ToolCall;
    fn deref(&self) -> &ToolCall {
        &self.call
    }
}

impl fmt::Debug for ToolInvocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolInvocation")
            .field("call", &self.call)
            .field("owner", &self.owner)
            .field("request", &self.request)
            .field("rpc_id", &self.responder.rpc_id)
            .finish()
    }
}

/// A handler's answer.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolReply {
    /// Reply now with this output.
    Output(ToolOutput),
    /// The handler kept [`ToolInvocation::responder`] and replies later. If nobody replies,
    /// the bridge times the call out (the tool's timeout) and the model sees an error.
    Deferred,
}

impl From<ToolOutput> for ToolReply {
    fn from(output: ToolOutput) -> Self {
        Self::Output(output)
    }
}

impl From<String> for ToolReply {
    fn from(text: String) -> Self {
        Self::Output(ToolOutput::Text(text))
    }
}

impl From<&str> for ToolReply {
    fn from(text: &str) -> Self {
        Self::Output(ToolOutput::text(text))
    }
}

impl From<Value> for ToolReply {
    fn from(value: Value) -> Self {
        Self::Output(ToolOutput::Json(value))
    }
}

impl<T: Into<ToolOutput>, E: fmt::Display> From<Result<T, E>> for ToolReply {
    fn from(result: Result<T, E>) -> Self {
        match result {
            Ok(output) => Self::Output(output.into()),
            Err(error) => Self::Output(ToolOutput::Error(error.to_string())),
        }
    }
}

#[derive(Default)]
pub(crate) struct CallState {
    answered: AtomicBool,
    cancelled: AtomicBool,
}

/// Answers one `tool/call`. Cheap to clone, `Send + Sync`; the first reply wins.
#[derive(Clone)]
pub struct ToolResponder {
    backend: Arc<dyn AiBackend>,
    pub(crate) rpc_id: Value,
    pub(crate) state: Arc<CallState>,
}

impl ToolResponder {
    pub(crate) fn new(backend: Arc<dyn AiBackend>, rpc_id: Value) -> Self {
        Self {
            backend,
            rpc_id,
            state: Arc::default(),
        }
    }

    /// Sends the output. Returns `false` if the call was already answered or cancelled (the
    /// bridge would ignore the reply) or the backend is gone.
    pub fn reply(&self, output: impl Into<ToolOutput>) -> bool {
        if self.is_cancelled() || self.state.answered.swap(true, Ordering::AcqRel) {
            return false;
        }
        let line = rpc::result(&self.rpc_id, &output.into().to_tool_result());
        self.backend.send(&line).is_ok()
    }

    /// Answers with a JSON-RPC error (the model sees `Error: message`).
    pub fn fail(&self, message: impl Into<String>) -> bool {
        if self.is_cancelled() || self.state.answered.swap(true, Ordering::AcqRel) {
            return false;
        }
        let error = AiError::new(crate::AiErrorCode::Other("tool_error".into()), message);
        self.backend.send(&rpc::error(&self.rpc_id, &error)).is_ok()
    }

    /// Whether a reply was sent.
    pub fn is_answered(&self) -> bool {
        self.state.answered.load(Ordering::Acquire)
    }

    /// Whether the bridge no longer needs the output (`tool/cancel`: timeout, cancelled or
    /// finished turn). Stop the action if you can.
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn mark_cancelled(&self) {
        self.state.cancelled.store(true, Ordering::Release);
    }
}

impl fmt::Debug for ToolResponder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolResponder")
            .field("rpc_id", &self.rpc_id)
            .field("answered", &self.is_answered())
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_validate() {
        let tool = AiTool::new("open_gate", "Opens a gate.")
            .parameters(json!({"type": "object", "properties": {"gate": {"type": "string"}}}))
            .timeout_seconds(30.0);
        assert!(tool.validate().is_ok());
        assert_eq!(tool.definition()["timeoutSeconds"], 30.0);
        assert!(AiTool::new("bad name", "").validate().is_err());
        assert!(AiTool::new("", "").validate().is_err());
        assert!(AiTool::new("x".repeat(65), "").validate().is_err());
        assert!(AiTool::new("a", "").parameters(json!("nope")).validate().is_err());
        assert!(AiTool::new("a", "").timeout_seconds(-1.0).validate().is_err());
        assert!(AiTool::new("a", "").timeout_seconds(f64::NAN).validate().is_err());
        assert!(is_valid_tool_name("read_world_state.v2-b"));
    }

    #[test]
    fn replies_convert() {
        assert_eq!(ToolReply::from("ok"), ToolReply::Output(ToolOutput::text("ok")));
        assert_eq!(
            ToolReply::from(json!({"a": 1})),
            ToolReply::Output(ToolOutput::Json(json!({"a": 1})))
        );
        let failed: Result<String, String> = Err("no stock".into());
        assert_eq!(
            ToolReply::from(failed),
            ToolReply::Output(ToolOutput::error("no stock"))
        );
    }
}
