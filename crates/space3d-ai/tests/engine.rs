//! The engine-level API on the scripted backend: `Ai` tasks, `AiEvent`s, tool handlers with
//! ECS access on the engine thread, deferred replies, errors and frame behavior.

mod common;

use common::{run_until, tick_until};
use serde_json::json;
use space3d_ai::prelude::*;
use space3d_ai::{AiErrorCode, AiStatus, BackendKind, Created, NpcOptions, ReplyFormat, RequestId, ToolOwner};
use space3d_core::prelude::*;
use space3d_tasks::AsyncQueue;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

/// A gate entity the model can open.
struct Gate {
    name: &'static str,
    open: bool,
}

/// What tool handlers observed.
#[derive(Default)]
struct Seen {
    threads: Vec<ThreadId>,
    frames: Vec<u64>,
}

/// Collects every `AiEvent` in order (read each frame with its own cursor).
#[derive(Default)]
struct Log {
    reader: EventReader,
    events: Vec<AiEvent>,
}

fn app(backend: ScriptedBackend) -> App {
    let mut app = App::new();
    app.init_resource::<Seen>().init_resource::<Log>();
    app.add_plugin(AiPlugin::scripted(backend));
    app.add_system(Stage::Update, |ctx| {
        ctx.resources.scope::<Log, _>(|resources, log| {
            let new: Vec<AiEvent> = log
                .reader
                .read(resources.expect::<Events<AiEvent>>())
                .cloned()
                .collect();
            log.events.extend(new);
        });
    });
    app
}

fn open_gate_tool() -> AiTool {
    AiTool::new("open_gate", "Open a named gate. Returns whether it opened.")
        .parameters(json!({"type": "object", "properties": {"gate": {"type": "string"}}, "required": ["gate"]}))
        .handler(|ctx, call| {
            let frame = ctx.time().frame;
            let seen = ctx.resources.expect_mut::<Seen>();
            seen.threads.push(std::thread::current().id());
            seen.frames.push(frame);
            let wanted = call.string("gate").map_err(|e| e.to_string())?.to_owned();
            let mut opened = false;
            for gate in ctx.world.query_mut::<&mut Gate>() {
                if gate.name == wanted {
                    gate.open = true;
                    opened = true;
                }
            }
            Ok::<_, String>(ToolOutput::json(json!({"gate": wanted, "opened": opened})))
        })
}

#[test]
fn session_tools_run_on_the_engine_thread_with_ecs_access() {
    let mut app = app(ScriptedBackend::new());
    let north = app.world.spawn((Gate {
        name: "north",
        open: false,
    },));
    let ai = app.resources.expect_mut::<Ai>();
    ai.register_tool(open_gate_tool()).unwrap();
    let script = Script::new()
        .step(ScriptStep::tool_call("open_gate", json!({"gate": "north"})))
        .step(ScriptStep::template("Done: {toolOutput}"));
    let config = SessionConfig::new()
        .instructions("You are a castle guard.")
        .tool("open_gate")
        .tool_choice(ToolChoice::Required)
        .model(script);
    let mut created = ai.create_session("guard", config);
    let mut turn = ai.respond("guard", "Open the north gate, please.");
    let request = ai.last_request_id().unwrap();

    let created: Created = run_until(&mut app, &mut created).unwrap();
    assert_eq!(created.id, "guard");
    assert_eq!(created.tools, ["open_gate"]);
    let response = run_until(&mut app, &mut turn).unwrap();
    assert_eq!(response.text, r#"Done: {"gate":"north","opened":true}"#);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(
        response.tool_calls[0].output,
        ToolOutput::Json(json!({"gate": "north", "opened": true}))
    );
    assert!(
        app.world.get::<&Gate>(north).unwrap().open,
        "the handler changed the ECS world"
    );

    let seen = app.resources.expect::<Seen>();
    assert_eq!(
        seen.threads,
        [std::thread::current().id()],
        "handlers run on the engine thread"
    );

    // Streaming events, in order, for this request.
    tick_until(&mut app, |app| {
        app.resources.expect::<Log>().events.iter().any(|e| {
            matches!(
                e,
                AiEvent::Session {
                    event: TurnEvent::Text { .. },
                    ..
                }
            )
        })
    });
    let log = &app.resources.expect::<Log>().events;
    let turn_events: Vec<&TurnEvent> = log
        .iter()
        .filter_map(|e| match e {
            AiEvent::Session {
                session,
                request: Some(r),
                event,
            } if session == "guard" && *r == request => Some(event),
            _ => None,
        })
        .collect();
    assert!(matches!(turn_events[0], TurnEvent::ModelStep(step) if step.tool_calling_mode == "required"));
    assert!(
        turn_events
            .iter()
            .any(|e| matches!(e, TurnEvent::ToolCallStarted { call, .. } if call.name == "open_gate"))
    );
    assert!(
        turn_events
            .iter()
            .any(|e| matches!(e, TurnEvent::ToolCallCompleted(r) if !r.output.is_error()))
    );
    let text: String = turn_events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::Text { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, response.text);
    assert!(
        matches!(app.resources.expect::<Ai>().status(), AiStatus::Ready(info) if info.server.name == "space3d-scripted")
    );
    assert!(log.iter().any(|e| matches!(e, AiEvent::Ready(_))));
}

