//! Opt-in: the same protocol scenarios and engine-level flows against Apple's real bridge
//! (`libOpenAppleModelsFFI`) through [`AppleBackend`].
//!
//! ```sh
//! (cd open-apple-models && swift build -c release --product OpenAppleModelsFFI)
//! OAM_APPLE_LIB_DIR=$PWD/open-apple-models/.build/release \
//!     cargo test -p space3d-ai --features apple-ffi --test apple_live -- --nocapture
//! # also run Apple's on-device model (needs Apple Intelligence):
//! OAM_APPLE_LIVE_MODEL=1 OAM_APPLE_LIB_DIR=… cargo test -p space3d-ai --features apple-ffi --test apple_live
//! ```
//!
//! Without the feature the tests pass as skipped.

mod common;

use common::{Harness, run_all, run_until};
use serde_json::json;
use space3d_ai::prelude::*;
use space3d_ai::{AiStatus, AppleBackend};
use space3d_core::prelude::*;
use std::time::Instant;

fn linked() -> bool {
    if !AppleBackend::is_linked() {
        eprintln!("skipped: build with `--features apple-ffi` and OAM_APPLE_LIB_DIR to run against Apple's bridge");
    }
    AppleBackend::is_linked()
}

#[test]
fn apple_bridge_passes_the_shared_scenarios() {
    if !linked() {
        return;
    }
    eprintln!(
        "open-apple-models {}",
        AppleBackend::library_version().unwrap_or_default()
    );
    run_all(|| Box::new(AppleBackend::new()));
}

#[test]
fn apple_bridge_identifies_itself() {
    if !linked() {
        return;
    }
    let mut h = Harness::new(Box::new(AppleBackend::new()));
    let init = h.result("initialize", json!({"protocolVersion": "1.0"}));
    assert_eq!(init["server"]["name"], "open-apple-models");
    eprintln!("model: {}", init["model"]);
    assert_eq!(h.backend.kind(), space3d_ai::BackendKind::Apple);
}

/// The engine-level API (tools with ECS access, streaming events) over Apple's bridge with a
/// scripted model.
#[test]
fn apple_bridge_runs_engine_tools_with_a_scripted_model() {
    if !linked() {
        return;
    }
    struct Gold(u32);
    let mut app = App::new();
    app.insert_resource(Gold(60));
    app.add_plugin(
        AiPlugin::new(BackendChoice::backend(AppleBackend::new())).with_tool(
            AiTool::new("pay", "Take gold from the player.")
                .parameters(
                    json!({"type": "object", "properties": {"amount": {"type": "integer"}}, "required": ["amount"]}),
                )
                .handler(|ctx, call| {
                    let amount = call.i64("amount").unwrap_or(0) as u32;
                    let gold = ctx.resources.expect_mut::<Gold>();
                    gold.0 = gold.0.saturating_sub(amount);
                    json!({"paid": amount, "left": gold.0})
                }),
        ),
    );
    let script = Script::new()
        .step(ScriptStep::tool_call("pay", json!({"amount": 45})))
        .step(ScriptStep::npc_reply(
            "Pleasure doing business.",
            Emotion::Happy,
            ["Bye."],
            true,
        ));
    let ai = app.resources.expect_mut::<Ai>();
    let mut created = ai.create_npc(
        "gorm",
        NpcConfig::new(Persona::new("Gorm")).grounding_tool("pay").model(script),
    );
    let mut turn = ai.talk("gorm", "Here you go.");
    let created = run_until(&mut app, &mut created).expect("npc/create");
    assert_eq!(created.tools, ["pay"]);
    let turn = run_until(&mut app, &mut turn).expect("npc/talk");
    assert_eq!(turn.line, "Pleasure doing business.");
    assert!(turn.ends_conversation);
    assert_eq!(app.resources.expect::<Gold>().0, 15);
    assert_eq!(
        turn.tool_calls[0].output,
        ToolOutput::Json(json!({"paid": 45, "left": 15}))
    );
    assert!(
        matches!(app.resources.expect::<Ai>().status(), AiStatus::Ready(info) if info.server.name == "open-apple-models")
    );
}

/// Apple's on-device model with an engine tool (opt-in: `OAM_APPLE_LIVE_MODEL=1`).
#[test]
fn apple_on_device_model_grounds_an_npc_with_an_engine_tool() {
    if !linked() || std::env::var_os("OAM_APPLE_LIVE_MODEL").is_none() {
        eprintln!("skipped: set OAM_APPLE_LIVE_MODEL=1 to run Apple's on-device model");
        return;
    }
    struct Stock(u32);
    let mut app = App::new();
    app.insert_resource(Stock(3));
    app.add_plugin(
        AiPlugin::new(BackendChoice::backend(AppleBackend::new())).with_tool(
            AiTool::new(
                "check_inventory",
                "Look up how many of an item are in stock and its price in gold.",
            )
            .parameters(json!({"type": "object", "properties": {"item": {"type": "string"}}, "required": ["item"]}))
            .handler(|ctx, call| {
                let item = call.string("item").unwrap_or("?").to_owned();
                let stock = ctx.resources.expect::<Stock>().0;
                json!({"item": item, "stock": stock, "price_gold": 45})
            }),
        ),
    );
    let ai = app.resources.expect_mut::<Ai>();
    let mut availability = ai.availability();
    let availability = run_until(&mut app, &mut availability).expect("model/availability");
    eprintln!("availability: {availability:?}");
    if !availability.available {
        eprintln!(
            "skipped: the on-device model is unavailable ({:?})",
            availability.reason
        );
        return;
    }
    let persona = Persona::new("Gorm")
        .role("the village blacksmith")
        .personality("Gruff but fair.")
        .speaking_style("Short, blunt sentences.");
    let ai = app.resources.expect_mut::<Ai>();
    let mut created = ai.create_npc("gorm", NpcConfig::new(persona).grounding_tool("check_inventory"));
    run_until(&mut app, &mut created).expect("npc/create");
    let started = Instant::now();
    let mut turn = app
        .resources
        .expect_mut::<Ai>()
        .talk("gorm", "Do you have iron swords? How much?");
    let turn = run_until(&mut app, &mut turn).expect("npc/talk");
    eprintln!(
        "{:.1}s {:?}: {} (options {:?})",
        started.elapsed().as_secs_f64(),
        turn.emotion,
        turn.line,
        turn.player_options
    );
    assert!(!turn.line.is_empty());
    assert_eq!(
        turn.tool_calls.first().map(|r| r.call.name.as_str()),
        Some("check_inventory")
    );

    let started = Instant::now();
    let mut decision = app.resources.expect_mut::<Ai>().decide(
        DecisionRequest::new(
            "The goblin has 3 HP left and the player is at full health.",
            [
                ("attack", "Keep fighting"),
                ("flee", "Run into the woods"),
                ("beg", "Beg for mercy"),
            ],
        )
        .actor(Persona::new("Snik").personality("Timid and greedy."))
        .fallback("flee"),
    );
    let decision = run_until(&mut app, &mut decision).expect("decision/decide");
    eprintln!(
        "{:.1}s decision {} ({}): {}",
        started.elapsed().as_secs_f64(),
        decision.option_id,
        decision.confidence,
        decision.reasoning
    );
    assert!(["attack", "flee", "beg"].contains(&decision.option_id.as_str()));
}
