//! The game client: sends input, renders the players from the server's snapshots.

use crate::protocol::{self, ARENA, ClientMsg, PlayerState, ServerMsg};
use space3d::prelude::*;

/// Networking + [`ClientScenePlugin`].
pub struct ClientPlugin {
    /// Server URL, e.g. `ws://127.0.0.1:7777`.
    pub url: String,
    /// Display name sent in `Hello`.
    pub name: String,
}

/// Rendering of the replicated players (no networking) — used on its own by stories and
/// screenshot tests.
pub struct ClientScenePlugin;

/// A player replicated from the server.
pub struct Replica {
    /// Server id.
    pub id: u64,
    /// Where the latest snapshot put it (rendered position eases towards it).
    pub target: Vec3,
}

/// Connection state (resource).
#[derive(Debug, Default)]
pub struct Session {
    /// Our player id once the server welcomed us.
    pub me: Option<u64>,
    /// Latest snapshot tick.
    pub last_tick: u64,
    /// Players in the latest snapshot.
    pub players: usize,
    /// Human-readable status.
    pub status: String,
}

struct Link {
    connection: ConnectionId,
    name: String,
    last_input: [f32; 2],
}

/// Shared mesh for all players.
struct PlayerMesh(MeshHandle);

/// Input action names.
pub mod actions {
    /// −x.
    pub const LEFT: &str = "left";
    /// +x.
    pub const RIGHT: &str = "right";
    /// −z.
    pub const FORWARD: &str = "forward";
    /// +z.
    pub const BACK: &str = "back";
    /// Quit.
    pub const QUIT: &str = "quit";
}

impl Plugin for ClientScenePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Session>();
        app.add_system(Stage::Startup, setup_scene);
        app.add_system(Stage::Update, ease_replicas);
    }
}

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(InputPlugin);
        app.add_plugin(NetPlugin::default());
        app.add_plugin(ClientScenePlugin);
        bind_controls(app.resources.expect_mut::<ActionMap>());
        let connection = app.resources.expect_mut::<WsClient>().open_with(
            self.url.clone(),
            WsConfig::default().reconnect(ReconnectPolicy::default()),
        );
        app.resources.expect_mut::<Session>().status = format!("connecting to {}", self.url);
        app.insert_resource(Link {
            connection,
            name: self.name.clone(),
            last_input: [f32::NAN; 2],
        });
        let mut reader = EventReader::default();
        app.add_system(Stage::Update, move |ctx| {
            let events: Vec<WsEvent> = reader
                .read(ctx.resources.expect::<Events<WsEvent>>())
                .cloned()
                .collect();
            for event in events {
                receive(ctx, event);
            }
        });
        app.add_system(Stage::FixedUpdate, send_input);
        app.add_system(Stage::Update, quit_on_escape);
    }
}

/// Keyboard + touch-edge bindings.
pub fn bind_controls(map: &mut ActionMap) {
    let region = |min: (f32, f32), max: (f32, f32)| Binding::TouchRegion {
        min: Vec2::new(min.0, min.1),
        max: Vec2::new(max.0, max.1),
    };
    map.bind(actions::LEFT, Binding::Key(KeyCode::ArrowLeft))
        .bind(actions::LEFT, Binding::Key(KeyCode::KeyA))
        .bind(actions::LEFT, region((0.0, 0.25), (0.25, 0.75)))
        .bind(actions::RIGHT, Binding::Key(KeyCode::ArrowRight))
        .bind(actions::RIGHT, Binding::Key(KeyCode::KeyD))
        .bind(actions::RIGHT, region((0.75, 0.25), (1.0, 0.75)))
        .bind(actions::FORWARD, Binding::Key(KeyCode::ArrowUp))
        .bind(actions::FORWARD, Binding::Key(KeyCode::KeyW))
        .bind(actions::FORWARD, region((0.25, 0.0), (0.75, 0.25)))
        .bind(actions::BACK, Binding::Key(KeyCode::ArrowDown))
        .bind(actions::BACK, Binding::Key(KeyCode::KeyS))
        .bind(actions::BACK, region((0.25, 0.75), (0.75, 1.0)))
        .bind(actions::QUIT, Binding::Key(KeyCode::Escape));
}

