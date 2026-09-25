//! Shared test helpers: a raw JSON-RPC harness over any [`AiBackend`], protocol scenarios that
//! hold on every backend (run against the scripted backend always and against Apple's bridge
//! when it is linked), and an [`App`] driver for engine-level tests.

#![allow(dead_code)]

use serde_json::{Value, json};
use space3d_ai::{AiBackend, AiResult, Inbox};
use space3d_core::App;
use space3d_tasks::Task;
use std::time::{Duration, Instant};

type ToolResponder = Box<dyn FnMut(&Value) -> Option<Value>>;

/// Drives a backend with raw protocol lines and records everything it sends.
pub struct Harness {
    pub backend: Box<dyn AiBackend>,
    inbox: Inbox,
    /// Every message received, in order.
    pub messages: Vec<Value>,
    next_id: u64,
    /// Answers `tool/call` requests (params → result); `None` leaves them unanswered.
    responder: Option<ToolResponder>,
}

impl Harness {
    pub fn new(backend: Box<dyn AiBackend>) -> Self {
        let inbox = Inbox::new();
        backend.start(inbox.clone()).expect("backend starts");
        Self {
            backend,
            inbox,
            messages: Vec::new(),
            next_id: 1000,
            responder: None,
        }
    }

    /// Answers every `tool/call` with `f(params)` (a `result` object), or leaves it pending (`None`).
    pub fn respond_to_tools(&mut self, f: impl FnMut(&Value) -> Option<Value> + 'static) {
        self.responder = Some(Box::new(f));
    }

    pub fn send_raw(&mut self, line: &str) {
        self.backend.send(line).expect("send");
    }

