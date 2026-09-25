//! The scripted bridge: JSON-RPC handling, methods, jobs and per-session / per-NPC queues.

use super::params::{Params, ToolSpec, check_id, check_schema, tools};
use super::turn::{CHANGE_RELATIONSHIP, Goal, LocalTools, Progress, REMEMBER_FACT, Turn};
use super::world::{Change, Worlds};
use super::{Config, ModelRequest, RequestKind, Responder, SystemModel};
use crate::backend::Inbox;
use crate::error::{AiError, AiErrorCode, AiResult};
use crate::rpc;
use crate::script::{Script, ScriptStep};
use crate::types::{Emotion, ReplyFormat, ToolChoice, ToolOutput};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

const MAX_SESSIONS: usize = 64;
const MAX_NPCS: usize = 128;
const MAX_SAFE_ID: f64 = 9_007_199_254_740_991.0;

/// Every method this backend serves (listed in `initialize`).
const METHODS: &[&str] = &[
    "initialize",
    "ping",
    "model/availability",
    "shutdown",
    "session/create",
    "session/respond",
    "session/cancel",
    "session/reset",
    "session/delete",
    "session/list",
    "session/setInstructions",
    "session/setContextNote",
    "session/setTools",
    "npc/create",
    "npc/talk",
    "npc/bark",
    "npc/cancel",
    "npc/reset",
    "npc/delete",
    "npc/list",
    "decision/decide",
    "decision/decideMany",
    "content/generate",
    "world/create",
    "world/get",
    "world/set",
    "world/merge",
    "world/remove",
    "world/snapshot",
    "world/delete",
    "world/list",
    "world/subscribe",
    "world/unsubscribe",
];

/// Protocol methods this backend does not implement (answered `-32601` with an explanation).
const UNSUPPORTED: &[&str] = &[
    "session/transcript",
    "session/compact",
    "schema/validate",
    "tools/validate",
    "npc/state",
    "npc/restore",
    "npc/update",
];

const DEFAULT_FALLBACK_LINES: [&str; 3] = [
    "Let's talk about something else.",
    "I'd rather not speak of that.",
    "Hmm. Ask me something else.",
];

// ------------------------------------------------------------------------------------------
// Models

/// A scripted model shared by a session/NPC and its turns.
pub(super) type SharedModel = Arc<Mutex<ScriptModel>>;

pub(super) enum ScriptModel {
    Steps {
        steps: VecDeque<ScriptStep>,
        fallback: ScriptStep,
    },
    Responder(Arc<Responder>),
    Unavailable(String),
}

impl ScriptModel {
    fn script(script: &Script) -> Self {
        Self::Steps {
            steps: script.steps.iter().cloned().collect(),
            fallback: script
                .fallback
                .clone()
                .unwrap_or_else(|| ScriptStep::text("(script exhausted)")),
        }
    }

    pub(super) fn next(&mut self, request: &ModelRequest) -> AiResult<ScriptStep> {
        match self {
            Self::Steps { steps, fallback } => Ok(steps.pop_front().unwrap_or_else(|| fallback.clone())),
            Self::Responder(responder) => Ok(responder(request)),
            Self::Unavailable(reason) => Err(AiError::new(AiErrorCode::ModelUnavailable, reason.clone())),
        }
    }
}

// ------------------------------------------------------------------------------------------
// State

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum QueueKey {
    Session(String),
    Npc(String),
}

struct SessionState {
    instructions: Option<String>,
    tools: Vec<ToolSpec>,
    choice: ToolChoice,
    max_rounds: u32,
    max_calls: u32,
    enabled: Option<Vec<String>>,
    tool_timeout: Option<Option<Duration>>,
    model: SharedModel,
    model_kind: String,
    entries: u64,
    created_at: String,
}

pub(super) struct NpcOpts {
    pub reply_format: ReplyFormat,
    pub grounding: Option<String>,
    pub choice: ToolChoice,
    pub max_rounds: u32,
    pub max_calls: u32,
    pub tool_timeout: Option<Option<Duration>>,
    pub player_option_count: usize,
    pub can_end_conversation: bool,
    pub fallback_on_guardrail: bool,
    pub fallback_lines: Vec<String>,
    pub emotions: Option<Vec<Emotion>>,
    pub world_readable: Vec<String>,
    pub world_writable: Vec<String>,
    pub remember_fact: bool,
    pub change_relationship: bool,
    pub max_facts: usize,
    pub max_relationship_change: i64,
}

pub(super) struct NpcState {
    pub name: String,
    role: String,
    pub default_emotion: Emotion,
    tools: Vec<ToolSpec>,
    world: Option<String>,
    pub opts: NpcOpts,
    pub relationship: i32,
    pub facts: Vec<String>,
    model: SharedModel,
    model_kind: String,
    turn_count: u64,
    created_at: String,
    pub fallback_index: usize,
}

type Op = Box<dyn FnOnce(&mut Engine) -> AiResult<Value> + Send>;

enum Work {
    Turn(Box<Turn>),
    Op(Option<Op>),
    Many(Box<Many>),
}

struct Job {
    request: Option<Value>,
    queue: Option<QueueKey>,
    work: Work,
}

/// A validated decision (for `decision/decide` and each `decideMany` request).
struct DecisionSpec {
    situation: String,
    ids: Vec<String>,
    fallback: Option<String>,
    tools: Vec<ToolSpec>,
    choice: ToolChoice,
    max_rounds: u32,
    tool_timeout: Option<Duration>,
}

/// `decision/decideMany`: decisions run one after another in request order.
struct Many {
    specs: VecDeque<DecisionSpec>,
    model: SharedModel,
    current: Option<Box<Turn>>,
    results: Vec<Value>,
    /// The `decideMany` request id (the decisions' `tool/call`s carry it).
    request_id: Value,
}

enum Reply {
    Now(Value),
    Later,
}

// ------------------------------------------------------------------------------------------
// Engine

#[derive(Default)]
pub(super) struct Engine {
    pub(super) config: Config,
    inbox: Option<Inbox>,
    sessions: HashMap<String, SessionState>,
    session_order: Vec<String>,
    pub(super) npcs: HashMap<String, NpcState>,
    npc_order: Vec<String>,
    pub(super) worlds: Worlds,
    jobs: BTreeMap<u64, Job>,
    queues: HashMap<QueueKey, VecDeque<u64>>,
    /// `tool/call` id → the job waiting for it.
    pub(super) tool_calls: HashMap<String, u64>,
    next_job: u64,
    pub(super) next_tool: u64,
    next_session: u64,
    next_npc: u64,
    call_counter: u64,
    shut_down: bool,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("sessions", &self.sessions.len())
            .field("npcs", &self.npcs.len())
            .field("jobs", &self.jobs.len())
            .field("shut_down", &self.shut_down)
            .finish()
    }
}

