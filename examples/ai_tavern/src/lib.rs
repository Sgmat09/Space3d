//! Mira the innkeeper: an on-device-AI NPC whose tools read and write ECS state.
//!
//! The menu lives in the ECS (`MenuItem` + `Stock` entities) and the player's gold in the
//! `Purse` resource. Mira's two tools run as Rust closures on the engine thread:
//! `check_menu` reads the entities, `take_order` decrements stock, takes the gold and spawns a
//! `Served` dish on the counter. Her lines stream in (typewriter), and her color follows her
//! emotion.
//!
//! ```text
//! cargo run -p ai_tavern                          # scripted Mira (Linux, Windows, the web, CI)
//! OAM_APPLE_LIB_DIR=… cargo run -p ai_tavern --features apple   # Apple's on-device model (macOS 27)
//! SPACE3D_HEADLESS=1 SPACE3D_EXIT_AFTER_FRAMES=900 cargo run -p ai_tavern -- --demo   # smoke test
//! ```
//!
//! The backend is `BackendChoice::auto_or(scripted)`: Apple's Foundation Models when linked,
//! Gemini Nano on Android (in the Gradle host app, see docs/ai-android.md), otherwise a
//! keyword-driven scripted Mira that goes through exactly the same tool calls.
//! Controls: type and press Enter, or click a suggested reply; `Esc` quits.
//!
//! The game is a library (`build`), so the desktop/web binary (`src/main.rs`) and the Android
//! host's native library (`android/native`, see docs/ai-android.md) share it.

use serde_json::{Value, json};
use space3d::ai::{Ai, AiEvent, AiTool, BackendChoice, ModelRequest, NpcOptions, RequestKind, ScriptedBackend};
use space3d::prelude::*;
use space3d::ui::{UiAppExt, egui};
use std::collections::VecDeque;
use std::time::Duration;

/// The NPC id.
const MIRA: &str = "mira";
/// The player's starting gold.
const START_GOLD: u32 = 12;
/// What the player says in `--demo` mode (and in headless smoke runs).
const DEMO: [&str; 5] = [
    "Hello!",
    "What's on the menu tonight?",
    "I'll take the stew.",
    "And two pies, please.",
    "Goodbye!",
];

/// A dish or drink the tavern serves.
struct MenuItem {
    name: &'static str,
    price: u32,
}

/// How many are left.
struct Stock(u32);

/// The player's gold.
struct Purse(u32);

/// Marks Mira's entity.
struct Innkeeper;

/// A dish served on the counter.
struct Served {
    item: &'static str,
}

/// The conversation as the UI shows it.
#[derive(Default)]
struct Dialogue {
    /// Mira's line as streamed so far.
    shown: String,
    emotion: Emotion,
    options: Vec<String>,
    /// `(speaker, line)` log.
    log: Vec<(String, String)>,
    /// Lines the player will say next (the demo script, or UI input).
    queue: VecDeque<String>,
    turn: Option<Task<Result<DialogueTurn, AiError>>>,
    input: String,
    ended: bool,
    /// Finished NPC turns.
    turns: usize,
}

fn menu() -> [(MenuItem, Stock); 4] {
    [
        (MenuItem { name: "stew", price: 5 }, Stock(4)),
        (MenuItem { name: "ale", price: 2 }, Stock(12)),
        (
            MenuItem {
                name: "bread",
                price: 1,
            },
            Stock(9),
        ),
        (MenuItem { name: "pie", price: 3 }, Stock(1)),
    ]
}

/// Configures the tavern: engine plugins, the AI backend (the platform's model, or scripted
/// Mira), the game logic and the visuals.
pub fn build(app: &mut App) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let demo = args.iter().any(|a| a == "--demo")
        || std::env::var_os("SPACE3D_EXIT_AFTER_FRAMES").is_some()
        || std::env::var_os("SPACE3D_EXIT_AFTER_SECS").is_some();
    app.add_plugin(
        DefaultPlugins::new()
            .title("Space3d — AI tavern")
            .without_physics()
            .without_net()
            .without_audio(),
    );
    add_tavern(app, BackendChoice::auto_or(scripted_mira()), demo);
    add_visuals(app);
}

