//! The [`Ai`] resource and [`AiPlugin`].

use crate::backend::{AiBackend, BackendChoice, BackendKind, Inbox, ScriptedBackend, UnsupportedBackend};
use crate::config::{
    Actor, ContentRequest, Created, DecisionRequest, NpcConfig, RespondOptions, SessionConfig, TalkOptions,
};
use crate::error::{AiError, AiErrorCode, AiResult};
use crate::events::{self, AiEvent, WorldChange};
use crate::rpc::{self, Message, RequestId};
use crate::tools::{AiTool, ToolHandler, ToolInvocation, ToolReply, ToolResponder};
use crate::types::{AgentResponse, Decision, DialogueTurn, ModelAvailability, ServerInfo, ToolCall, ToolOutput};
use futures::channel::oneshot;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use space3d_core::{App, Events, Plugin, Stage, SystemCtx};
use space3d_tasks::{Task, TasksPlugin};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Where the connection to the bridge stands.
#[derive(Clone, Debug, PartialEq)]
pub enum AiStatus {
    /// `initialize` was sent; requests can already be sent (they are queued in order).
    Starting,
    /// The bridge answered `initialize`.
    Ready(Box<ServerInfo>),
    /// No usable backend: every request fails with this error's code.
    Unavailable(AiError),
}

enum Pending {
    Task(oneshot::Sender<AiResult<Value>>),
    Initialize,
}

struct RegisteredTool {
    definition: Value,
    handler: Option<ToolHandler>,
}

/// The engine's connection to an on-device model bridge (a resource inserted by
/// [`AiPlugin`]).
///
/// Every method is non-blocking: it sends one JSON-RPC request and returns a
/// [`Task`] that a system polls (or hands to `AsyncQueue::track`). Streaming turns
/// additionally emit [`AiEvent`]s, and `tool/call` requests run the registered
/// [`AiTool`] handlers on the engine thread in `Stage::Update`. Requests are handled by the
/// bridge in the order they were sent, so `create_npc` followed by `talk` needs no waiting.
///
/// ```
/// use space3d_core::prelude::*;
/// use space3d_ai::{Ai, AiPlugin, NpcConfig, Persona, Script, ScriptStep, ScriptedBackend};
/// let mut app = App::new();
/// app.add_plugin(AiPlugin::scripted(ScriptedBackend::new()));
/// let ai = app.resources.expect_mut::<Ai>();
/// let script = Script::new().step(ScriptStep::text("[happy] Welcome!"));
/// let _created = ai.create_npc("mira", NpcConfig::new(Persona::new("Mira")).model(script));
/// let mut turn = ai.talk("mira", "Hello!");
/// let line = loop {
///     app.step_ticks(1);
///     if let Some(result) = turn.poll() { break result.unwrap().line; }
///     std::thread::sleep(std::time::Duration::from_millis(1));
/// };
/// assert_eq!(line, "Welcome!");
/// ```
pub struct Ai {
    backend: Arc<dyn AiBackend>,
    inbox: Inbox,
    next_id: u64,
    last_id: Option<RequestId>,
    pending: HashMap<u64, Pending>,
    // Mutex only so the resource is `Sync`; always used through `&mut self`.
    tools: Mutex<HashMap<String, RegisteredTool>>,
    queue: VecDeque<ToolInvocation>,
    deferred: HashMap<String, ToolResponder>,
    status: AiStatus,
    shut_down: bool,
}

impl Ai {
    /// Connects `backend`. If it cannot start (for example an Android app without the
    /// open-android-models AAR), an [`UnsupportedBackend`] explaining why takes its place.
    pub fn new(backend: Box<dyn AiBackend>) -> Self {
        let inbox = Inbox::new();
        let backend: Arc<dyn AiBackend> = match backend.start(inbox.clone()) {
            Ok(()) => Arc::from(backend),
            Err(error) => {
                tracing::warn!(backend = %backend.kind(), %error, "space3d-ai: backend failed to start");
                let fallback = UnsupportedBackend::new(format!(
                    "The {} backend failed to start: {}",
                    backend.kind(),
                    error.message
                ));
                fallback
                    .start(inbox.clone())
                    .expect("the unsupported backend always starts");
                Arc::new(fallback)
            }
        };
        Self {
            backend,
            inbox,
            next_id: 1,
            last_id: None,
            pending: HashMap::new(),
            tools: Mutex::new(HashMap::new()),
            queue: VecDeque::new(),
            deferred: HashMap::new(),
            status: AiStatus::Starting,
            shut_down: false,
        }
    }

