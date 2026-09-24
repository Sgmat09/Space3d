//! Client-side prediction and reconciliation.
//!
//! Each predicted tick records, for every `Predicted` entity, the encoded bytes of its
//! `Predict*` components after the sim ran ([`PredictionHistory`]). When a snapshot for tick S
//! confirms different bytes (or `ComponentRule::rollback_when` says so), `rollback_from = S`:
//! in the next `client_tick` the client writes the confirmed values, then replays
//! `S+1 ..= predicted_tick` with the stored inputs (`SimTime::resimulating = true`), rewriting
//! history, and emits `Mispredicted`. The visual jump (old − new render value) goes into
//! [`PredictionError`] and decays with `smoothing_half_life` (snaps beyond `snap_distance`).
//! Replays longer than `max_rollback_ticks`, or S older than the history, snap instead.
//!
//! Physics bodies are not rolled back in v1: predicted characters use kinematic controllers in
//! shared sim code; replicated dynamic bodies are interpolated.

use crate::Tick;
use space3d_core::glam::{Quat, Vec3};

/// Per-entity ring of `(tick, encoded predicted components)` (internal layout is the builder's).
#[derive(Clone, Debug, Default)]
pub struct PredictionHistory {
    /// `(tick, kind, bytes)` for the last `max_rollback_ticks` ticks.
    pub entries: Vec<(Tick, u16, Vec<u8>)>,
}

/// Decaying visual correction of a predicted entity (render-only; never simulated).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PredictionError {
    /// Translation offset added to the rendered transform.
    pub translation: Vec3,
    /// Rotation offset.
    pub rotation: Quat,
}

/// Inputs of past predicted ticks (erased; postcard bytes + value) for replay and redundancy.
#[derive(Default)]
pub struct InputHistory {
    /// `(tick, encoded input)`.
    pub entries: std::collections::VecDeque<(Tick, Vec<u8>)>,
}