// ------------------------------------------------------------------------------------------
// Game logic (no rendering: the tests run it headless)

fn add_tavern(app: &mut App, backend: BackendChoice, demo: bool) {
    app.insert_resource(Purse(START_GOLD));
    app.insert_resource(Dialogue {
        queue: if demo {
            DEMO.iter().map(|l| (*l).to_owned()).collect()
        } else {
            VecDeque::from(["Hello!".to_owned()])
        },
        ..Dialogue::default()
    });
    app.add_plugin(AiPlugin::new(backend).with_tool(check_menu()).with_tool(take_order()));
    app.add_system(Stage::Startup, open_tavern);
    app.add_system(Stage::Update, converse);
    let mut reader = EventReader::default();
    app.add_system(Stage::Update, move |ctx| stream_lines(ctx, &mut reader));
}

/// `check_menu`: reads the menu entities and the purse.
fn check_menu() -> AiTool {
    AiTool::new(
        "check_menu",
        "The tavern's menu: every dish with its price in gold and how many are left, and the player's gold.",
    )
    .handler(|ctx, _call| {
        let mut items: Vec<Value> = ctx
            .world
            .query_mut::<(&MenuItem, &Stock)>()
            .into_iter()
            .map(|(item, stock)| json!({"item": item.name, "price_gold": item.price, "left": stock.0}))
            .collect();
        items.sort_by_key(|i| i["item"].as_str().map(str::to_owned));
        json!({"items": items, "player_gold": ctx.resources.expect::<Purse>().0})
    })
}

/// `take_order`: checks stock and gold, then writes the ECS: stock down, gold down, dishes
/// served on the counter.
fn take_order() -> AiTool {
    AiTool::new(
        "take_order",
        "Serve the player an item from the menu and take payment. Fails if it is sold out or the player cannot pay.",
    )
    .parameters(json!({
        "type": "object",
        "properties": {
            "item": {"type": "string", "description": "The menu item, e.g. stew"},
            "quantity": {"type": "integer", "minimum": 1, "maximum": 5},
        },
        "required": ["item"],
    }))
    .handler(|ctx, call| -> Result<Value, String> {
        let wanted = call.string("item").map_err(|e| e.to_string())?.trim().to_lowercase();
        let quantity = call.i64("quantity").unwrap_or(1).clamp(1, 5) as u32;
        let gold = ctx.resources.expect::<Purse>().0;
        let mut served = None;
        for (item, stock) in ctx.world.query_mut::<(&MenuItem, &mut Stock)>() {
            if item.name != wanted.trim_end_matches('s') && item.name != wanted {
                continue;
            }
            if stock.0 < quantity {
                return Err(format!("Only {} {} left.", stock.0, item.name));
            }
            let cost = item.price * quantity;
            if cost > gold {
                return Err(format!(
                    "{quantity} {} cost {cost} gold, but the player has only {gold}.",
                    item.name
                ));
            }
            stock.0 -= quantity;
            served = Some((item.name, cost, stock.0));
        }
        let Some((item, cost, left)) = served else {
            return Err(format!("'{wanted}' is not on the menu."));
        };
        ctx.resources.expect_mut::<Purse>().0 -= cost;
        let already = ctx.world.query_mut::<&Served>().into_iter().count();
        for i in 0..quantity as usize {
            let x = -1.6 + 0.45 * (already + i) as f32;
            ctx.world.spawn((Served { item }, Transform::from_xyz(x, 1.2, 0.6)));
        }
        Ok(json!({"served": item, "quantity": quantity, "paid": cost,
                "player_gold": ctx.resources.expect::<Purse>().0, "left": left}))
    })
}

