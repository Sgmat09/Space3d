//! The multiplayer_arena protocol through the deterministic in-process harness (one room, real
//! client plugins, memory network, manual clock): WASD through the `ActionMap` drives the cube
//! on the server, the owner predicts it without mispredictions, a second client interpolates
//! it, and wandering bots converge under mobile-like conditions.
#![cfg(feature = "client")]

use multiplayer_arena::{CubeColor, CubesProtocol, CubesRules, SPEED, Wander, controls};
use space3d_core::prelude::*;
use space3d_platform::{Input, InputPlugin, KeyCode};
use space3d_replication::bots::install_bot;
use space3d_replication::client::Mispredicted;
use space3d_replication::harness::{ClientHandle, ClientSpec, HarnessConfig, NetHarness};
use space3d_replication::transport::conditioner::{LinkConditions, LinkProfile};
use space3d_replication::{Controlled, NetEntity};
use std::time::Duration;

fn room(seed: u64, link: LinkProfile) -> NetHarness {
    NetHarness::new(
        HarnessConfig {
            seed,
            link,
            ..Default::default()
        },
        |app| {
            app.add_plugin(CubesProtocol).add_plugin(CubesRules);
        },
    )
}

fn own_cube(app: &App) -> Option<Entity> {
    app.world
        .query::<(Entity, &Controlled, &NetEntity)>()
        .iter()
        .map(|(e, _, _)| e)
        .next()
}

fn cubes(app: &App) -> usize {
    app.world.query::<(&CubeColor, &NetEntity)>().iter().count()
}

fn server_cube(h: &NetHarness, c: ClientHandle) -> Entity {
    multiplayer_arena::cube_of(&h.server().world, h.player(c)).expect("cube spawned")
}

#[test]
fn wasd_through_the_action_map_moves_the_cube_everywhere() {
    let mut h = room(
        5,
        LinkProfile::symmetric(LinkConditions {
            latency: Duration::from_millis(40),
            jitter: Duration::from_millis(3),
            ..LinkConditions::perfect()
        }),
    );
    let player = h.add_client(ClientSpec::named("alice"), |app| {
        app.add_plugin(CubesProtocol).add_plugin(InputPlugin);
        controls::install(app);
    });
    let viewer = h.add_client(ClientSpec::named("bob"), |app| {
        app.add_plugin(CubesProtocol);
    });
    assert!(h.run_until(600, |h| {
        [player, viewer]
            .iter()
            .all(|c| h.client_net(*c).is_connected() && cubes(h.client(*c)) == 2)
    }));
    h.step_ticks(60);
    let cube = server_cube(&h, player);
    let start = h.server().world.get::<&Transform>(cube).unwrap().translation;
    let mut mis = EventReader::default();
    let _ = mis
        .read(h.client(player).resources.expect::<Events<Mispredicted>>())
        .count();

    // Hold W for one second: forward is −Z.
    h.client_mut(player)
        .resources
        .expect_mut::<Input>()
        .inject_key_down(KeyCode::KeyW);
    h.step_ticks(60);
    h.client_mut(player)
        .resources
        .expect_mut::<Input>()
        .inject_key_up(KeyCode::KeyW);
    h.step_ticks(90);
    let end = h.server().world.get::<&Transform>(cube).unwrap().translation;
    let moved = start.z - end.z;
    assert!(
        (moved - SPEED).abs() < SPEED * 0.1,
        "one second of W moves ~{SPEED} m forward, moved {moved}"
    );
    assert!((end.x - start.x).abs() < 1e-3, "no sideways drift");
    let replays = mis
        .read(h.client(player).resources.expect::<Events<Mispredicted>>())
        .count();
    assert_eq!(replays, 0, "deterministic shared sim: nothing to correct");

    // The owner's predicted cube and the viewer's interpolated copy both end where the server is.
    let own = own_cube(h.client(player)).unwrap();
    let p = h.client(player).world.get::<&Transform>(own).unwrap().translation;
    let remote = h.client_entity(viewer, cube).unwrap();
    let v = h.client(viewer).world.get::<&Transform>(remote).unwrap().translation;
    assert!((p - end).length() < 1e-3, "predicted {p} vs server {end}");
    assert!((v - end).length() < 1e-3, "interpolated {v} vs server {end}");
    for c in [player, viewer] {
        h.assert_converged(c).unwrap();
    }
}

#[test]
fn wandering_bots_converge_on_a_mobile_link() {
    let mut h = room(9, LinkProfile::symmetric(LinkConditions::mobile_4g()));
    let bots: Vec<ClientHandle> = (0..4)
        .map(|i| {
            let name = format!("bot-{i}");
            h.add_client(ClientSpec::named(&name), |app| {
                app.add_plugin(CubesProtocol);
                install_bot(app, Wander::new(Wander::seed_of(&name)));
            })
        })
        .collect();
    assert!(h.run_until(900, |h| {
        bots.iter()
            .all(|c| h.client_net(*c).is_connected() && cubes(h.client(*c)) == 4)
    }));
    h.step_ticks(600);
    // Converged means: same entity set, byte-identical components as of each client's latest
    // snapshot (predicted components compared at the last confirmed tick).
    for &c in &bots {
        assert!(
            h.run_until(120, |h| h.assert_converged(c).is_ok()),
            "{:?}",
            h.assert_converged(c)
        );
    }
    // The bots actually moved.
    let spread: f32 = bots
        .iter()
        .map(|c| {
            let t = h
                .server()
                .world
                .get::<&Transform>(server_cube(&h, *c))
                .unwrap()
                .translation;
            (t - multiplayer_arena::spawn_point(0)).length()
        })
        .sum();
    assert!(spread > 1.0, "bots wander");
}
