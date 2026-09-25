//! The bridge protocol on the scripted backend: the shared scenarios (which also run against
//! Apple's bridge in `apple_live.rs`) plus scripted-backend specifics.

mod common;

use common::{Harness, error_name, gorm, run_all, scripted};
use serde_json::json;
use space3d_ai::{AiBackend, RequestKind, Script, ScriptStep, ScriptedBackend};
use std::time::{Duration, Instant};

#[test]
fn shared_protocol_scenarios() {
    run_all(|| Box::new(ScriptedBackend::new()));
}

#[test]
fn system_model_is_unavailable_unless_configured() {
    let mut h = Harness::new(Box::new(ScriptedBackend::new()));
    let availability = h.result("model/availability", json!({}));
    assert_eq!(availability["available"], false);
    let created = h.result("session/create", json!({"session": "s"}));
    assert!(
        created["warnings"][0].as_str().unwrap().contains("unavailable"),
        "{created}"
    );
    let turn = h.call("session/respond", json!({"session": "s", "prompt": "Hi"}));
    assert_eq!(error_name(&turn), Some("model_unavailable"));
    assert_eq!(turn["error"]["code"], -32001);
}

#[test]
fn system_script_and_responder_play_for_the_system_model() {
    let backend = ScriptedBackend::new().with_system_script(Script::new().step(ScriptStep::text("From the script.")));
    let mut h = Harness::new(Box::new(backend));
    assert_eq!(h.result("model/availability", json!({}))["available"], true);
    h.result("session/create", json!({"session": "a"}));
    h.result("session/create", json!({"session": "b"}));
    // Each session plays its own copy.
    assert_eq!(
        h.result("session/respond", json!({"session": "a", "prompt": "1"}))["text"],
        "From the script."
    );
    assert_eq!(
        h.result("session/respond", json!({"session": "b", "prompt": "2"}))["text"],
        "From the script."
    );

    let backend = ScriptedBackend::new().with_system_model(|request| match request.kind {
        RequestKind::NpcTalk if request.prompt.contains("menu") => ScriptStep::text("[happy] Stew and bread."),
        RequestKind::NpcTalk => ScriptStep::text("[neutral] Hm."),
        _ => ScriptStep::text("?"),
    });
    let mut h = Harness::new(Box::new(backend));
    h.result("npc/create", json!({"npc": "mira", "persona": {"name": "Mira"}}));
    let turn = h.result("npc/talk", json!({"npc": "mira", "line": "What's on the menu?"}));
    assert_eq!(
        (turn["line"].as_str(), turn["emotion"].as_str()),
        (Some("Stew and bread."), Some("happy"))
    );
    assert_eq!(
        h.result("npc/talk", json!({"npc": "mira", "line": "Hello"}))["line"],
        "Hm."
    );
}

#[test]
fn delays_and_pacing_run_on_poll() {
    let backend = ScriptedBackend::new().with_pacing(Duration::from_millis(20));
    let mut h = Harness::new(Box::new(backend));
    h.result(
        "npc/create",
        json!({"npc": "slow", "persona": gorm(),
        "model": scripted(json!([{"text": "[amused] One two three four five six.", "chunks": 3, "delayMs": 40}]))}),
    );
    let started = Instant::now();
    let id = h.send("npc/talk", json!({"npc": "slow", "line": "Count.", "stream": true}));
    h.pump();
    assert!(h.index(|m| m["id"] == id).is_none(), "the delay has not elapsed");
    let turn = h.response(&id)["result"].clone();
    assert!(started.elapsed() >= Duration::from_millis(40 + 2 * 20));
    assert_eq!(turn["line"], "One two three four five six.");
    assert_eq!(h.notifications("npc/event", &id).len(), 4, "emotion + 3 paced deltas");
    h.assert_ordered(&id);
}

#[test]
fn shutdown_cancels_work_and_rejects_later_requests() {
    let mut h = Harness::new(Box::new(ScriptedBackend::new()));
    h.result(
        "session/create",
        json!({"session": "s", "model": scripted(json!([{"text": "late", "delayMs": 60_000}]))}),
    );
    let turn = h.send("session/respond", json!({"session": "s", "prompt": "Wait."}));
    assert_eq!(h.result("shutdown", json!({})), json!({}));
    assert_eq!(error_name(&h.response(&turn)), Some("cancelled"));
    let after = h.call("ping", json!({}));
    assert_eq!(error_name(&after), Some("shut_down"));
    assert_eq!(after["error"]["code"], -32023);
}

