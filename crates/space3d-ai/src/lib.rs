//! # space3d-ai
//!
//! On-device AI for Space3d games: agent sessions with **real tool calls into the engine**,
//! NPC dialogue with streaming lines, enum-constrained decisions and schema-shaped content —
//! on Apple's Foundation Models ([open-apple-models]) and Android's Gemini Nano
//! ([open-android-models]).
//!
//! Both libraries speak the same JSON-RPC 2.0 bridge protocol (v1.0), so the engine implements
//! it once and the model **backend is injected** per platform ([`BackendChoice`]):
//!
//! | backend | platforms | transport |
//! |---|---|---|
//! | [`AppleBackend`] | macOS / iOS / visionOS 27 (`apple-ffi` feature) | open-apple-models' C ABI |
//! | `AndroidBackend` | Android (Gradle host app with the open-android-models AAR) | JNI into `com.spacecorps.oam.jni.OamJni` |
//! | [`ScriptedBackend`] | everywhere, including `wasm32` | pure Rust, scripted models |
//! | [`UnsupportedBackend`] | fallback | every request fails with [`AiErrorCode::Unsupported`] |
//!
//! ## Frame model — never blocks
//!
//! * [`Ai`] methods send one request and return a [`Task`](space3d_tasks::Task) to poll.
//! * `ai::pump` (`Stage::PreUpdate`) drains the backend's lines: responses complete tasks,
//!   streaming notifications become [`AiEvent`]s, `tool/call` requests are queued.
//! * `ai::run_tools` (`Stage::Update`) runs the [`AiTool`] handlers **on the engine thread
//!   with full ECS access** and replies (or defers the reply to a [`ToolResponder`]).
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_ai::prelude::*;
//! use serde_json::json;
//!
//! struct Stock(u32);
//!
//! let mut app = App::new();
//! app.insert_resource(Stock(3));
//! app.add_plugin(
//!     AiPlugin::new(BackendChoice::auto_or(ScriptedBackend::new())).with_tool(
//!         AiTool::new("check_stock", "How many iron swords are in stock.")
//!             .handler(|ctx, _call| json!({"stock": ctx.resources.expect::<Stock>().0})),
//!     ),
//! );
//! // A scripted model: identical on every platform, so this also runs in CI.
//! let script = Script::new()
//!     .step(ScriptStep::tool_call("check_stock", json!({})))
//!     .step(ScriptStep::npc_reply("Three swords left.", Emotion::Proud, ["Buy one"], false));
//! let ai = app.resources.expect_mut::<Ai>();
//! let _ = ai.create_npc("gorm", NpcConfig::new(Persona::new("Gorm")).grounding_tool("check_stock").model(script));
//! let mut turn = ai.talk("gorm", "Any swords?");
//! let turn = loop {
//!     app.step_ticks(1);
//!     if let Some(result) = turn.poll() { break result.unwrap(); }
//!     std::thread::sleep(std::time::Duration::from_millis(1));
//! };
//! assert_eq!(turn.line, "Three swords left.");
//! assert_eq!(turn.tool_calls[0].output, ToolOutput::Json(json!({"stock": 3})));
//! ```
//!
//! ## Cargo features
//!
//! | feature | effect |
//! |---|---|
//! | `apple-ffi` | links open-apple-models' C ABI (`libOpenAppleModelsFFI`) so [`AppleBackend`] drives Apple's on-device model: on macOS set `OAM_APPLE_LIB_DIR`, on iOS Xcode links the library. Without it `AppleBackend` compiles everywhere and reports `Unsupported`. |
//! | `android-jvm` | test-only: also compiles `AndroidBackend` on desktop hosts, to test it against open-android-models' real `OamJni` in a JVM |
//!
//! Games usually enable the crate through the `space3d` facade: feature `ai` (re-exported as
//! `space3d::ai`, common types in `space3d::prelude`) or `ai-apple-ffi`.
//!
//! ## Status
//!
//! The Android backend has been built into an APK and tested against the real `OamJni` in a
//! desktop JVM, but has not yet run on a Gemini Nano device. On iOS the Apple backend has
//! answered `initialize` in the simulator, linked from a static XCFramework slice, but has not
//! run in an Xcode app project or on a device.
//!
//! ## Guides
//!
//! `docs/ai.md` in the Space3d repository covers the `Ai` resource, tools, events, backend
//! selection, scripted models, Apple linking, errors and testing; `docs/ai-android.md` covers
//! the Gradle host app Android needs.
//!
//! [open-apple-models]: https://github.com/SpaceCorps/open-apple-models
//! [open-android-models]: https://github.com/SpaceCorps/open-android-models

#![warn(missing_docs)]

mod ai;
pub mod backend;
mod config;
mod error;
mod events;
mod rpc;
pub mod script;
mod tools;
mod types;

pub use ai::{Ai, AiPlugin, AiStatus};
pub use backend::{
    AiBackend, AppleBackend, BackendChoice, BackendKind, Inbox, ModelRequest, RequestKind, ScriptedBackend,
    UnsupportedBackend,
};
#[cfg(any(target_os = "android", feature = "android-jvm"))]
pub use backend::{AndroidBackend, OAM_JNI_CLASS};
pub use config::{
    Actor, ContentRequest, Created, DecisionRequest, NpcConfig, NpcOptions, RespondOptions, SessionConfig,
    SessionOptions, TalkOptions,
};
pub use error::{AiError, AiErrorCode, AiResult};
pub use events::{AiEvent, NpcEvent, TurnEvent, WorldChange};
pub use rpc::RequestId;
pub use script::{ModelSpec, Script, ScriptStep, ScriptedToolCall, StepAction};
pub use tools::{AiTool, ToolHandler, ToolInvocation, ToolOwner, ToolReply, ToolResponder};
pub use types::{
    AgentResponse, Decision, DecisionOption, DialogueTurn, Emotion, ModelAvailability, ModelStep, NpcMemory, Persona,
    ReplyFormat, Sampling, ServerInfo, ServerName, TokenUsage, ToolArgumentError, ToolCall, ToolChoice, ToolExecution,
    ToolOutput, ToolRecord,
};

/// The types most games use.
pub mod prelude {
    pub use crate::{
        Ai, AiError, AiErrorCode, AiEvent, AiPlugin, AiTool, BackendChoice, ContentRequest, Decision, DecisionOption,
        DecisionRequest, DialogueTurn, Emotion, NpcConfig, NpcEvent, Persona, RespondOptions, Script, ScriptStep,
        ScriptedBackend, SessionConfig, TalkOptions, ToolChoice, ToolInvocation, ToolOutput, ToolReply, ToolResponder,
        TurnEvent,
    };
}
