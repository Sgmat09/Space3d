//! Arena client (native + web). Connects to a Space3d game server, predicts its own pawn and
//! interpolates everyone else.
//!
//! ```text
//! # against a dev server (game server with `game.allow_dev_auth = true` on loopback):
//! cargo run -p arena-client -- --server ws://127.0.0.1:7777/play --name alice
//! # with a join ticket from the player service (`Assignment { server_url, join_ticket }`):
//! cargo run -p arena-client -- --server wss://gs-3.play.example.com/play --ticket <join_ticket>
//! # headless bot (smoke / load testing), exits after 600 frames:
//! cargo run -p arena-client -- --headless --bot --name bot-1 --frames 600
//! ```
//!
//! On the web the same options come from the page URL:
//! `index.html?server=wss://…/play&name=alice` (or `&ticket=…`, `&bot`).
//!
//! Controls: WASD move (relative to facing), Shift sprint, mouse or Q / E turn, Space or left
//! click fire, Enter says hello in chat, Esc quits. The window title shows the connection
//! state, RTT, own health / ammo / score and rollbacks.

use arena_shared::*;
use space3d_core::prelude::*;
use space3d_platform::prelude::*;
use space3d_render::{AmbientLight, Camera3d, DirectionalLight, Material, Mesh3d, MeshHandle, RenderAssets};
use space3d_replication::bots::install_bot;
use space3d_replication::client::{ConnectionEvent, RemoteDespawned};
use space3d_replication::prelude::*;
use space3d_replication::{Controlled, NetEntity};

/// Command-line / URL options.
#[derive(Clone, Debug)]
struct Options {
    server: String,
    name: String,
    ticket: Option<String>,
    bot: bool,
    headless: bool,
    frames: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            server: "ws://127.0.0.1:7777/play".into(),
            name: "player".into(),
            ticket: None,
            bot: false,
            headless: false,
            frames: std::env::var("SPACE3D_EXIT_AFTER_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok()),
        }
    }
}

impl Options {
    fn apply(&mut self, key: &str, value: Option<String>) {
        match (key, value) {
            ("server", Some(v)) => self.server = v,
            ("name", Some(v)) => self.name = v,
            ("ticket", Some(v)) => self.ticket = Some(v),
            ("frames", Some(v)) => self.frames = v.parse().ok(),
            ("bot", v) => self.bot = v.is_none_or(|v| v != "0"),
            ("headless", v) => self.headless = v.is_none_or(|v| v != "0"),
            (other, _) => eprintln!("arena-client: unknown option {other:?}"),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn parse() -> Self {
        let mut o = Self::default();
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            let Some(key) = a.strip_prefix("--") else {
                eprintln!("arena-client: unexpected argument {a:?}");
                continue;
            };
            let value = match key {
                "bot" | "headless" => None,
                _ => args.next(),
            };
            o.apply(key, value);
        }
        o
    }

    #[cfg(target_arch = "wasm32")]
    fn parse() -> Self {
        let mut o = Self::default();
        let search = web_sys::window()
            .and_then(|w| w.location().search().ok())
            .unwrap_or_default();
        for pair in search.trim_start_matches('?').split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair
                .split_once('=')
                .map_or((pair, None), |(k, v)| (k, Some(v.to_owned())));
            o.apply(k, v);
        }
        o
    }
}

/// Aim and latched fire presses, read by the input sampler every predicted tick.
#[derive(Default)]
struct Controls {
    yaw: f32,
    fire_seq: u8,
}

fn main() {
    let options = Options::parse();
    init_logging();
    if options.frames.is_some() {
        // Scripted run: never steal the developer's focus.
        space3d_platform::set_background_mode(true);
    }
    let mut app = App::new().with_tickrate(TICK_HZ);
    if !options.headless {
        app.add_plugin(PlatformPlugin {
            window: WindowPlugin::titled("Space3d arena"),
        });
    }
    // Replication first, then the game (stage order is registration order).
    app.add_plugin(ClientReplicationPlugin {
        config: ClientConfig {
            game_id: GAME_ID.into(),
            game_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        },
    })
    .add_plugin(ArenaProtocol);
    app.init_resource::<Controls>();
    if options.bot {
        install_bot(&mut app, wander);
    } else if !options.headless {
        app.add_system_named(Stage::PreUpdate, "arena::controls", controls);
        app.set_input_sampler(sample);
    }
    if !options.headless {
        app.add_system_named(Stage::Startup, "arena::scene", scene);
        app.add_system_named(Stage::Update, "arena::dress", dress);
        app.add_system_named(Stage::Update, "arena::camera", follow_camera);
        app.add_system_named(Stage::Last, "arena::hud", hud);
    }
    app.add_system_named(Stage::Update, "arena::log", log_events());
    let frames = options.frames;
    app.add_system_named(Stage::Last, "arena::exit", move |ctx| {
        if frames.is_some_and(|f| ctx.time().frame >= f) {
            let net = ctx.resources.expect::<NetClient>();
            println!(
                "arena-client: exiting after {} frames: {:?}, {:?}",
                ctx.time().frame,
                net.state(),
                net.stats()
            );
            ctx.send(AppExit);
        }
    });
    let target = match options.ticket.clone() {
        Some(ticket) => ConnectTarget::Ticket {
            url: options.server.clone(),
            ticket,
        },
        None => ConnectTarget::Dev {
            url: options.server.clone(),
            name: options.name.clone(),
        },
    };
    app.resources.expect_mut::<NetClient>().connect(target);
    app.run();
}

