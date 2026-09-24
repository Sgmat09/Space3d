//! Save slots and settings with `space3d-db`: every database call returns a `Task` that is
//! completed on the database worker thread, so saving and loading never stall a frame.
//!
//! ```text
//! cargo run -p save_game                      # collect coins, F5/F9 quicksave/quickload
//! cargo run -p save_game -- --reset           # delete every save first
//! SPACE3D_EXIT_AFTER_FRAMES=240 cargo run -p save_game   # smoke test (autosaves after 1 s)
//! ```
//! Controls: arrows/WASD move · **F5** quicksave · **F9** quickload · **Delete** removes all
//! saves · **Esc** quits. An autosave is written every 5 seconds and restored on the next
//! launch. The number of launches lives in a [`KvStore`] (`settings` namespace).
//!
//! Native builds store everything in `<tmp>/space3d_save_game.db` (override with
//! `--db PATH`); on the web the same API is backed by `localStorage`.

use serde::{Deserialize, Serialize};
use space3d::prelude::*;

/// Everything that goes into a save slot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct GameState {
    player: [f32; 3],
    coins: u32,
    /// Ids of the collected coins.
    collected: Vec<u32>,
    play_time: f64,
}

/// The player cube.
struct Player;

/// A collectible coin with a stable id (so saves can remember which ones are gone).
struct Coin(u32);

/// Handles to the stores (cheap clones of the `Db` resource).
struct Stores {
    saves: SaveSlots,
    settings: KvStore,
}

/// Live game state and bookkeeping.
#[derive(Default)]
struct Game {
    state: GameState,
    last_autosave: f64,
    status: String,
    launches: u64,
    /// Saves written this run (shown in the title).
    saves_written: u32,
    /// `--reset`: every save is being deleted, so nothing is restored at startup.
    reset: bool,
}

const COINS: u32 = 24;
const SPEED: f32 = 4.0;
const AUTOSAVE_EVERY: f64 = 5.0;

fn db_path(args: &[String]) -> String {
    if let Some(i) = args.iter().position(|a| a == "--db")
        && let Some(path) = args.get(i + 1)
    {
        return path.clone();
    }
    if cfg!(target_arch = "wasm32") {
        "save_game".into()
    } else {
        std::env::temp_dir()
            .join("space3d_save_game.db")
            .to_string_lossy()
            .into_owned()
    }
}

fn build(app: &mut App) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = db_path(&args);
    println!("database: {path}");
    app.add_plugin(
        DefaultPlugins::new()
            .title("Space3d — save game")
            .with_db(DbPlugin::new(path))
            .without_physics()
            .without_net(),
    );
    add_game(app, args.iter().any(|a| a == "--reset"));
}

/// The game on top of an app that already has the engine plugins and a [`Db`] (the tests use
/// a headless app). `reset` deletes every save first and does not restore the autosave.
fn add_game(app: &mut App, reset: bool) {
    let db = app.resources.expect::<Db>().clone();
    app.insert_resource(Stores {
        saves: db.save_slots(),
        settings: db.kv("settings"),
    });
    app.insert_resource(Game {
        reset,
        ..Default::default()
    });
    if reset {
        // Queued before anything else, and the database worker runs requests in order: the
        // deletes land before the first autosave. `startup_io` skips restoring the old one.
        delete_all_saves(
            app.resources.expect::<Stores>().saves.clone(),
            app.resources.expect_mut::<AsyncQueue>(),
        );
    }
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::Startup, startup_io);
    app.add_system(Stage::FixedUpdate, movement);
    app.add_system(Stage::FixedUpdate, collect_coins);
    app.add_system(Stage::Update, controls);
    app.add_system(Stage::Update, autosave);
    app.add_system(Stage::Last, title);
}

fn coin_position(id: u32) -> Vec3 {
    // Deterministic scatter on a 12×12 field.
    let h = id.wrapping_mul(2_654_435_761);
    let x = (h % 1000) as f32 / 1000.0 * 11.0 - 5.5;
    let z = ((h / 1000) % 1000) as f32 / 1000.0 * 11.0 - 5.5;
    Vec3::new(x, 0.35, z)
}

