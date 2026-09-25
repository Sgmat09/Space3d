//! One model turn as a resumable state machine: model steps, tool rounds (client `tool/call`s
//! and built-in local tools), streaming and the goal-specific answer (session text, NPC reply,
//! bark, decision, content).

use super::engine::{Engine, SharedModel};
use super::params::ToolSpec;
use super::{ModelRequest, RequestKind};
use crate::error::{AiError, AiErrorCode, AiResult};
use crate::rpc;
use crate::script::{ScriptStep, ScriptedToolCall, StepAction};
use crate::types::{Emotion, ReplyFormat, ToolChoice, ToolOutput};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::time::Duration;
use web_time::Instant;

/// Upper bound on model steps per turn (a runaway script fails instead of looping).
const MAX_STEPS: usize = 32;

/// Built-in tools executed inside the bridge (NPC world and memory tools).
#[derive(Clone, Debug, Default)]
pub(super) struct LocalTools {
    /// `(world, readable paths, writable paths)`.
    pub world: Option<(String, Vec<String>, Vec<String>)>,
    pub remember_fact: bool,
    pub change_relationship: bool,
    pub max_facts: usize,
    pub max_relationship_change: i64,
}

pub(super) const READ_WORLD: &str = "read_world_state";
pub(super) const UPDATE_WORLD: &str = "update_world_state";
pub(super) const REMEMBER_FACT: &str = "remember_fact";
pub(super) const CHANGE_RELATIONSHIP: &str = "change_relationship";

impl LocalTools {
    pub(super) fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if let Some((_, readable, writable)) = &self.world {
            if !readable.is_empty() {
                names.push(READ_WORLD);
            }
            if !writable.is_empty() {
                names.push(UPDATE_WORLD);
            }
        }
        if self.remember_fact {
            names.push(REMEMBER_FACT);
        }
        if self.change_relationship {
            names.push(CHANGE_RELATIONSHIP);
        }
        names
    }
}

/// What the turn produces.
pub(super) enum Goal {
    Session { schema: Option<Value> },
    Npc { format: ReplyFormat },
    Bark,
    Decision { ids: Vec<String>, fallback: Option<String> },
    Content,
}

/// A client tool call waiting for the engine.
pub(super) struct Outstanding {
    pub rpc_id: String,
    pub call: Value,
    pub name: String,
    pub started: Instant,
    pub deadline: Option<Instant>,
    pub timeout: Option<Duration>,
    pub done: bool,
}

pub(super) enum Phase {
    Start,
    Waiting {
        until: Instant,
        step: ScriptStep,
    },
    Play(ScriptStep),
    Tools(Vec<Outstanding>),
    Streaming {
        pieces: VecDeque<Value>,
        next_at: Instant,
        result: Value,
    },
    Finished,
}

pub(super) struct Turn {
    pub job: u64,
    pub kind: RequestKind,
    pub goal: Goal,
    /// Session or NPC id.
    pub owner: Option<String>,
    /// Params context for events and `tool/call`s (`{"session": id}`, `{"npc": id}`, `{"index": n}`).
    pub context: Map<String, Value>,
    /// The request id (`null` for notifications).
    pub request_id: Value,
    /// `session/event` / `npc/event` when streaming.
    pub event_method: Option<&'static str>,
    pub model: SharedModel,
    pub prompt: String,
    pub tools: Vec<ToolSpec>,
    pub local: LocalTools,
    pub choice: ToolChoice,
    pub max_rounds: u32,
    pub max_calls: u32,
    pub tool_timeout: Option<Duration>,
    pub phase: Phase,
    pub records: Vec<Value>,
    pub outputs: Vec<String>,
    pub steps: Vec<Value>,
    pub rounds: u32,
    pub calls: u32,
    pub output_chars: usize,
    /// NPC automatic format: the retry after a blocked structured reply.
    pub text_mode: bool,
    /// The answer was a fallback line / option.
    pub fallback: bool,
}

pub(super) enum Progress {
    Idle,
    Advanced,
    Done(AiResult<Value>),
}

enum Answer {
    Text(String, u32),
    Json(Value),
}

