//! Separate-process deployment over real sockets: a `player-service` process and
//! `game-server` processes (here: separate supervisor `App`s with their own listeners,
//! databases and data directories, talking only over the network) — fleet WebSocket with the
//! shared fleet token, checkpoints uploaded to the player service over HTTP, lease expiry after
//! a game-server loss, rejoin and restore on another game server.

mod common;

use common::{DuelProtocol, GAME_ID, SetHealth, healths, remote_count};
use space3d_core::App;
use space3d_net::{Http, HttpRequest};
use space3d_replication::api::{QuickPlayRequest, SessionTokens};
use space3d_replication::client::{ClientConfig, ClientReplicationPlugin, ConnectTarget, NetClient};
use space3d_replication::services::{DeviceCredentials, PlayerServiceClient};
use space3d_server::config::{Role, ServerConfig};
use space3d_server::gs::GameServerHandle;
use space3d_server::health::HealthState;
use space3d_server::ps::PlayerServiceHandle;
use space3d_server::ps::fleet::ServerState;
use space3d_server::security::Secrets;
use space3d_server::{AdminHandle, BuildOptions, ServerMain};
use std::time::{Duration, Instant};

const ADMIN: &str = "split-admin";

/// What every process reads from `SPACE3D_KEYS` / `SPACE3D_FLEET_TOKEN` /
/// `SPACE3D_ADMIN_TOKENS`: one signing key ring (tickets are signed by the player service and
/// verified offline by game servers), the fleet token and an admin token.
fn secrets() -> Secrets {
    static KEYS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let spec = KEYS.get_or_init(|| space3d_server::security::token::generate_key_spec("k1"));
    let config = ServerConfig::default();
    let env = |k: &str| match k {
        "SPACE3D_KEYS" => Some(spec.clone()),
        "SPACE3D_FLEET_TOKEN" => Some("split-fleet-token-0123456789abcdef".to_owned()),
        "SPACE3D_ADMIN_TOKENS" => Some(ADMIN.to_owned()),
        _ => None,
    };
    Secrets::load(&config, &env).expect("secrets")
}

fn base(dir: &std::path::Path, role: Role, id: &str) -> ServerConfig {
    let mut c = ServerConfig {
        role,
        server_id: id.into(),
        ..Default::default()
    };
    c.admin.bind = "127.0.0.1:0".parse().unwrap();
    c.fleet.heartbeat = Duration::from_millis(300);
    c.fleet.lease_ttl = Duration::from_secs(2);
    c.persistence.data_dir = dir.to_path_buf();
    c.persistence.checkpoint_interval = Duration::from_secs(1);
    c.player_service.http_rps_per_ip = 10_000;
    c.player_service.login_attempts_per_min = 10_000;
    c
}

fn player_service(dir: &std::path::Path) -> App {
    let mut c = base(dir, Role::PlayerService, "players");
    c.player_service.bind = "127.0.0.1:0".parse().unwrap();
    c.player_service.db_path = dir.join("players.db");
    ServerMain::with_config(common::DuelGame, c)
        .options(BuildOptions {
            secrets: Some(secrets()),
            ..Default::default()
        })
        .build()
        .expect("player service starts")
}

fn game_server(dir: &std::path::Path, id: &str, ps_url: &str) -> App {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut c = base(dir, Role::GameServer, id);
    c.game.bind = format!("127.0.0.1:{port}").parse().unwrap();
    c.game.public_url = format!("ws://127.0.0.1:{port}/play");
    c.game.shards = 1;
    c.fleet.player_service_url = format!("{}/internal/v1/fleet", ps_url.replacen("http://", "ws://", 1));
    c.fleet.player_service_http = ps_url.to_owned();
    ServerMain::with_config(common::DuelGame, c)
        .options(BuildOptions {
            secrets: Some(secrets()),
            ..Default::default()
        })
        .build()
        .expect("game server starts")
}

struct World {
    ps: App,
    gss: Vec<App>,
    clients: Vec<App>,
    last: Instant,
}

impl World {
    fn pump_until(&mut self, deadline: Duration, mut pred: impl FnMut(&Self) -> bool) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            let now = Instant::now();
            let dt = now - self.last;
            self.last = now;
            self.ps.update(dt);
            for gs in &mut self.gss {
                gs.update(dt);
            }
            for c in &mut self.clients {
                c.update(dt);
            }
            if pred(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        false
    }

    fn net(&self, i: usize) -> &NetClient {
        self.clients[i].resources.expect()
    }
}

fn registered(app: &App) -> bool {
    app.resources.expect::<HealthState>().is_registered()
}

fn connect(app: &mut App, a: &space3d_replication::api::Assignment) {
    app.resources.expect_mut::<NetClient>().connect(ConnectTarget::Ticket {
        url: a.server_url.clone(),
        ticket: a.join_ticket.clone(),
    });
}

fn client() -> App {
    let mut app = App::new().with_tickrate(60.0);
    app.add_plugin(ClientReplicationPlugin {
        config: ClientConfig {
            game_id: GAME_ID.into(),
            ..Default::default()
        },
    });
    app.add_plugin(DuelProtocol);
    app
}

