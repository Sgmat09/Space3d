//! INPUT block (C→S, unreliable lane, redundant):
//!
//! ```text
//! InputWindow := varu32 newest_tick · u8 run_count (1..=max_input_runs) · Run* (newest → oldest)
//!                · varu32 view_delay_q8        // (newest_tick − client render tick) × 256, for lag compensation
//! Run         := varu16 repeat (≥1) · varu32 len (≤ max_input_bytes) · postcard(I)
//! ```
//!
//! Consecutive identical inputs collapse into one run, so a held stick costs one payload per
//! packet. On stream transports the window holds just the newest input (the stream is
//! reliable); on datagram transports it holds every input the server has not acknowledged
//! (`SnapshotHeader::input_ack`), capped by `ClientConfig::input_redundancy`.

use super::WireLimits;
use crate::Tick;
use crate::wire::{DecodeError, Reader, Writer};
use smallvec::SmallVec;

/// One run of identical encoded inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputRun<'a> {
    /// How many consecutive ticks use this input.
    pub repeat: u16,
    /// postcard bytes of the input.
    pub payload: &'a [u8],
}

/// A decoded INPUT block (payloads borrowed from the packet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputWindow<'a> {
    /// Tick of the first input of the first run.
    pub newest_tick: Tick,
    /// Runs, newest first.
    pub runs: SmallVec<[InputRun<'a>; 4]>,
    /// Interpolation delay of the client's view in 1/256 ticks.
    pub view_delay_q8: u32,
}

impl<'a> InputWindow<'a> {
    /// Iterate `(tick, payload)` newest → oldest, expanding runs.
    pub fn iter(&self) -> impl Iterator<Item = (Tick, &'a [u8])> + '_ {
        let mut tick = self.newest_tick.0;
        self.runs.iter().flat_map(move |run| {
            let start = tick;
            tick = tick.saturating_sub(u32::from(run.repeat));
            (0..u32::from(run.repeat)).filter_map(move |i| start.checked_sub(i).map(|t| (Tick(t), run.payload)))
        })
    }

    /// Encode. `inputs` are `(tick, postcard bytes)` newest first with consecutive ticks.
    pub fn encode(w: &mut Writer, inputs: &[(Tick, &[u8])], view_delay_q8: u32) {
        todo!()
    }

    /// Decode with limits (run count, payload size, tick underflow).
    pub fn decode(r: &mut Reader<'a>, limits: &WireLimits) -> Result<Self, DecodeError> {
        todo!()
    }
}
