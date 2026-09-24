//! The authoritative game server (native only).

use crate::protocol::{self, ClientMsg, PlayerState, SNAPSHOT_EVERY, ServerMsg};
use space3d::prelude::*;
use std::collections::BTreeMap;
use std::net::SocketAddr;

/// Listens for players and simulates them on the fixed tick.
pub struct ServerPlugin {
    /// Address to listen on; port 0 picks a free one (see [`GameServer::url`]).
    pub bind: SocketAddr,
}

/// Server state (resource).
pub struct GameServer {
    handle: ServerHandle,
    players: BTreeMap<ClientId, ServerPlayer>,
}

struct ServerPlayer {
    name: String,
    position: [f32; 2],
    input: [f32; 2],
}

impl GameServer {
    /// `ws://` URL clients on this machine can connect to.
    pub fn url(&self) -> String {
        self.handle.ws_url("/")
    }

    /// Current state of every player, sorted by id.
    pub fn players(&self) -> Vec<PlayerState> {
        self.players
            .iter()
            .map(|(id, p)| PlayerState {
                id: id.0,
                name: p.name.clone(),
                position: p.position,
            })
            .collect()
    }
}

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(NetPlugin::default());
        let server = WsServer::new();
        let handle = server
            .listen(self.bind)
            .unwrap_or_else(|e| panic!("cannot listen on {}: {e}", self.bind));
        space3d::tracing::info!("server listening on {}", handle.ws_url("/"));
        // NetPlugin turns the inbox of a `WsServer` resource into `Events<ServerEvent>`.
        app.insert_resource(server);
        app.insert_resource(GameServer {
            handle,
            players: BTreeMap::new(),
        });
        let mut reader = EventReader::default();
        app.add_system(Stage::Update, move |ctx| {
            let events: Vec<ServerEvent> = reader
                .read(ctx.resources.expect::<Events<ServerEvent>>())
                .cloned()
                .collect();
            for event in events {
                handle_event(ctx, event);
            }
        });
        app.add_system(Stage::FixedUpdate, simulate);
    }
}

fn handle_event(ctx: &mut SystemCtx, event: ServerEvent) {
    let server = ctx.resources.expect::<WsServer>().clone();
    let game = ctx.resources.expect_mut::<GameServer>();
    match event {
        ServerEvent::Connected { client, addr } => {
            space3d::tracing::info!("player {} connected from {addr}", client.0);
            game.players.insert(
                client,
                ServerPlayer {
                    name: format!("player {}", client.0),
                    position: protocol::spawn_position(client.0),
                    input: [0.0, 0.0],
                },
            );
            let _ = server.send(client, protocol::encode(&ServerMsg::Welcome { id: client.0 }));
        }
        ServerEvent::Message { client, message } => {
            let (Some(player), Some(msg)) = (
                game.players.get_mut(&client),
                message.as_text().and_then(protocol::decode::<ClientMsg>),
            ) else {
                return;
            };
            match msg {
                ClientMsg::Hello { name } => player.name = name.chars().take(24).collect(),
                ClientMsg::Input { x, z } => player.input = [x, z],
            }
        }
        ServerEvent::Disconnected { client, reason } => {
            space3d::tracing::info!("player {} left ({reason})", client.0);
            game.players.remove(&client);
        }
    }
}

/// Move every player, then broadcast a snapshot every [`SNAPSHOT_EVERY`] ticks.
fn simulate(ctx: &mut SystemCtx) {
    let (dt, tick) = (ctx.time().fixed_delta.as_secs_f32(), ctx.time().tick);
    let game = ctx.resources.expect_mut::<GameServer>();
    for player in game.players.values_mut() {
        player.position = protocol::step(player.position, player.input, dt);
    }
    if tick.is_multiple_of(SNAPSHOT_EVERY) {
        let snapshot = protocol::encode(&ServerMsg::Snapshot {
            tick,
            players: game.players(),
        });
        ctx.resources.expect::<WsServer>().broadcast(snapshot);
    }
}
