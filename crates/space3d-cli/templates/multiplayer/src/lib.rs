//! {{title}} — a multiplayer [Space3d](https://github.com/SpaceCorps/Space3d) game.
//!
//! Server-authoritative: clients send their input, the server simulates every player on the
//! fixed tick and broadcasts snapshots over WebSockets. Nothing ever waits on the network —
//! systems only poll.
//!
//! ```text
//! cargo run                                      # local server + client in one process
//! cargo run --bin server -- --port 7777          # dedicated headless server
//! cargo run -- --connect ws://HOST:7777 --name ann   # join it (also from the web build)
//! ```
//!
//! * [`protocol`] — messages and the shared movement rule,
//! * `server` — [`server::ServerPlugin`] (native only),
//! * [`client`] — [`client::ClientPlugin`] (networking) and [`client::ClientScenePlugin`]
//!   (rendering of the replicated players; used alone by stories and screenshot tests).

use space3d::prelude::*;

pub mod client;
pub mod protocol;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;
pub mod stories;

pub use client::{ClientPlugin, ClientScenePlugin};

/// Port of the dedicated server.
pub const DEFAULT_PORT: u16 = 7777;

/// Value following `flag` on the command line.
pub fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Configure the game client. Without `--connect URL` it also starts a local server in this
/// process (native), so `cargo run` is immediately playable.
pub fn build(app: &mut App) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    app.add_plugin(DefaultPlugins::new().window(WindowPlugin {
        title: "{{title}}".into(),
        canvas_id: Some("space3d".into()),
        ..WindowPlugin::default()
    }));
    let name = arg_value(&args, "--name").unwrap_or_else(|| "player".into());
    let url = match arg_value(&args, "--connect") {
        Some(url) => url,
        #[cfg(not(target_arch = "wasm32"))]
        None => {
            // Listen server: bind any free local port and connect to it.
            app.add_plugin(server::ServerPlugin {
                bind: ([127, 0, 0, 1], 0).into(),
            });
            app.resources.expect::<server::GameServer>().url()
        }
        #[cfg(target_arch = "wasm32")]
        None => format!("ws://127.0.0.1:{DEFAULT_PORT}"),
    };
    app.add_plugin(ClientPlugin { url, name });
}

/// What `space3d editor` (`src/bin/editor.rs`) opens: the client scene with four demo players
/// (no networking).
pub fn editor_scene(app: &mut App) {
    app.add_plugin(ClientScenePlugin);
    let players = stories::demo_players(4);
    app.add_system(Stage::Update, move |ctx| {
        client::apply_snapshot(ctx.world, ctx.resources, &players, Some(1));
    });
}

/// Configure a dedicated, windowless server listening on `port` (all interfaces).
#[cfg(not(target_arch = "wasm32"))]
pub fn build_server(app: &mut App, port: u16) {
    app.add_plugin(MinimalPlugins::new().with_log());
    app.add_plugin(server::ServerPlugin {
        bind: ([0, 0, 0, 0], port).into(),
    });
}
