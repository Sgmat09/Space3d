//! `client`: the multiplayer_arena client (native and web). WASD / arrow keys drive your cube;
//! it is predicted locally, and everyone else's cube is interpolated.
//!
//! ```text
//! # through the player service (guest login → quick play → signed join ticket → /play):
//! client --ps http://127.0.0.1:7777 --name alice
//! # straight to a dev game server (`game.allow_dev_auth`, loopback only):
//! client --server ws://127.0.0.1:7777/play --name bob
//! # headless bot for tests and smoke runs; exits after 10 s with code 0 if it was connected:
//! client --headless --bot --ps http://127.0.0.1:7777 --name bot-1 --seconds 10
//! ```
//!
//! On the web the same options come from the page URL: `index.html?ps=https://…&name=alice`
//! (or `server=wss://…/play`, `bot`).
//!
//! With `--ps`, a failed session (room crash or handoff, server loss, expired resume) is
//! recovered through `POST /v1/sessions/rejoin`, which may restore the room on another game
//! server; retry hints are honoured with backoff (see `online_flow`).
//!
//! The window title shows the connection state, RTT, cube count, rollbacks and bandwidth. On
//! exit (`--frames` / `--seconds`, Esc, or closing the window) a one-line summary is printed.

use multiplayer_arena::{COLORS, CubeColor, CubesProtocol, GAME_ID, HALF_EXTENT, MODE, TICK_HZ, Wander};
use space3d_core::prelude::*;
use space3d_platform::prelude::*;
use space3d_render::{AmbientLight, Camera3d, DirectionalLight, Material, Mesh3d, MeshHandle, RenderAssets};
use space3d_replication::api::{Assignment, QuickPlayRequest, SessionTokens};
use space3d_replication::bots::install_bot;
use space3d_replication::client::{ConnectionEvent, ConnectionState};
use space3d_replication::prelude::*;
use space3d_replication::services::{DeviceCredentials, OnlineError, PlayerServiceClient};
use space3d_replication::{Controlled, NetEntity};
use space3d_tasks::Task;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Command-line / URL options.
#[derive(Clone, Debug)]
struct Options {
    /// Game server `/play` URL for dev auth (ignored with `ps`).
    server: String,
    /// Player-service base URL.
    ps: Option<String>,
    name: String,
    bot: bool,
    headless: bool,
    frames: Option<u64>,
    seconds: Option<f64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            server: "ws://127.0.0.1:7777/play".into(),
            ps: None,
            name: "player".into(),
            bot: false,
            headless: false,
            frames: std::env::var("SPACE3D_EXIT_AFTER_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok()),
            seconds: std::env::var("SPACE3D_EXIT_AFTER_SECS")
                .ok()
                .and_then(|v| v.parse().ok()),
        }
    }
}