#[test]
fn npc_talk_streams_emotion_and_line_deltas() {
    let mut app = app(ScriptedBackend::new());
    struct Stock(u32);
    app.insert_resource(Stock(3));
    let ai = app.resources.expect_mut::<Ai>();
    ai.register_tool(
        AiTool::new("check_inventory", "Stock of an item.")
            .parameters(json!({"type": "object", "properties": {"item": {"type": "string"}}}))
            .handler(|ctx, call| {
                assert_eq!(call.owner, ToolOwner::Npc("gorm".into()));
                json!({"item": call.string("item").unwrap_or("?"), "stock": ctx.resources.expect::<Stock>().0})
            }),
    )
    .unwrap();
    let script = Script::new()
        .step(ScriptStep::tool_call("check_inventory", json!({"item": "iron sword"})))
        .step(ScriptStep::npc_reply(
            "Three swords, forty-five gold each.",
            Emotion::Proud,
            ["Buy one", "Too pricey"],
            false,
        ));
    let persona = Persona::new("Gorm")
        .role("the village blacksmith")
        .default_emotion(Emotion::Annoyed);
    let mut created = ai.create_npc(
        "gorm",
        NpcConfig::new(persona).grounding_tool("check_inventory").model(script),
    );
    let mut turn = ai.talk_with(
        "gorm",
        "Iron swords?",
        TalkOptions::new().context("The player just walked in."),
    );
    let request = ai.last_request_id();
    assert_eq!(run_until(&mut app, &mut created).unwrap().tools, ["check_inventory"]);
    let turn = run_until(&mut app, &mut turn).unwrap();
    assert_eq!(turn.npc, "gorm");
    assert_eq!(turn.line, "Three swords, forty-five gold each.");
    assert_eq!(turn.emotion, Emotion::Proud);
    assert_eq!(turn.player_options, ["Buy one", "Too pricey"]);
    assert_eq!(
        turn.tool_calls[0].output,
        ToolOutput::Json(json!({"item": "iron sword", "stock": 3}))
    );
    assert!(!turn.is_fallback);

    tick_until(&mut app, |app| {
        app.resources.expect::<Log>().events.iter().any(|e| {
            matches!(
                e,
                AiEvent::Npc {
                    event: NpcEvent::LineDelta(_),
                    ..
                }
            )
        })
    });
    let mut shown = String::new();
    let mut emotions = Vec::new();
    for event in &app.resources.expect::<Log>().events {
        if let AiEvent::Npc { npc, request: r, event } = event {
            assert_eq!((npc.as_str(), *r), ("gorm", request));
            match event {
                NpcEvent::Emotion(e) => emotions.push(*e),
                NpcEvent::LineDelta(delta) => shown.push_str(delta),
                NpcEvent::LineReset(line) => shown = line.clone(),
                _ => {}
            }
        }
    }
    assert_eq!(emotions, [Emotion::Proud]);
    assert_eq!(shown, turn.line);
}

