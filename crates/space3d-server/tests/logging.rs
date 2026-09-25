//! Log context (its own test binary: `tracing` caches callsite interest process-wide, so a
//! scoped subscriber must not race tests on other threads that log without one).

mod common;

use common::{DuelProtocol, remote_count};
use space3d_replication::RoomId;
use space3d_replication::api::SessionTokens;
use space3d_replication::harness::ClientHandle;
use space3d_server::testing::{TestCluster, TestClusterConfig};

fn connected_pair(c: &mut TestCluster, a: &SessionTokens, b: &SessionTokens) -> (ClientHandle, ClientHandle, RoomId) {
    let found = c.matchmake(&[a, b], "duel");
    let ca = c.connect(&found[0], |app| {
        app.add_plugin(DuelProtocol);
    });
    let cb = c.connect(&found[1], |app| {
        app.add_plugin(DuelProtocol);
    });
    let ok = c.run_until(600, |c| {
        c.client_net(ca).is_connected()
            && c.client_net(cb).is_connected()
            && remote_count(c.client(ca)) == 2
            && remote_count(c.client(cb)) == 2
    });
    assert!(ok, "both players connected");
    (ca, cb, found[0].room_id)
}

/// §14.2 regression: room ticks run in a `room` span (room id, server id), so a room's log
/// lines — including warnings from the replication layer, which do not name the room — can be
/// tied to it. (The process-wide `server` span with `server_id` / `role` is entered by
/// `ServerMain` and the shard threads.)
#[test]
fn room_logs_carry_the_room_span() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = Buf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    let room = tracing::subscriber::with_default(subscriber, || {
        let mut c = TestCluster::new(common::DuelGame, TestClusterConfig::default());
        let alice = c.guest("alice");
        let bob = c.guest("bob");
        let (_ca, _cb, room) = connected_pair(&mut c, &alice, &bob);
        c.crash_room(room);
        assert!(c.run_until(60, |c| c.room_crashes() == 1));
        room
    });
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let line = out
        .lines()
        .find(|l| l.contains("room tick panicked"))
        .unwrap_or_else(|| panic!("crash logged:\n{out}"));
    assert!(line.contains(&format!("\"room_id\":\"{room}\"")), "{line}");
    assert!(line.contains("\"server_id\":\"gs-1\""), "{line}");
}

/// Regression: the shards `record`ed the tick on one long-lived `room` span per room. The text
/// ("pretty") formatter appends every recorded value to a span's stored fields, so the span —
/// and every line logged inside it — grew by ` tick=N` on every tick (about 1.5 KB/s per room,
/// in memory and in each log line). Each tick now runs in a fresh span.
#[test]
fn room_spans_on_shards_stay_one_tick_long_in_text_logs() {
    use space3d_server::config::{Role, ServerConfig};
    use space3d_server::game::RoomSpec;
    use space3d_server::gs::GameServerHandle;
    use space3d_server::gs::rooms::RoomCommand;
    use space3d_server::security::Secrets;
    use space3d_server::{BuildOptions, ServerMain};
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = Buf::default();
    let writer = buf.clone();
    // The `pretty` log format (`LogFormat::Pretty`, the default): the plain text formatter.
    let subscriber = tracing_subscriber::fmt()
        .with_target(true)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    let room = space3d_replication::RoomId(0x5eed);
    tracing::subscriber::with_default(subscriber, || {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut c = ServerConfig {
            role: Role::AllInOne,
            server_id: "log-1".into(),
            ..Default::default()
        };
        c.game.bind = format!("127.0.0.1:{port}").parse().unwrap();
        c.player_service.bind = c.game.bind;
        c.admin.bind = "127.0.0.1:0".parse().unwrap();
        c.game.public_url = format!("ws://127.0.0.1:{port}/play");
        c.game.shards = 1;
        c.player_service.db_path = dir.path().join("players.db");
        c.persistence.data_dir = dir.path().to_path_buf();
        let mut server = ServerMain::with_config(common::DuelGame, c)
            .options(BuildOptions {
                secrets: Some(Secrets::for_tests("fleet", &["admin"])),
                ..Default::default()
            })
            .build()
            .expect("server starts");
        let gs = server.resources.expect::<GameServerHandle>().clone();
        let handle = gs
            .rooms
            .create(
                RoomSpec {
                    room_id: room,
                    mode: "duel".into(),
                    max_players: 2,
                    ..Default::default()
                },
                None,
            )
            .expect("room created");
        // About 30 ticks on the shard thread, then close the room (logged inside its span).
        let pump = |server: &mut space3d_core::App, until: Instant| {
            while Instant::now() < until {
                server.update(Duration::from_millis(5));
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        pump(&mut server, Instant::now() + Duration::from_millis(500));
        handle
            .command(RoomCommand::Close {
                reason: "test over".into(),
            })
            .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end && gs.rooms.get(room).is_some() {
            pump(&mut server, Instant::now() + Duration::from_millis(20));
        }
    });
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let line = out
        .lines()
        .find(|l| l.contains("room closed") && l.contains("test over"))
        .unwrap_or_else(|| panic!("room close logged:\n{out}"));
    assert!(line.contains(&format!("room_id={room}")), "{line}");
    assert!(line.contains("server_id=log-1"), "{line}");
    let ticks = line.matches(" tick=").count() + line.matches("{tick=").count();
    assert_eq!(ticks, 1, "one tick field per line: {line}");
    let tick: u32 = line
        .split("tick=")
        .nth(1)
        .and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("tick in {line}"));
    assert!(tick >= 10, "the room ran on its shard for a while: {line}");
    assert!(line.len() < 400, "{} bytes: {line}", line.len());
}