    /// Sends a request and returns its id (a string, like the Swift test harness uses).
    pub fn send(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(format!("r{}", self.next_id));
        self.send_raw(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string());
        id
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send_raw(&json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
    }

    /// Pulls delivered lines, answering `tool/call`s with the responder.
    pub fn pump(&mut self) {
        self.backend.poll();
        for line in self.inbox.drain() {
            let message: Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid JSON from backend: {e}: {line}"));
            assert!(!line.contains('\n'), "a message must be one line");
            let is_tool_call = message["method"] == "tool/call";
            self.messages.push(message.clone());
            if is_tool_call
                && let Some(responder) = self.responder.as_mut()
                && let Some(result) = responder(&message["params"])
            {
                let reply = if result.get("error").is_some() {
                    json!({"jsonrpc": "2.0", "id": message["id"], "error": result["error"]})
                } else {
                    json!({"jsonrpc": "2.0", "id": message["id"], "result": result})
                };
                self.send_raw(&reply.to_string());
            }
        }
    }

    /// Waits (pumping) until `predicate` matches a received message.
    pub fn wait_for(&mut self, what: &str, timeout: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if let Some(found) = self.messages.iter().find(|m| predicate(m)) {
                return found.clone();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; got {:#?}",
                self.messages
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The response to request `id` (waits up to 20 s).
    pub fn response(&mut self, id: &Value) -> Value {
        let id = id.clone();
        self.wait_for(&format!("the response to {id}"), Duration::from_secs(20), move |m| {
            m["id"] == id && m.get("method").is_none()
        })
    }

    /// Sends a request and waits for its response.
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.response(&id)
    }

    /// Sends a request and returns its `result` (panics on an error response).
    pub fn result(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(response.get("error").is_none(), "{method} failed: {response}");
        response["result"].clone()
    }

    /// Index of the first message matching `predicate`.
    pub fn index(&self, predicate: impl Fn(&Value) -> bool) -> Option<usize> {
        self.messages.iter().position(predicate)
    }

    /// Notifications `method` of request `id`.
    pub fn notifications(&self, method: &str, id: &Value) -> Vec<Value> {
        self.messages
            .iter()
            .filter(|m| m["method"] == method && m["params"]["requestId"] == *id)
            .cloned()
            .collect()
    }

    /// Asserts every notification and `tool/call` of request `id` precedes its response.
    pub fn assert_ordered(&self, id: &Value) {
        let response = self
            .index(|m| m["id"] == *id && m.get("method").is_none())
            .expect("a response");
        for (index, message) in self.messages.iter().enumerate() {
            if message["params"]["requestId"] == *id
                && message.get("method").is_some()
                && message["method"] != "tool/cancel"
            {
                assert!(index < response, "{message} arrived after the response");
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.backend.shutdown();
    }
}

pub fn error_name(response: &Value) -> Option<&str> {
    response["error"]["data"]["code"].as_str()
}

pub fn scripted(steps: Value) -> Value {
    json!({"type": "scripted", "steps": steps})
}

pub fn gate_tool() -> Value {
    json!({"name": "open_gate", "description": "Ask the game engine to open a named gate.",
           "parameters": {"type": "object", "properties": {"gate": {"type": "string"}}, "required": ["gate"]}})
}

pub fn inventory_tool() -> Value {
    json!({"name": "check_inventory", "description": "Look up stock and price of an item.",
           "parameters": {"type": "object", "properties": {"item": {"type": "string"}}, "required": ["item"]}})
}

pub fn gorm() -> Value {
    json!({"name": "Gorm", "role": "the village blacksmith", "personality": "Gruff but fair."})
}

pub fn reply(line: &str, emotion: &str) -> Value {
    json!({"json": {"emotion": emotion, "line": line, "player_options": ["Buy one.", "Goodbye."], "ends_conversation": false}})
}

// ------------------------------------------------------------------------------------------
// Protocol scenarios (identical expectations on every backend)

/// `initialize`, `ping`, unknown methods, parse errors, batches.
pub fn scenario_core(h: &mut Harness) {
    let init = h.result(
        "initialize",
        json!({"client": {"name": "space3d-tests", "version": "1"}, "protocolVersion": "1.0"}),
    );
    assert_eq!(init["protocolVersion"], "1.0");
    assert!(
        init["capabilities"]["clientRequests"]
            .as_array()
            .unwrap()
            .contains(&json!("tool/call"))
    );
    assert!(init["model"]["available"].is_boolean());
    assert_eq!(h.result("ping", json!({})), json!({}));

    let unknown = h.call("quest/narrate", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(error_name(&unknown), Some("method_not_found"));

    h.send_raw("{not json");
    let parse = h.wait_for("a parse error", Duration::from_secs(5), |m| {
        m["error"]["code"] == -32700
    });
    assert_eq!(parse["id"], Value::Null);
    h.send_raw(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#);
    h.wait_for("a batch error", Duration::from_secs(5), |m| {
        m["error"]["code"] == -32600
    });
    let major = h.call("initialize", json!({"protocolVersion": "2.0"}));
    assert_eq!(major["error"]["code"], -32602);
}

/// A session turn with a client tool: streaming events, `tool/call`, ordering, result shape.
pub fn scenario_session_tool_round_trip(h: &mut Harness) {
    let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = calls.clone();
    h.respond_to_tools(move |params| {
        seen.borrow_mut().push(params.clone());
        Some(json!({"output": "The portcullis chain is jammed."}))
    });
    let created = h.result(
        "session/create",
        json!({
            "session": "guard",
            "instructions": "You are a castle guard.",
            "tools": [gate_tool()],
            "options": {"toolChoice": "required"},
            "model": scripted(json!([
                {"toolCalls": [{"name": "open_gate", "arguments": {"gate": "north"}}]},
                {"template": "Gate report: {toolOutput}", "chunks": 3},
            ])),
        }),
    );
    assert_eq!(created["session"], "guard");
    let id = h.send(
        "session/respond",
        json!({"session": "guard", "prompt": "Open the north gate.", "stream": true}),
    );
    let response = h.response(&id);
    let result = &response["result"];
    assert_eq!(result["session"], "guard");
    assert_eq!(result["text"], "Gate report: The portcullis chain is jammed.");
    let record = &result["toolCalls"][0];
    assert_eq!(record["call"]["name"], "open_gate");
    assert_eq!(record["call"]["arguments"], json!({"gate": "north"}));
    assert_eq!(record["output"], "The portcullis chain is jammed.");
    assert_eq!(record["isError"], false);
    assert!(result["usage"]["totalTokens"].as_u64().is_some());
    assert!(result["steps"].as_array().unwrap().len() >= 2);

    let call = calls.borrow()[0].clone();
    assert_eq!(call["session"], "guard");
    assert_eq!(call["requestId"], id);
    assert_eq!(call["call"]["id"], record["call"]["id"]);

    let events: Vec<Value> = h
        .notifications("session/event", &id)
        .iter()
        .map(|e| e["params"]["event"].clone())
        .collect();
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    assert_eq!(types.first(), Some(&"modelStep"));
    let started = types
        .iter()
        .position(|t| *t == "toolCallStarted")
        .expect("toolCallStarted");
    let completed = types
        .iter()
        .position(|t| *t == "toolCallCompleted")
        .expect("toolCallCompleted");
    assert!(started < completed);
    assert_eq!(events[started]["execution"], "client");
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "text")
        .map(|e| e["delta"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(text, "Gate report: The portcullis chain is jammed.");
    h.assert_ordered(&id);
    let tool_call = h.index(|m| m["method"] == "tool/call").unwrap();
    let response_index = h.index(|m| m["id"] == id && m.get("method").is_none()).unwrap();
    assert!(tool_call < response_index);
}

/// Client tool errors reach the model; timeouts send `tool/cancel`.
pub fn scenario_tool_errors_and_timeouts(h: &mut Harness) {
    h.respond_to_tools(|params| {
        if params["call"]["name"] == "check_inventory" {
            Some(json!({"error": {"code": -32000, "message": "The shop is closed."}}))
        } else {
            None
        }
    });
    let mut slow = inventory_tool();
    slow["name"] = json!("ledger");
    slow["timeoutSeconds"] = json!(0.05);
    h.result(
        "session/create",
        json!({
            "session": "shop",
            "tools": [inventory_tool(), slow],
            "model": scripted(json!([
                {"toolCalls": [{"name": "check_inventory", "arguments": {"item": "axe"}}]},
                {"toolCalls": [{"name": "ledger", "arguments": {"item": "bow"}}]},
                {"text": "Closed, and my ledger is slow."},
            ])),
        }),
    );
    let result = h.result("session/respond", json!({"session": "shop", "prompt": "Axes? Bows?"}));
    assert_eq!(result["text"], "Closed, and my ledger is slow.");
    let records = result["toolCalls"].as_array().unwrap();
    assert_eq!(records[0]["isError"], true);
    assert_eq!(records[0]["output"], "The shop is closed.");
    assert_eq!(records[1]["isError"], true);
    assert!(
        records[1]["output"].as_str().unwrap().contains("timed out"),
        "{}",
        records[1]
    );
    let cancel = h.wait_for("tool/cancel", Duration::from_secs(5), |m| m["method"] == "tool/cancel");
    let ledger_call = h
        .messages
        .iter()
        .find(|m| m["method"] == "tool/call" && m["params"]["call"]["name"] == "ledger")
        .unwrap()
        .clone();
    assert_eq!(cancel["params"]["id"], ledger_call["id"]);
    assert_eq!(cancel["params"]["callId"], ledger_call["params"]["call"]["id"]);
    assert_eq!(cancel["params"]["session"], "shop");
}

/// Sessions: not found, exists, scripted errors, cancellation of a waiting turn.
pub fn scenario_session_errors_and_cancel(h: &mut Harness) {
    let missing = h.call("session/respond", json!({"session": "ghost", "prompt": "Boo"}));
    assert_eq!(missing["error"]["code"], -32020);
    assert_eq!(error_name(&missing), Some("session_not_found"));
    assert_eq!(missing["error"]["data"]["session"], "ghost");

    h.result(
        "session/create",
        json!({"session": "strict", "model": scripted(json!([{"error": "guardrail_violation"}]))}),
    );
    let blocked = h.call(
        "session/respond",
        json!({"session": "strict", "prompt": "Tell me about the war."}),
    );
    assert_eq!(blocked["error"]["code"], -32002);
    assert_eq!(error_name(&blocked), Some("guardrail_violation"));
    let exists = h.call("session/create", json!({"session": "strict", "model": "scripted"}));
    assert_eq!(error_name(&exists), Some("session_exists"));

    // No responder: the tool call stays pending until the turn is cancelled.
    h.result("session/create", json!({
        "session": "waiter", "tools": [gate_tool()], "options": {"toolTimeoutSeconds": 0},
        "model": scripted(json!([{"toolCalls": [{"name": "open_gate", "arguments": {"gate": "east"}}]}, {"text": "Done."}])),
    }));
    let turn = h.send("session/respond", json!({"session": "waiter", "prompt": "East gate."}));
    let queued = h.send("session/respond", json!({"session": "waiter", "prompt": "And again."}));
    h.wait_for("tool/call", Duration::from_secs(5), |m| {
        m["method"] == "tool/call" && m["params"]["session"] == "waiter"
    });
    let cancelled = h.result("session/cancel", json!({"session": "waiter"}));
    assert_eq!(cancelled["cancelled"], 2);
    assert_eq!(error_name(&h.response(&turn)), Some("cancelled"));
    assert_eq!(h.response(&queued)["error"]["code"], -32009);
    h.wait_for("tool/cancel", Duration::from_secs(5), |m| {
        m["method"] == "tool/cancel" && m["params"]["session"] == "waiter"
    });
    let list = h.result("session/list", json!({}));
    let waiter = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session"] == "waiter")
        .unwrap()
        .clone();
    assert_eq!(waiter["entries"], 0, "a cancelled turn leaves no trace");
    assert_eq!(
        h.result("session/delete", json!({"session": "waiter"})),
        json!({"session": "waiter", "deleted": true})
    );
}

/// NPCs: create, streamed talk, client tools, fallbacks and text replies, cancel.
pub fn scenario_npcs(h: &mut Harness) {
    h.respond_to_tools(|params| {
        Some(json!({"output": {"item": params["call"]["arguments"]["item"], "stock": 3, "price_gold": 45}}))
    });
    let created = h.result(
        "npc/create",
        json!({
            "npc": "gorm", "persona": gorm(), "tools": [inventory_tool()],
            "options": {"groundingTool": "check_inventory"},
            "model": scripted(json!([
                {"toolCalls": [{"name": "check_inventory", "arguments": {"item": "iron sword"}}]},
                reply("Three iron swords, forty-five gold each.", "proud"),
            ])),
        }),
    );
    assert_eq!(created["npc"], "gorm");
    assert_eq!(created["tools"], json!(["check_inventory"]));
    let id = h.send(
        "npc/talk",
        json!({"npc": "gorm", "line": "Iron swords?", "stream": true}),
    );
    let turn = h.response(&id)["result"].clone();
    assert_eq!(turn["line"], "Three iron swords, forty-five gold each.");
    assert_eq!(turn["emotion"], "proud");
    assert_eq!(turn["playerOptions"], json!(["Buy one.", "Goodbye."]));
    assert_eq!(turn["endsConversation"], false);
    assert_eq!(turn["isFallback"], false);
    assert_eq!(turn["relationship"], 0);
    assert_eq!(turn["toolCalls"][0]["output"]["stock"], 3);
    let tool_call = h.messages.iter().find(|m| m["method"] == "tool/call").unwrap().clone();
    assert_eq!(tool_call["params"]["npc"], "gorm");
    assert_eq!(tool_call["params"]["requestId"], id);
    let events: Vec<Value> = h
        .notifications("npc/event", &id)
        .iter()
        .map(|e| e["params"]["event"].clone())
        .collect();
    assert_eq!(
        events.iter().find(|e| e["type"] == "emotion").unwrap()["emotion"],
        "proud"
    );
    let mut shown = String::new();
    for event in &events {
        if event["type"] == "lineDelta" {
            shown.push_str(event["delta"].as_str().unwrap());
        }
        if event["type"] == "lineReset" {
            shown = event["line"].as_str().unwrap().to_owned();
        }
    }
    assert_eq!(shown, "Three iron swords, forty-five gold each.");
    h.assert_ordered(&id);
    let list = h.result("npc/list", json!({}));
    assert_eq!(list["npcs"][0]["turnCount"], 1);
    assert_eq!(list["npcs"][0]["name"], "Gorm");

    // Structured-only: a blocked turn becomes a fallback line.
    h.result(
        "npc/create",
        json!({"npc": "strict", "persona": gorm(),
        "options": {"replyFormat": "structured", "fallbackLines": ["Not now, lad."]},
        "model": scripted(json!([{"error": "guardrail_violation"}]))}),
    );
    let blocked = h.result("npc/talk", json!({"npc": "strict", "line": "Tell me about the war."}));
    assert_eq!(blocked["isFallback"], true);
    assert_eq!(blocked["line"], "Not now, lad.");
    // Automatic: retried once as plain text.
    h.result(
        "npc/create",
        json!({"npc": "auto", "persona": gorm(),
        "model": scripted(json!([{"error": "guardrail_violation"}, {"text": "[worried] Let's not speak of it."}]))}),
    );
    let retried = h.result("npc/talk", json!({"npc": "auto", "line": "Tell me about the war."}));
    assert_eq!(retried["isFallback"], false);
    assert_eq!(retried["line"], "Let's not speak of it.");
    assert_eq!(retried["emotion"], "worried");
    // Text format.
    h.result(
        "npc/create",
        json!({"npc": "plain", "persona": gorm(), "options": {"replyFormat": "text"},
        "model": scripted(json!([{"text": "[happy] Welcome to the forge!"}]))}),
    );
    let plain = h.result("npc/talk", json!({"npc": "plain", "line": "Hello"}));
    assert_eq!(plain["line"], "Welcome to the forge!");
    assert_eq!(plain["emotion"], "happy");
    assert_eq!(plain["playerOptions"], json!([]));
    // Without the fallback, the error is returned.
    h.result(
        "npc/create",
        json!({"npc": "raw", "persona": gorm(),
        "options": {"replyFormat": "structured", "fallbackOnGuardrail": false},
        "model": scripted(json!([{"error": "guardrail_violation"}]))}),
    );
    let raw = h.call("npc/talk", json!({"npc": "raw", "line": "War?"}));
    assert_eq!(raw["error"]["code"], -32002);
    // Barks.
    h.result("npc/create", json!({"npc": "barker", "persona": gorm(), "model": scripted(json!([{"text": "Rain again. Good for quenching."}]))}));
    assert_eq!(
        h.result("npc/bark", json!({"npc": "barker", "situation": "It starts to rain."})),
        json!({"npc": "barker", "line": "Rain again. Good for quenching."})
    );
    // Errors.
    let missing = h.call("npc/talk", json!({"npc": "nobody", "line": "Hello?"}));
    assert_eq!(error_name(&missing), Some("npc_not_found"));
    assert_eq!(missing["error"]["data"]["npc"], "nobody");
    let duplicate = h.call(
        "npc/create",
        json!({"npc": "gorm", "persona": gorm(), "model": "scripted"}),
    );
    assert_eq!(error_name(&duplicate), Some("npc_exists"));
    let bad = h.call(
        "npc/create",
        json!({"npc": "x", "persona": gorm(), "options": {"groundingTool": "nope"}, "model": "scripted"}),
    );
    assert_eq!(bad["error"]["code"], -32602);
    assert_eq!(
        h.result("npc/delete", json!({"npc": "raw"})),
        json!({"npc": "raw", "deleted": true})
    );
}

/// An NPC turn waiting on a tool is cancelled with its queued turn.
pub fn scenario_npc_cancel(h: &mut Harness) {
    h.result("npc/create", json!({"npc": "waiting", "persona": gorm(), "tools": [inventory_tool()],
        "options": {"toolTimeoutSeconds": 0},
        "model": scripted(json!([{"toolCalls": [{"name": "check_inventory", "arguments": {"item": "shield"}}]}, reply("Never mind.", "neutral")]))}));
    let talk = h.send("npc/talk", json!({"npc": "waiting", "line": "Shields?"}));
    let queued = h.send("npc/talk", json!({"npc": "waiting", "line": "And helmets?"}));
    h.wait_for("tool/call", Duration::from_secs(5), |m| {
        m["method"] == "tool/call" && m["params"]["npc"] == "waiting"
    });
    assert_eq!(h.result("npc/cancel", json!({"npc": "waiting"}))["cancelled"], 2);
    assert_eq!(error_name(&h.response(&talk)), Some("cancelled"));
    assert_eq!(error_name(&h.response(&queued)), Some("cancelled"));
    h.wait_for("tool/cancel", Duration::from_secs(5), |m| {
        m["method"] == "tool/cancel" && m["params"]["npc"] == "waiting"
    });
    let list = h.result("npc/list", json!({}));
    let npc = list["npcs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["npc"] == "waiting")
        .unwrap()
        .clone();
    assert_eq!(npc["turnCount"], 0);
}

/// Decisions and content.
pub fn scenario_decisions_and_content(h: &mut Harness) {
    let options =
        json!([{"id": "attack", "description": "Keep fighting"}, {"id": "flee", "description": "Run"}, "beg"]);
    let decision = h.result(
        "decision/decide",
        json!({
            "situation": "The goblin has 3 HP left.", "options": options,
            "actor": {"name": "Snik", "personality": "Timid and greedy."}, "context": {"hp": 3},
            "model": scripted(json!([{"json": {"reasoning": "Snik is timid.", "choice": "flee", "confidence": 78}}])),
        }),
    );
    assert_eq!(decision["optionID"], "flee");
    assert_eq!(decision["reasoning"], "Snik is timid.");
    assert_eq!(decision["confidence"], 78);
    assert_eq!(decision["isFallback"], false);
    let only = h.result(
        "decision/decide",
        json!({"situation": "Cornered.", "options": ["fight"], "model": "scripted"}),
    );
    assert_eq!(only["optionID"], "fight");
    assert_eq!(only["confidence"], 100);
    let fallback = h.result(
        "decision/decide",
        json!({"situation": "The ogre swings.", "options": options,
        "fallbackOptionID": "flee", "model": scripted(json!([{"error": "guardrail_violation"}]))}),
    );
    assert_eq!(fallback["optionID"], "flee");
    assert_eq!(fallback["isFallback"], true);
    let duplicate = h.call(
        "decision/decide",
        json!({"situation": "x", "options": ["a", "a"], "model": "scripted"}),
    );
    assert!(
        duplicate["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Duplicate option id 'a'")
    );
    let no_situation = h.call("decision/decide", json!({"options": ["a", "b"], "model": "scripted"}));
    assert_eq!(
        no_situation["error"]["message"],
        "Missing required parameter 'situation'."
    );

    h.respond_to_tools(|_| Some(json!({"output": {"distance": 12}})));
    let with_tool = h.result("decision/decide", json!({
        "situation": "An adventurer approaches.", "options": options,
        "tools": [{"name": "measure", "description": "Distance to a target in meters.",
                   "parameters": {"type": "object", "properties": {"target": {"type": "string"}}}}],
        "toolChoice": "required",
        "model": scripted(json!([{"toolCalls": [{"name": "measure", "arguments": {"target": "adventurer"}}]},
                                 {"json": {"reasoning": "Far enough to run.", "choice": "flee", "confidence": 60}}])),
    }));
    assert_eq!(with_tool["toolCalls"][0]["output"]["distance"], 12);

    let many = h.result(
        "decision/decideMany",
        json!({
            "maxConcurrency": 1,
            "requests": [{"situation": "A hears a noise.", "options": ["investigate", "ignore"]},
                         {"situation": "B sees blood.", "options": ["investigate", "ignore"]},
                         {"situation": "C is alone.", "options": ["wait"]}],
            "model": scripted(json!([{"json": {"reasoning": "Duty.", "choice": "investigate", "confidence": 90}},
                                     {"error": "guardrail_violation"}])),
        }),
    );
    let results = many["results"].as_array().unwrap();
    assert_eq!(results[0]["optionID"], "investigate");
    assert_eq!(results[1]["error"]["data"]["code"], "guardrail_violation");
    assert_eq!(results[2]["optionID"], "wait");

    let schema = json!({"type": "object", "properties": {"name": {"type": "string"},
        "rarity": {"type": "string", "enum": ["common", "rare", "legendary"]}}, "required": ["name", "rarity"]});
    let content = h.result(
        "content/generate",
        json!({"prompt": "A cursed sword.", "schema": schema,
        "model": scripted(json!([{"json": {"name": "Drowned Fang", "rarity": "legendary"}}]))}),
    );
    assert_eq!(
        content["content"],
        json!({"name": "Drowned Fang", "rarity": "legendary"})
    );
    let invalid = h.call(
        "content/generate",
        json!({"prompt": "x", "schema": {"type": "nope"}, "model": "scripted"}),
    );
    assert_eq!(invalid["error"]["code"], -32007);
}

/// World state: create, set, get, merge, subscriptions.
pub fn scenario_world(h: &mut Harness) {
    assert_eq!(
        h.result(
            "world/create",
            json!({"world": "village", "state": {"player": {"gold": 60}}})
        )["version"],
        0
    );
    let sub = h.result("world/subscribe", json!({"world": "village", "path": "player"}));
    let subscription = sub["subscription"].clone();
    h.result(
        "world/set",
        json!({"world": "village", "path": "player.gold", "value": 45}),
    );
    let changed = h.wait_for("world/changed", Duration::from_secs(5), |m| {
        m["method"] == "world/changed"
    });
    assert_eq!(changed["params"]["subscription"], subscription);
    assert_eq!(changed["params"]["path"], "player.gold");
    assert_eq!(changed["params"]["oldValue"], 60);
    assert_eq!(changed["params"]["newValue"], 45);
    let got = h.result("world/get", json!({"world": "village", "path": "player.gold"}));
    assert_eq!(got["value"], 45);
    assert_eq!(got["exists"], true);
    let absent = h.result("world/get", json!({"world": "village", "path": "player.shield"}));
    assert_eq!(absent["exists"], false);
    h.result(
        "world/merge",
        json!({"world": "village", "patch": {"quest": {"stage": 2}}}),
    );
    let snapshot = h.result("world/snapshot", json!({"world": "village"}));
    assert_eq!(
        snapshot["state"],
        json!({"player": {"gold": 45}, "quest": {"stage": 2}})
    );
    let missing = h.call("world/get", json!({"world": "nowhere"}));
    assert_eq!(error_name(&missing), Some("world_not_found"));
    assert_eq!(
        h.result("world/unsubscribe", json!({"subscription": subscription}))["unsubscribed"],
        true
    );
}

/// A protocol scenario.
pub type Scenario = fn(&mut Harness);

/// Every scenario, in order.
pub fn run_all(backend: impl Fn() -> Box<dyn AiBackend>) {
    let scenarios: [(&str, Scenario); 8] = [
        ("core", scenario_core),
        ("session tool round trip", scenario_session_tool_round_trip),
        ("tool errors and timeouts", scenario_tool_errors_and_timeouts),
        ("session errors and cancel", scenario_session_errors_and_cancel),
        ("npcs", scenario_npcs),
        ("npc cancel", scenario_npc_cancel),
        ("decisions and content", scenario_decisions_and_content),
        ("world", scenario_world),
    ];
    for (name, scenario) in scenarios {
        eprintln!("scenario: {name}");
        let mut harness = Harness::new(backend());
        scenario(&mut harness);
    }
}

// ------------------------------------------------------------------------------------------
// Engine-level driving

/// Steps `app` (one tick per iteration, a millisecond apart) until `task` resolves.
pub fn run_until<T>(app: &mut App, task: &mut Task<AiResult<T>>) -> AiResult<T> {
    for _ in 0..20_000 {
        app.step_ticks(1);
        if let Some(result) = task.poll() {
            return result;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the task did not finish");
}

/// Steps `app` until `done` holds.
pub fn tick_until(app: &mut App, mut done: impl FnMut(&mut App) -> bool) {
    for _ in 0..20_000 {
        app.step_ticks(1);
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the condition never held");
}