    /// The connected backend's kind.
    pub fn backend_kind(&self) -> BackendKind {
        self.backend.kind()
    }

    /// Connection status (updated when `initialize` is answered).
    pub fn status(&self) -> &AiStatus {
        &self.status
    }

    /// The bridge's `initialize` result, once answered.
    pub fn server_info(&self) -> Option<&ServerInfo> {
        match &self.status {
            AiStatus::Ready(info) => Some(info),
            _ => None,
        }
    }

    /// The id of the most recent request, to match [`AiEvent`]s and `tool/call`s to it.
    pub fn last_request_id(&self) -> Option<RequestId> {
        self.last_id
    }

    /// Requests sent and not yet answered.
    pub fn pending_requests(&self) -> usize {
        self.pending.len()
    }

    // --------------------------------------------------------------------------------------
    // Tools

    /// Registers a tool (replacing one with the same name). Sessions, NPCs and requests name
    /// the tools they use; `tool/call`s for it run its handler.
    pub fn register_tool(&mut self, tool: AiTool) -> AiResult<()> {
        tool.validate()?;
        let definition = tool.definition();
        let name = tool.name().to_owned();
        self.tools.get_mut().insert(
            name,
            RegisteredTool {
                definition,
                handler: tool.handler,
            },
        );
        Ok(())
    }

    /// Removes a tool; returns whether it existed.
    pub fn unregister_tool(&mut self, name: &str) -> bool {
        self.tools.get_mut().remove(name).is_some()
    }

    /// A registered tool's protocol definition.
    pub fn tool_definition(&self, name: &str) -> Option<Value> {
        self.tools.lock().get(name).map(|t| t.definition.clone())
    }

    fn definitions(&self, names: &[String]) -> AiResult<Vec<Value>> {
        let tools = self.tools.lock();
        names
            .iter()
            .map(|name| {
                tools.get(name).map(|t| t.definition.clone()).ok_or_else(|| {
                    AiError::invalid_params(format!("Tool '{name}' is not registered (Ai::register_tool)."))
                })
            })
            .collect()
    }

    // --------------------------------------------------------------------------------------
    // Raw protocol

    /// Sends any request and returns its raw `result` (extensions, methods without a typed
    /// wrapper such as `npc/state` or `world/subscribe`).
    pub fn request(&mut self, method: &str, params: Value) -> Task<AiResult<Value>> {
        self.call(method, Ok(params), Ok)
    }

    /// Sends a notification (no response), e.g. `session/cancel`.
    pub fn notify(&mut self, method: &str, params: Value) -> AiResult<()> {
        self.backend.send(&rpc::notification(method, &params))
    }

