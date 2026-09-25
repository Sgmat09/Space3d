//! [`ScriptedBackend`]: a pure-Rust bridge that plays scripted models (protocol §10).
//!
//! It implements the protocol's core in-process, with no model and no threads, so it runs on
//! every platform including `wasm32`: `initialize`, `ping`, `model/availability`, `shutdown`,
//! `session/*` (except `transcript` and `compact`), `npc/create|talk|bark|cancel|reset|delete|list`,
//! `decision/decide|decideMany`, `content/generate` and `world/*` (with subscriptions and the
//! NPC `read_world_state` / `update_world_state` tools). Other methods answer `-32601`.
//!
//! Streaming events, `tool/call` round trips (with timeouts and `tool/cancel`), per-session and
//! per-NPC ordering, cancellation, NPC fallback lines and decision fallbacks behave as on the
//! real bridges, so game code tested against it runs unchanged on Apple and Android.

mod engine;
mod params;
mod turn;
mod world;

use super::{AiBackend, BackendKind, Inbox};
use crate::error::{AiError, AiResult};
use crate::script::{Script, ScriptStep};
use engine::Engine;
use parking_lot::Mutex;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// What a scripted model is asked to do, for [`ScriptedBackend::with_system_model`] responders.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    /// Which kind of request is running.
    pub kind: RequestKind,
    /// The session or NPC id, if any.
    pub owner: Option<String>,
    /// The prompt, player line, bark situation, decision situation or content prompt.
    pub prompt: String,
    /// The step within the turn (0 for the first inference).
    pub step_index: u32,
    /// Tool outputs of this turn so far, as the model reads them (`Error: …` for errors).
    pub tool_outputs: Vec<String>,
    /// Tools the model may call on this step.
    pub enabled_tools: Vec<String>,
    /// `allowed`, `required` or `disallowed`.
    pub tool_calling_mode: &'static str,
    /// Whether a structured (JSON) answer is expected on this step.
    pub wants_json: bool,
    /// Decision option ids (decisions only).
    pub options: Vec<String>,
}

/// The kind of request a scripted model step serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// `session/respond`.
    Session,
    /// `npc/talk`.
    NpcTalk,
    /// `npc/bark`.
    NpcBark,
    /// `decision/decide` / `decideMany`.
    Decision,
    /// `content/generate`.
    Content,
}

type Responder = dyn Fn(&ModelRequest) -> ScriptStep + Send + Sync;

/// What the `"system"` model is on the scripted backend.
#[derive(Clone, Default)]
pub(super) enum SystemModel {
    /// Unavailable: turns fail with `model_unavailable` (scripted models still work).
    #[default]
    None,
    /// Each session/NPC/request gets its own copy of the script.
    Script(Script),
    /// Every step is decided by the closure.
    Responder(Arc<Responder>),
}

/// Configuration shared by the engine.
#[derive(Clone)]
pub(super) struct Config {
    pub system: SystemModel,
    pub pacing: Duration,
    pub tool_timeout: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            system: SystemModel::None,
            pacing: Duration::ZERO,
            tool_timeout: Some(Duration::from_secs(120)),
        }
    }
}

/// A pure-Rust implementation of the bridge protocol's core that plays scripted models: for
/// tests, CI, the web and development without an on-device model.
///
/// Sessions, NPCs and requests take the same `model` parameter as on the real bridges
/// (`{"type": "scripted", "steps": […]}`, see [`Script`]). The `"system"` model is unavailable
/// unless configured with [`with_system_script`](Self::with_system_script) or
/// [`with_system_model`](Self::with_system_model), which lets a web build ship scripted NPCs
/// through exactly the code paths the on-device model uses.
///
/// Work runs synchronously inside [`send`](AiBackend::send) and, for delays, pacing and tool
/// timeouts, in [`poll`](AiBackend::poll) (called every frame by the engine), so it never
/// needs a thread. Cloning shares the same bridge.
///
/// ```
/// use space3d_ai::{ScriptedBackend, ScriptStep, RequestKind};
/// // A keyword-driven innkeeper for the web build.
/// let backend = ScriptedBackend::new().with_system_model(|request| {
///     match request.kind {
///         RequestKind::NpcTalk if request.prompt.contains("menu") => ScriptStep::text("[happy] Stew and bread."),
///         _ => ScriptStep::text("[neutral] Hm."),
///     }
/// });
/// # let _ = backend;
/// ```
#[derive(Clone, Default)]
pub struct ScriptedBackend {
    engine: Arc<Mutex<Engine>>,
}

impl ScriptedBackend {
    /// A backend whose `"system"` model is unavailable.
    pub fn new() -> Self {
        Self::default()
    }

    /// Plays a copy of `script` for every session, NPC and request on the `"system"` model.
    pub fn with_system_script(self, script: Script) -> Self {
        self.engine.lock().config.system = SystemModel::Script(script);
        self
    }

    /// Decides every `"system"` model step with `responder` (called with the engine lock held:
    /// do not call back into the backend from it).
    pub fn with_system_model(self, responder: impl Fn(&ModelRequest) -> ScriptStep + Send + Sync + 'static) -> Self {
        self.engine.lock().config.system = SystemModel::Responder(Arc::new(responder));
        self
    }

    /// Streams text pieces `per_chunk` apart (a typewriter pace for demos). Default: all at once.
    pub fn with_pacing(self, per_chunk: Duration) -> Self {
        self.engine.lock().config.pacing = per_chunk;
        self
    }

    /// The default `tool/call` timeout (`None` = wait forever). Default 120 s, as on the bridges.
    pub fn with_default_tool_timeout(self, timeout: Option<Duration>) -> Self {
        self.engine.lock().config.tool_timeout = timeout;
        self
    }
}

impl fmt::Debug for ScriptedBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let engine = self.engine.lock();
        f.debug_struct("ScriptedBackend").field("engine", &*engine).finish()
    }
}

impl AiBackend for ScriptedBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Scripted
    }

    fn start(&self, inbox: Inbox) -> AiResult<()> {
        let mut engine = self.engine.lock();
        if engine.is_started() {
            return Err(AiError::transport("The ScriptedBackend is already started."));
        }
        engine.start(inbox);
        Ok(())
    }

    fn send(&self, line: &str) -> AiResult<()> {
        let mut engine = self.engine.lock();
        if !engine.is_started() {
            return Err(AiError::transport("The ScriptedBackend is not started."));
        }
        engine.receive(line);
        Ok(())
    }

    fn poll(&self) {
        self.engine.lock().tick();
    }

    fn shutdown(&self) {
        self.engine.lock().close();
    }
}
