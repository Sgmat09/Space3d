//! The storybook: the client scene with fake snapshots (no server needed). Browse with
//! `space3d stories`; every story is also a golden screenshot test.

use crate::client::{ClientScenePlugin, apply_snapshot};
use crate::protocol::{PlayerState, spawn_position};
use space3d::prelude::*;

/// `n` players at their spawn points.
pub fn demo_players(n: u64) -> Vec<PlayerState> {
    (1..=n)
        .map(|id| PlayerState {
            id,
            name: format!("player {id}"),
            position: spawn_position(id),
        })
        .collect()
}

/// All stories of the game.
pub fn storybook() -> Storybook {
    let mut book = Storybook::new();
    book.add(
        Story::new("Lobby/Players", |app, knobs| {
            app.add_plugin(ClientScenePlugin);
            let players = demo_players(knobs.i64("players").max(0) as u64);
            app.add_system(Stage::Update, move |ctx| {
                apply_snapshot(ctx.world, ctx.resources, &players, Some(1));
            });
        })
        .description("Replicated players; player 1 is 'me' (highlighted)")
        .knob_i64("players", 4, 0..=12)
        .ticks(2)
        .size(320, 240),
    );
    book.add(
        Story::new("Lobby/Empty", |app, _| {
            app.add_plugin(ClientScenePlugin);
        })
        .description("The arena before anybody joined")
        .size(320, 240),
    );
    book
}