    fn call<T, F>(&mut self, method: &str, params: AiResult<Value>, map: F) -> Task<AiResult<T>>
    where
        T: Send + 'static,
        F: FnOnce(Value) -> AiResult<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let sent = match params {
            Ok(params) => self.send_request(method, &params, Pending::Task(tx)),
            Err(error) => Err((tx, error)),
        };
        if let Err((tx, error)) = sent {
            let _ = tx.send(Err(error));
        }
        space3d_tasks::spawn(async move {
            match rx.await {
                Ok(Ok(value)) => map(value),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(AiError::cancelled(
                    "The Ai resource was dropped before the request finished.",
                )),
            }
        })
    }

    /// Sends a request and records how to route its response. On failure, hands the sender
    /// back so the caller can fail its task.
    fn send_request(
        &mut self,
        method: &str,
        params: &Value,
        pending: Pending,
    ) -> Result<(), (oneshot::Sender<AiResult<Value>>, AiError)> {
        let fail = |pending: Pending, error: AiError| match pending {
            Pending::Task(tx) => Err((tx, error)),
            Pending::Initialize => Ok(()),
        };
        if self.shut_down {
            return fail(
                pending,
                AiError::new(AiErrorCode::ShutDown, "The Ai connection was shut down."),
            );
        }
        let id = self.next_id;
        self.next_id += 1;
        self.last_id = Some(RequestId(id));
        self.pending.insert(id, pending);
        if let Err(error) = self.backend.send(&rpc::request(&json!(id), method, params)) {
            let pending = self.pending.remove(&id).expect("just inserted");
            if matches!(pending, Pending::Initialize) {
                self.set_unavailable(error);
                return Ok(());
            }
            return fail(pending, error);
        }
        Ok(())
    }

    /// Sends `initialize` (done by [`AiPlugin`]); the answer sets [`status`](Self::status).
    pub fn initialize(&mut self, client_name: &str, client_version: &str) {
        let params = json!({"client": {"name": client_name, "version": client_version}, "protocolVersion": "1.0"});
        let _ = self.send_request("initialize", &params, Pending::Initialize);
    }

    // --------------------------------------------------------------------------------------
    // Model

    /// `model/availability`: whether the system model can run now.
    pub fn availability(&mut self) -> Task<AiResult<ModelAvailability>> {
        self.call("model/availability", Ok(json!({})), decode)
    }

    // --------------------------------------------------------------------------------------
    // Sessions

    /// `session/create`: an agent with its own conversation.
    pub fn create_session(&mut self, id: impl Into<String>, config: SessionConfig) -> Task<AiResult<Created>> {
        let id = id.into();
        let tools = config.tools.clone();
        let params = self.definitions(&config.tools).map(|definitions| {
            let mut params = json!({"session": id, "tools": definitions, "model": config.model.to_json()});
            if let Some(instructions) = config.instructions {
                params["instructions"] = json!(instructions);
            }
            let options = serde_json::to_value(&config.options).unwrap_or_default();
            if options.as_object().is_some_and(|o| !o.is_empty()) {
                params["options"] = options;
            }
            if let Some(history) = config.history {
                params["history"] = history;
            }
            params
        });
        self.call("session/create", params, move |result| {
            Ok(Created {
                id: string_field(&result, "session")?,
                tools,
                warnings: strings(&result, "warnings"),
            })
        })
    }

    /// `session/respond` with streaming on: one turn.
    pub fn respond(&mut self, session: &str, prompt: impl Into<String>) -> Task<AiResult<AgentResponse>> {
        self.respond_with(session, prompt, RespondOptions::default())
    }

    /// `session/respond` with options (schema, overrides, streaming).
    pub fn respond_with(
        &mut self,
        session: &str,
        prompt: impl Into<String>,
        options: RespondOptions,
    ) -> Task<AiResult<AgentResponse>> {
        let mut params = json!({"session": session, "prompt": prompt.into(), "stream": options.stream});
        if let Some(schema) = options.schema {
            params["schema"] = schema;
        }
        if let Some(choice) = options.tool_choice {
            params["toolChoice"] = choice.to_json();
        }
        if let Some(rounds) = options.max_tool_rounds {
            params["maxToolRounds"] = json!(rounds);
        }
        if let Some(calls) = options.max_tool_calls {
            params["maxToolCalls"] = json!(calls);
        }
        if let Some(enabled) = options.enabled_tools {
            params["enabledTools"] = json!(enabled);
        }
        self.call("session/respond", Ok(params), decode)
    }

    /// `session/cancel`: cancels the running turn and queued operations; returns how many.
    pub fn cancel_session(&mut self, session: &str) -> Task<AiResult<u32>> {
        self.call("session/cancel", Ok(json!({"session": session})), cancelled_count)
    }

    /// `session/reset`: clears the conversation (keeps instructions and tools).
    pub fn reset_session(&mut self, session: &str) -> Task<AiResult<()>> {
        self.call("session/reset", Ok(json!({"session": session})), |_| Ok(()))
    }

    /// `session/delete`.
    pub fn delete_session(&mut self, session: &str) -> Task<AiResult<()>> {
        self.call("session/delete", Ok(json!({"session": session})), |_| Ok(()))
    }

    /// `session/setContextNote`: extra context (world facts, quest state) from the next turn.
    pub fn set_context_note(&mut self, session: &str, note: Option<String>) -> Task<AiResult<()>> {
        self.call(
            "session/setContextNote",
            Ok(json!({"session": session, "note": note})),
            |_| Ok(()),
        )
    }

    // --------------------------------------------------------------------------------------
    // NPCs

    /// `npc/create`: a persona with memory, tools and a conversation.
    pub fn create_npc(&mut self, id: impl Into<String>, config: NpcConfig) -> Task<AiResult<Created>> {
        let id = id.into();
        let params = self.definitions(&config.tools).map(|definitions| {
            let mut params = json!({
                "npc": id,
                "persona": serde_json::to_value(&config.persona).unwrap_or_default(),
                "tools": definitions,
                "model": config.model.to_json(),
            });
            if let Some(world) = config.world {
                params["world"] = json!(world);
            }
            if let Some(memory) = config.memory {
                params["memory"] = serde_json::to_value(memory).unwrap_or_default();
            }
            let options = serde_json::to_value(&config.options).unwrap_or_default();
            if options.as_object().is_some_and(|o| !o.is_empty()) {
                params["options"] = options;
            }
            params
        });
        self.call("npc/create", params, |result| {
            Ok(Created {
                id: string_field(&result, "npc")?,
                tools: strings(&result, "tools"),
                warnings: strings(&result, "warnings"),
            })
        })
    }

    /// `npc/talk` with streaming on: the player says `line` (`""` = nothing).
    pub fn talk(&mut self, npc: &str, line: impl Into<String>) -> Task<AiResult<DialogueTurn>> {
        self.talk_with(npc, line, TalkOptions::default())
    }

    /// `npc/talk` with context and options.
    pub fn talk_with(
        &mut self,
        npc: &str,
        line: impl Into<String>,
        options: TalkOptions,
    ) -> Task<AiResult<DialogueTurn>> {
        let mut params = json!({"npc": npc, "line": line.into(), "stream": options.stream});
        if let Some(context) = options.context {
            params["context"] = context;
        }
        if let Some(choice) = options.tool_choice {
            params["toolChoice"] = choice.to_json();
        }
        self.call("npc/talk", Ok(params), decode)
    }

    /// `npc/bark`: a short ambient line (no history, memory or tools).
    pub fn bark(&mut self, npc: &str, situation: impl Into<Value>) -> Task<AiResult<String>> {
        let params = json!({"npc": npc, "situation": situation.into()});
        self.call("npc/bark", Ok(params), |result| string_field(&result, "line"))
    }

    /// `npc/cancel`: cancels the running turn and queued requests; returns how many.
    pub fn cancel_npc(&mut self, npc: &str) -> Task<AiResult<u32>> {
        self.call("npc/cancel", Ok(json!({"npc": npc})), cancelled_count)
    }

    /// `npc/reset`: clears the conversation (and the memory if `clear_memory`).
    pub fn reset_npc(&mut self, npc: &str, clear_memory: bool) -> Task<AiResult<()>> {
        self.call(
            "npc/reset",
            Ok(json!({"npc": npc, "clearMemory": clear_memory})),
            |_| Ok(()),
        )
    }

    /// `npc/delete`.
    pub fn delete_npc(&mut self, npc: &str) -> Task<AiResult<()>> {
        self.call("npc/delete", Ok(json!({"npc": npc})), |_| Ok(()))
    }

    // --------------------------------------------------------------------------------------
    // Decisions and content

    /// `decision/decide`: picks one option (enemy tactics, companion reactions, haggling).
    pub fn decide(&mut self, request: DecisionRequest) -> Task<AiResult<Decision>> {
        let params = self.definitions(&request.tools).map(|definitions| {
            let mut params = json!({
                "situation": request.situation,
                "options": request.options,
                "model": request.model.to_json(),
            });
            if !definitions.is_empty() {
                params["tools"] = json!(definitions);
            }
            match request.actor {
                Some(Actor::Persona(persona)) => params["actor"] = serde_json::to_value(persona).unwrap_or_default(),
                Some(Actor::Npc(npc)) => params["actor"] = json!(npc),
                None => {}
            }
            let optional = [
                ("context", request.context),
                ("toolChoice", request.tool_choice.map(|c| c.to_json())),
                ("fallbackOptionID", request.fallback_option_id.map(Value::from)),
                ("instructions", request.instructions.map(Value::from)),
                ("temperature", request.temperature.map(Value::from)),
                ("maxToolRounds", request.max_tool_rounds.map(Value::from)),
                ("toolTimeoutSeconds", request.tool_timeout_seconds.map(Value::from)),
            ];
            for (key, value) in optional {
                if let Some(value) = value {
                    params[key] = value;
                }
            }
            params
        });
        self.call("decision/decide", params, decode)
    }

    /// `content/generate`: JSON matching the request's schema.
    pub fn generate_content(&mut self, request: ContentRequest) -> Task<AiResult<Value>> {
        let params = self.definitions(&request.tools).map(|definitions| {
            let mut params =
                json!({"prompt": request.prompt, "schema": request.schema, "model": request.model.to_json()});
            if !definitions.is_empty() {
                params["tools"] = json!(definitions);
            }
            let optional = [
                ("instructions", request.instructions.map(Value::from)),
                ("context", request.context),
                ("temperature", request.temperature.map(Value::from)),
            ];
            for (key, value) in optional {
                if let Some(value) = value {
                    params[key] = value;
                }
            }
            params
        });
        self.call("content/generate", params, |mut result| {
            result
                .get_mut("content")
                .map(Value::take)
                .ok_or_else(|| AiError::protocol("content/generate returned no 'content'."))
        })
    }

    // --------------------------------------------------------------------------------------
    // World state

    /// `world/create`: a shared JSON blackboard (NPCs created with `world` can read it).
    pub fn create_world(&mut self, world: &str, state: Value) -> Task<AiResult<u64>> {
        self.call("world/create", Ok(json!({"world": world, "state": state})), version)
    }

    /// `world/get`: the value at a dot path (`None` when absent).
    pub fn world_get(&mut self, world: &str, path: &str) -> Task<AiResult<Option<Value>>> {
        self.call("world/get", Ok(json!({"world": world, "path": path})), |result| {
            let exists = result.get("exists").and_then(Value::as_bool).unwrap_or(false);
            Ok(exists.then(|| result.get("value").cloned().unwrap_or(Value::Null)))
        })
    }

    /// `world/set`: writes a value at a dot path; returns the new version.
    pub fn world_set(&mut self, world: &str, path: &str, value: Value) -> Task<AiResult<u64>> {
        self.call(
            "world/set",
            Ok(json!({"world": world, "path": path, "value": value})),
            version,
        )
    }

    /// `world/merge`: applies an RFC 7386 merge patch at a path; returns the new version.
    pub fn world_merge(&mut self, world: &str, path: &str, patch: Value) -> Task<AiResult<u64>> {
        self.call(
            "world/merge",
            Ok(json!({"world": world, "path": path, "patch": patch})),
            version,
        )
    }

    // --------------------------------------------------------------------------------------
    // Lifecycle

    /// `shutdown`: cancels every turn and pending `tool/call`, deletes everything; later
    /// requests fail with `shut_down`.
    pub fn shutdown(&mut self) -> Task<AiResult<()>> {
        let task = self.call("shutdown", Ok(json!({})), |_| Ok(()));
        self.shut_down = true;
        task
    }

    fn set_unavailable(&mut self, error: AiError) {
        self.status = AiStatus::Unavailable(error);
    }

    // --------------------------------------------------------------------------------------
    // Frame work

    /// Polls the backend and routes every line it delivered: responses complete tasks,
    /// notifications become [`AiEvent`]s, and `tool/call`s are queued for
    /// [`run_tools`](Self::run_tools). Called by [`AiPlugin`] in `Stage::PreUpdate`.
    pub fn pump(&mut self, events: &mut Events<AiEvent>) {
        self.backend.poll();
        for line in self.inbox.drain() {
            match rpc::parse(&line) {
                Ok(message) => self.route(message, events),
                Err(error) => {
                    tracing::warn!(%error, "space3d-ai: unreadable line from the backend");
                    events.send(AiEvent::BackendError(error));
                }
            }
        }
        self.deferred.retain(|_, r| !r.is_answered() && !r.is_cancelled());
    }

    fn route(&mut self, message: Message, events: &mut Events<AiEvent>) {
        match message {
            Message::Response { id, outcome } => {
                let pending = id.as_u64().and_then(|id| self.pending.remove(&id));
                match pending {
                    Some(Pending::Task(tx)) => {
                        let _ = tx.send(outcome);
                    }
                    Some(Pending::Initialize) => match outcome.and_then(decode::<ServerInfo>) {
                        Ok(info) => {
                            tracing::info!(server = %info.server.name, version = %info.server.version,
                                available = info.model.available, "space3d-ai: bridge ready");
                            self.status = AiStatus::Ready(Box::new(info.clone()));
                            events.send(AiEvent::Ready(Box::new(info)));
                        }
                        Err(error) => {
                            tracing::info!(%error, "space3d-ai: no model backend");
                            self.set_unavailable(error.clone());
                            events.send(AiEvent::Unavailable(error));
                        }
                    },
                    None => {
                        if let Err(error) = outcome {
                            tracing::warn!(%error, %id, "space3d-ai: error response to an unknown request");
                            events.send(AiEvent::BackendError(error));
                        }
                    }
                }
            }
            Message::Request { id, method, params } => {
                if method == "tool/call" {
                    self.enqueue_tool_call(id, params);
                } else {
                    let error = AiError::new(
                        AiErrorCode::MethodNotFound,
                        format!("The engine does not serve '{method}'."),
                    )
                    .with_data("method", method);
                    let _ = self.backend.send(&rpc::error(&id, &error));
                }
            }
            Message::Notification { method, params } => self.notification(method, params, events),
        }
    }

    fn notification(&mut self, method: String, params: Value, events: &mut Events<AiEvent>) {
        let request = params.get("requestId").and_then(RequestId::from_json);
        let text = |key: &str| params.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let event = params.get("event").cloned().unwrap_or(Value::Null);
        match method.as_str() {
            "session/event" => events.send(AiEvent::Session {
                session: text("session"),
                request,
                event: events::turn_event(&event),
            }),
            "npc/event" => events.send(AiEvent::Npc {
                npc: text("npc"),
                request,
                event: events::npc_event(&event),
            }),
            "tool/cancel" => {
                let rpc_id = params.get("id").map(id_key).unwrap_or_default();
                if let Some(responder) = self.deferred.remove(&rpc_id) {
                    responder.mark_cancelled();
                }
                for queued in self.queue.iter().filter(|q| id_key(&q.responder.rpc_id) == rpc_id) {
                    queued.responder.mark_cancelled();
                }
                events.send(AiEvent::ToolCancelled {
                    rpc_id,
                    call_id: text("callId"),
                    owner: events::owner_of(&params),
                    request,
                    reason: text("reason"),
                });
            }
            "world/changed" => match serde_json::from_value::<WorldChange>(params.clone()) {
                Ok(change) => events.send(AiEvent::WorldChanged(change)),
                Err(_) => events.send(AiEvent::Notification { method, params }),
            },
            _ => events.send(AiEvent::Notification { method, params }),
        }
    }

    fn enqueue_tool_call(&mut self, id: Value, params: Value) {
        let responder = ToolResponder::new(self.backend.clone(), id);
        let call = params.get("call").cloned().map(serde_json::from_value::<ToolCall>);
        let Some(Ok(call)) = call else {
            responder.reply(ToolOutput::error("The tool/call request had no valid 'call'."));
            return;
        };
        let mut context: Map<String, Value> = params.as_object().cloned().unwrap_or_default();
        for key in ["call", "requestId", "session", "npc"] {
            context.remove(key);
        }
        self.queue.push_back(ToolInvocation {
            owner: events::owner_of(&params),
            request: params.get("requestId").and_then(RequestId::from_json),
            call,
            context,
            responder,
        });
    }

    /// `tool/call`s waiting for [`run_tools`](Self::run_tools).
    pub fn queued_tool_calls(&self) -> usize {
        self.queue.len()
    }

    /// Runs the handlers of queued `tool/call`s with full access to `ctx` and replies. Called by
    /// [`AiPlugin`] in `Stage::Update`; the [`Ai`] resource must be in `ctx.resources`.
    pub fn run_tools(ctx: &mut SystemCtx) {
        let Some(ai) = ctx.resources.get_mut::<Ai>() else {
            return;
        };
        let calls = std::mem::take(&mut ai.queue);
        for invocation in calls {
            if invocation.responder.is_cancelled() {
                continue;
            }
            let name = invocation.call.name.clone();
            let handler = ctx
                .resources
                .get_mut::<Ai>()
                .and_then(|ai| ai.tools.get_mut().get_mut(&name).and_then(|t| t.handler.take()));
            let reply = match handler {
                Some(mut handler) => {
                    let reply = handler(ctx, &invocation);
                    if let Some(ai) = ctx.resources.get_mut::<Ai>()
                        && let Some(tool) = ai.tools.get_mut().get_mut(&name)
                        && tool.handler.is_none()
                    {
                        tool.handler = Some(handler);
                    }
                    reply
                }
                None => ToolReply::Output(ToolOutput::error(format!(
                    "No handler is registered for tool '{name}'."
                ))),
            };
            match reply {
                ToolReply::Output(output) => {
                    invocation.responder.reply(output);
                }
                ToolReply::Deferred => {
                    if !invocation.responder.is_answered()
                        && let Some(ai) = ctx.resources.get_mut::<Ai>()
                    {
                        ai.deferred
                            .insert(id_key(&invocation.responder.rpc_id), invocation.responder.clone());
                    }
                }
            }
        }
    }
}