#[cfg(not(target_arch = "wasm32"))]
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(target_arch = "wasm32")]
fn init_logging() {
    space3d_platform::init_web_logging();
}

/// PreUpdate: turn, and latch the fire edge. The sampler runs in FixedUpdate (zero or several
/// times per frame), so presses are counted here and sent as a wrapping counter.
fn controls(ctx: &mut SystemCtx) {
    let dt = ctx.time().delta_secs();
    let (turn, fire, quit, say) = {
        let input = ctx.resources.expect::<Input>();
        let mut turn = -input.mouse_delta().x * 0.003;
        if input.pressed(KeyCode::KeyQ) {
            turn += 2.5 * dt;
        }
        if input.pressed(KeyCode::KeyE) {
            turn -= 2.5 * dt;
        }
        let fire = input.just_pressed(KeyCode::Space) || input.mouse_just_pressed(MouseButton::Left);
        (
            turn,
            fire,
            input.just_pressed(KeyCode::Escape),
            input.just_pressed(KeyCode::Enter),
        )
    };
    let c = ctx.resources.expect_mut::<Controls>();
    c.yaw += turn;
    if fire {
        c.fire_seq = c.fire_seq.wrapping_add(1);
    }
    if say {
        let _ = ctx
            .resources
            .expect_mut::<NetClient>()
            .send(&Say("hello from the arena client".into()));
    }
    if quit {
        ctx.send(AppExit);
    }
}

/// The input sampler (once per predicted tick).
fn sample(ctx: &mut SystemCtx) -> ArenaInput {
    let input = ctx.resources.expect::<Input>();
    let axis = |neg: KeyCode, pos: KeyCode| -> f32 {
        match (input.pressed(neg), input.pressed(pos)) {
            (true, false) => -127.0,
            (false, true) => 127.0,
            _ => 0.0,
        }
    };
    let sprint = input.pressed(KeyCode::ShiftLeft) || input.pressed(KeyCode::ShiftRight);
    let c = ctx.resources.expect::<Controls>();
    // Movement relative to the facing direction.
    let local = Vec3::new(
        axis(KeyCode::KeyA, KeyCode::KeyD),
        0.0,
        axis(KeyCode::KeyW, KeyCode::KeyS),
    );
    let world = Quat::from_rotation_y(c.yaw) * local;
    ArenaInput {
        move_x: world.x.clamp(-127.0, 127.0) as i8,
        move_z: world.z.clamp(-127.0, 127.0) as i8,
        yaw: ArenaInput::yaw_from_radians(c.yaw),
        buttons: if sprint { ArenaInput::SPRINT } else { 0 },
        fire_seq: c.fire_seq,
    }
}

/// `--bot`: wander in a slow circle and shoot at whoever is closest.
fn wander(world: &World, tick: Tick) -> ArenaInput {
    let me = world
        .query::<(&Transform, &Controlled)>()
        .iter()
        .map(|(t, _)| t.translation)
        .next()
        .unwrap_or_default();
    let target = world
        .query::<(&Transform, &PlayerColor)>()
        .without::<&Controlled>()
        .iter()
        .map(|(t, _)| t.translation)
        .min_by(|a, b| a.distance(me).total_cmp(&b.distance(me)));
    let heading = tick.0 as f32 / 90.0;
    let yaw = target.map_or(heading, |t| {
        let d = t - me;
        (-d.x).atan2(-d.z)
    });
    ArenaInput {
        move_x: (heading.cos() * 90.0) as i8,
        move_z: (heading.sin() * 90.0) as i8,
        yaw: ArenaInput::yaw_from_radians(yaw),
        buttons: 0,
        fire_seq: if target.is_some() { (tick.0 / 30) as u8 } else { 0 },
    }
}

/// Mesh shared by every pawn.
struct PawnMesh(MeshHandle);