impl Options {
    fn apply(&mut self, key: &str, value: Option<String>) {
        match (key, value) {
            ("server", Some(v)) => self.server = v,
            ("ps", Some(v)) => self.ps = Some(v.trim_end_matches('/').to_owned()),
            ("name", Some(v)) => self.name = v,
            ("frames", Some(v)) => self.frames = v.parse().ok(),
            ("seconds", Some(v)) => self.seconds = v.parse().ok(),
            ("bot", v) => self.bot = v.is_none_or(|v| v != "0"),
            ("headless", v) => self.headless = v.is_none_or(|v| v != "0"),
            (other, _) => eprintln!("client: unknown option {other:?}"),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn parse() -> Self {
        let mut o = Self::default();
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            let Some(key) = a.strip_prefix("--") else {
                eprintln!("client: unexpected argument {a:?}");
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

fn main() {
    let options = Options::parse();
    init_logging();
    if options.frames.is_some() || options.seconds.is_some() {
        // Scripted run: never steal the developer's focus.
        space3d_platform::set_background_mode(true);
    }
    let mut app = App::new().with_tickrate(TICK_HZ);
    if !options.headless {
        app.add_plugin(PlatformPlugin {
            window: WindowPlugin::titled("Space3d multiplayer arena"),
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
    .add_plugin(CubesProtocol);

    if options.bot {
        install_bot(&mut app, Wander::new(Wander::seed_of(&options.name)));
    } else if !options.headless {
        multiplayer_arena::controls::install(&mut app);
    }
    if !options.headless {
        app.add_system_named(Stage::Startup, "arena::scene", scene);
        app.add_system_named(Stage::Update, "arena::dress", dress);
        app.add_system_named(Stage::Update, "arena::camera", follow_camera);
        app.add_system_named(Stage::Last, "arena::hud", hud);
        app.add_system_named(Stage::PreUpdate, "arena::quit", |ctx| {
            if ctx.resources.expect::<Input>().just_pressed(KeyCode::Escape) {
                ctx.send(AppExit);
            }
        });
    }

    // Join: through the player service (ticket) or straight to a dev server.
    let online = match &options.ps {
        Some(url) => {
            let api = PlayerServiceClient::new(&space3d_net::Http::new(), url);
            let login = api.login_guest(&DeviceCredentials::generate(), Some(&options.name));
            Online::LoggingIn(api, login)
        }
        None => {
            app.resources.expect_mut::<NetClient>().connect(ConnectTarget::Dev {
                url: options.server.clone(),
                name: options.name.clone(),
            });
            Online::Dev
        }
    };
    app.insert_resource(online);
    app.add_system_named(Stage::PreUpdate, "arena::online", online_flow);
    app.add_system_named(Stage::Update, "arena::log", log_events());

    let code = Arc::new(AtomicU8::new(0));
    app.add_system_named(Stage::Last, "arena::exit", exit_when_done(&options, code.clone()));
    app.run();
    #[cfg(not(target_arch = "wasm32"))]
    std::process::exit(i32::from(code.load(Ordering::Acquire)));
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

// ---- player service flow ------------------------------------------------------------------------

/// Where the client is in the online flow (resource).
enum Online {
    /// Dev auth: `NetClient` was pointed at the game server directly.
    Dev,
    LoggingIn(PlayerServiceClient, Task<Result<SessionTokens, OnlineError>>),
    QuickPlay(PlayerServiceClient, Task<Result<Assignment, OnlineError>>),
    /// Connected (or connecting) with a ticket; watches for a failed session.
    Playing {
        api: PlayerServiceClient,
        rejoin: Rejoins,
    },
    /// Waiting before asking the player service to rejoin (a retry hint or backoff).
    Waiting {
        api: PlayerServiceClient,
        until_s: f64,
        rejoin: Rejoins,
    },
    Rejoining {
        api: PlayerServiceClient,
        task: Task<Result<Option<Assignment>, OnlineError>>,
        rejoin: Rejoins,
    },
    Failed,
}

/// Rejoin attempts since the session last worked: the budget resets once connected again.
#[derive(Clone, Copy, Debug, Default)]
struct Rejoins {
    attempts: u32,
}

/// Rejoin attempts in a row (without getting connected in between) before giving up.
const MAX_REJOINS: u32 = 8;
/// Backoff between rejoin attempts in a row: 250 ms, doubling, at most 4 s (≈ 16 s in all).
const REJOIN_BACKOFF_S: (f64, f64) = (0.25, 4.0);

impl Rejoins {
    /// Seconds to wait before the next attempt: the server's or player service's retry hint
    /// when there is one (never less), else exponential backoff — the first attempt after a
    /// working session goes at once (a room handoff or a crash wants a new assignment now).
    fn delay_s(self, hint: Option<std::time::Duration>) -> f64 {
        let backoff = if self.attempts == 0 {
            0.0
        } else {
            (REJOIN_BACKOFF_S.0 * 2f64.powi(self.attempts as i32 - 1)).min(REJOIN_BACKOFF_S.1)
        };
        hint.map_or(backoff, |h| h.as_secs_f64().max(backoff))
    }
}

/// PreUpdate: poll the player-service calls (never blocks the frame) and hand tickets to
/// `NetClient`. A lost session (room moved, crashed or closed, server gone) is recovered
/// through `POST /v1/sessions/rejoin`, honouring retry hints (`Reject.retry_after_ms` of a
/// draining server, `ApiError.retry_after_ms` of a player service holding a moving room) with
/// backoff, and giving up after [`MAX_REJOINS`] attempts in a row.
fn online_flow(ctx: &mut SystemCtx) {
    let now_s = ctx.time().elapsed_secs();
    let state = std::mem::replace(ctx.resources.expect_mut::<Online>(), Online::Failed);
    let quick = |api: &PlayerServiceClient| {
        api.quick_play(&QuickPlayRequest {
            mode: MODE.into(),
            ..Default::default()
        })
    };
    // Schedule the next rejoin attempt, or give up.
    let retry = |api: PlayerServiceClient, rejoin: Rejoins, hint: Option<std::time::Duration>, why: String| {
        if rejoin.attempts >= MAX_REJOINS {
            tracing::error!(attempts = rejoin.attempts, %why, "giving up rejoining");
            return Online::Failed;
        }
        let wait = rejoin.delay_s(hint);
        tracing::warn!(%why, attempt = rejoin.attempts + 1, wait_s = wait, "session lost; rejoining through the player service");
        Online::Waiting {
            api,
            until_s: now_s + wait,
            rejoin: Rejoins {
                attempts: rejoin.attempts + 1,
            },
        }
    };
    let next = match state {
        Online::LoggingIn(api, mut task) => match task.poll() {
            None => Online::LoggingIn(api, task),
            Some(Ok(tokens)) => {
                tracing::info!(player = %tokens.profile.player_id, "logged in as a guest");
                let task = quick(&api);
                Online::QuickPlay(api, task)
            }
            Some(Err(e)) => {
                tracing::error!(error = %e, "guest login failed");
                Online::Failed
            }
        },
        Online::QuickPlay(api, mut task) => match task.poll() {
            None => Online::QuickPlay(api, task),
            Some(Ok(a)) => {
                tracing::info!(server = %a.server_id, room = %a.room_id, "join ticket received");
                connect_ticket(ctx, &a);
                Online::Playing {
                    api,
                    rejoin: Rejoins::default(),
                }
            }
            Some(Err(e)) => {
                tracing::error!(error = %e, "quick play failed");
                Online::Failed
            }
        },
        Online::Playing { api, mut rejoin } => match ctx.resources.expect::<NetClient>().state().clone() {
            // Resume tokens are process-local: after a room handoff or crash, or a server
            // loss, the player service knows where the room lives now (restoring it if
            // needed). A draining server's refusal says when to ask again.
            ConnectionState::Failed(reason) => retry(api, rejoin, reason.retry_after(), format!("{reason:?}")),
            state => {
                if matches!(state, ConnectionState::Connected) {
                    rejoin = Rejoins::default();
                }
                Online::Playing { api, rejoin }
            }
        },
        Online::Waiting { api, until_s, rejoin } if now_s < until_s => Online::Waiting { api, until_s, rejoin },
        Online::Waiting { api, rejoin, .. } => Online::Rejoining {
            task: api.rejoin(),
            api,
            rejoin,
        },
        Online::Rejoining { api, mut task, rejoin } => match task.poll() {
            None => Online::Rejoining { api, task, rejoin },
            Some(Ok(Some(a))) => {
                tracing::info!(server = %a.server_id, room = %a.room_id, "rejoining");
                connect_ticket(ctx, &a);
                Online::Playing { api, rejoin }
            }
            Some(Ok(None)) => {
                tracing::info!("room is gone; quick play again");
                let task = quick(&api);
                Online::QuickPlay(api, task)
            }
            // The room is moving (`503 room_moving`), no capacity yet, or the player service is
            // briefly unreachable: try again.
            Some(Err(e)) if e.retry_after_ms().is_some() || matches!(e, OnlineError::Net(_)) => {
                let hint = e
                    .retry_after_ms()
                    .map(|ms| std::time::Duration::from_millis(u64::from(ms)));
                retry(api, rejoin, hint, format!("rejoin failed: {e}"))
            }
            Some(Err(e)) => {
                tracing::error!(error = %e, "rejoin failed");
                Online::Failed
            }
        },
        s @ (Online::Dev | Online::Failed) => s,
    };
    *ctx.resources.expect_mut::<Online>() = next;
}

fn connect_ticket(ctx: &mut SystemCtx, a: &Assignment) {
    ctx.resources.expect_mut::<NetClient>().connect(ConnectTarget::Ticket {
        url: a.server_url.clone(),
        ticket: a.join_ticket.clone(),
    });
}

// ---- exit and summary ---------------------------------------------------------------------------

fn cube_count(world: &World) -> usize {
    world.query::<(&CubeColor, &NetEntity)>().iter().count()
}

/// Last: exit after `--frames` / `--seconds` (or on `AppExit` from Esc / window close) with a
/// one-line summary; a bot that is not connected at the end exits with code 1.
fn exit_when_done(options: &Options, code: Arc<AtomicU8>) -> impl FnMut(&mut SystemCtx) + Send + 'static {
    let (frames, seconds, bot) = (options.frames, options.seconds, options.bot);
    let mut exit_reader = EventReader::default();
    move |ctx| {
        let time = ctx.time();
        let limit = frames.is_some_and(|f| time.frame >= f) || seconds.is_some_and(|s| time.elapsed_secs() >= s);
        let requested = exit_reader.read(ctx.resources.expect::<Events<AppExit>>()).count() > 0;
        if !limit && !requested {
            return;
        }
        let net = ctx.resources.expect::<NetClient>();
        let s = net.stats();
        let connected = net.is_connected();
        println!(
            "multiplayer_arena client: frames={} state={:?} cubes={} rtt_ms={:.1} down_bytes_per_sec={} \
             up_bytes_per_sec={} rollbacks={} resim_ticks={}",
            time.frame,
            net.state(),
            cube_count(ctx.world),
            s.rtt.as_secs_f64() * 1000.0,
            s.bytes_in_per_sec,
            s.bytes_out_per_sec,
            s.rollbacks,
            s.resim_ticks,
        );
        code.store(u8::from(bot && !connected), Ordering::Release);
        if limit {
            ctx.send(AppExit);
        }
    }
}

/// Connection events to the log.
fn log_events() -> impl FnMut(&mut SystemCtx) + Send + 'static {
    let mut conn = EventReader::default();
    move |ctx| {
        for e in conn.read(ctx.resources.expect::<Events<ConnectionEvent>>()) {
            tracing::info!(?e, "connection");
        }
    }
}

// ---- rendering (windowed only) ----------------------------------------------------------------

/// Mesh shared by every cube.
struct CubeMesh(MeshHandle);

const PALETTE: [[f32; 3]; COLORS as usize] = [
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
    let (cube, floor) = {
        let assets = ctx.resources.expect_mut::<RenderAssets>();
        (
            assets.add_mesh(MeshData::cube(1.0)),
            assets.add_mesh(MeshData::cube(1.0)),
        )
    };
    ctx.resources.insert(CubeMesh(cube));
    let side = HALF_EXTENT * 2.0 + 1.0;
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
        Transform::from_xyz(0.0, 18.0, 16.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.02, 0.025, 0.035, 1.0)),
    ));
}

/// Give newly replicated cubes a mesh and their colour (own cube slightly larger).
fn dress(ctx: &mut SystemCtx) {
    let Some(mesh) = ctx.resources.get::<CubeMesh>().map(|m| m.0) else {
        return;
    };
    let fresh: Vec<(Entity, u8, bool)> = ctx
        .world
        .query::<(Entity, &CubeColor, Option<&Controlled>)>()
        .without::<&Mesh3d>()
        .iter()
        .map(|(e, c, own)| (e, c.0, own.is_some()))
        .collect();
    for (e, color, own) in fresh {
        let [r, g, b] = PALETTE[usize::from(color % COLORS)];
        let material = Material::srgb(r, g, b).with_metallic_roughness(0.0, if own { 0.2 } else { 0.6 });
        let _ = ctx.world.insert(e, (Mesh3d(mesh), material));
    }
}

/// Camera above and behind the controlled cube.
fn follow_camera(ctx: &mut SystemCtx) {
    let Some(pos) = ctx
        .world
        .query::<(&Transform, &Controlled)>()
        .iter()
        .map(|(t, _)| t.translation)
        .next()
    else {
        return;
    };
    let eye = pos + Vec3::new(0.0, 14.0, 12.0);
    for (t, _) in ctx.world.query_mut::<(&mut Transform, &Camera3d)>() {
        *t = Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(pos, Vec3::Y);
    }
}

/// Window title: connection state, RTT, cubes, rollbacks, bandwidth.
fn hud(ctx: &mut SystemCtx) {
    if !ctx.time().frame.is_multiple_of(15) {
        return;
    }
    let cubes = cube_count(ctx.world);
    let net = ctx.resources.expect::<NetClient>();
    let s = net.stats();
    let title = format!(
        "Space3d multiplayer arena — {:?} | rtt {:.0} ms | {cubes} cubes | rollbacks {} | ↓ {:.1} KB/s ↑ {:.1} KB/s",
        net.state(),
        s.rtt.as_secs_f64() * 1000.0,
        s.rollbacks,
        f64::from(s.bytes_in_per_sec) / 1000.0,
        f64::from(s.bytes_out_per_sec) / 1000.0,
    );
    if let Some(w) = ctx.resources.get_mut::<WindowState>() {
        w.title = title;
    }
}
