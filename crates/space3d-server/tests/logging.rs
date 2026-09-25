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