fn open_tavern(ctx: &mut SystemCtx) {
    for entry in menu() {
        ctx.world.spawn(entry);
    }
    ctx.world.spawn((Innkeeper, Transform::from_xyz(0.0, 1.0, -0.6)));
    let persona = Persona::new("Mira")
        .role("the innkeeper of the Rusty Anchor tavern")
        .personality("Warm, quick-witted and proud of her stew.")
        .speaking_style("Friendly and brief.")
        .max_sentences(2);
    let options = NpcOptions {
        extra_instructions: Some(
            "Call check_menu before you mention prices or what is left; never guess them. \
             When the player orders, call take_order and tell them what it cost."
                .into(),
        ),
        max_tool_rounds: Some(2),
        ..NpcOptions::default()
    };
    let config = NpcConfig::new(persona)
        .tool("check_menu")
        .tool("take_order")
        .options(options);
    let ai = ctx.resources.expect_mut::<Ai>();
    println!("space3d-ai backend: {}", ai.backend_kind());
    // Not awaited: the bridge handles requests in order, so talks queued after it wait for it.
    let created = ai.create_npc(MIRA, config);
    ctx.resources.expect_mut::<AsyncQueue>().track(created, |_ctx, result| {
        if let Err(error) = result {
            eprintln!("Mira could not be created: {error}");
        }
    });
}

/// Sends the player's next line when Mira is idle and collects finished turns.
fn converse(ctx: &mut SystemCtx) {
    let dialogue = ctx.resources.expect_mut::<Dialogue>();
    if let Some(task) = dialogue.turn.as_mut() {
        let Some(result) = task.poll() else { return };
        dialogue.turn = None;
        dialogue.turns += 1;
        match result {
            Ok(turn) => {
                for record in &turn.tool_calls {
                    println!("  [Mira used {} → {}]", record.call.name, record.output.model_text());
                }
                println!("Mira ({}): {}", turn.emotion, turn.line);
                dialogue.shown = turn.line.clone();
                dialogue.emotion = turn.emotion;
                dialogue.options = turn.player_options.clone();
                dialogue.ended |= turn.ends_conversation;
                dialogue.log.push(("Mira".into(), turn.line));
            }
            Err(error) => {
                println!("Mira is lost for words ({error}).");
                dialogue.shown = format!("(Mira is busy: {})", error.code);
            }
        }
        return;
    }
    let Some(line) = dialogue.queue.pop_front() else { return };
    println!("You: {line}");
    dialogue.log.push(("You".into(), line.clone()));
    dialogue.shown.clear();
    dialogue.options.clear();
    let task = ctx.resources.expect_mut::<Ai>().talk(MIRA, line);
    ctx.resources.expect_mut::<Dialogue>().turn = Some(task);
}

/// Typewriter: appends streamed deltas; the emotion arrives first.
fn stream_lines(ctx: &mut SystemCtx, reader: &mut EventReader) {
    let events: Vec<AiEvent> = reader
        .read(ctx.resources.expect::<Events<AiEvent>>())
        .cloned()
        .collect();
    let dialogue = ctx.resources.expect_mut::<Dialogue>();
    for event in events {
        match event {
            AiEvent::Npc {
                event: NpcEvent::Emotion(emotion),
                ..
            } => dialogue.emotion = emotion,
            AiEvent::Npc {
                event: NpcEvent::LineDelta(delta),
                ..
            } => dialogue.shown.push_str(&delta),
            AiEvent::Npc {
                event: NpcEvent::LineReset(line),
                ..
            } => dialogue.shown = line,
            AiEvent::Unavailable(error) => println!("No on-device model: {error}"),
            _ => {}
        }
    }
}

// ------------------------------------------------------------------------------------------
// Scripted Mira: the same tool calls, decided by keywords

/// A keyword-driven Mira for platforms without an on-device model. Her replies are built from
/// the tools' real outputs, so the ECS state is exercised exactly as with a real model.
fn scripted_mira() -> ScriptedBackend {
    ScriptedBackend::new()
        .with_pacing(Duration::from_millis(30))
        .with_system_model(|request| match request.kind {
            RequestKind::NpcTalk => mira_step(request),
            RequestKind::NpcBark => ScriptStep::text("Mind the step, the floor's just been scrubbed."),
            _ => ScriptStep::text("Hm."),
        })
}

