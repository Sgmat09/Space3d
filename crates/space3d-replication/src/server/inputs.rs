//! Server input buffering: per slot, a ring of the last 64 ticks of decoded inputs.
//!
//! For tick T the driver uses `buffer[T]`; if missing: `last.repeat()` (counted as missing);
//! if the slot is disconnected: `NetInput::neutral()`. Inputs for ticks `< T` are late
//! (dropped, counted); ticks `> T + max_input_ahead` are rejected (strike). `input_slack` =
//! how many ticks before use the input for T arrived (EWMA), reported in every snapshot.

use crate::Tick;
use std::any::Any;

/// Type-erased decoded input (the registry's input driver knows the concrete `I`).
pub type ErasedInput = Box<dyn Any + Send + Sync>;

/// Per-slot input ring (internal to the replication builder).
#[derive(Default)]
pub struct InputBuffer {
    ring: Vec<Option<(Tick, ErasedInput, Tick /* arrival tick */)>>,
    last: Option<ErasedInput>,
    newest: Option<Tick>,
    slack_ewma: f32,
}

impl InputBuffer {
    /// Newest input tick received.
    pub fn newest(&self) -> Option<Tick> {
        self.newest
    }

    /// Smoothed slack in ticks.
    pub fn slack(&self) -> f32 {
        self.slack_ewma
    }
}

/// A server-side "virtual client" (bot / backfill): produces the input for each tick without
/// any network link. Build one with [`brain`].
pub trait VirtualBrain: Send + Sync + 'static {
    /// Input for `tick` (must be the registered input type).
    fn think(&mut self, world: &space3d_core::World, tick: Tick) -> ErasedInput;
}

struct FnBrain<F>(F);

impl<I, F> VirtualBrain for FnBrain<F>
where
    I: crate::NetInput,
    F: FnMut(&space3d_core::World, Tick) -> I + Send + Sync + 'static,
{
    fn think(&mut self, world: &space3d_core::World, tick: Tick) -> ErasedInput {
        Box::new((self.0)(world, tick))
    }
}

/// Wrap a typed closure as a [`VirtualBrain`].
pub fn brain<I: crate::NetInput>(
    f: impl FnMut(&space3d_core::World, Tick) -> I + Send + Sync + 'static,
) -> Box<dyn VirtualBrain> {
    Box::new(FnBrain(f))
}
