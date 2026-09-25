//! Player input: sampled on the client every predicted tick, sent redundantly with its tick,
//! buffered on the server and applied at exactly that tick.
//!
//! Inputs carry **state, not edges** (axes, absolute view angles, held-button bitmask, and a
//! wrapping counter for discrete actions), so redundancy and repetition are idempotent. Latch
//! per-frame edges (e.g. `just_pressed`) in the game's sampler.

use serde::Serialize;
use serde::de::DeserializeOwned;
use space3d_core::SystemCtx;

/// Why an input was rejected by [`NetInput::sanitize`] (counts as a strike).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid input: {0}")]
pub struct InvalidInput(pub &'static str);

/// The game's input type (exactly one per game, registered with `add_input`).
pub trait NetInput: Serialize + DeserializeOwned + Clone + Default + PartialEq + Send + Sync + 'static {
    /// Clamp/normalise in place (NaN, ranges, unknown bits). `Err` → strike, neutral input used.
    fn sanitize(&mut self) -> Result<(), InvalidInput> {
        Ok(())
    }

    /// Input assumed for a tick whose input never arrived (default: repeat the last one).
    fn repeat(&self) -> Self {
        self.clone()
    }

    /// Input used while the controlling player is disconnected (default: `Default`).
    fn neutral() -> Self {
        Self::default()
    }
}

/// The input of the current tick, written onto every `ControlledBy` entity by the server
/// driver and onto `Controlled` entities by the client driver before `SimSchedule` runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerInput<I>(pub I);

type SamplerFn<I> = Box<dyn FnMut(&mut SystemCtx) -> I + Send>;

/// Client resource: produces the input for the next predicted tick (called once per predicted
/// tick from `FixedUpdate`; reads the game's action state / latched edges).
pub struct InputSampler<I> {
    f: parking_lot::Mutex<SamplerFn<I>>,
}

impl<I: NetInput> InputSampler<I> {
    /// Wrap a sampling closure.
    pub fn new(f: impl FnMut(&mut SystemCtx) -> I + Send + 'static) -> Self {
        Self {
            f: parking_lot::Mutex::new(Box::new(f)),
        }
    }

    /// Sample one input.
    pub fn sample(&mut self, ctx: &mut SystemCtx) -> I {
        (self.f.get_mut())(ctx)
    }
}