#[test]
fn npc_fallbacks_and_text_replies() {
    let mut app = app(ScriptedBackend::new());
    let ai = app.resources.expect_mut::<Ai>();
    let blocked = Script::new().step(ScriptStep::error(AiErrorCode::GuardrailViolation));
    let options = NpcOptions {
        reply_format: Some(ReplyFormat::Structured),
        fallback_lines: Some(vec!["Not now, lad.".into()]),
        ..NpcOptions::default()
    };
    let _ = ai.create_npc(
        "strict",
        NpcConfig::new(Persona::new("Gorm"))
            .options(options)
            .model(blocked.clone()),
    );
    let _ = ai.create_npc(
        "plain",
        NpcConfig::new(Persona::new("Mira"))
            .reply_format(ReplyFormat::Text)
            .model(Script::new().step(ScriptStep::text("[grateful] Thank you, traveller."))),
    );
    let mut fallback = ai.talk("strict", "Tell me about the war.");
    let mut plain = ai.talk("plain", "Here's your ring.");
    let fallback = run_until(&mut app, &mut fallback).unwrap();
    assert!(fallback.is_fallback);
    assert_eq!(fallback.line, "Not now, lad.");
    let plain = run_until(&mut app, &mut plain).unwrap();
    assert_eq!(
        (plain.line.as_str(), plain.emotion),
        ("Thank you, traveller.", Emotion::Grateful)
    );
    assert!(plain.player_options.is_empty());
}

#[test]
fn decisions_content_and_barks_return_typed_results() {
    let mut app = app(ScriptedBackend::new());
    let ai = app.resources.expect_mut::<Ai>();
    let mut decision = ai.decide(
        DecisionRequest::new(
            "The goblin has 3 HP left.",
            [("attack", "Keep fighting"), ("flee", "Run"), ("beg", "")],
        )
        .actor(Persona::new("Snik").personality("Timid."))
        .context(json!({"hp": 3}))
        .model(Script::new().step(ScriptStep::decision("Snik is timid.", "FLEE", 81))),
    );
    let mut fallback = ai.decide(
        DecisionRequest::new("The ogre swings.", ["dodge", "block"])
            .fallback("dodge")
            .model(Script::new().step(ScriptStep::error(AiErrorCode::GuardrailViolation))),
    );
    let mut blocked = ai.decide(
        DecisionRequest::new("The ogre swings.", ["dodge", "block"])
            .model(Script::new().step(ScriptStep::error(AiErrorCode::GuardrailViolation))),
    );
    let schema = json!({"type": "object", "properties": {"name": {"type": "string"}, "damage": {"type": "integer"}}});
    let mut content = ai.generate_content(
        ContentRequest::new("A cursed sword.", schema)
            .model(Script::new().step(ScriptStep::json(json!({"name": "Drowned Fang", "damage": 45})))),
    );
    let _ = ai.create_npc(
        "gorm",
        NpcConfig::new(Persona::new("Gorm")).model(Script::new().step(ScriptStep::text("Gorm: \"Rain again.\""))),
    );
    let mut bark = ai.bark("gorm", "It starts to rain.");

    let decision = run_until(&mut app, &mut decision).unwrap();
    assert_eq!(
        (decision.option_id.as_str(), decision.confidence),
        ("flee", 81),
        "lenient case match"
    );
    assert_eq!(decision.reasoning, "Snik is timid.");
    let fallback = run_until(&mut app, &mut fallback).unwrap();
    assert!(fallback.is_fallback);
    assert_eq!(fallback.option_id, "dodge");
    let error = run_until(&mut app, &mut blocked).unwrap_err();
    assert_eq!(error.code, AiErrorCode::GuardrailViolation);
    assert!(error.is_blocked());
    assert_eq!(
        run_until(&mut app, &mut content).unwrap(),
        json!({"name": "Drowned Fang", "damage": 45})
    );
    assert_eq!(run_until(&mut app, &mut bark).unwrap(), "Rain again.");
}