fn setup(ctx: &mut SystemCtx) {
    let (cube, sphere, plane) = {
        let assets = ctx.resources.expect_mut::<RenderAssets>();
        (
            assets.add_mesh(MeshData::cube(0.8)),
            assets.add_mesh(MeshData::sphere(0.25, 16, 12)),
            assets.add_mesh(MeshData::plane(13.0)),
        )
    };
    ctx.world.spawn((
        Player,
        Transform::from_xyz(0.0, 0.4, 0.0),
        Mesh3d(cube),
        Material::srgb(0.25, 0.6, 0.95).with_metallic_roughness(0.1, 0.4),
    ));
    for id in 0..COINS {
        let p = coin_position(id);
        ctx.world.spawn((
            Coin(id),
            Visibility::Visible,
            Transform::from_xyz(p.x, p.y, p.z),
            Mesh3d(sphere),
            Material::srgb(1.0, 0.8, 0.2)
                .with_metallic_roughness(1.0, 0.25)
                .with_emissive(Vec3::new(0.2, 0.15, 0.0)),
        ));
    }
    ctx.world.spawn((
        Transform::default(),
        Mesh3d(plane),
        Material::srgb(0.3, 0.34, 0.3).with_metallic_roughness(0.0, 0.95),
    ));
    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.3, -1.0, -0.5), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.2,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.7, 0.8, 1.0),
        intensity: 0.2,
    });
    ctx.world.spawn((
        Transform::from_xyz(0.0, 11.0, 9.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.05, 0.07, 0.1, 1.0)),
    ));
}

/// Bump the launch counter, print the save list and restore the autosave — all asynchronous.
/// With `--reset` the list and the restore are skipped (the saves are being deleted).
fn startup_io(ctx: &mut SystemCtx) {
    let (saves, settings) = {
        let s = ctx.resources.expect::<Stores>();
        (s.saves.clone(), s.settings.clone())
    };
    let reset = ctx.resources.expect::<Game>().reset;
    let queue = ctx.resources.expect_mut::<AsyncQueue>();
    let counter = settings.clone();
    queue.track(settings.get::<u64>("launches"), move |ctx, result| {
        let launches = result.ok().flatten().unwrap_or(0) + 1;
        ctx.resources.expect_mut::<Game>().launches = launches;
        // Fire and forget: the write completes on the worker.
        let _ = counter.set("launches", &launches);
        println!("launch #{launches}");
    });
    if reset {
        return;
    }
    queue.track(saves.list(), |_ctx, result| match result {
        Ok(list) if list.is_empty() => println!("no saves yet"),
        Ok(list) => {
            println!("saves:");
            for meta in list {
                println!(
                    "  {:<10} {:<28} {:>6} bytes  {}",
                    meta.slot, meta.label, meta.size_bytes, meta.metadata
                );
            }
        }
        Err(e) => eprintln!("listing saves failed: {e}"),
    });
    load_slot(ctx, "autosave");
}

/// Queue a load of `slot`; the world is updated when the worker answers.
fn load_slot(ctx: &mut SystemCtx, slot: &'static str) {
    let task = ctx.resources.expect::<Stores>().saves.load(slot);
    ctx.resources
        .expect_mut::<AsyncQueue>()
        .track(task, move |ctx, result| match result {
            Ok(Some(save)) => match save.decode::<GameState>() {
                Ok(state) => {
                    apply_state(ctx, &state);
                    let game = ctx.resources.expect_mut::<Game>();
                    game.state = state;
                    game.status = format!("loaded '{slot}' ({})", save.meta.label);
                    println!("{}", game.status);
                }
                Err(e) => eprintln!("save '{slot}' is corrupt: {e}"),
            },
            Ok(None) => {
                if slot != "autosave" {
                    ctx.resources.expect_mut::<Game>().status = format!("no save in '{slot}'");
                }
            }
            Err(e) => eprintln!("loading '{slot}' failed: {e}"),
        });
}

/// Move the player and hide collected coins according to `state`.
fn apply_state(ctx: &mut SystemCtx, state: &GameState) {
    for (transform, _) in ctx.world.query_mut::<(&mut Transform, &Player)>() {
        transform.translation = Vec3::from_array(state.player);
    }
    for (visibility, coin) in ctx.world.query_mut::<(&mut Visibility, &Coin)>() {
        *visibility = if state.collected.contains(&coin.0) {
            Visibility::Hidden
        } else {
            Visibility::Visible
        };
    }
}