impl Answer {
    /// The answer as a JSON object: a JSON step, or text that parses as one.
    fn as_object(&self) -> Option<Value> {
        match self {
            Self::Json(value) => Some(value.clone()).filter(Value::is_object),
            Self::Text(text, _) => serde_json::from_str::<Value>(text.trim()).ok().filter(Value::is_object),
        }
    }
}

enum AnswerKind {
    Session { structured: bool },
    Npc { format: ReplyFormat },
    Bark,
    Decision { ids: Vec<String> },
    Content,
}

fn not_json() -> AiError {
    AiError::new(
        AiErrorCode::GenerationFailed,
        "The scripted answer is not a JSON object, but this request needs a structured answer.",
    )
}

impl Turn {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        job: u64,
        kind: RequestKind,
        goal: Goal,
        owner: Option<String>,
        context: Map<String, Value>,
        request_id: Value,
        event_method: Option<&'static str>,
        model: SharedModel,
        prompt: String,
    ) -> Self {
        Self {
            job,
            kind,
            goal,
            owner,
            context,
            request_id,
            event_method,
            model,
            prompt,
            tools: Vec::new(),
            local: LocalTools::default(),
            choice: ToolChoice::Auto,
            max_rounds: 4,
            max_calls: 12,
            tool_timeout: None,
            phase: Phase::Start,
            records: Vec::new(),
            outputs: Vec::new(),
            steps: Vec::new(),
            rounds: 0,
            calls: 0,
            output_chars: 0,
            text_mode: false,
            fallback: false,
        }
    }

    fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| t.name.clone())
            .chain(self.local.names().into_iter().map(str::to_owned))
            .collect()
    }

    fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name) || self.local.names().contains(&name)
    }

    fn mode(&self) -> &'static str {
        let no_tools = self.tools.is_empty() && self.local.names().is_empty();
        if no_tools || self.choice == ToolChoice::None || self.rounds >= self.max_rounds {
            "disallowed"
        } else if self.rounds == 0 && matches!(self.choice, ToolChoice::Required | ToolChoice::Tool(_)) {
            "required"
        } else {
            "allowed"
        }
    }

    fn wants_json(&self) -> bool {
        match &self.goal {
            Goal::Session { schema } => schema.is_some(),
            Goal::Npc { format } => *format != ReplyFormat::Text && !self.text_mode,
            Goal::Bark => false,
            Goal::Decision { .. } | Goal::Content => true,
        }
    }

    fn usage(&self) -> Value {
        let context: usize = self.prompt.len() + self.outputs.iter().map(String::len).sum::<usize>();
        let input = ((context / 4 + 24) * self.steps.len().max(1)) as u64;
        let output = (self.output_chars / 4).max(1) as u64;
        json!({"inputTokens": input, "cachedInputTokens": 0, "outputTokens": output, "totalTokens": input + output})
    }

    /// The pending client calls (for cancellation).
    pub(super) fn outstanding(&self) -> impl Iterator<Item = &Outstanding> {
        match &self.phase {
            Phase::Tools(outstanding) => outstanding.iter().filter(|o| !o.done).collect::<Vec<_>>().into_iter(),
            _ => Vec::new().into_iter(),
        }
    }
}

/// Splits `text` into about `chunks` pieces (character-based, like the Swift scripted model).
pub(super) fn split(text: &str, chunks: u32) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let chunks = chunks.max(1) as usize;
    if chunks <= 1 || chars.len() <= chunks {
        return vec![text.to_owned()];
    }
    let size = chars.len().div_ceil(chunks);
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

/// `[emotion] line` → (`Some(emotion)`, `line`).
pub(super) fn split_tag(text: &str) -> (Option<String>, String) {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(close) = rest.find(']')
    {
        return (
            Some(rest[..close].to_owned()),
            rest[close + 1..].trim_start().to_owned(),
        );
    }
    (None, trimmed.to_owned())
}

/// A spoken line without a leading speaker label or wrapping quotes.
pub(super) fn spoken_line(text: &str, speaker: &str) -> String {
    let mut line = text.trim();
    if !speaker.is_empty()
        && let Some(rest) = line.strip_prefix(speaker)
        && let Some(rest) = rest.strip_prefix(':')
    {
        line = rest.trim_start();
    }
    for (open, close) in [('"', '"'), ('“', '”'), ('\'', '\'')] {
        if line.len() >= 2 && line.starts_with(open) && line.ends_with(close) {
            let inner = &line[open.len_utf8()..line.len() - close.len_utf8()];
            if !inner.contains(close) {
                line = inner.trim();
            }
        }
    }
    line.to_owned()
}