const PALETTE: [[f32; 3]; 8] = [
    [0.95, 0.35, 0.30],
    [0.30, 0.75, 0.95],
    [0.95, 0.80, 0.30],
    [0.50, 0.90, 0.50],
    [0.80, 0.45, 0.95],
    [0.95, 0.60, 0.20],
    [0.35, 0.95, 0.85],
    [0.90, 0.90, 0.90],
];

fn scene(ctx: &mut SystemCtx) {
    let (pawn, floor) = {
        let assets = ctx.resources.expect_mut::<RenderAssets>();
        (
            assets.add_mesh(MeshData::cube(1.0)),
            assets.add_mesh(MeshData::cube(1.0)),
        )
    };
    ctx.resources.insert(PawnMesh(pawn));
    let side = ARENA_HALF * 2.0 + 1.0;
    ctx.world.spawn((
        Transform::from_xyz(0.0, -0.1, 0.0).with_scale(Vec3::new(side, 0.2, side)),
        Mesh3d(floor),
        Material::srgb(0.18, 0.2, 0.24).with_metallic_roughness(0.0, 0.9),
    ));
    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.3), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.96, 0.9),
            intensity: 2.5,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.2,
    });
    ctx.world.spawn((
        Transform::from_xyz(0.0, 30.0, 30.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.02, 0.025, 0.035, 1.0)),
    ));
}

/// Give newly replicated pawns a mesh and a material.
fn dress(ctx: &mut SystemCtx) {
    let Some(mesh) = ctx.resources.get::<PawnMesh>().map(|m| m.0) else {
        return;
    };
    let fresh: Vec<(Entity, u8)> = ctx
        .world
        .query::<(Entity, &PlayerColor)>()
        .without::<&Mesh3d>()
        .iter()
        .map(|(e, c)| (e, c.0))
        .collect();
    for (e, color) in fresh {
        let [r, g, b] = PALETTE[usize::from(color) % PALETTE.len()];
        let material = Material::srgb(r, g, b).with_metallic_roughness(0.0, 0.5);
        let _ = ctx.world.insert(e, (Mesh3d(mesh), material));
    }
}

/// Third-person camera behind the controlled pawn.
fn follow_camera(ctx: &mut SystemCtx) {
    let Some((pos, rot)) = ctx
        .world
        .query::<(&Transform, &Controlled)>()
        .iter()
        .map(|(t, _)| (t.translation, t.rotation))
        .next()
    else {
        return;
    };
    let eye = pos + rot * Vec3::new(0.0, 9.0, 12.0);
    for (t, _) in ctx.world.query_mut::<(&mut Transform, &Camera3d)>() {
        *t = Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(pos, Vec3::Y);
    }
}

/// Window title: connection, RTT, own health / ammo / score, rollbacks.
fn hud(ctx: &mut SystemCtx) {
    if !ctx.time().frame.is_multiple_of(15) {
        return;
    }
    let own = ctx
        .world
        .query::<(&Controlled, Option<&Health>, Option<&Ammo>, Option<&Score>)>()
        .iter()
        .map(|(_, h, a, s)| (h.map(|h| h.0), a.map(|a| a.0), s.map(|s| s.0)))
        .next()
        .unwrap_or_default();
    let players = ctx.world.query::<(&PlayerColor, &NetEntity)>().iter().count();
    let net = ctx.resources.expect::<NetClient>();
    let title = format!(
        "Space3d arena — {:?} | rtt {:.0} ms | hp {:?} ammo {:?} score {:?} | {players} players | rollbacks {}",
        net.state(),
        net.stats().rtt.as_secs_f64() * 1000.0,
        own.0,
        own.1,
        own.2,
        net.stats().rollbacks,
    );
    if let Some(w) = ctx.resources.get_mut::<WindowState>() {
        w.title = title;
    }
}

/// Connection events, chat, hits and despawns to the log.
fn log_events() -> impl FnMut(&mut SystemCtx) + Send + 'static {
    let (mut conn, mut chat, mut hits, mut gone) = (
        EventReader::default(),
        EventReader::default(),
        EventReader::default(),
        EventReader::default(),
    );
    move |ctx| {
        for e in conn.read(ctx.resources.expect::<Events<ConnectionEvent>>()) {
            tracing::info!(?e, "connection");
        }
        for m in chat.read(ctx.resources.expect::<Events<FromServer<Chat>>>()) {
            tracing::info!(from = %m.message.from, "chat: {}", m.message.text);
        }
        for m in hits.read(ctx.resources.expect::<Events<FromServer<Hit>>>()) {
            tracing::info!(tick = m.tick.0, kill = m.message.kill, "hit at {:?}", m.message.point);
        }
        for d in gone.read(ctx.resources.expect::<Events<RemoteDespawned>>()) {
            tracing::debug!(net_id = ?d.net_id, out_of_scope = d.out_of_scope, "remote despawned");
        }
    }
}
