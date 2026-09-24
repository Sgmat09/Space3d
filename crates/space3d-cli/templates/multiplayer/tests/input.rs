//! End-to-end: a real WebSocket server and client in one headless app. Pressing a key on the
//! client moves the player on the *server*, and the snapshot moves the client's replica.
//!
//! Networking runs on background threads, so these tests step the app in a loop with short
//! sleeps (the engine itself never blocks on the network).

use {{crate_name}}::client::{ClientPlugin, Replica, Session};
use {{crate_name}}::protocol;
use {{crate_name}}::server::{GameServer, ServerPlugin};
use space3d::prelude::*;
use space3d::test::prelude::*;
use std::time::{Duration, Instant};

fn step_until(app: &mut TestApp, what: &str, mut done: impl FnMut(&TestApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.step();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn connected_app() -> TestApp {
    let mut app = TestApp::no_render();
    app.add_plugin(ServerPlugin {
        bind: ([127, 0, 0, 1], 0).into(),
    });
    let url = app.resource::<GameServer>().url();
    app.add_plugin(ClientPlugin {
        url,
        name: "tester".into(),
    });
    step_until(&mut app, "the welcome + first snapshot", |app| {
        app.resource::<Session>().me.is_some() && app.count::<Replica>() == 1
    });
    app
}

#[test]
fn joining_spawns_a_replica_with_our_name() {
    let mut app = connected_app();
    step_until(&mut app, "the Hello to arrive", |app| {
        app.resource::<GameServer>()
            .players()
            .first()
            .is_some_and(|p| p.name == "tester")
    });
    let me = app.resource::<Session>().me.unwrap();
    let state = &app.resource::<GameServer>().players()[0];
    assert_eq!(state.id, me);
    assert_eq!(state.position, protocol::spawn_position(me));
}

#[test]
fn input_moves_the_player_on_the_server_and_the_replica_follows() {
    let mut app = connected_app();
    let start = app.resource::<GameServer>().players()[0].position;
    app.press_key(KeyCode::ArrowRight);
    step_until(&mut app, "the server to move us", |app| {
        app.resource::<GameServer>().players()[0].position[0] > start[0] + 1.0
    });
    app.release_key(KeyCode::ArrowRight);
    let server_x = app.resource::<GameServer>().players()[0].position[0];
    step_until(&mut app, "the replica to catch up", |app| {
        let replica = app.entities_with::<Replica>()[0];
        app.translation(replica).x > server_x - 0.2
    });
}

#[test]
fn leaving_removes_the_player() {
    let mut app = connected_app();
    let link: Vec<_> = app.resource::<WsClient>().iter().map(|c| c.id()).collect();
    for id in link {
        app.resource::<WsClient>().close(id);
    }
    step_until(&mut app, "the server to notice", |app| {
        app.resource::<GameServer>().players().is_empty()
    });
}