fn setup_scene(ctx: &mut SystemCtx) {
    let (cube, ground) = {
        let assets = ctx.resources.expect_mut::<RenderAssets>();
        (
            assets.add_mesh(MeshData::cube(0.8)),
            assets.add_mesh(MeshData::plane(2.0 * ARENA + 1.0)),
        )
    };
    ctx.resources.insert(PlayerMesh(cube));
    ctx.world.spawn((
        Transform::default(),
        Mesh3d(ground),
        Material::srgb(0.28, 0.32, 0.38).with_metallic_roughness(0.0, 0.9),
    ));
    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.3, -1.0, -0.4), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.4,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.7, 0.8, 1.0),
        intensity: 0.2,
    });
    ctx.world.spawn((
        Transform::from_xyz(0.0, 10.0, 10.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.04, 0.05, 0.08, 1.0)),
    ));
}

/// Make the world match a snapshot: spawn new players, move known ones, despawn leavers.
/// `me` is highlighted (emissive). Call after start-up (the player mesh must exist).
pub fn apply_snapshot(world: &mut World, resources: &Resources, players: &[PlayerState], me: Option<u64>) {
    let on_ground = |p: &PlayerState| Vec3::new(p.position[0], 0.4, p.position[1]);
    let mut known = Vec::new();
    let mut gone = Vec::new();
    for (entity, replica) in world.query_mut::<(Entity, &mut Replica)>() {
        match players.iter().find(|p| p.id == replica.id) {
            Some(p) => {
                replica.target = on_ground(p);
                known.push(p.id);
            }
            None => gone.push(entity),
        }
    }
    for entity in gone {
        let _ = world.despawn(entity);
    }
    let Some(mesh) = resources.get::<PlayerMesh>().map(|m| m.0) else {
        return;
    };
    for p in players.iter().filter(|p| !known.contains(&p.id)) {
        let [r, g, b] = protocol::color(p.id);
        let mut material = Material::color(Vec4::new(r, g, b, 1.0)).with_metallic_roughness(0.1, 0.4);
        if Some(p.id) == me {
            material = material.with_emissive(Vec3::new(r, g, b) * 0.35);
        }
        let target = on_ground(p);
        world.spawn((
            Name(p.name.clone()),
            Replica { id: p.id, target },
            Transform::from_xyz(target.x, target.y, target.z),
            Mesh3d(mesh),
            material,
        ));
    }
}

fn receive(ctx: &mut SystemCtx, event: WsEvent) {
    match event.kind {
        WsEventKind::Opened => {
            let link = ctx.resources.expect::<Link>();
            let hello = protocol::encode(&ClientMsg::Hello {
                name: link.name.clone(),
            });
            let _ = ctx.resources.expect::<WsClient>().send(link.connection, hello);
            ctx.resources.expect_mut::<Session>().status = "connected".into();
        }
        WsEventKind::Message(message) => match message.as_text().and_then(protocol::decode::<ServerMsg>) {
            Some(ServerMsg::Welcome { id }) => ctx.resources.expect_mut::<Session>().me = Some(id),
            Some(ServerMsg::Snapshot { tick, players }) => {
                let me = {
                    let session = ctx.resources.expect_mut::<Session>();
                    session.last_tick = tick;
                    session.players = players.len();
                    session.me
                };
                apply_snapshot(ctx.world, ctx.resources, &players, me);
            }
            None => {}
        },
        WsEventKind::Closed { reason, reconnecting } => {
            ctx.resources.expect_mut::<Session>().status = format!(
                "disconnected: {reason}{}",
                if reconnecting { " (retrying)" } else { "" }
            );
        }
    }
}

/// Send the input whenever it changes (and twice a second as a keep-alive).
fn send_input(ctx: &mut SystemCtx) {
    let tick = ctx.time().tick;
    let input = {
        let a = ctx.resources.expect::<ActionState>();
        [
            a.axis(actions::LEFT, actions::RIGHT),
            a.axis(actions::FORWARD, actions::BACK),
        ]
    };
    let link = ctx.resources.expect_mut::<Link>();
    if input == link.last_input && !tick.is_multiple_of(30) {
        return;
    }
    link.last_input = input;
    let (connection, msg) = (
        link.connection,
        protocol::encode(&ClientMsg::Input {
            x: input[0],
            z: input[1],
        }),
    );
    let _ = ctx.resources.expect::<WsClient>().send(connection, msg);
}

/// Ease rendered positions towards the latest snapshot (snapshots arrive at 20 Hz).
fn ease_replicas(ctx: &mut SystemCtx) {
    let k = (ctx.time().delta_secs() * 15.0).min(1.0);
    for (transform, replica) in ctx.world.query_mut::<(&mut Transform, &Replica)>() {
        transform.translation = transform.translation.lerp(replica.target, k);
    }
}

fn quit_on_escape(ctx: &mut SystemCtx) {
    if ctx.resources.expect::<ActionState>().just_pressed(actions::QUIT) {
        ctx.send(AppExit);
    }
}