fn seconds_text(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{}s", secs as u64)
    } else {
        let text = format!("{secs:.3}");
        format!("{}s", text.trim_end_matches('0').trim_end_matches('.'))
    }
}

fn is_readable(path: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|a| a.is_empty() || path == a || path.starts_with(&format!("{a}.")))
}

impl Engine {
    /// Advances a turn as far as it can go right now.
    pub(super) fn step_turn(&mut self, turn: &mut Turn, now: Instant) -> Progress {
        let mut advanced = false;
        loop {
            match std::mem::replace(&mut turn.phase, Phase::Finished) {
                Phase::Start => {
                    advanced = true;
                    if let Some(done) = self.begin_step(turn, now) {
                        return Progress::Done(done);
                    }
                }
                Phase::Waiting { until, step } => {
                    if now < until {
                        turn.phase = Phase::Waiting { until, step };
                        return if advanced { Progress::Advanced } else { Progress::Idle };
                    }
                    turn.phase = Phase::Play(step);
                    advanced = true;
                }
                Phase::Play(step) => {
                    advanced = true;
                    if let Some(done) = self.play(turn, step, now) {
                        return Progress::Done(done);
                    }
                }
                Phase::Tools(mut outstanding) => {
                    advanced |= self.expire_tools(turn, &mut outstanding, now);
                    if outstanding.iter().any(|o| !o.done) {
                        turn.phase = Phase::Tools(outstanding);
                        return if advanced { Progress::Advanced } else { Progress::Idle };
                    }
                    turn.rounds += 1;
                    turn.phase = Phase::Start;
                    advanced = true;
                }
                Phase::Streaming {
                    mut pieces,
                    mut next_at,
                    result,
                } => {
                    while now >= next_at {
                        let Some(event) = pieces.pop_front() else { break };
                        self.emit_event(turn, event);
                        advanced = true;
                        next_at += self.config.pacing;
                    }
                    if pieces.is_empty() {
                        return Progress::Done(Ok(result));
                    }
                    turn.phase = Phase::Streaming {
                        pieces,
                        next_at,
                        result,
                    };
                    return if advanced { Progress::Advanced } else { Progress::Idle };
                }
                Phase::Finished => {
                    return Progress::Done(Err(AiError::new(
                        AiErrorCode::InternalError,
                        "The turn already finished.",
                    )));
                }
            }
        }
    }

    fn begin_step(&mut self, turn: &mut Turn, now: Instant) -> Option<AiResult<Value>> {
        if turn.steps.is_empty()
            && let ToolChoice::Tool(name) = &turn.choice
            && !turn.has_tool(name)
        {
            let error = match turn.kind {
                RequestKind::NpcTalk => AiError::new(
                    AiErrorCode::InvalidRequest,
                    format!("'toolChoice' names '{name}', which is not one of the NPC's tools."),
                ),
                _ => AiError::invalid_params(format!("'toolChoice' names '{name}', which is not one of the tools.")),
            };
            return Some(Err(error));
        }
        if turn.steps.len() >= MAX_STEPS {
            return Some(Err(AiError::new(
                AiErrorCode::GenerationFailed,
                format!("The script ran more than {MAX_STEPS} model steps in one turn."),
            )));
        }
        let mode = turn.mode();
        let enabled = turn.tool_names();
        let step = json!({"index": turn.steps.len(), "completedToolRounds": turn.rounds, "toolCallingMode": mode,
            "enabledTools": enabled, "trimmedEntries": 0});
        turn.steps.push(step.clone());
        if turn.kind == RequestKind::Session {
            self.emit_event(turn, json!({"type": "modelStep", "step": step}));
        }
        let request = ModelRequest {
            kind: turn.kind,
            owner: turn.owner.clone(),
            prompt: turn.prompt.clone(),
            step_index: (turn.steps.len() - 1) as u32,
            tool_outputs: turn.outputs.clone(),
            enabled_tools: if mode == "disallowed" { Vec::new() } else { enabled },
            tool_calling_mode: mode,
            wants_json: turn.wants_json(),
            options: match &turn.goal {
                Goal::Decision { ids, .. } => ids.clone(),
                _ => Vec::new(),
            },
        };
        let next = turn.model.lock().next(&request);
        match next {
            Err(error) => self.fail_turn(turn, error, now),
            Ok(step) if step.delay_ms > 0 => {
                turn.phase = Phase::Waiting {
                    until: now + Duration::from_millis(step.delay_ms),
                    step: ScriptStep { delay_ms: 0, ..step },
                };
                None
            }
            Ok(step) => {
                turn.phase = Phase::Play(step);
                None
            }
        }
    }