fn mira_step(request: &ModelRequest) -> ScriptStep {
    let said = request.prompt.to_lowercase();
    let item = ["stew", "ale", "bread", "pie"].into_iter().find(|i| said.contains(i));
    let Some(output) = request.tool_outputs.last() else {
        if said.contains("bye") {
            return ScriptStep::npc_reply(
                "Safe travels, friend! Come back soon.",
                Emotion::Happy,
                Vec::<String>::new(),
                true,
            );
        }
        let ordering = ["take", "have", "want", "order", "please", "buy"]
            .iter()
            .any(|w| said.contains(w));
        if let (Some(item), true) = (item, ordering) {
            let quantity = if said.contains("two") { 2 } else { 1 };
            return ScriptStep::tool_call("take_order", json!({"item": item, "quantity": quantity}));
        }
        if ["menu", "eat", "drink", "serve", "food"]
            .iter()
            .any(|w| said.contains(w))
        {
            return ScriptStep::tool_call("check_menu", json!({}));
        }
        return ScriptStep::npc_reply(
            "Welcome to the Rusty Anchor! What can I get you?",
            Emotion::Happy,
            ["What's on the menu?", "I'll take the stew."],
            false,
        );
    };
    if let Some(error) = output.strip_prefix("Error: ") {
        return ScriptStep::npc_reply(
            format!("Ah, sorry, love. {error}"),
            Emotion::Worried,
            ["Never mind.", "Goodbye!"],
            false,
        );
    }
    let data: Value = serde_json::from_str(output).unwrap_or_default();
    if let Some(items) = data["items"].as_array() {
        let listed: Vec<String> = items
            .iter()
            .filter(|i| i["left"].as_u64().unwrap_or(0) > 0)
            .map(|i| format!("{} for {} gold", i["item"].as_str().unwrap_or("?"), i["price_gold"]))
            .collect();
        return ScriptStep::npc_reply(
            format!("Tonight we have {}.", listed.join(", ")),
            Emotion::Proud,
            ["I'll take the stew.", "Two pies, please."],
            false,
        );
    }
    if let Some(served) = data["served"].as_str() {
        let line = format!(
            "{} {served}{}, coming right up! That's {} gold; you've {} left.",
            data["quantity"],
            if data["quantity"] == 1 { "" } else { "s" },
            data["paid"],
            data["player_gold"]
        );
        return ScriptStep::npc_reply(
            line.replacen("1 ", "One ", 1),
            Emotion::Happy,
            ["Thanks!", "Goodbye!"],
            false,
        );
    }
    ScriptStep::npc_reply(
        "Hm, let me check on that.",
        Emotion::Confused,
        Vec::<String>::new(),
        false,
    )
}

// ------------------------------------------------------------------------------------------
// Visuals: a counter, Mira (colored by emotion), dishes, and the dialogue window

fn add_visuals(app: &mut App) {
    app.add_system(Stage::Startup, setup_scene);
    app.add_system(Stage::Update, show_served);
    app.add_system(Stage::Update, color_mira);
    app.add_system(Stage::Update, |ctx| {
        if ctx
            .resources
            .get::<Input>()
            .is_some_and(|i| i.just_pressed(KeyCode::Escape))
        {
            ctx.send(AppExit);
        }
    });
    app.add_ui_system(dialogue_window);
}

fn setup_scene(ctx: &mut SystemCtx) {
    let Some(assets) = ctx.resources.get_mut::<RenderAssets>() else {
        return;
    };
    let (cube, sphere, plane) = (
        assets.add_mesh(MeshData::cube(1.0)),
        assets.add_mesh(MeshData::sphere(0.5, 24, 16)),
        assets.add_mesh(MeshData::plane(12.0)),
    );
    ctx.world
        .spawn((Transform::default(), Mesh3d(plane), Material::srgb(0.32, 0.22, 0.14)));
    ctx.world.spawn((
        Transform::from_xyz(0.0, 0.5, 0.6).with_scale(Vec3::new(4.2, 1.0, 0.8)),
        Mesh3d(cube),
        Material::srgb(0.45, 0.28, 0.16).with_metallic_roughness(0.0, 0.8),
    ));
    let mira: Vec<Entity> = ctx
        .world
        .query_mut::<(Entity, &Innkeeper)>()
        .into_iter()
        .map(|(e, _)| e)
        .collect();
    for entity in mira {
        let _ = ctx
            .world
            .insert(entity, (Mesh3d(sphere), Material::srgb(0.9, 0.8, 0.5)));
    }
    ctx.resources.insert(DishMesh(cube));
    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.6), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.85, 0.65),
            intensity: 2.0,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(1.0, 0.8, 0.6),
        intensity: 0.25,
    });
    ctx.world.spawn((
        Transform::from_xyz(0.0, 2.6, 5.0).looking_at(Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.08, 0.05, 0.04, 1.0)),
    ));
}