/// Write the current state to `slot` (with a tiny mini-map PNG as thumbnail).
fn save_slot(ctx: &mut SystemCtx, slot: &'static str, label: String) {
    let state = ctx.resources.expect::<Game>().state.clone();
    let save = match NewSave::from_state(label, &state) {
        Ok(save) => save,
        Err(e) => {
            eprintln!("cannot encode save: {e}");
            return;
        }
    };
    let save = save
        .with_metadata(serde_json::json!({ "coins": state.coins, "play_time": state.play_time.round() }))
        .with_thumbnail(minimap_png(&state));
    let task = ctx.resources.expect::<Stores>().saves.save(slot, save);
    ctx.resources
        .expect_mut::<AsyncQueue>()
        .track(task, move |ctx, result| {
            let game = ctx.resources.expect_mut::<Game>();
            match result {
                Ok(meta) => {
                    game.saves_written += 1;
                    game.status = format!("saved '{}' ({} bytes)", meta.slot, meta.size_bytes);
                    if slot != "autosave" {
                        println!("{}", game.status);
                    }
                }
                Err(e) => game.status = format!("save failed: {e}"),
            }
        });
}

/// 16×16 PNG: player in blue, remaining coins in yellow.
fn minimap_png(state: &GameState) -> Vec<u8> {
    let mut image = ImageData::filled(16, 16, [40, 48, 40, 255]);
    let to_px = |p: Vec3| {
        (
            ((p.x + 6.5) / 13.0 * 15.0).clamp(0.0, 15.0) as u32,
            ((p.z + 6.5) / 13.0 * 15.0).clamp(0.0, 15.0) as u32,
        )
    };
    for id in (0..COINS).filter(|id| !state.collected.contains(id)) {
        let (x, y) = to_px(coin_position(id));
        image.set(x, y, [255, 210, 60, 255]);
    }
    let (x, y) = to_px(Vec3::from_array(state.player));
    image.set(x, y, [60, 150, 255, 255]);
    edit::texture::encode_png(&image).unwrap_or_default()
}

fn delete_all_saves(saves: SaveSlots, queue: &mut AsyncQueue) {
    let deleter = saves.clone();
    queue.track(saves.list(), move |ctx, list| {
        for meta in list.unwrap_or_default() {
            let _ = deleter.delete(meta.slot);
        }
        ctx.resources.expect_mut::<Game>().status = "all saves deleted".into();
        println!("all saves deleted");
    });
}

fn movement(ctx: &mut SystemCtx) {
    let dt = ctx.time().fixed_delta.as_secs_f32();
    let input = ctx.resources.expect::<Input>();
    let axis = |neg: [KeyCode; 2], pos: [KeyCode; 2]| {
        input.any_pressed(pos) as i32 as f32 - input.any_pressed(neg) as i32 as f32
    };
    let dir = Vec3::new(
        axis(
            [KeyCode::ArrowLeft, KeyCode::KeyA],
            [KeyCode::ArrowRight, KeyCode::KeyD],
        ),
        0.0,
        axis([KeyCode::ArrowUp, KeyCode::KeyW], [KeyCode::ArrowDown, KeyCode::KeyS]),
    )
    .normalize_or_zero();
    let mut position = None;
    for (transform, _) in ctx.world.query_mut::<(&mut Transform, &Player)>() {
        transform.translation =
            (transform.translation + dir * SPEED * dt).clamp(Vec3::new(-6.0, 0.4, -6.0), Vec3::new(6.0, 0.4, 6.0));
        position = Some(transform.translation);
    }
    let game = ctx.resources.expect_mut::<Game>();
    game.state.play_time += dt as f64;
    if let Some(p) = position {
        game.state.player = p.to_array();
    }
}

fn collect_coins(ctx: &mut SystemCtx) {
    let player = Vec3::from_array(ctx.resources.expect::<Game>().state.player);
    let mut picked = Vec::new();
    for (transform, coin, visibility) in ctx.world.query_mut::<(&Transform, &Coin, &mut Visibility)>() {
        if *visibility == Visibility::Visible && transform.translation.distance(player) < 0.7 {
            *visibility = Visibility::Hidden;
            picked.push(coin.0);
        }
    }
    let game = ctx.resources.expect_mut::<Game>();
    for id in picked {
        game.state.coins += 1;
        game.state.collected.push(id);
    }
}

fn controls(ctx: &mut SystemCtx) {
    let input = ctx.resources.expect::<Input>();
    let (quit, save, load, wipe) = (
        input.just_pressed(KeyCode::Escape),
        input.just_pressed(KeyCode::F5),
        input.just_pressed(KeyCode::F9),
        input.just_pressed(KeyCode::Delete),
    );
    if quit {
        ctx.send(AppExit);
    }
    if save {
        let coins = ctx.resources.expect::<Game>().state.coins;
        save_slot(ctx, "quick", format!("Quicksave — {coins} coins"));
    }
    if load {
        load_slot(ctx, "quick");
    }
    if wipe {
        let saves = ctx.resources.expect::<Stores>().saves.clone();
        delete_all_saves(saves, ctx.resources.expect_mut::<AsyncQueue>());
    }
}