    fn play(&mut self, turn: &mut Turn, step: ScriptStep, now: Instant) -> Option<AiResult<Value>> {
        match step.action {
            StepAction::ToolCalls(calls) => {
                let outstanding = self.start_tools(turn, calls, now);
                turn.phase = Phase::Tools(outstanding);
                None
            }
            StepAction::Text { text, chunks } => self.answer(turn, Answer::Text(text, chunks), now),
            StepAction::Template(template) => {
                let text = template
                    .replace("{prompt}", &turn.prompt)
                    .replace("{toolOutputs}", &turn.outputs.join("\n"))
                    .replace("{toolOutput}", turn.outputs.last().map_or("", String::as_str));
                self.answer(turn, Answer::Text(text, 3), now)
            }
            StepAction::Json(value) => self.answer(turn, Answer::Json(value), now),
            StepAction::Error { code, message } => {
                let message = message.unwrap_or_else(|| format!("Scripted {code}."));
                self.fail_turn(turn, AiError::new(code, message), now)
            }
        }
    }

    // --------------------------------------------------------------------------------------
    // Tools

    fn start_tools(&mut self, turn: &mut Turn, calls: Vec<ScriptedToolCall>, now: Instant) -> Vec<Outstanding> {
        let allowed = turn.mode() != "disallowed";
        let mut outstanding = Vec::new();
        for call in calls {
            let call_json = json!({"id": self.call_id(), "name": call.name, "arguments": call.arguments});
            if turn.calls >= turn.max_calls {
                let output = ToolOutput::error(format!(
                    "The tool call limit ({}) for this turn was reached.",
                    turn.max_calls
                ));
                self.record(turn, call_json, output, now);
                continue;
            }
            if !allowed || !turn.has_tool(&call.name) {
                let output = ToolOutput::error(format!("Tool '{}' is not available on this step.", call.name));
                self.record(turn, call_json, output, now);
                continue;
            }
            turn.calls += 1;
            if turn.local.names().contains(&call.name.as_str()) {
                self.emit_event(
                    turn,
                    json!({"type": "toolCallStarted", "call": call_json, "execution": "local"}),
                );
                let output = self.run_local_tool(turn, &call.name, &call.arguments);
                self.record(turn, call_json, output, now);
                continue;
            }
            let spec_timeout = turn.tools.iter().find(|t| t.name == call.name).and_then(|t| t.timeout);
            self.emit_event(
                turn,
                json!({"type": "toolCallStarted", "call": call_json, "execution": "client"}),
            );
            self.next_tool += 1;
            let rpc_id = format!("t-{}", self.next_tool);
            let mut params = turn.context.clone();
            params.insert("requestId".into(), turn.request_id.clone());
            params.insert("call".into(), call_json.clone());
            self.emit(rpc::request(&json!(rpc_id), "tool/call", &Value::Object(params)));
            self.tool_calls.insert(rpc_id.clone(), turn.job);
            let timeout = spec_timeout.unwrap_or(turn.tool_timeout);
            outstanding.push(Outstanding {
                rpc_id,
                call: call_json,
                name: call.name,
                started: now,
                deadline: timeout.map(|t| now + t),
                timeout,
                done: false,
            });
        }
        outstanding
    }

    /// Records a finished tool call and streams `toolCallCompleted`.
    fn record(&mut self, turn: &mut Turn, call: Value, output: ToolOutput, started: Instant) {
        let seconds = (Instant::now().saturating_duration_since(started).as_secs_f64() * 1000.0).round() / 1000.0;
        let record = json!({"call": call, "output": output.wire_output(), "isError": output.is_error(),
            "durationSeconds": seconds});
        turn.outputs.push(output.model_text());
        turn.records.push(record.clone());
        self.emit_event(turn, json!({"type": "toolCallCompleted", "record": record}));
    }