/// The mesh used for served dishes.
struct DishMesh(MeshHandle);

/// Gives newly served dishes a mesh.
fn show_served(ctx: &mut SystemCtx) {
    let Some(mesh) = ctx.resources.get::<DishMesh>().map(|m| m.0) else {
        return;
    };
    let new: Vec<(Entity, &'static str)> = ctx
        .world
        .query_mut::<(Entity, &Served)>()
        .without::<&Mesh3d>()
        .into_iter()
        .map(|(entity, served)| (entity, served.item))
        .collect();
    for (entity, item) in new {
        let color = match item {
            "stew" => Material::srgb(0.6, 0.3, 0.1),
            "ale" => Material::srgb(0.9, 0.7, 0.2),
            "pie" => Material::srgb(0.8, 0.55, 0.3),
            _ => Material::srgb(0.85, 0.75, 0.55),
        };
        let _ = ctx.world.insert(entity, (Mesh3d(mesh), color));
        if let Ok(mut transform) = ctx.world.get::<&mut Transform>(entity) {
            transform.scale = Vec3::splat(0.3);
        }
    }
}

fn emotion_color(emotion: Emotion) -> Material {
    match emotion {
        Emotion::Happy | Emotion::Grateful | Emotion::Excited | Emotion::Amused => Material::srgb(1.0, 0.8, 0.3),
        Emotion::Proud => Material::srgb(0.95, 0.55, 0.2),
        Emotion::Worried | Emotion::Afraid | Emotion::Sad => Material::srgb(0.4, 0.5, 0.9),
        Emotion::Angry | Emotion::Annoyed | Emotion::Disgusted => Material::srgb(0.9, 0.25, 0.2),
        Emotion::Confused | Emotion::Curious | Emotion::Surprised | Emotion::Suspicious => {
            Material::srgb(0.6, 0.8, 0.5)
        }
        Emotion::Neutral => Material::srgb(0.9, 0.8, 0.5),
    }
}

fn color_mira(ctx: &mut SystemCtx) {
    let emotion = ctx.resources.expect::<Dialogue>().emotion;
    for (material, _) in ctx.world.query_mut::<(&mut Material, &Innkeeper)>() {
        *material = emotion_color(emotion);
    }
}

fn dialogue_window(ui: &egui::Context, ctx: &mut SystemCtx) {
    let gold = ctx.resources.expect::<Purse>().0;
    let dialogue = ctx.resources.expect_mut::<Dialogue>();
    let mut say = None;
    egui::Window::new("The Rusty Anchor")
        .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -12.0])
        .default_width(560.0)
        .resizable(false)
        .collapsible(false)
        .show(ui, |ui| {
            ui.label(format!("Your purse: {gold} gold"));
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for (who, line) in &dialogue.log {
                        ui.label(format!("{who}: {line}"));
                    }
                });
            ui.separator();
            let thinking = dialogue.turn.is_some() && dialogue.shown.is_empty();
            ui.strong(format!(
                "Mira ({}): {}",
                dialogue.emotion,
                if thinking { "…" } else { &dialogue.shown }
            ));
            ui.horizontal_wrapped(|ui| {
                for option in &dialogue.options {
                    if ui.button(option).clicked() {
                        say = Some(option.clone());
                    }
                }
            });
            if !dialogue.ended {
                ui.horizontal(|ui| {
                    let edit = ui.text_edit_singleline(&mut dialogue.input);
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Say").clicked() || enter) && !dialogue.input.trim().is_empty() {
                        say = Some(std::mem::take(&mut dialogue.input));
                    }
                });
            }
        });
    if let Some(line) = say
        && dialogue.turn.is_none()
    {
        dialogue.queue.push_back(line);
    }
}