#[test]
fn unsupported_methods_and_invalid_messages_are_reported() {
    let mut h = Harness::new(Box::new(ScriptedBackend::new()));
    let transcript = h.call("session/transcript", json!({"session": "x"}));
    assert_eq!(transcript["error"]["code"], -32601);
    assert!(
        transcript["error"]["message"]
            .as_str()
            .unwrap()
            .contains("scripted backend")
    );
    h.send_raw(r#"{"jsonrpc":"2.0","id":1e300,"method":"ping"}"#);
    h.wait_for("an invalid id", Duration::from_secs(1), |m| {
        m["error"]["code"] == -32600
    });
    h.send_raw(r#"{"id":5,"method":"ping"}"#);
    h.wait_for("a missing jsonrpc", Duration::from_secs(1), |m| {
        m["id"] == 5 && m["error"]["code"] == -32600
    });
    let bad_step = h.call(
        "session/create",
        json!({"model": {"type": "scripted", "steps": [{"error": "nope"}]}}),
    );
    assert_eq!(bad_step["error"]["code"], -32602);
    assert!(
        bad_step["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("'model.steps[0].error'")
    );
    let bad_type = h.call("session/create", json!({"model": "quantum"}));
    assert!(
        bad_type["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown model type 'quantum'")
    );
    // Unknown members are warned about, not rejected.
    let warned = h.result(
        "session/create",
        json!({"model": "scripted", "colour": "red", "options": {"flavour": 1}}),
    );
    let warnings = warned["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|w| w.as_str().unwrap().contains("'colour'")));
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("'options.flavour'"))
    );
    // A notification never gets a response.
    h.notify("session/cancel", json!({"session": warned["session"]}));
    h.pump();
    assert!(h.messages.iter().all(|m| m["error"]["data"]["session"].is_null()));
}

#[test]
fn npc_world_and_memory_tools_run_locally() {
    let mut h = Harness::new(Box::new(ScriptedBackend::new()));
    h.result(
        "world/create",
        json!({"world": "village", "state": {"player": {"gold": 60}, "secret": "x"}}),
    );
    h.result("world/subscribe", json!({"world": "village", "path": "player"}));
    let created = h.result(
        "npc/create",
        json!({
            "npc": "gorm", "persona": gorm(), "world": "village",
            "options": {"worldReadable": ["player"], "worldWritable": ["player.gold"], "memoryTools": "all"},
            "model": scripted(json!([
                {"toolCalls": [{"name": "read_world_state", "arguments": {"path": "player.gold"}},
                               {"name": "read_world_state", "arguments": {"path": "secret"}}]},
                {"toolCalls": [{"name": "update_world_state", "arguments": {"path": "player.gold", "value": "15"}},
                               {"name": "change_relationship", "arguments": {"delta": 50}},
                               {"name": "remember_fact", "arguments": {"fact": "Aria paid 45 gold."}}]},
                {"json": {"emotion": "happy", "line": "Pleasure doing business."}},
            ])),
        }),
    );
    assert_eq!(
        created["tools"],
        json!([
            "read_world_state",
            "update_world_state",
            "remember_fact",
            "change_relationship"
        ])
    );
    let id = h.send(
        "npc/talk",
        json!({"npc": "gorm", "line": "Here's 45 gold.", "stream": true}),
    );
    let turn = h.response(&id)["result"].clone();
    let records = turn["toolCalls"].as_array().unwrap();
    assert_eq!(records[0]["output"], json!({"path": "player.gold", "value": 60}));
    assert_eq!(records[1]["isError"], true, "'secret' is not readable");
    assert_eq!(
        records[2]["output"],
        json!({"path": "player.gold", "value": 15}),
        "numbers stay numbers"
    );
    assert_eq!(turn["relationship"], 10, "clamped to maxRelationshipChange");
    let started: Vec<_> = h
        .notifications("npc/event", &id)
        .into_iter()
        .filter(|e| e["params"]["event"]["type"] == "toolCallStarted")
        .collect();
    assert!(started.iter().all(|e| e["params"]["event"]["execution"] == "local"));
    assert!(h.index(|m| m["method"] == "tool/call").is_none());
    let changed = h.index(|m| m["method"] == "world/changed").expect("world/changed");
    let response = h.index(|m| m["id"] == id && m.get("method").is_none()).unwrap();
    assert!(
        changed < response,
        "world changes from NPC tools arrive before the response"
    );
    assert_eq!(
        h.result("world/get", json!({"world": "village", "path": "player.gold"}))["value"],
        15
    );
    let list = h.result("world/list", json!({}));
    assert_eq!(list["worlds"][0]["npcs"], 1);
}

#[test]
fn backend_contract() {
    let backend = ScriptedBackend::new();
    assert!(backend.send("{}").is_err(), "not started");
    let inbox = space3d_ai::Inbox::new();
    backend.start(inbox.clone()).unwrap();
    assert!(backend.start(inbox.clone()).is_err(), "started twice");
    backend.send(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
    assert_eq!(inbox.drain(), [r#"{"id":1,"jsonrpc":"2.0","result":{}}"#]);
    backend.shutdown();
    backend.send(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#).unwrap_err();
    assert!(inbox.is_empty(), "nothing is delivered after shutdown");
}