impl Drop for Ai {
    fn drop(&mut self) {
        self.backend.shutdown();
    }
}

impl std::fmt::Debug for Ai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ai")
            .field("backend", &self.backend.kind())
            .field("status", &self.status)
            .field("pending", &self.pending.len())
            .field("queued_tool_calls", &self.queue.len())
            .finish()
    }
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> AiResult<T> {
    serde_json::from_value(value).map_err(|e| AiError::protocol(format!("Unexpected result shape: {e}.")))
}

fn string_field(result: &Value, key: &str) -> AiResult<String> {
    result
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| AiError::protocol(format!("The result has no string '{key}'.")))
}

fn strings(result: &Value, key: &str) -> Vec<String> {
    result
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn cancelled_count(result: Value) -> AiResult<u32> {
    Ok(result
        .get("cancelled")
        .and_then(Value::as_u64)
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)))
}

fn version(result: Value) -> AiResult<u64> {
    result
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| AiError::protocol("The world result has no 'version'."))
}

/// Adds the [`Ai`] resource, `Events<AiEvent>` and two systems: `ai::pump`
/// (`Stage::PreUpdate`: poll the backend, route responses, events and `tool/call`s) and
/// `ai::run_tools` (`Stage::Update`: run tool handlers with ECS access, reply). Adds
/// [`TasksPlugin`] too.
///
/// ```
/// use space3d_core::prelude::*;
/// use space3d_ai::{AiPlugin, BackendChoice, ScriptedBackend};
/// let mut app = App::new();
/// // The platform's model where there is one, a scripted model elsewhere (web, Linux, CI).
/// app.add_plugin(AiPlugin::new(BackendChoice::auto_or(ScriptedBackend::new())));
/// ```
pub struct AiPlugin {
    backend: Mutex<Option<BackendChoice>>,
    tools: Mutex<Vec<AiTool>>,
    client: (String, String),
}