// ------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The demo conversation, headless, on the scripted Mira: the tools really change the ECS.
    #[test]
    fn demo_conversation_changes_the_world() {
        let mut app = App::new();
        app.add_plugin(MinimalPlugins::new());
        add_tavern(&mut app, BackendChoice::backend(scripted_mira()), true);
        for _ in 0..20_000 {
            app.step_ticks(1);
            let dialogue = app.resources.expect::<Dialogue>();
            if dialogue.queue.is_empty() && dialogue.turn.is_none() && dialogue.turns == DEMO.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let dialogue = app.resources.expect::<Dialogue>();
        assert_eq!(
            dialogue.turns,
            DEMO.len(),
            "every line was answered: {:?}",
            dialogue.log
        );
        let mira: Vec<&str> = dialogue
            .log
            .iter()
            .filter(|(who, _)| who == "Mira")
            .map(|(_, l)| l.as_str())
            .collect();
        assert_eq!(mira[0], "Welcome to the Rusty Anchor! What can I get you?");
        assert_eq!(
            mira[1],
            "Tonight we have ale for 2 gold, bread for 1 gold, pie for 3 gold, stew for 5 gold."
        );
        assert_eq!(mira[2], "One stew, coming right up! That's 5 gold; you've 7 left.");
        assert_eq!(
            mira[3], "Ah, sorry, love. Only 1 pie left.",
            "the tool refused: there is one pie"
        );
        assert!(dialogue.ended);
        assert_eq!(dialogue.shown, mira[4], "the streamed deltas add up to the line");
        assert_eq!(app.resources.expect::<Purse>().0, START_GOLD - 5);
        let served: Vec<&str> = app.world.query_mut::<&Served>().into_iter().map(|s| s.item).collect();
        assert_eq!(served, ["stew"]);
        let stew = app
            .world
            .query_mut::<(&MenuItem, &Stock)>()
            .into_iter()
            .find(|(item, _)| item.name == "stew")
            .map(|(_, stock)| stock.0);
        assert_eq!(stew, Some(3));
    }

    #[test]
    fn orders_are_validated_against_the_world() {
        let mut app = App::new();
        app.add_plugin(MinimalPlugins::new());
        app.insert_resource(Purse(4));
        for entry in menu() {
            app.world.spawn(entry);
        }
        let mut tool = take_order();
        let mut call = |app: &mut App, args: Value| {
            let invocation = space3d::ai::ToolCall {
                id: "c".into(),
                name: "take_order".into(),
                arguments: args,
            };
            let mut ctx = SystemCtx {
                world: &mut app.world,
                resources: &mut app.resources,
            };
            run_tool(&mut tool, &mut ctx, invocation)
        };
        assert_eq!(
            call(&mut app, json!({"item": "stew"})),
            Err("1 stew cost 5 gold, but the player has only 4.".into())
        );
        assert_eq!(
            call(&mut app, json!({"item": "pies", "quantity": 2})),
            Err("Only 1 pie left.".into())
        );
        assert_eq!(
            call(&mut app, json!({"item": "soup"})),
            Err("'soup' is not on the menu.".into())
        );
        let ok = call(&mut app, json!({"item": "ale", "quantity": 2})).unwrap();
        assert_eq!(ok["paid"], 4);
        assert_eq!(app.resources.expect::<Purse>().0, 0);
    }

    /// Runs a tool's handler directly (what `ai::run_tools` does for a `tool/call`).
    fn run_tool(tool: &mut AiTool, ctx: &mut SystemCtx, call: space3d::ai::ToolCall) -> Result<Value, String> {
        match tool.invoke(ctx, call) {
            ToolReply::Output(ToolOutput::Json(value)) => Ok(value),
            ToolReply::Output(ToolOutput::Error(message)) => Err(message),
            other => panic!("unexpected reply {other:?}"),
        }
    }
}