    /// Applies the engine's reply to a `tool/call`.
    pub(super) fn tool_reply(&mut self, turn: &mut Turn, rpc_id: &str, output: ToolOutput) {
        let mut phase = std::mem::replace(&mut turn.phase, Phase::Finished);
        if let Phase::Tools(outstanding) = &mut phase
            && let Some(call) = outstanding.iter_mut().find(|o| o.rpc_id == rpc_id && !o.done)
        {
            call.done = true;
            let (call_json, started) = (call.call.clone(), call.started);
            self.record(turn, call_json, output, started);
        }
        turn.phase = phase;
    }

    /// Times out overdue calls: error output for the model, `tool/cancel` for the engine.
    fn expire_tools(&mut self, turn: &mut Turn, outstanding: &mut [Outstanding], now: Instant) -> bool {
        let mut changed = false;
        for call in outstanding.iter_mut().filter(|o| !o.done) {
            if call.deadline.is_some_and(|deadline| now >= deadline) {
                call.done = true;
                changed = true;
                self.tool_calls.remove(&call.rpc_id);
                let message = format!(
                    "Tool '{}' timed out after {}.",
                    call.name,
                    seconds_text(call.timeout.unwrap_or_default())
                );
                let output = ToolOutput::error(message);
                let reason = output.model_text();
                let (call_json, started) = (call.call.clone(), call.started);
                self.record(turn, call_json, output, started);
                self.cancel_tool_call(turn, &call.rpc_id, &call.call, &reason);
            }
        }
        changed
    }

    /// Sends `tool/cancel` for a call the bridge no longer needs.
    pub(super) fn cancel_tool_call(&mut self, turn: &Turn, rpc_id: &str, call: &Value, reason: &str) {
        self.tool_calls.remove(rpc_id);
        let mut params = turn.context.clone();
        params.insert("requestId".into(), turn.request_id.clone());
        params.insert("id".into(), json!(rpc_id));
        params.insert("callId".into(), call.get("id").cloned().unwrap_or(Value::Null));
        params.insert("reason".into(), json!(reason));
        self.emit(rpc::notification("tool/cancel", &Value::Object(params)));
    }

    fn run_local_tool(&mut self, turn: &Turn, name: &str, arguments: &Value) -> ToolOutput {
        let npc = turn.owner.clone().unwrap_or_default();
        let text = |key: &str| arguments.get(key).and_then(Value::as_str).map(str::to_owned);
        match name {
            READ_WORLD | UPDATE_WORLD => {
                let Some((world, readable, writable)) = turn.local.world.clone() else {
                    return ToolOutput::error("This NPC has no world.");
                };
                let path = text("path").unwrap_or_default();
                if name == READ_WORLD {
                    if !is_readable(&path, &readable) {
                        return ToolOutput::error(format!(
                            "'{path}' is not readable. Readable paths: {}.",
                            readable.join(", ")
                        ));
                    }
                    return match self.worlds.get(&world, &path) {
                        Ok(Some(value)) => ToolOutput::Json(json!({"path": path, "value": value})),
                        Ok(None) => ToolOutput::error(format!("Nothing is stored at '{path}'.")),
                        Err(error) => ToolOutput::error(error.message),
                    };
                }
                if path.is_empty() || !is_readable(&path, &writable) {
                    return ToolOutput::error(format!(
                        "'{path}' is not writable. Writable paths: {}.",
                        writable.join(", ")
                    ));
                }
                let mut value = arguments.get("value").cloned().unwrap_or(Value::Null);
                // Keep the stored type: a number stays a number.
                if let (Ok(Some(old)), Value::String(s)) = (self.worlds.get(&world, &path), &value)
                    && old.is_number()
                    && let Ok(number) = s.trim().parse::<f64>()
                {
                    value = serde_json::Number::from_f64(number).map_or(value.clone(), Value::Number);
                    if number.fract() == 0.0 && number.abs() < 9.0e15 {
                        value = json!(number as i64);
                    }
                }
                match self.worlds.set(&world, &path, value.clone()) {
                    Ok(changes) => {
                        self.notify_world(&world, &changes);
                        ToolOutput::Json(json!({"path": path, "value": value}))
                    }
                    Err(error) => ToolOutput::error(error.message),
                }
            }
            REMEMBER_FACT => {
                let Some(fact) = text("fact").filter(|f| !f.trim().is_empty()) else {
                    return ToolOutput::error("Argument 'fact' of tool 'remember_fact' must be a string.");
                };
                let max = turn.local.max_facts.max(1);
                let Some(state) = self.npcs.get_mut(&npc) else {
                    return ToolOutput::error("The NPC is gone.");
                };
                state.facts.push(fact.trim().to_owned());
                if state.facts.len() > max {
                    let excess = state.facts.len() - max;
                    state.facts.drain(..excess);
                }
                ToolOutput::text("Remembered.")
            }
            CHANGE_RELATIONSHIP => {
                let delta = arguments
                    .get("delta")
                    .and_then(Value::as_f64)
                    .map_or(0, |d| d.round() as i64);
                let limit = turn.local.max_relationship_change;
                let Some(state) = self.npcs.get_mut(&npc) else {
                    return ToolOutput::error("The NPC is gone.");
                };
                let delta = delta.clamp(-limit, limit);
                state.relationship = (i64::from(state.relationship) + delta).clamp(-100, 100) as i32;
                ToolOutput::Json(json!({"relationship": state.relationship}))
            }
            _ => ToolOutput::error(format!("Unknown local tool '{name}'.")),
        }
    }

