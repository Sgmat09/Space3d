//! Bots: headless clients whose input comes from code instead of a player (feature `client`).
//! Used by harness tests, the load-test tool (`space3d_server::loadtest`) and server-side backfill
//! (`NetServer::add_virtual_client` + `server::inputs::brain`).

use crate::{NetInput, Tick};
use space3d_core::{App, World};

/// A client-side bot brain for input type `I`.
pub trait Bot<I: NetInput>: Send + Sync + 'static {
    /// Input for the predicted tick `tick`, looking at the client's replicated world.
    fn think(&mut self, world: &World, tick: Tick) -> I;
}

impl<I: NetInput, F: FnMut(&World, Tick) -> I + Send + Sync + 'static> Bot<I> for F {
    fn think(&mut self, world: &World, tick: Tick) -> I {
        self(world, tick)
    }
}

/// Install `bot` as the app's `InputSampler<I>` (replaces a player sampler).
pub fn install_bot<I: NetInput>(app: &mut App, mut bot: impl Bot<I>) {
    app.insert_resource(crate::InputSampler::<I>::new(move |ctx| {
        let tick = ctx
            .resources
            .get::<crate::client::NetClient>()
            .and_then(|n| n.predicted_tick())
            .unwrap_or_default();
        bot.think(ctx.world, tick)
    }));
}

/// Deterministic scripted inputs: `script(tick) -> I`.
pub fn scripted<I: NetInput>(script: impl Fn(Tick) -> I + Send + Sync + 'static) -> impl Bot<I> {
    move |_world: &World, tick: Tick| script(tick)
}