/// Autosave one second in, then every [`AUTOSAVE_EVERY`] seconds.
fn autosave(ctx: &mut SystemCtx) {
    let now = ctx.time().elapsed_secs();
    let game = ctx.resources.expect::<Game>();
    let due = if game.saves_written == 0 && game.last_autosave == 0.0 {
        now >= 1.0
    } else {
        now - game.last_autosave >= AUTOSAVE_EVERY
    };
    if due {
        ctx.resources.expect_mut::<Game>().last_autosave = now;
        let coins = ctx.resources.expect::<Game>().state.coins;
        save_slot(ctx, "autosave", format!("Autosave — {coins} coins"));
    }
}

fn title(ctx: &mut SystemCtx) {
    if !ctx.time().frame.is_multiple_of(15) {
        return;
    }
    let game = ctx.resources.expect::<Game>();
    let text = format!(
        "Space3d — save game | coins {}/{COINS} | launch #{} | {} saves this run | {}",
        game.state.coins, game.launches, game.saves_written, game.status
    );
    if let Some(window) = ctx.resources.get_mut::<WindowState>() {
        window.title = text;
    }
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_roundtrip_through_slots() {
        let db = Db::open_in_memory();
        let saves = db.save_slots();
        let state = GameState {
            player: [1.0, 0.4, -2.0],
            coins: 3,
            collected: vec![1, 5, 7],
            play_time: 12.5,
        };
        let save = NewSave::from_state("t", &state)
            .unwrap()
            .with_thumbnail(minimap_png(&state));
        let mut task = saves.save("slot", save);
        let meta = loop {
            if let Some(r) = task.poll() {
                break r.unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert!(meta.has_thumbnail);
        let mut task = saves.load("slot");
        let loaded = loop {
            if let Some(r) = task.poll() {
                break r.unwrap().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(loaded.decode::<GameState>().unwrap(), state);
    }

    /// A headless game on the database file `path`.
    fn headless(path: &std::path::Path, reset: bool) -> App {
        let mut app = App::new();
        app.add_plugin(
            DefaultPlugins::headless(32, 32)
                .with_db(DbPlugin::new(path))
                .without_physics()
                .without_net()
                .without_audio(),
        );
        add_game(&mut app, reset);
        app
    }

    /// Step until `done` holds (database answers arrive on the worker thread).
    fn step_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        for _ in 0..5_000 {
            if done(app) {
                return;
            }
            app.step_ticks(1);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn autosave_is_restored_unless_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("saves.db");

        // Run 1: walk right for half a second, then wait for the autosave (after 1 s).
        let mut app = headless(&path, false);
        app.step_ticks(2);
        app.resources.expect_mut::<Input>().inject_key_down(KeyCode::ArrowRight);
        app.step_ticks(30);
        app.resources.expect_mut::<Input>().inject_key_up(KeyCode::ArrowRight);
        step_until(&mut app, "the first autosave", |app| {
            app.resources.expect::<Game>().saves_written > 0
        });
        let saved_x = app.resources.expect::<Game>().state.player[0];
        assert!(saved_x > 1.0, "the player moved: {saved_x}");
        drop(app);

        // Run 2: the autosave is restored.
        let mut app = headless(&path, false);
        step_until(&mut app, "the autosave to load", |app| {
            app.resources.expect::<Game>().status.starts_with("loaded 'autosave'")
        });
        assert_eq!(app.resources.expect::<Game>().state.player[0], saved_x);
        drop(app);

        // Run 3 with --reset: nothing is restored, and the saves are gone.
        let mut app = headless(&path, true);
        step_until(&mut app, "the reset", |app| {
            app.resources.expect::<Game>().status == "all saves deleted"
        });
        app.step_ticks(30);
        let game = app.resources.expect::<Game>();
        assert_eq!(game.state.player, [0.0, 0.4, 0.0], "the old autosave was not restored");
        assert!(game.state.play_time < 1.0);
        let mut list = app.resources.expect::<Stores>().saves.list();
        let slots = loop {
            if let Some(r) = list.poll() {
                break r.unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert!(slots.is_empty(), "{slots:?}");
    }

    #[test]
    fn coins_are_on_the_field() {
        for id in 0..COINS {
            let p = coin_position(id);
            assert!(p.x.abs() <= 5.5 && p.z.abs() <= 5.5, "{p:?}");
        }
    }
}