    // --------------------------------------------------------------------------------------
    // Answers

    fn deliver(&mut self, turn: &mut Turn, events: Vec<Value>, result: Value, now: Instant) -> Option<AiResult<Value>> {
        if turn.event_method.is_none() || events.is_empty() {
            return Some(Ok(result));
        }
        if self.config.pacing.is_zero() {
            for event in events {
                self.emit_event(turn, event);
            }
            return Some(Ok(result));
        }
        turn.phase = Phase::Streaming {
            pieces: events.into(),
            next_at: now,
            result,
        };
        None
    }

    fn answer(&mut self, turn: &mut Turn, answer: Answer, now: Instant) -> Option<AiResult<Value>> {
        let kind = match &turn.goal {
            Goal::Session { schema } => AnswerKind::Session {
                structured: schema.is_some(),
            },
            Goal::Npc { format } => AnswerKind::Npc { format: *format },
            Goal::Bark => AnswerKind::Bark,
            Goal::Decision { ids, .. } => AnswerKind::Decision { ids: ids.clone() },
            Goal::Content => AnswerKind::Content,
        };
        match kind {
            AnswerKind::Session { structured } => self.answer_session(turn, answer, structured, now),
            AnswerKind::Npc { format } => self.answer_npc(turn, answer, format, now),
            AnswerKind::Bark => {
                let owner = turn.owner.clone().unwrap_or_default();
                let name = self.npcs.get(&owner).map(|n| n.name.clone()).unwrap_or_default();
                let text = match &answer {
                    Answer::Text(text, _) => split_tag(text).1,
                    Answer::Json(value) => value.get("line").and_then(Value::as_str).map_or_else(
                        || value.as_str().map_or_else(|| value.to_string(), str::to_owned),
                        str::to_owned,
                    ),
                };
                Some(Ok(json!({"npc": owner, "line": spoken_line(&text, &name)})))
            }
            AnswerKind::Decision { ids } => {
                let Some(value) = answer.as_object() else {
                    return self.fail_turn(turn, not_json(), now);
                };
                let raw = value
                    .get("choice")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let Some(id) = ids
                    .iter()
                    .find(|id| **id == raw)
                    .or_else(|| ids.iter().find(|id| id.eq_ignore_ascii_case(&raw)))
                    .cloned()
                else {
                    let message = format!("The model chose '{raw}', which is not one of: {}.", ids.join(", "));
                    return self.fail_turn(turn, AiError::new(AiErrorCode::GenerationFailed, message), now);
                };
                let confidence = value
                    .get("confidence")
                    .and_then(Value::as_f64)
                    .map_or(50, |c| c.round().clamp(0.0, 100.0) as i64);
                let reasoning = value
                    .get("reasoning")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                turn.output_chars += value.to_string().len();
                Some(Ok(
                    json!({"optionID": id, "reasoning": reasoning, "confidence": confidence,
                    "toolCalls": turn.records, "usage": turn.usage(), "isFallback": false}),
                ))
            }
            AnswerKind::Content => match answer.as_object() {
                Some(content) => {
                    turn.output_chars += content.to_string().len();
                    Some(Ok(json!({"content": content})))
                }
                None => self.fail_turn(turn, not_json(), now),
            },
        }
    }

