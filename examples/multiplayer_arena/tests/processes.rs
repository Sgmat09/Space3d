//! The shipped binaries as separate processes over real localhost sockets and real time (this
//! is what a developer or an orchestrator runs); every wait has a deadline.
//!
//! * All-in-one: the `server` (player service + game server) and two `client --headless --bot`
//!   processes that log in as guests, get join tickets through quick play and play in the same
//!   room; SIGTERM drains the server (exit code 0).
//! * Split drain handoff: a player service and two game servers (`server --role ...`), a
//!   persistent arena queue and two bots; SIGTERM the game server hosting their room mid-play:
//!   the room moves to the other game server and both bots play on there.
//!
//! Every process's whole output (tracing goes to stdout) is captured and must hold no ERROR
//! line and no panic.
#![cfg(all(unix, feature = "client"))]

mod common;

use common::{Proc, Server, bot, free_port, http_get, server_cmd, wait_ready};
use std::time::{Duration, Instant};

#[test]
fn server_and_two_headless_bot_clients_over_localhost() {
    let mut server = Server::start(&[]);
    let ps = format!("http://127.0.0.1:{}", server.port);
    let mut bots = [bot("bot-1", &ps, 4), bot("bot-2", &ps, 4)];
    for bot in &mut bots {
        let status = bot.wait(Duration::from_secs(20)).expect("client exits on its own");
        let out = bot.output();
        assert!(status.success(), "client failed ({status}):\n{out}");
        assert!(out.contains("state=Connected"), "{out}");
        assert!(out.contains("cubes=2"), "both bots see both cubes: {out}");
        bot.assert_no_errors();
    }

    // Graceful shutdown: SIGTERM → drain (3 s in the dev config) → exit 0.
    let status = server.stop();
    assert!(status.success(), "server exit {status}:\n{}", server.proc.output());
    server.proc.assert_no_errors();
}

/// Regression (test harness): the process tests piped stderr only and looked for "ERROR" there,
/// but every tracing line — ERROR included — goes to stdout, so a process logging errors
/// passed. The harness now captures both streams; this checks it sees an error the server
/// only reports through tracing (stderr gets a plain `startup failed: ...`).
#[test]
fn errors_a_process_logs_are_seen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"").unwrap();
    // Its database would live under a regular file: startup fails.
    let mut cmd = server_cmd(&file.join("data"));
    cmd.env("SPACE3D__GAME__BIND", format!("127.0.0.1:{}", free_port()))
        .env("SPACE3D__PLAYER_SERVICE__BIND", format!("127.0.0.1:{}", free_port()))
        .env("SPACE3D__ADMIN__BIND", format!("127.0.0.1:{}", free_port()));
    let mut server = Proc::spawn("server", cmd);
    let status = server.wait(Duration::from_secs(20)).expect("exits");
    assert!(!status.success());
    let errors = server.errors();
    assert!(
        errors.iter().any(|l| l.contains("startup failed")),
        "the ERROR line (stdout) is seen: {errors:?}\n{}",
        server.output()
    );
}

/// One game server of the split deployment (`server --role game-server`).
struct GameServer {
    proc: Proc,
    admin: u16,
}

/// Rooms a game server reports on its admin API: `(room id, players)`.
fn rooms(admin: u16) -> Vec<(String, u64)> {
    let Some((true, body)) = http_get(admin, "/admin/v1/rooms") else {
        return Vec::new();
    };
    let rooms: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap_or_default();
    rooms
        .iter()
        .filter_map(|r| Some((r["room_id"].as_str()?.to_owned(), r["players"].as_u64()?)))
        .collect()
}

