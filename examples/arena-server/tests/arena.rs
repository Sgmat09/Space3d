//! The arena on the in-process `TestCluster`: matchmaking into a duel, bots playing over the
//! memory network, a decisive result flowing to the player service, the room closing.
#![cfg(not(target_arch = "wasm32"))]

use arena_server::ArenaGame;
use arena_server::bot::WanderBot;
use arena_shared::{ArenaInput, ArenaProtocol, Score};
use space3d_replication::bots::install_bot;
use space3d_replication::client::{ConnectionState, FailReason};
use space3d_replication::protocol::handshake::DisconnectCode;
use space3d_replication::{ControlledBy, NetEntity};
use space3d_server::config::QueueConfig;
use space3d_server::testing::{TestCluster, TestClusterConfig};
use std::collections::BTreeMap;

#[test]
fn duel_is_played_decided_reported_and_closed() {
    let mut config = TestClusterConfig::default();
    config.server.matchmaking.queues = vec![QueueConfig {
        name: "duel".into(),
        mode: "duel".into(),
        params: BTreeMap::from([("score_limit".into(), "3".into())]),
        ..Default::default()
    }];
    let mut c = TestCluster::new(ArenaGame, config);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let found = c.matchmake(&[&alice, &bob], "duel");
    let room = found[0].room_id;
    let clients: Vec<_> = found
        .iter()
        .enumerate()
        .map(|(i, a)| {
            c.connect(a, |app| {
                app.add_plugin(ArenaProtocol);
                install_bot::<ArenaInput>(app, WanderBot::new(i as u64));
            })
        })
        .collect();
    let playing = c.run_until(600, |c| {
        clients
            .iter()
            .all(|&h| c.client_net(h).is_connected() && c.client(h).world.query::<&NetEntity>().iter().count() >= 2)
    });
    assert!(playing, "both bots connected and see both pawns");
    c.step_ticks(120);

    // Server authority: alice reaches the kill limit.
    let alice_id = alice.profile.player_id;
    let world = &mut c.room_mut(room).expect("room").world;
    for (owner, score) in world.query_mut::<(&ControlledBy, &mut Score)>() {
        if owner.0 == alice_id {
            score.0 = 3;
        }
    }
    let board = c.api(&alice);
    let reported = c.run_until(600, |_| {
        board
            .leaderboard("arena.kills", 10)
            .block()
            .unwrap()
            .is_ok_and(|rows| rows.first().is_some_and(|r| r.player_id == alice_id && r.score == 3))
    });
    assert!(reported, "match result reached the leaderboard");
    let a = c.api(&alice).me().block().unwrap().unwrap();
    let b = c.api(&bob).me().block().unwrap().unwrap();
    assert!(a.rating > b.rating, "{} vs {}", a.rating, b.rating);

    // The room closes a few seconds after the match and tells the clients.
    let closed = c.run_until(600, |c| {
        c.room(room).is_none()
            && clients.iter().all(|&h| {
                matches!(
                    c.client_net(h).state(),
                    ConnectionState::Failed(FailReason::Disconnected(DisconnectCode::RoomClosed, _))
                )
            })
    });
    assert!(closed, "room closed after the match");
}