    fn answer_session(
        &mut self,
        turn: &mut Turn,
        answer: Answer,
        structured: bool,
        now: Instant,
    ) -> Option<AiResult<Value>> {
        let owner = turn.owner.clone().unwrap_or_default();
        let (text, structured, chunks) = match (structured, &answer) {
            (true, _) => match answer.as_object() {
                Some(value) => (value.to_string(), Some(value), 1),
                None => return self.fail_turn(turn, not_json(), now),
            },
            (false, Answer::Text(text, chunks)) => (text.clone(), None, *chunks),
            (false, Answer::Json(value)) => (value.as_str().map_or_else(|| value.to_string(), str::to_owned), None, 1),
        };
        turn.output_chars += text.len();
        let mut events = Vec::new();
        if let Some(value) = &structured {
            events.push(json!({"type": "partial", "value": value}));
        } else {
            let mut shown = String::new();
            for piece in split(&text, chunks) {
                shown.push_str(&piece);
                events.push(json!({"type": "text", "delta": piece, "text": shown, "isReset": false}));
            }
        }
        let mut result = json!({"session": owner, "text": text, "toolCalls": turn.records,
            "usage": turn.usage(), "steps": turn.steps});
        if let Some(value) = structured {
            result["structured"] = value;
        }
        self.deliver(turn, events, result, now)
    }