fn wait_for<T>(what: &str, limit: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Regression (drain handoff over real processes, 3 of 4 runs failed): the draining game server
/// froze the room and disconnected its clients before the player service knew the room was
/// moving, so their rejoin got a ticket straight back to the draining server
/// (`Reject(ServerDraining)`), and the client burnt its rejoin budget in ~170 ms. Now the room
/// keeps its clients until the player service acknowledged the move, the player service holds
/// their rejoins until the room is stored, and the client honours retry hints with backoff.
/// Also checks that a drained game server leaves the fleet with a clean close (the player
/// service used to warn about a connection reset).
#[test]
fn a_draining_game_server_hands_its_persistent_room_to_another_over_real_processes() {
    let dir = tempfile::tempdir().unwrap();
    let queues = r#"[{ name = "arena", mode = "arena", teams = 1, team_size = 16, min_players = 1, max_wait = "5s", backfill = true, params = { persistent = "true" } }]"#;
    let fleet_token = "process-test-fleet-token-0123456789";
    let (ps_port, ps_admin) = (free_port(), free_port());
    let mut cmd = server_cmd(&dir.path().join("ps"));
    cmd.args(["--role", "player-service"])
        .env("SPACE3D__PLAYER_SERVICE__BIND", format!("127.0.0.1:{ps_port}"))
        .env("SPACE3D__ADMIN__BIND", format!("127.0.0.1:{ps_admin}"))
        .env("SPACE3D__MATCHMAKING__QUEUES", queues)
        .env("SPACE3D__LOG__FILTER", "info")
        .env("SPACE3D_FLEET_TOKEN", fleet_token);
    let mut ps = Proc::spawn("player-service", cmd);
    wait_ready(ps_admin, "player service");
    let mut game_servers: Vec<GameServer> = ["gs-1", "gs-2"]
        .into_iter()
        .map(|id| {
            let (port, admin) = (free_port(), free_port());
            let mut cmd = server_cmd(&dir.path().join(id));
            cmd.args(["--role", "game-server"])
                .env("SPACE3D__SERVER_ID", id)
                .env("SPACE3D__GAME__BIND", format!("127.0.0.1:{port}"))
                .env("SPACE3D__GAME__PUBLIC_URL", format!("ws://127.0.0.1:{port}/play"))
                .env("SPACE3D__ADMIN__BIND", format!("127.0.0.1:{admin}"))
                .env("SPACE3D__MATCHMAKING__QUEUES", queues)
                .env(
                    "SPACE3D__FLEET__PLAYER_SERVICE_URL",
                    format!("ws://127.0.0.1:{ps_port}/internal/v1/fleet"),
                )
                .env(
                    "SPACE3D__FLEET__PLAYER_SERVICE_HTTP",
                    format!("http://127.0.0.1:{ps_port}"),
                )
                .env("SPACE3D__LOG__FILTER", "info")
                .env("SPACE3D_FLEET_TOKEN", fleet_token);
            GameServer {
                proc: Proc::spawn(id, cmd),
                admin,
            }
        })
        .collect();
    for gs in &game_servers {
        wait_ready(gs.admin, "game server");
    }

    let ps_url = format!("http://127.0.0.1:{ps_port}");
    let mut bots = [bot("bot-1", &ps_url, 10), bot("bot-2", &ps_url, 10)];
    let (host, room) = wait_for("both bots in one room", Duration::from_secs(15), || {
        game_servers.iter().enumerate().find_map(|(i, gs)| {
            rooms(gs.admin)
                .into_iter()
                .find(|(_, players)| *players == 2)
                .map(|(room, _)| (i, room))
        })
    });
    // Let them play a moment, then drain the hosting game server (SIGTERM).
    std::thread::sleep(Duration::from_millis(500));
    let other = 1 - host;
    game_servers[host].proc.term();
    let status = game_servers[host]
        .proc
        .wait(Duration::from_secs(20))
        .expect("the drained game server exits");
    assert!(
        status.success(),
        "drained game server exit {status}:\n{}",
        game_servers[host].proc.output()
    );
    // The room comes back on the other game server with both players in it.
    wait_for("the room restored with both bots", Duration::from_secs(10), || {
        rooms(game_servers[other].admin)
            .contains(&(room.clone(), 2))
            .then_some(())
    });
    for bot in &mut bots {
        let status = bot.wait(Duration::from_secs(30)).expect("client exits on its own");
        let out = bot.output();
        assert!(status.success(), "client failed ({status}):\n{out}");
        assert!(out.contains("state=Connected"), "{out}");
        assert!(out.contains("cubes=2"), "both bots see both cubes: {out}");
        bot.assert_no_errors();
    }
    game_servers[other].proc.term();
    let status = game_servers[other]
        .proc
        .wait(Duration::from_secs(20))
        .expect("game server exits");
    assert!(status.success(), "{}", game_servers[other].proc.output());
    ps.term();
    let status = ps.wait(Duration::from_secs(20)).expect("player service exits");
    assert!(status.success(), "{}", ps.output());

    for p in game_servers.iter().map(|g| &g.proc).chain([&ps]) {
        p.assert_no_errors();
    }
    let ps_out = ps.output();
    assert!(
        !ps_out.contains("reset without closing handshake")
            && !ps.warnings().iter().any(|l| l.contains("fleet link closed")),
        "drained game servers leave the fleet with a clean close:\n{ps_out}"
    );
    assert!(ps_out.contains("fleet link closed (server left the fleet)"), "{ps_out}");
    let host_out = game_servers[host].proc.output();
    assert!(
        !host_out.contains("did not acknowledge the move"),
        "the player service acknowledged the move:\n{host_out}"
    );
}