#[test]
fn split_processes_register_play_checkpoint_lose_a_server_and_restore_elsewhere() {
    let ps_dir = tempfile::tempdir().unwrap();
    let gs1_dir = tempfile::tempdir().unwrap();
    let gs2_dir = tempfile::tempdir().unwrap();
    let ps = player_service(ps_dir.path());
    let handle = ps.resources.expect::<PlayerServiceHandle>().clone();
    let ps_url = handle.url();
    let gs1 = game_server(gs1_dir.path(), "gs-1", &ps_url);
    let mut w = World {
        ps,
        gss: vec![gs1],
        clients: Vec::new(),
        last: Instant::now(),
    };
    assert!(
        w.pump_until(Duration::from_secs(10), |w| registered(&w.gss[0])),
        "gs-1 registered over the fleet WebSocket"
    );

    // Two players drop into the same room on gs-1.
    let http = Http::new();
    let mut sessions: Vec<SessionTokens> = Vec::new();
    let mut room = None;
    for name in ["ann", "ben"] {
        let api = PlayerServiceClient::new(&http, &ps_url);
        sessions.push(
            api.login_guest(&DeviceCredentials::generate(), Some(name))
                .block()
                .unwrap()
                .unwrap(),
        );
        let a = api
            .quick_play(&QuickPlayRequest {
                mode: "ffa".into(),
                ..Default::default()
            })
            .block()
            .unwrap()
            .unwrap();
        assert_eq!(a.server_id, "gs-1");
        assert_eq!(*room.get_or_insert(a.room_id), a.room_id);
        let mut app = client();
        connect(&mut app, &a);
        w.clients.push(app);
    }
    let room = room.unwrap();
    let playing = w.pump_until(Duration::from_secs(10), |w| {
        (0..2).all(|i| w.net(i).is_connected() && remote_count(&w.clients[i]) == 2)
    });
    assert!(
        playing,
        "both clients play on gs-1: {:?} {:?} / {} {}",
        w.net(0).state(),
        w.net(1).state(),
        remote_count(&w.clients[0]),
        remote_count(&w.clients[1])
    );
    w.clients[0]
        .resources
        .expect_mut::<NetClient>()
        .send(&SetHealth(55))
        .unwrap();
    assert!(w.pump_until(Duration::from_secs(5), |w| healths(&w.clients[1]) == vec![55, 100]));

    // Force a checkpoint through gs-1's admin API; it lands in the player service over HTTP.
    let admin = w.gss[0].resources.expect::<AdminHandle>().addr;
    let res = http
        .request(
            HttpRequest::post(format!("http://{admin}/admin/v1/rooms/{room}/checkpoint"))
                .header("authorization", format!("Bearer {ADMIN}")),
        )
        .block()
        .unwrap()
        .unwrap();
    assert_eq!(res.status, 202, "{}", res.text());
    let store = handle.service.store.clone();
    let uploaded = w.pump_until(Duration::from_secs(10), |_| {
        store.get_checkpoint(room).block().unwrap().unwrap().is_some()
    });
    assert!(uploaded, "checkpoint uploaded to the player service");

    // Lose gs-1 (process gone), bring up gs-2, wait for the lease to expire.
    drop(w.gss.remove(0));
    w.gss.push(game_server(gs2_dir.path(), "gs-2", &ps_url));
    let fleet_ok = w.pump_until(Duration::from_secs(15), |w| {
        let fleet = handle.service.fleet.lock();
        registered(&w.gss[0]) && fleet.server("gs-1").is_none_or(|s| s.state == ServerState::Dead)
    });
    assert!(fleet_ok, "gs-1 lease expired and gs-2 registered");

    // Rejoin: the orphaned room is restored on gs-2 from the uploaded checkpoint.
    for (i, s) in sessions.iter().enumerate() {
        let api = PlayerServiceClient::new(&http, &ps_url);
        api.set_session(Some(s.clone()));
        let a = api.rejoin().block().unwrap().unwrap().expect("room to rejoin");
        assert_eq!((a.room_id, a.server_id.as_str()), (room, "gs-2"));
        connect(&mut w.clients[i], &a);
    }
    let back = w.pump_until(Duration::from_secs(10), |w| (0..2).all(|i| w.net(i).is_connected()));
    assert!(back, "both players reconnected to gs-2");
    let gs2 = w.gss[0].resources.expect::<GameServerHandle>().clone();
    let status = gs2.rooms.get(room).expect("room restored on gs-2").status();
    assert_eq!(status.room_id, room);
    // Server-side restore: gs-2's admin API reports the restored pawns (not duplicates).
    let admin2 = w.gss[0].resources.expect::<AdminHandle>().addr;
    let mut detail = String::new();
    let restored = w.pump_until(Duration::from_secs(5), |_| {
        let res = Http::new()
            .request(
                HttpRequest::get(format!("http://{admin2}/admin/v1/rooms/{room}"))
                    .header("authorization", format!("Bearer {ADMIN}")),
            )
            .block()
            .unwrap()
            .unwrap();
        detail = res.text();
        res.status == 200 && detail.contains("\"entities\":2") && detail.matches("connected").count() >= 2
    });
    assert!(restored, "restored room detail: {detail}");
}