    fn answer_npc(
        &mut self,
        turn: &mut Turn,
        answer: Answer,
        format: ReplyFormat,
        now: Instant,
    ) -> Option<AiResult<Value>> {
        let owner = turn.owner.clone().unwrap_or_default();
        let Some(npc) = self.npcs.get(&owner) else {
            return Some(Err(AiError::cancelled("The NPC was deleted.")));
        };
        let (name, default_emotion, allowed, option_count, can_end, relationship) = (
            npc.name.clone(),
            npc.default_emotion,
            npc.opts.emotions.clone(),
            npc.opts.player_option_count,
            npc.opts.can_end_conversation,
            npc.relationship,
        );
        let plain = format == ReplyFormat::Text || turn.text_mode;
        // Lenient: a JSON reply works in every format, a `[emotion] line` text reply too.
        let structured = answer.as_object().filter(|v| !plain || v.get("line").is_some());
        let (tag, line, options, ends, chunks) = match (&structured, &answer) {
            (Some(value), _) => (
                value.get("emotion").and_then(Value::as_str).map(str::to_owned),
                value.get("line").and_then(Value::as_str).unwrap_or_default().to_owned(),
                value
                    .get("player_options")
                    .or_else(|| value.get("playerOptions"))
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default(),
                value
                    .get("ends_conversation")
                    .or_else(|| value.get("endsConversation"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                3,
            ),
            (None, Answer::Text(text, chunks)) => {
                let (tag, rest) = split_tag(text);
                (tag, rest, Vec::new(), false, *chunks)
            }
            (None, Answer::Json(value)) => (None, value.to_string(), Vec::new(), false, 1),
        };
        let line = spoken_line(&line, &name);
        if line.is_empty() {
            let error = AiError::new(AiErrorCode::GenerationFailed, "The scripted reply has no line.");
            return self.fail_turn(turn, error, now);
        }
        let emotion = tag
            .as_deref()
            .and_then(Emotion::matching)
            .filter(|e| allowed.as_ref().is_none_or(|allowed| allowed.contains(e)))
            .unwrap_or(default_emotion);
        let options: Vec<String> = if plain {
            Vec::new()
        } else {
            options
                .iter()
                .map(|o| o.trim().trim_start_matches(['-', '*', '•']).trim().to_owned())
                .filter(|o| !o.is_empty())
                .take(option_count)
                .collect()
        };
        let ends = !plain && can_end && ends;
        turn.output_chars += line.len();
        let mut events = vec![json!({"type": "emotion", "emotion": emotion.as_str()})];
        events.extend(
            split(&line, chunks)
                .into_iter()
                .map(|piece| json!({"type": "lineDelta", "delta": piece})),
        );
        let result = json!({"npc": owner, "line": line, "emotion": emotion.as_str(), "playerOptions": options,
            "endsConversation": ends, "toolCalls": turn.records, "relationship": relationship,
            "isFallback": false, "usage": turn.usage()});
        self.deliver(turn, events, result, now)
    }

    /// A failed step: NPC retries and fallbacks, decision fallbacks, or the error.
    fn fail_turn(&mut self, turn: &mut Turn, error: AiError, now: Instant) -> Option<AiResult<Value>> {
        let npc_format = match &turn.goal {
            Goal::Npc { format } => Some(*format),
            _ => None,
        };
        let decision_fallback = match &turn.goal {
            Goal::Decision { fallback, .. } => fallback.clone(),
            _ => None,
        };
        if !error.is_blocked() {
            return Some(Err(error));
        }
        if let Some(format) = npc_format {
            if format == ReplyFormat::Automatic && !turn.text_mode {
                // Retried once as plain text with tools off; gathered tool results stay.
                turn.text_mode = true;
                turn.tools.clear();
                turn.local = LocalTools::default();
                turn.phase = Phase::Start;
                return None;
            }
            let owner = turn.owner.clone().unwrap_or_default();
            let Some(npc) = self.npcs.get_mut(&owner) else {
                return Some(Err(error));
            };
            if !npc.opts.fallback_on_guardrail {
                return Some(Err(error));
            }
            let lines = &npc.opts.fallback_lines;
            let line = lines[npc.fallback_index % lines.len()].clone();
            npc.fallback_index += 1;
            let (emotion, relationship) = (npc.default_emotion, npc.relationship);
            turn.fallback = true;
            let events = vec![
                json!({"type": "emotion", "emotion": emotion.as_str()}),
                json!({"type": "lineReset", "line": line}),
            ];
            let result = json!({"npc": owner, "line": line, "emotion": emotion.as_str(), "playerOptions": [],
                "endsConversation": false, "toolCalls": turn.records, "relationship": relationship,
                "isFallback": true, "usage": turn.usage()});
            return self.deliver(turn, events, result, now);
        }
        if let Some(fallback) = decision_fallback {
            turn.fallback = true;
            let zero = json!({"inputTokens": 0, "cachedInputTokens": 0, "outputTokens": 0, "totalTokens": 0});
            return Some(Ok(
                json!({"optionID": fallback, "reasoning": "", "confidence": 0, "toolCalls": [],
                "usage": zero, "isFallback": true}),
            ));
        }
        Some(Err(error))
    }

    /// Streams an event if the turn streams (`session/event` / `npc/event`).
    fn emit_event(&mut self, turn: &Turn, event: Value) {
        let Some(method) = turn.event_method else { return };
        let mut params = turn.context.clone();
        params.insert("requestId".into(), turn.request_id.clone());
        params.insert("event".into(), event);
        self.emit(rpc::notification(method, &Value::Object(params)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers() {
        assert_eq!(split("Hello there", 3), ["Hell", "o th", "ere"]);
        assert_eq!(split("Hi", 3), ["Hi"]);
        assert_eq!(split("héllo wörld", 2), ["héllo ", "wörld"]);
        assert_eq!(
            split_tag("[worried] Let's not."),
            (Some("worried".into()), "Let's not.".into())
        );
        assert_eq!(split_tag("No tag."), (None, "No tag.".into()));
        assert_eq!(spoken_line("Gorm: \"Aye, lad.\"", "Gorm"), "Aye, lad.");
        assert_eq!(spoken_line("\"One\" and \"two\"", "Gorm"), "\"One\" and \"two\"");
        assert_eq!(seconds_text(Duration::from_secs(120)), "120s");
        assert_eq!(seconds_text(Duration::from_millis(50)), "0.05s");
        assert!(is_readable("player.gold", &["player".into()]));
        assert!(!is_readable("players", &["player".into()]));
        assert!(is_readable("x", &[String::new()]));
    }
}