impl AiPlugin {
    /// A plugin with this backend choice.
    pub fn new(choice: BackendChoice) -> Self {
        Self {
            backend: Mutex::new(Some(choice)),
            tools: Mutex::new(Vec::new()),
            client: ("space3d".into(), env!("CARGO_PKG_VERSION").into()),
        }
    }

    /// The platform's backend ([`BackendChoice::Auto`]).
    pub fn auto() -> Self {
        Self::new(BackendChoice::Auto)
    }

    /// A [`ScriptedBackend`].
    pub fn scripted(backend: ScriptedBackend) -> Self {
        Self::new(BackendChoice::backend(backend))
    }

    /// Registers a tool at startup.
    pub fn with_tool(self, tool: AiTool) -> Self {
        self.tools.lock().push(tool);
        self
    }

    /// The `client` sent in `initialize` (default `space3d` and this crate's version).
    pub fn client(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.client = (name.into(), version.into());
        self
    }
}

impl Default for AiPlugin {
    fn default() -> Self {
        Self::auto()
    }
}

impl Plugin for AiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(TasksPlugin);
        app.add_event::<AiEvent>();
        let backend = self.backend.lock().take().unwrap_or_default().resolve();
        let mut ai = Ai::new(backend);
        for tool in self.tools.lock().drain(..) {
            let name = tool.name().to_owned();
            if let Err(error) = ai.register_tool(tool) {
                tracing::error!(tool = %name, %error, "space3d-ai: invalid tool");
            }
        }
        ai.initialize(&self.client.0, &self.client.1);
        app.insert_resource(ai);
        app.add_system_named(Stage::PreUpdate, "ai::pump", |ctx| {
            ctx.resources
                .scope::<Ai, _>(|resources, ai| ai.pump(resources.init::<Events<AiEvent>>()));
        });
        app.add_system_named(Stage::Update, "ai::run_tools", Ai::run_tools);
    }
}