/// A resource holding the responder of a deferred call and the frame to answer on.
#[derive(Default)]
struct Pending {
    responder: Option<(ToolResponder, u64)>,
}

#[test]
fn deferred_tool_replies_and_timeouts() {
    let mut app = app(ScriptedBackend::new());
    app.init_resource::<Pending>();
    // Replies three frames later (an "animation"), from a system.
    app.add_system(Stage::Update, |ctx| {
        let frame = ctx.time().frame;
        let pending = ctx.resources.expect_mut::<Pending>();
        if let Some((responder, due)) = pending.responder.take() {
            if frame >= due {
                assert!(responder.reply("The drawbridge is down."));
                assert!(!responder.reply("twice"), "the first reply wins");
            } else {
                pending.responder = Some((responder, due));
            }
        }
    });
    let ai = app.resources.expect_mut::<Ai>();
    ai.register_tool(
        AiTool::new("lower_bridge", "Lower the drawbridge.").handler(|ctx, call| {
            let due = ctx.time().frame + 3;
            ctx.resources.expect_mut::<Pending>().responder = Some((call.responder(), due));
            ToolReply::Deferred
        }),
    )
    .unwrap();
    ai.register_tool(
        AiTool::new("stuck", "Never answers.")
            .timeout_seconds(0.05)
            .handler(|ctx, call| {
                ctx.resources.insert(call.responder());
                ToolReply::Deferred
            }),
    )
    .unwrap();
    let script = Script::new()
        .step(ScriptStep::tool_call("lower_bridge", json!({})))
        .step(ScriptStep::tool_call("stuck", json!({})))
        .step(ScriptStep::template("{toolOutputs}"));
    let config = SessionConfig::new().tools(["lower_bridge", "stuck"]).model(script);
    let _ = ai.create_session("keep", config);
    let mut turn = ai.respond("keep", "Let us in.");
    let started = app.resources.expect::<Time>().frame;
    let response = run_until(&mut app, &mut turn).unwrap();
    assert_eq!(
        response.tool_calls[0].output,
        ToolOutput::text("The drawbridge is down.")
    );
    assert!(response.tool_calls[1].output.is_error());
    assert_eq!(
        response.text,
        "The drawbridge is down.\nError: Tool 'stuck' timed out after 0.05s."
    );
    assert!(
        app.resources.expect::<Time>().frame >= started + 3,
        "the engine kept ticking while waiting"
    );

    tick_until(&mut app, |app| {
        app.resources
            .expect::<Log>()
            .events
            .iter()
            .any(|e| matches!(e, AiEvent::ToolCancelled { .. }))
    });
    let cancelled = app
        .resources
        .expect::<Log>()
        .events
        .iter()
        .find_map(|e| match e {
            AiEvent::ToolCancelled { reason, owner, .. } => Some((reason.clone(), owner.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        cancelled,
        (
            "Error: Tool 'stuck' timed out after 0.05s.".into(),
            ToolOwner::Session("keep".into())
        )
    );
    let stuck = app.resources.remove::<ToolResponder>().unwrap();
    assert!(stuck.is_cancelled());
    assert!(!stuck.reply("too late"), "replies after tool/cancel are dropped");
}

#[test]
fn errors_are_typed() {
    let mut app = app(ScriptedBackend::new());
    let ai = app.resources.expect_mut::<Ai>();
    // Unknown tools fail before anything is sent.
    let mut unknown_tool = ai.create_session("s", SessionConfig::new().tool("teleport"));
    assert_eq!(ai.pending_requests(), 1, "only initialize is in flight");
    let mut missing = ai.respond("nobody", "Hello?");
    let mut no_model = ai.create_npc("m", NpcConfig::new(Persona::new("Mira")));
    let mut unavailable = ai.talk("m", "Hi");
    let bad_tool = ai.register_tool(AiTool::new("bad name", ""));
    assert_eq!(bad_tool.unwrap_err().code, AiErrorCode::InvalidParams);
    // A tool without a handler answers with an error output the model sees.
    ai.register_tool(AiTool::new("mute", "Has no handler.")).unwrap();
    let script = Script::new()
        .step(ScriptStep::tool_call("mute", json!({})))
        .step(ScriptStep::template("{toolOutput}"));
    let _ = ai.create_session("q", SessionConfig::new().tool("mute").model(script));
    let mut mute = ai.respond("q", "?");

    let error = run_until(&mut app, &mut unknown_tool).unwrap_err();
    assert_eq!(error.code, AiErrorCode::InvalidParams);
    assert!(error.message.contains("'teleport' is not registered"));
    let error = run_until(&mut app, &mut missing).unwrap_err();
    assert_eq!(error.code, AiErrorCode::SessionNotFound);
    assert_eq!(error.data.unwrap()["session"], "nobody");
    assert!(
        run_until(&mut app, &mut no_model).is_ok(),
        "creating on an unavailable system model works"
    );
    let error = run_until(&mut app, &mut unavailable).unwrap_err();
    assert!(error.is_unavailable());
    assert_eq!(error.code, AiErrorCode::ModelUnavailable);
    assert_eq!(
        run_until(&mut app, &mut mute).unwrap().text,
        "Error: No handler is registered for tool 'mute'."
    );
}

#[test]
fn unsupported_backend_fails_every_request() {
    let mut app = App::new();
    app.add_plugin(AiPlugin::new(BackendChoice::backend(
        space3d_ai::UnsupportedBackend::new("No model on this test rig."),
    )));
    let ai = app.resources.expect_mut::<Ai>();
    assert_eq!(ai.backend_kind(), BackendKind::Unsupported);
    let mut turn = ai.talk("anyone", "Hello?");
    let error = run_until(&mut app, &mut turn).unwrap_err();
    assert_eq!(error.code, AiErrorCode::Unsupported);
    assert_eq!(error.message, "No model on this test rig.");
    assert!(error.is_unavailable());
    tick_until(&mut app, |app| {
        matches!(app.resources.expect::<Ai>().status(), AiStatus::Unavailable(_))
    });
}

#[cfg(not(any(target_os = "android", target_vendor = "apple")))]
#[test]
fn auto_is_unsupported_off_apple_and_android() {
    let mut app = App::new();
    app.add_plugin(AiPlugin::auto());
    assert_eq!(app.resources.expect::<Ai>().backend_kind(), BackendKind::Unsupported);
}

#[test]
fn auto_or_scripted_falls_back_where_there_is_no_platform_model() {
    let mut app = App::new();
    app.add_plugin(AiPlugin::new(BackendChoice::auto_or(ScriptedBackend::new())));
    let kind = app.resources.expect::<Ai>().backend_kind();
    if space3d_ai::AppleBackend::is_linked() {
        assert_eq!(kind, BackendKind::Apple);
    } else {
        assert_eq!(kind, BackendKind::Scripted);
    }
}

#[test]
fn requests_never_block_the_frame() {
    let mut app = app(ScriptedBackend::new());
    let ai = app.resources.expect_mut::<Ai>();
    let slow = Script::new().step(ScriptStep::text("Finally.").delayed(150));
    let _ = ai.create_session("slow", SessionConfig::new().model(slow));
    let started = Instant::now();
    let mut turn = ai.respond("slow", "Take your time.");
    assert!(started.elapsed() < Duration::from_millis(50), "respond returns at once");
    let mut frames = 0;
    let mut slowest = Duration::ZERO;
    let text = loop {
        let frame = Instant::now();
        app.step_ticks(1);
        slowest = slowest.max(frame.elapsed());
        frames += 1;
        if let Some(result) = turn.poll() {
            break result.unwrap().text;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(text, "Finally.");
    assert!(
        frames > 10,
        "the engine ticked while the model worked ({frames} frames)"
    );
    assert!(
        slowest < Duration::from_millis(50),
        "no frame waited on the model ({slowest:?})"
    );
}

#[test]
fn tasks_integrate_with_async_queue_and_cancellation() {
    let mut app = app(ScriptedBackend::new());
    #[derive(Default)]
    struct Heard(Vec<String>);
    app.init_resource::<Heard>();
    let ai = app.resources.expect_mut::<Ai>();
    let waiting = Script::new().step(ScriptStep::text("…").delayed(60_000));
    let _ = ai.create_npc("sleepy", NpcConfig::new(Persona::new("Sleepy")).model(waiting));
    let turn = ai.talk("sleepy", "Wake up!");
    let mut cancel = ai.cancel_npc("sleepy");
    app.resources.expect_mut::<AsyncQueue>().track(turn, |ctx, result| {
        let text = match result {
            Ok(turn) => turn.line,
            Err(error) => error.code.to_string(),
        };
        ctx.resources.expect_mut::<Heard>().0.push(text);
    });
    assert_eq!(run_until(&mut app, &mut cancel).unwrap(), 1);
    tick_until(&mut app, |app| !app.resources.expect::<Heard>().0.is_empty());
    assert_eq!(app.resources.expect::<Heard>().0, ["cancelled"]);
}

#[test]
fn shutdown_and_drop_resolve_pending_tasks() {
    let mut app = app(ScriptedBackend::new());
    let ai = app.resources.expect_mut::<Ai>();
    let _ = ai.create_session(
        "s",
        SessionConfig::new().model(Script::new().step(ScriptStep::text("x").delayed(60_000))),
    );
    let mut turn = ai.respond("s", "wait");
    let mut shutdown = ai.shutdown();
    let mut after = ai.respond("s", "again");
    run_until(&mut app, &mut shutdown).unwrap();
    assert_eq!(run_until(&mut app, &mut turn).unwrap_err().code, AiErrorCode::Cancelled);
    assert_eq!(run_until(&mut app, &mut after).unwrap_err().code, AiErrorCode::ShutDown);

    let mut app = self::app(ScriptedBackend::new());
    let ai = app.resources.expect_mut::<Ai>();
    let _ = ai.create_session(
        "s",
        SessionConfig::new().model(Script::new().step(ScriptStep::text("x").delayed(60_000))),
    );
    let turn = ai.respond("s", "wait");
    drop(app);
    let result = turn.block().expect("the task resolves");
    assert_eq!(result.unwrap_err().code, AiErrorCode::Cancelled);
}

#[test]
fn world_state_and_change_events() {
    let mut app = app(ScriptedBackend::new());
    let ai = app.resources.expect_mut::<Ai>();
    let mut created = ai.create_world("village", json!({"player": {"gold": 60}}));
    let mut subscribed = ai.request("world/subscribe", json!({"world": "village", "path": "player"}));
    let mut set = ai.world_set("village", "player.gold", json!(45));
    let mut merged = ai.world_merge("village", "", json!({"day": 2}));
    let mut gold = ai.world_get("village", "player.gold");
    let mut absent = ai.world_get("village", "player.shield");
    assert_eq!(run_until(&mut app, &mut created).unwrap(), 0);
    assert_eq!(run_until(&mut app, &mut subscribed).unwrap()["subscription"], "sub1");
    assert_eq!(run_until(&mut app, &mut set).unwrap(), 1);
    assert_eq!(run_until(&mut app, &mut merged).unwrap(), 2);
    assert_eq!(run_until(&mut app, &mut gold).unwrap(), Some(json!(45)));
    assert_eq!(run_until(&mut app, &mut absent).unwrap(), None);
    tick_until(&mut app, |app| {
        app.resources
            .expect::<Log>()
            .events
            .iter()
            .any(|e| matches!(e, AiEvent::WorldChanged(_)))
    });
    let change = app
        .resources
        .expect::<Log>()
        .events
        .iter()
        .find_map(|e| match e {
            AiEvent::WorldChanged(change) => Some(change.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (change.path.as_str(), change.old_value, change.new_value),
        ("player.gold", Some(json!(60)), Some(json!(45)))
    );
}

#[test]
fn request_ids_are_sequential() {
    let mut app = app(ScriptedBackend::new());
    let ai = app.resources.expect_mut::<Ai>();
    let before = ai.last_request_id().unwrap();
    let _ = ai.availability();
    assert_eq!(ai.last_request_id(), Some(RequestId(before.0 + 1)));
    app.step_ticks(1);
}