/// Current UTC time as ISO 8601 (`2026-09-25T12:00:00Z`).
pub(super) fn iso_now() -> String {
    let secs = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn not_found_session(id: &str) -> AiError {
    AiError::new(AiErrorCode::SessionNotFound, format!("No session '{id}'.")).with_data("session", id)
}

fn not_found_npc(id: &str) -> AiError {
    AiError::new(AiErrorCode::NpcNotFound, format!("No NPC '{id}'.")).with_data("npc", id)
}

fn shut_down_error() -> AiError {
    AiError::new(AiErrorCode::ShutDown, "The bridge was shut down.")
}

impl Engine {
    pub(super) fn is_started(&self) -> bool {
        self.inbox.is_some()
    }

    pub(super) fn start(&mut self, inbox: Inbox) {
        self.inbox = Some(inbox);
    }

    /// Destroys the bridge: work is dropped without responses, nothing is delivered afterwards.
    pub(super) fn close(&mut self) {
        self.inbox = None;
        self.jobs.clear();
        self.queues.clear();
        self.tool_calls.clear();
        self.shut_down = true;
    }

    pub(super) fn emit(&self, line: String) {
        if let Some(inbox) = &self.inbox {
            inbox.deliver(line);
        }
    }

    fn respond(&self, id: &Value, result: AiResult<Value>) {
        match result {
            Ok(value) => self.emit(rpc::result(id, &value)),
            Err(error) => self.emit(rpc::error(id, &error)),
        }
    }

    /// A fresh `call_…` id (24 alphanumerics, like the libraries').
    pub(super) fn call_id(&mut self) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        self.call_counter += 1;
        let mut state = self.call_counter.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        let mut id = String::with_capacity(29);
        id.push_str("call_");
        for _ in 0..24 {
            // splitmix64
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            id.push(ALPHABET[(z % ALPHABET.len() as u64) as usize] as char);
        }
        id
    }

    pub(super) fn notify_world(&self, world: &str, changes: &[Change]) {
        for params in self.worlds.notifications(world, changes) {
            self.emit(rpc::notification("world/changed", &params));
        }
    }

    fn model(&self, value: Option<&Value>) -> AiResult<(SharedModel, String)> {
        let system = || -> (SharedModel, String) {
            let model = match &self.config.system {
                SystemModel::None => ScriptModel::Unavailable(
                    "The scripted backend has no system model: pass a scripted `model`, or configure \
                     ScriptedBackend::with_system_script / with_system_model."
                        .into(),
                ),
                SystemModel::Script(script) => ScriptModel::script(script),
                SystemModel::Responder(responder) => ScriptModel::Responder(responder.clone()),
            };
            (Arc::new(Mutex::new(model)), "system".into())
        };
        let unknown = |kind: &str| {
            AiError::invalid_params(format!(
                "Unknown model type '{kind}'. Supported: \"system\", \"scripted\"."
            ))
        };
        match value {
            None | Some(Value::Null) => Ok(system()),
            Some(Value::String(kind)) => match kind.as_str() {
                "system" => Ok(system()),
                "scripted" => Ok((
                    Arc::new(Mutex::new(ScriptModel::script(&Script::new()))),
                    "scripted".into(),
                )),
                other => Err(unknown(other)),
            },
            Some(Value::Object(object)) => match object.get("type").and_then(Value::as_str) {
                Some("system") => Ok(system()),
                Some("scripted") => {
                    let script = Script::from_json(value.unwrap_or(&Value::Null), "model")?;
                    Ok((Arc::new(Mutex::new(ScriptModel::script(&script))), "scripted".into()))
                }
                Some(other) => Err(unknown(other)),
                None => Err(AiError::invalid_params("Missing required parameter 'model.type'.")),
            },
            Some(_) => Err(AiError::invalid_params(
                "'model' must be \"system\" or an object with a 'type'.",
            )),
        }
    }

    // --------------------------------------------------------------------------------------
    // Incoming messages

    pub(super) fn receive(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                let error = AiError::new(AiErrorCode::ParseError, format!("Parse error: {e}."));
                return self.respond(&Value::Null, Err(error));
            }
        };
        let invalid = |message: &str| AiError::new(AiErrorCode::InvalidMessage, message);
        let Value::Object(object) = value else {
            let message = if value.is_array() {
                "Batches are not supported."
            } else {
                "A message must be an object."
            };
            return self.respond(&Value::Null, Err(invalid(message)));
        };
        let id = object.get("id").cloned();
        let id_ok = match &id {
            None => true,
            Some(Value::String(_)) => true,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f.is_finite() && f.abs() <= MAX_SAFE_ID),
            Some(_) => false,
        };
        if !id_ok {
            return self.respond(
                &Value::Null,
                Err(invalid("Invalid 'id': use a string or an integer up to 2^53-1.")),
            );
        }
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return self.respond(
                &id.unwrap_or(Value::Null),
                Err(invalid("\"jsonrpc\": \"2.0\" is required.")),
            );
        }
        match (object.get("method"), id) {
            (Some(Value::String(method)), id) => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                self.request(id, method, &params);
            }
            (Some(_), id) => self.respond(&id.unwrap_or(Value::Null), Err(invalid("'method' must be a string."))),
            (None, Some(id)) if object.contains_key("result") || object.contains_key("error") => {
                let outcome = match object.get("error").filter(|e| !e.is_null()) {
                    Some(error) => Err(AiError::from_rpc(error)),
                    None => Ok(object.get("result").cloned().unwrap_or(Value::Null)),
                };
                self.tool_response(&id, outcome);
            }
            (None, id) => self.respond(&id.unwrap_or(Value::Null), Err(invalid("A request needs a 'method'."))),
        }
        self.run();
    }

    /// Advances delays, pacing and tool timeouts.
    pub(super) fn tick(&mut self) {
        if self.inbox.is_some() {
            self.run();
        }
    }

    fn request(&mut self, id: Option<Value>, method: &str, params: &Value) {
        if self.shut_down {
            if let Some(id) = id {
                self.respond(&id, Err(shut_down_error()));
            }
            return;
        }
        if method == "shutdown" {
            let jobs: Vec<u64> = self.jobs.keys().copied().collect();
            for job in jobs {
                self.cancel_job(job, "The bridge is shutting down.");
            }
            self.sessions.clear();
            self.session_order.clear();
            self.npcs.clear();
            self.npc_order.clear();
            self.worlds = Worlds::default();
            if let Some(id) = &id {
                self.respond(id, Ok(json!({})));
            }
            self.shut_down = true;
            return;
        }
        let outcome = Params::of(params).and_then(|p| self.dispatch(method, &p, id.as_ref()));
        match (id, outcome) {
            (Some(id), Ok(Reply::Now(result))) => self.respond(&id, Ok(result)),
            (Some(id), Err(error)) => self.respond(&id, Err(error)),
            _ => {}
        }
    }

    fn dispatch(&mut self, method: &str, params: &Params, id: Option<&Value>) -> AiResult<Reply> {
        let request_id = id.cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => self.initialize(params).map(Reply::Now),
            "ping" => Ok(Reply::Now(json!({}))),
            "model/availability" => Ok(Reply::Now(self.availability())),
            "session/create" => self.session_create(params).map(Reply::Now),
            "session/respond" => self.session_respond(params, request_id, id.is_some()),
            "session/cancel" => {
                let session = self.session_id(params)?;
                let cancelled = self.cancel_queue(&QueueKey::Session(session.clone()));
                Ok(Reply::Now(json!({"session": session, "cancelled": cancelled})))
            }
            "session/delete" => {
                let session = self.session_id(params)?;
                self.cancel_queue(&QueueKey::Session(session.clone()));
                self.sessions.remove(&session);
                self.session_order.retain(|s| *s != session);
                Ok(Reply::Now(json!({"session": session, "deleted": true})))
            }
            "session/list" => Ok(Reply::Now(self.session_list())),
            "session/reset" | "session/setInstructions" | "session/setContextNote" | "session/setTools" => {
                self.session_op(method, params, id)
            }
            "npc/create" => self.npc_create(params).map(Reply::Now),
            "npc/talk" => self.npc_talk(params, request_id, id.is_some()),
            "npc/bark" => self.npc_bark(params, request_id, id.is_some()),
            "npc/cancel" => {
                let npc = self.npc_id(params)?;
                let cancelled = self.cancel_queue(&QueueKey::Npc(npc.clone()));
                Ok(Reply::Now(json!({"npc": npc, "cancelled": cancelled})))
            }
            "npc/delete" => {
                let npc = self.npc_id(params)?;
                self.cancel_queue(&QueueKey::Npc(npc.clone()));
                self.npcs.remove(&npc);
                self.npc_order.retain(|n| *n != npc);
                Ok(Reply::Now(json!({"npc": npc, "deleted": true})))
            }
            "npc/list" => Ok(Reply::Now(self.npc_list())),
            "npc/reset" => {
                let npc = self.npc_id(params)?;
                let clear = params.opt_bool("clearMemory")?.unwrap_or(false);
                let op_npc = npc.clone();
                let op: Op = Box::new(move |engine: &mut Engine| {
                    if let Some(state) = engine.npcs.get_mut(&op_npc) {
                        state.turn_count = 0;
                        if clear {
                            state.facts.clear();
                            state.relationship = 0;
                        }
                    }
                    Ok(json!({"npc": op_npc}))
                });
                self.enqueue(id.cloned(), Some(QueueKey::Npc(npc)), Work::Op(Some(op)));
                Ok(Reply::Later)
            }
            "decision/decide" => self.decide(params, request_id, id.is_some()),
            "decision/decideMany" => self.decide_many(params, request_id, id.is_some()),
            "content/generate" => self.content(params, request_id, id.is_some()),
            m if m.starts_with("world/") => {
                let npcs = &self.npcs;
                let (result, changes) = self.worlds.handle(m, params, |world| {
                    npcs.values().filter(|n| n.world.as_deref() == Some(world)).count()
                })?;
                if let Some((world, changes)) = changes {
                    self.notify_world(&world, &changes);
                }
                Ok(Reply::Now(result))
            }
            m if UNSUPPORTED.contains(&m) => Err(AiError::new(
                AiErrorCode::MethodNotFound,
                format!("Method '{m}' is not supported by the scripted backend."),
            )
            .with_data("method", m)),
            m => Err(
                AiError::new(AiErrorCode::MethodNotFound, format!("Method '{m}' not found.")).with_data("method", m),
            ),
        }
    }

    fn initialize(&mut self, params: &Params) -> AiResult<Value> {
        if let Some(version) = params.opt_string("protocolVersion")?
            && version.split('.').next() != Some("1")
        {
            return Err(AiError::invalid_params(format!(
                "Unsupported protocolVersion '{version}'; this bridge speaks 1.0."
            )));
        }
        Ok(json!({
            "protocolVersion": "1.0",
            "server": {"name": "space3d-scripted", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "methods": METHODS,
                "notifications": ["session/event", "tool/cancel", "npc/event", "world/changed"],
                "clientRequests": ["tool/call"],
                "streaming": true, "clientTools": true, "structuredOutput": true,
                "models": ["system", "scripted"], "maxSessions": MAX_SESSIONS, "batch": false,
            },
            "model": self.availability(),
        }))
    }

    fn availability(&self) -> Value {
        match &self.config.system {
            SystemModel::None => json!({"available": false, "reason": "unknown", "supportedLanguages": [],
                "detail": "The scripted backend has no system model; scripted models work."}),
            _ => json!({"available": true, "contextSize": 4096, "variant": "scripted", "supportedLanguages": []}),
        }
    }

    // --------------------------------------------------------------------------------------
    // Jobs

    fn enqueue(&mut self, request: Option<Value>, queue: Option<QueueKey>, work: Work) -> u64 {
        self.next_job += 1;
        let id = self.next_job;
        if let Some(key) = &queue {
            self.queues.entry(key.clone()).or_default().push_back(id);
        }
        self.jobs.insert(id, Job { request, queue, work });
        id
    }

    fn next_job_id(&self) -> u64 {
        self.next_job + 1
    }

    fn is_runnable(&self, id: u64, job: &Job) -> bool {
        match &job.queue {
            None => true,
            Some(key) => self.queues.get(key).and_then(VecDeque::front) == Some(&id),
        }
    }

    /// Runs every runnable job until nothing changes.
    fn run(&mut self) {
        loop {
            let now = Instant::now();
            let mut progressed = false;
            let ids: Vec<u64> = self.jobs.keys().copied().collect();
            for id in ids {
                let Some(job) = self.jobs.remove(&id) else { continue };
                if !self.is_runnable(id, &job) {
                    self.jobs.insert(id, job);
                    continue;
                }
                progressed |= self.advance(id, job, now);
            }
            if !progressed || self.inbox.is_none() {
                break;
            }
        }
    }

    /// Advances one job (already removed from `jobs`); re-inserts it unless it finished.
    fn advance(&mut self, id: u64, mut job: Job, now: Instant) -> bool {
        let outcome = match &mut job.work {
            Work::Op(op) => {
                let op = op.take().expect("an op runs once");
                Some(op(self))
            }
            Work::Turn(turn) => match self.step_turn(turn, now) {
                Progress::Idle => {
                    self.jobs.insert(id, job);
                    return false;
                }
                Progress::Advanced => {
                    self.jobs.insert(id, job);
                    return true;
                }
                Progress::Done(result) => {
                    if result.is_ok() {
                        self.commit(turn);
                    }
                    Some(result)
                }
            },
            Work::Many(many) => match self.step_many(id, many, now) {
                Some(progressed) => {
                    self.jobs.insert(id, job);
                    return progressed;
                }
                None => Some(Ok(json!({"results": std::mem::take(&mut many.results)}))),
            },
        };
        if let Some(result) = outcome {
            self.finish(&job, id, result);
        }
        true
    }

    /// Applies a successful turn's lasting effects.
    fn commit(&mut self, turn: &Turn) {
        let owner = turn.owner.clone().unwrap_or_default();
        match &turn.goal {
            Goal::Session { .. } => {
                if let Some(session) = self.sessions.get_mut(&owner) {
                    session.entries += 2 + 2 * turn.records.len() as u64;
                }
            }
            Goal::Npc { .. } if !turn.fallback => {
                if let Some(npc) = self.npcs.get_mut(&owner) {
                    npc.turn_count += 1;
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self, job: &Job, id: u64, result: AiResult<Value>) {
        if let Some(key) = &job.queue
            && let Some(queue) = self.queues.get_mut(key)
        {
            queue.retain(|j| *j != id);
            if queue.is_empty() {
                self.queues.remove(key);
            }
        }
        if let Some(request) = &job.request {
            self.respond(request, result);
        }
    }

    /// Steps `decideMany`; `None` when every decision is done.
    fn step_many(&mut self, id: u64, many: &mut Many, now: Instant) -> Option<bool> {
        let mut progressed = false;
        loop {
            if let Some(turn) = many.current.as_mut() {
                match self.step_turn(turn, now) {
                    Progress::Idle => return Some(progressed),
                    Progress::Advanced => return Some(true),
                    Progress::Done(result) => {
                        many.results
                            .push(result.unwrap_or_else(|error| json!({"error": error.to_rpc()})));
                        many.current = None;
                        progressed = true;
                    }
                }
            }
            let spec = many.specs.pop_front()?;
            let index = many.results.len();
            if spec.ids.len() == 1 {
                many.results.push(single_option(&spec.ids[0]));
                progressed = true;
                continue;
            }
            let mut context = Map::new();
            context.insert("index".into(), json!(index));
            let turn = self.decision_turn(id, spec, context, many.request_id.clone(), many.model.clone());
            many.current = Some(Box::new(turn));
        }
    }

    /// Cancels a job: `tool/cancel` for its outstanding calls, `-32009` for its request.
    fn cancel_job(&mut self, id: u64, reason: &str) -> bool {
        let Some(job) = self.jobs.remove(&id) else { return false };
        let turn = match &job.work {
            Work::Turn(turn) => Some(turn.as_ref()),
            Work::Many(many) => many.current.as_deref(),
            Work::Op(_) => None,
        };
        if let Some(turn) = turn {
            let pending: Vec<(String, Value)> =
                turn.outstanding().map(|o| (o.rpc_id.clone(), o.call.clone())).collect();
            for (rpc_id, call) in pending {
                self.cancel_tool_call(turn, &rpc_id, &call, "The turn ended before the tool finished.");
            }
        }
        self.finish(&job, id, Err(AiError::cancelled(reason.to_owned())));
        true
    }

    /// Cancels every job queued for `key`; returns how many.
    fn cancel_queue(&mut self, key: &QueueKey) -> usize {
        let ids: Vec<u64> = self
            .queues
            .get(key)
            .map(|q| q.iter().copied().collect())
            .unwrap_or_default();
        ids.into_iter()
            .filter(|id| self.cancel_job(*id, "The turn was cancelled."))
            .count()
    }

    /// Applies the engine's reply to one of our `tool/call`s (late replies are ignored).
    fn tool_response(&mut self, id: &Value, outcome: AiResult<Value>) {
        let Some(rpc_id) = id.as_str() else { return };
        let Some(job_id) = self.tool_calls.remove(rpc_id) else {
            return;
        };
        let Some(mut job) = self.jobs.remove(&job_id) else {
            return;
        };
        let output = match outcome {
            Ok(result) => ToolOutput::from_tool_result(&result),
            Err(error) => ToolOutput::Error(error.message),
        };
        match &mut job.work {
            Work::Turn(turn) => self.tool_reply(turn, rpc_id, output),
            Work::Many(many) => {
                if let Some(turn) = many.current.as_mut() {
                    self.tool_reply(turn, rpc_id, output);
                }
            }
            Work::Op(_) => {}
        }
        self.jobs.insert(job_id, job);
    }

    // --------------------------------------------------------------------------------------
    // Sessions

    fn session_id(&self, params: &Params) -> AiResult<String> {
        let id = params.string("session")?;
        if self.sessions.contains_key(&id) {
            Ok(id)
        } else {
            Err(not_found_session(&id))
        }
    }

    fn session_create(&mut self, params: &Params) -> AiResult<Value> {
        let mut warnings = Vec::new();
        params.warn_unknown(
            &["session", "instructions", "tools", "options", "history", "model"],
            "parameter",
            &mut warnings,
        );
        let id = match params.opt_string("session")? {
            Some(id) => id,
            None => loop {
                self.next_session += 1;
                let id = format!("s{}", self.next_session);
                if !self.sessions.contains_key(&id) {
                    break id;
                }
            },
        };
        check_id(&id, "session")?;
        if self.sessions.contains_key(&id) {
            return Err(
                AiError::new(AiErrorCode::SessionExists, format!("A session '{id}' already exists."))
                    .with_data("session", id),
            );
        }
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(
                AiError::new(AiErrorCode::SessionLimit, format!("At most {MAX_SESSIONS} sessions."))
                    .with_data("limit", MAX_SESSIONS),
            );
        }
        let instructions = params.opt_string("instructions")?;
        let tools = tools(params.get("tools"), "tools", &mut warnings)?;
        let options = Params::nested(params.get("options").unwrap_or(&Value::Null), "options.", "options")?;
        options.warn_unknown(
            &[
                "toolChoice",
                "maxToolRounds",
                "maxToolCalls",
                "enabledTools",
                "temperature",
                "maxResponseTokens",
                "sampling",
                "toolTimeoutSeconds",
                "trimHistory",
                "reservedResponseTokens",
                "maxAttempts",
                "userLabel",
                "assistantLabel",
            ],
            "option",
            &mut warnings,
        );
        let choice = options.opt_tool_choice("toolChoice")?.unwrap_or_default();
        let max_rounds = options.opt_u32("maxToolRounds", 0)?.unwrap_or(4);
        let max_calls = options.opt_u32("maxToolCalls", 0)?.unwrap_or(12);
        let enabled = options.opt_strings("enabledTools")?;
        let tool_timeout = options.opt_seconds("toolTimeoutSeconds")?;
        if params.get("history").is_some() {
            warnings.push("'history' is not supported by the scripted backend and was ignored.".into());
        }
        if tools.len() > 5 {
            warnings.push(format!(
                "{} tools: on-device models work best with 3-5 tools per request.",
                tools.len()
            ));
        }
        let (model, model_kind) = self.model(params.get("model"))?;
        if matches!(*model.lock(), ScriptModel::Unavailable(_)) {
            warnings.push("The system model is unavailable; turns will fail with model_unavailable.".into());
        }
        self.sessions.insert(
            id.clone(),
            SessionState {
                instructions,
                tools,
                choice,
                max_rounds,
                max_calls,
                enabled,
                tool_timeout,
                model,
                model_kind,
                entries: 0,
                created_at: iso_now(),
            },
        );
        self.session_order.push(id.clone());
        Ok(json!({"session": id, "warnings": warnings}))
    }

    fn session_respond(&mut self, params: &Params, request_id: Value, has_id: bool) -> AiResult<Reply> {
        let session_id = self.session_id(params)?;
        let prompt = params.string("prompt")?;
        let schema = params.opt_object("schema")?.cloned();
        if let Some(schema) = &schema {
            check_schema(schema, "schema")?;
        }
        let stream = params.opt_bool("stream")?.unwrap_or(false);
        let choice = params.opt_tool_choice("toolChoice")?;
        let rounds = params.opt_u32("maxToolRounds", 0)?;
        let calls = params.opt_u32("maxToolCalls", 0)?;
        let enabled = params.opt_strings("enabledTools")?;
        let session = &self.sessions[&session_id];
        let mut context = Map::new();
        context.insert("session".into(), json!(session_id));
        let job = self.next_job_id();
        let mut turn = Turn::new(
            job,
            RequestKind::Session,
            Goal::Session { schema },
            Some(session_id.clone()),
            context,
            request_id.clone(),
            stream.then_some("session/event"),
            session.model.clone(),
            prompt,
        );
        let enabled = enabled.or_else(|| session.enabled.clone());
        turn.tools = session
            .tools
            .iter()
            .filter(|t| enabled.as_ref().is_none_or(|e| e.contains(&t.name)))
            .cloned()
            .collect();
        turn.choice = choice.unwrap_or_else(|| session.choice.clone());
        turn.max_rounds = rounds.unwrap_or(session.max_rounds);
        turn.max_calls = calls.unwrap_or(session.max_calls);
        turn.tool_timeout = session.tool_timeout.unwrap_or(self.config.tool_timeout);
        self.enqueue(
            has_id.then_some(request_id),
            Some(QueueKey::Session(session_id)),
            Work::Turn(Box::new(turn)),
        );
        Ok(Reply::Later)
    }

    fn session_op(&mut self, method: &str, params: &Params, id: Option<&Value>) -> AiResult<Reply> {
        let session = self.session_id(params)?;
        let mut warnings = Vec::new();
        let op: Op = match method {
            "session/reset" => {
                let s = session.clone();
                Box::new(move |engine: &mut Engine| {
                    if let Some(state) = engine.sessions.get_mut(&s) {
                        state.entries = 0;
                    }
                    Ok(json!({"session": s}))
                })
            }
            "session/setInstructions" => {
                let instructions = params.opt_string("instructions")?;
                let s = session.clone();
                Box::new(move |engine: &mut Engine| {
                    if let Some(state) = engine.sessions.get_mut(&s) {
                        state.instructions = instructions;
                    }
                    Ok(json!({"session": s}))
                })
            }
            "session/setContextNote" => {
                params.opt_string("note")?;
                let s = session.clone();
                Box::new(move |_: &mut Engine| Ok(json!({"session": s})))
            }
            _ => {
                let parsed = tools(
                    Some(params.get("tools").unwrap_or(&Value::Array(Vec::new()))),
                    "tools",
                    &mut warnings,
                )?;
                let s = session.clone();
                Box::new(move |engine: &mut Engine| {
                    if let Some(state) = engine.sessions.get_mut(&s) {
                        state.tools = parsed;
                    }
                    Ok(json!({"session": s, "warnings": warnings}))
                })
            }
        };
        self.enqueue(id.cloned(), Some(QueueKey::Session(session)), Work::Op(Some(op)));
        Ok(Reply::Later)
    }

    fn session_list(&self) -> Value {
        let sessions: Vec<Value> = self
            .session_order
            .iter()
            .filter_map(|id| {
                let s = self.sessions.get(id)?;
                let pending = self.queues.get(&QueueKey::Session(id.clone())).map_or(0, VecDeque::len);
                let mut entry = json!({"session": id, "model": s.model_kind,
                    "tools": s.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                    "busy": pending > 0, "pendingOperations": pending, "entries": s.entries, "createdAt": s.created_at});
                if let Some(instructions) = &s.instructions {
                    entry["instructions"] = json!(instructions);
                }
                Some(entry)
            })
            .collect();
        json!({"sessions": sessions})
    }

    // --------------------------------------------------------------------------------------
    // NPCs

    fn npc_id(&self, params: &Params) -> AiResult<String> {
        let id = params.string("npc")?;
        if self.npcs.contains_key(&id) {
            Ok(id)
        } else {
            Err(not_found_npc(&id))
        }
    }

    fn npc_create(&mut self, params: &Params) -> AiResult<Value> {
        let mut warnings = Vec::new();
        params.warn_unknown(
            &["npc", "persona", "tools", "world", "memory", "options", "model"],
            "parameter",
            &mut warnings,
        );
        let id = match params.opt_string("npc")? {
            Some(id) => id,
            None => loop {
                self.next_npc += 1;
                let id = format!("npc{}", self.next_npc);
                if !self.npcs.contains_key(&id) {
                    break id;
                }
            },
        };
        check_id(&id, "npc")?;
        if self.npcs.contains_key(&id) {
            return Err(
                AiError::new(AiErrorCode::NpcExists, format!("An NPC '{id}' already exists.")).with_data("npc", id),
            );
        }
        if self.npcs.len() >= MAX_NPCS {
            return Err(
                AiError::new(AiErrorCode::LimitReached, format!("At most {MAX_NPCS} NPCs."))
                    .with_data("limit", MAX_NPCS),
            );
        }
        let persona = params
            .opt_object("persona")?
            .ok_or_else(|| AiError::invalid_params("Missing required parameter 'persona'."))?;
        let persona_params = Params::nested(persona, "persona.", "persona")?;
        let name = persona_params.string("name")?;
        if name.trim().is_empty() {
            return Err(AiError::invalid_params("'persona.name' must not be empty."));
        }
        let role = persona_params.opt_string("role")?.unwrap_or_default();
        let default_emotion = persona_params
            .opt_string("defaultEmotion")?
            .and_then(|e| Emotion::matching(&e))
            .unwrap_or_default();
        let tools = tools(params.get("tools"), "tools", &mut warnings)?;
        let world = params.opt_string("world")?;
        if let Some(world) = &world
            && !self.worlds.exists(world)
        {
            return Err(super::world::not_found(world));
        }
        let memory = Params::nested(params.get("memory").unwrap_or(&Value::Null), "memory.", "memory")?;
        let facts = memory.opt_strings("facts")?.unwrap_or_default();
        let relationship = memory.opt_i64("relationship")?.unwrap_or(0).clamp(-100, 100) as i32;
        let opts = self.npc_options(params, &tools, &mut warnings)?;
        let (model, model_kind) = self.model(params.get("model"))?;
        let state = NpcState {
            name,
            role,
            default_emotion,
            tools,
            world,
            opts,
            relationship,
            facts,
            model,
            model_kind,
            turn_count: 0,
            created_at: iso_now(),
            fallback_index: 0,
        };
        let names: Vec<String> = state
            .tools
            .iter()
            .map(|t| t.name.clone())
            .chain(npc_local_tools(&state).names().into_iter().map(str::to_owned))
            .collect();
        if names.len() > 5 {
            warnings.push(format!(
                "{} tools: on-device models work best with 3-5 tools per request.",
                names.len()
            ));
        }
        self.npcs.insert(id.clone(), state);
        self.npc_order.push(id.clone());
        Ok(json!({"npc": id, "tools": names, "warnings": warnings}))
    }

    fn npc_options(&self, params: &Params, tools: &[ToolSpec], warnings: &mut Vec<String>) -> AiResult<NpcOpts> {
        let options = Params::nested(params.get("options").unwrap_or(&Value::Null), "options.", "options")?;
        options.warn_unknown(
            &[
                "replyFormat",
                "groundingTool",
                "toolChoice",
                "maxToolRounds",
                "maxToolCalls",
                "toolTimeoutSeconds",
                "worldReadable",
                "worldWritable",
                "worldContextPaths",
                "memoryTools",
                "maxFacts",
                "maxRelationshipChange",
                "secretsUnlockAtRelationship",
                "emotions",
                "playerOptionCount",
                "canEndConversation",
                "extraInstructions",
                "compactAfterTurns",
                "keepRecentTurns",
                "fallbackOnGuardrail",
                "fallbackLines",
                "temperature",
                "maximumResponseTokens",
                "barkMaximumTokens",
            ],
            "option",
            warnings,
        );
        let reply_format = match options.opt_string("replyFormat")?.as_deref() {
            None | Some("automatic") => ReplyFormat::Automatic,
            Some("structured") => ReplyFormat::Structured,
            Some("text") => ReplyFormat::Text,
            Some(other) => {
                return Err(AiError::invalid_params(format!(
                    "'options.replyFormat' must be \"automatic\", \"structured\" or \"text\"; got '{other}'."
                )));
            }
        };
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let grounding = options.opt_string("groundingTool")?;
        let choice = options.opt_tool_choice("toolChoice")?.unwrap_or(ToolChoice::Explicit);
        let world_readable = options
            .opt_strings("worldReadable")?
            .unwrap_or_else(|| vec![String::new()]);
        let world_writable = options.opt_strings("worldWritable")?.unwrap_or_default();
        let (remember_fact, change_relationship) = match options.get("memoryTools") {
            None => (false, false),
            Some(Value::String(s)) if s == "all" => (true, true),
            Some(Value::String(s)) if s == "none" => (false, false),
            Some(Value::Array(items)) => {
                let items: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
                (items.contains(&"rememberFact"), items.contains(&"changeRelationship"))
            }
            Some(_) => {
                return Err(AiError::invalid_params(
                    "'options.memoryTools' must be [\"rememberFact\", \"changeRelationship\"], \"all\" or \"none\".",
                ));
            }
        };
        let mut available: HashSet<&str> = names.clone();
        if params.get("world").is_some() {
            if !world_readable.is_empty() {
                available.insert(super::turn::READ_WORLD);
            }
            if !world_writable.is_empty() {
                available.insert(super::turn::UPDATE_WORLD);
            }
        }
        if remember_fact {
            available.insert(REMEMBER_FACT);
        }
        if change_relationship {
            available.insert(CHANGE_RELATIONSHIP);
        }
        if let Some(grounding) = &grounding
            && !available.contains(grounding.as_str())
        {
            return Err(AiError::invalid_params(format!(
                "'options.groundingTool' names '{grounding}', which is not one of the NPC's tools."
            )));
        }
        if let ToolChoice::Tool(name) = &choice
            && !available.contains(name.as_str())
        {
            return Err(AiError::invalid_params(format!(
                "'options.toolChoice' names '{name}', which is not one of the NPC's tools."
            )));
        }
        let emotions = match options.opt_strings("emotions")? {
            None => None,
            Some(list) => Some(
                list.iter()
                    .map(|e| {
                        Emotion::matching(e).ok_or_else(|| {
                            AiError::invalid_params(format!("'options.emotions' has an unknown emotion '{e}'."))
                        })
                    })
                    .collect::<AiResult<Vec<_>>>()?,
            ),
        };
        let fallback_lines = options
            .opt_strings("fallbackLines")?
            .filter(|lines| !lines.is_empty())
            .unwrap_or_else(|| DEFAULT_FALLBACK_LINES.iter().map(|l| (*l).to_owned()).collect());
        Ok(NpcOpts {
            reply_format,
            grounding,
            choice,
            max_rounds: options.opt_u32("maxToolRounds", 0)?.unwrap_or(2),
            max_calls: options.opt_u32("maxToolCalls", 0)?.unwrap_or(6),
            tool_timeout: options.opt_seconds("toolTimeoutSeconds")?,
            player_option_count: options.opt_u32("playerOptionCount", 0)?.unwrap_or(3).min(4) as usize,
            can_end_conversation: options.opt_bool("canEndConversation")?.unwrap_or(true),
            fallback_on_guardrail: options.opt_bool("fallbackOnGuardrail")?.unwrap_or(true),
            fallback_lines,
            emotions,
            world_readable,
            world_writable,
            remember_fact,
            change_relationship,
            max_facts: options.opt_u32("maxFacts", 1)?.unwrap_or(12) as usize,
            max_relationship_change: i64::from(options.opt_u32("maxRelationshipChange", 0)?.unwrap_or(10)),
        })
    }

    fn npc_talk(&mut self, params: &Params, request_id: Value, has_id: bool) -> AiResult<Reply> {
        let npc_id = self.npc_id(params)?;
        let line = params.string("line")?;
        let situation = params.opt_text("context");
        let stream = params.opt_bool("stream")?.unwrap_or(false);
        let choice = params.opt_tool_choice("toolChoice")?;
        let npc = &self.npcs[&npc_id];
        let mut context = Map::new();
        context.insert("npc".into(), json!(npc_id));
        let job = self.next_job_id();
        let mut turn = Turn::new(
            job,
            RequestKind::NpcTalk,
            Goal::Npc {
                format: npc.opts.reply_format,
            },
            Some(npc_id.clone()),
            context,
            request_id.clone(),
            stream.then_some("npc/event"),
            npc.model.clone(),
            match situation {
                Some(situation) if !situation.is_empty() => format!("{line}\n{situation}"),
                _ => line,
            },
        );
        // `{prompt}` is the player's line; the context only informs the model.
        if let Some(first_line) = turn.prompt.split('\n').next() {
            turn.prompt = first_line.to_owned();
        }
        turn.tools = npc.tools.clone();
        turn.local = npc_local_tools(npc);
        turn.choice = match (choice, &npc.opts.grounding) {
            (Some(choice), _) => choice,
            (None, Some(grounding)) => ToolChoice::Tool(grounding.clone()),
            (None, None) => npc.opts.choice.clone(),
        };
        turn.max_rounds = npc.opts.max_rounds;
        turn.max_calls = npc.opts.max_calls;
        turn.tool_timeout = npc.opts.tool_timeout.unwrap_or(self.config.tool_timeout);
        self.enqueue(
            has_id.then_some(request_id),
            Some(QueueKey::Npc(npc_id)),
            Work::Turn(Box::new(turn)),
        );
        Ok(Reply::Later)
    }

    fn npc_bark(&mut self, params: &Params, request_id: Value, has_id: bool) -> AiResult<Reply> {
        let npc_id = self.npc_id(params)?;
        let situation = params.opt_text("situation").unwrap_or_default();
        let npc = &self.npcs[&npc_id];
        let job = self.next_job_id();
        let mut turn = Turn::new(
            job,
            RequestKind::NpcBark,
            Goal::Bark,
            Some(npc_id),
            Map::new(),
            request_id.clone(),
            None,
            npc.model.clone(),
            situation,
        );
        turn.max_rounds = 0;
        self.enqueue(has_id.then_some(request_id), None, Work::Turn(Box::new(turn)));
        Ok(Reply::Later)
    }

    fn npc_list(&self) -> Value {
        let npcs: Vec<Value> = self
            .npc_order
            .iter()
            .filter_map(|id| {
                let n = self.npcs.get(id)?;
                let pending = self.queues.get(&QueueKey::Npc(id.clone())).map_or(0, VecDeque::len);
                Some(
                    json!({"npc": id, "name": n.name, "role": n.role, "world": n.world, "model": n.model_kind,
                    "tools": n.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                    "turnCount": n.turn_count, "relationship": n.relationship, "busy": pending > 0,
                    "pendingOperations": pending, "createdAt": n.created_at}),
                )
            })
            .collect();
        json!({"npcs": npcs})
    }

    // --------------------------------------------------------------------------------------
    // Decisions and content

    fn decision_spec(&self, params: &Params, shared: &Params, warnings: &mut Vec<String>) -> AiResult<DecisionSpec> {
        let situation = params.opt_text("situation").ok_or_else(|| {
            AiError::invalid_params(format!("Missing required parameter '{}'.", params.path("situation")))
        })?;
        let options = params.opt_array("options")?.ok_or_else(|| {
            AiError::invalid_params(format!("Missing required parameter '{}'.", params.path("options")))
        })?;
        if options.is_empty() {
            return Err(AiError::invalid_params(format!(
                "'{}' must not be empty.",
                params.path("options")
            )));
        }
        let mut ids: Vec<String> = Vec::new();
        for (index, option) in options.iter().enumerate() {
            let id = match option {
                Value::String(id) => id.trim().to_owned(),
                Value::Object(o) => o
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                _ => String::new(),
            };
            if id.is_empty() {
                return Err(AiError::invalid_params(format!(
                    "'{}[{index}]' must be an id string or {{\"id\", \"description\"}} with a non-empty id.",
                    params.path("options")
                )));
            }
            if ids.contains(&id) {
                return Err(AiError::invalid_params(format!("Duplicate option id '{id}'.")));
            }
            ids.push(id);
        }
        let fallback = params.opt_string("fallbackOptionID")?.map(|f| f.trim().to_owned());
        if let Some(fallback) = &fallback
            && !ids.contains(fallback)
        {
            return Err(AiError::invalid_params(format!(
                "'fallbackOptionID' '{fallback}' is not one of: {}.",
                ids.join(", ")
            )));
        }
        match params.get("actor") {
            None | Some(Value::Object(_)) => {}
            Some(Value::String(npc)) => {
                if !self.npcs.contains_key(npc) {
                    return Err(not_found_npc(npc));
                }
            }
            Some(_) => {
                return Err(AiError::invalid_params(
                    "'actor' must be a persona object or an NPC id.",
                ));
            }
        }
        let tools = tools(params.get("tools"), &params.path("tools"), warnings)?;
        let choice = params.opt_tool_choice("toolChoice")?.unwrap_or_default();
        if let ToolChoice::Tool(name) = &choice
            && !tools.iter().any(|t| t.name == *name)
        {
            return Err(AiError::invalid_params(format!(
                "'toolChoice' names '{name}', which is not one of the decision's tools."
            )));
        }
        let max_rounds = params
            .opt_u32("maxToolRounds", 0)?
            .or(shared.opt_u32("maxToolRounds", 0)?)
            .unwrap_or(2);
        let tool_timeout = match params.opt_seconds("toolTimeoutSeconds")? {
            Some(t) => t,
            None => shared
                .opt_seconds("toolTimeoutSeconds")?
                .unwrap_or(self.config.tool_timeout),
        };
        Ok(DecisionSpec {
            situation,
            ids,
            fallback,
            choice: if tools.is_empty() { ToolChoice::None } else { choice },
            tools,
            max_rounds,
            tool_timeout,
        })
    }

    fn decision_turn(
        &self,
        job: u64,
        spec: DecisionSpec,
        context: Map<String, Value>,
        request_id: Value,
        model: SharedModel,
    ) -> Turn {
        let mut turn = Turn::new(
            job,
            RequestKind::Decision,
            Goal::Decision {
                ids: spec.ids,
                fallback: spec.fallback,
            },
            None,
            context,
            request_id,
            None,
            model,
            spec.situation,
        );
        turn.tools = spec.tools;
        turn.choice = spec.choice;
        turn.max_rounds = spec.max_rounds;
        turn.tool_timeout = spec.tool_timeout;
        turn
    }

    fn decide(&mut self, params: &Params, request_id: Value, has_id: bool) -> AiResult<Reply> {
        let mut warnings = Vec::new();
        let spec = self.decision_spec(params, params, &mut warnings)?;
        if spec.ids.len() == 1 {
            return Ok(Reply::Now(single_option(&spec.ids[0])));
        }
        let (model, _) = self.model(params.get("model"))?;
        let job = self.next_job_id();
        let turn = self.decision_turn(job, spec, Map::new(), request_id.clone(), model);
        self.enqueue(has_id.then_some(request_id), None, Work::Turn(Box::new(turn)));
        Ok(Reply::Later)
    }

    fn decide_many(&mut self, params: &Params, request_id: Value, has_id: bool) -> AiResult<Reply> {
        let requests = params
            .opt_array("requests")?
            .ok_or_else(|| AiError::invalid_params("Missing required parameter 'requests'."))?;
        if requests.is_empty() {
            return Err(AiError::invalid_params("'requests' must not be empty."));
        }
        let mut warnings = Vec::new();
        let mut specs = VecDeque::new();
        for (index, request) in requests.iter().enumerate() {
            let name = format!("requests[{index}]");
            let request = Params::nested(request, &format!("{name}."), &name)?;
            specs.push_back(self.decision_spec(&request, params, &mut warnings)?);
        }
        let (model, _) = self.model(params.get("model"))?;
        let many = Many {
            specs,
            model,
            current: None,
            results: Vec::with_capacity(requests.len()),
            request_id: request_id.clone(),
        };
        self.enqueue(has_id.then_some(request_id), None, Work::Many(Box::new(many)));
        Ok(Reply::Later)
    }

    fn content(&mut self, params: &Params, request_id: Value, has_id: bool) -> AiResult<Reply> {
        let prompt = params.string("prompt")?;
        let schema = params
            .get("schema")
            .ok_or_else(|| AiError::invalid_params("Missing required parameter 'schema'."))?;
        check_schema(schema, "schema")?;
        let mut warnings = Vec::new();
        let tools = tools(params.get("tools"), "tools", &mut warnings)?;
        let tool_timeout = params
            .opt_seconds("toolTimeoutSeconds")?
            .unwrap_or(self.config.tool_timeout);
        let (model, _) = self.model(params.get("model"))?;
        let job = self.next_job_id();
        let mut turn = Turn::new(
            job,
            RequestKind::Content,
            Goal::Content,
            None,
            Map::new(),
            request_id.clone(),
            None,
            model,
            prompt,
        );
        turn.choice = if tools.is_empty() {
            ToolChoice::None
        } else {
            ToolChoice::Auto
        };
        turn.tools = tools;
        turn.max_rounds = 2;
        turn.tool_timeout = tool_timeout;
        self.enqueue(has_id.then_some(request_id), None, Work::Turn(Box::new(turn)));
        Ok(Reply::Later)
    }
}

fn single_option(id: &str) -> Value {
    json!({"optionID": id, "reasoning": "It is the only option.", "confidence": 100, "toolCalls": [],
        "usage": {"inputTokens": 0, "cachedInputTokens": 0, "outputTokens": 0, "totalTokens": 0}, "isFallback": false})
}

fn npc_local_tools(npc: &NpcState) -> LocalTools {
    LocalTools {
        world: npc
            .world
            .clone()
            .map(|world| (world, npc.opts.world_readable.clone(), npc.opts.world_writable.clone())),
        remember_fact: npc.opts.remember_fact,
        change_relationship: npc.opts.change_relationship,
        max_facts: npc.opts.max_facts,
        max_relationship_change: npc.opts.max_relationship_change,
    }
}
