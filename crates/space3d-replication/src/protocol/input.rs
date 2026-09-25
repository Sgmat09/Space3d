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

/// Most ticks one INPUT block may cover (hostile run lengths are rejected before expansion).
pub const MAX_WINDOW_TICKS: u64 = 256;

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

    /// Encode. `inputs` are `(tick, postcard bytes)` newest first with consecutive ticks
    /// (at most 255 runs are written; older inputs beyond that are dropped). Panics if
    /// `inputs` is empty.
    pub fn encode(w: &mut Writer, inputs: &[(Tick, &[u8])], view_delay_q8: u32) {
        assert!(!inputs.is_empty(), "InputWindow::encode without inputs");
        debug_assert!(
            inputs.windows(2).all(|p| p[0].0.0 == p[1].0.0 + 1),
            "input ticks must be consecutive, newest first"
        );
        // Collapse consecutive identical payloads into runs.
        let mut runs: SmallVec<[(u16, &[u8]); 8]> = SmallVec::new();
        for (_, payload) in inputs {
            match runs.last_mut() {
                Some((n, p)) if *p == *payload && *n < u16::MAX => *n += 1,
                _ => {
                    if runs.len() == usize::from(u8::MAX) {
                        break;
                    }
                    runs.push((1, payload));
                }
            }
        }
        w.varu32(inputs[0].0.0);
        w.u8(runs.len() as u8);
        for (repeat, payload) in &runs {
            w.varu16(*repeat);
            w.bytes(payload);
        }
        w.varu32(view_delay_q8);
    }

    /// Decode with limits (run count, payload size, tick underflow, at most
    /// [`MAX_WINDOW_TICKS`] ticks covered).
    pub fn decode(r: &mut Reader<'a>, limits: &WireLimits) -> Result<Self, DecodeError> {
        let newest_tick = Tick(r.varu32()?);
        let count = r.u8()?;
        if count == 0 || count > limits.max_input_runs {
            return Err(DecodeError::TooLong {
                len: usize::from(count),
                max: usize::from(limits.max_input_runs),
            });
        }
        let mut runs = SmallVec::new();
        let mut covered = 0u64;
        for _ in 0..count {
            let repeat = r.varu16()?;
            if repeat == 0 {
                return Err(DecodeError::Invalid("empty input run"));
            }
            covered += u64::from(repeat);
            if covered > MAX_WINDOW_TICKS || covered > u64::from(newest_tick.0) + 1 {
                return Err(DecodeError::Invalid("input window too long"));
            }
            let payload = r.bytes(limits.max_input_bytes as usize)?;
            runs.push(InputRun { repeat, payload });
        }
        let view_delay_q8 = r.varu32()?;
        Ok(Self {
            newest_tick,
            runs,
            view_delay_q8,
        })
    }

    /// Number of ticks covered by the window.
    pub fn len_ticks(&self) -> u32 {
        self.runs.iter().map(|r| u32::from(r.repeat)).sum()
    }

    /// Oldest tick covered.
    pub fn oldest_tick(&self) -> Tick {
        Tick(self.newest_tick.0 + 1 - self.len_ticks().max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn runs_collapse_and_expand() {
        let a = [1u8, 2];
        let b = [3u8];
        let inputs: Vec<(Tick, &[u8])> = vec![(Tick(10), &a), (Tick(9), &a), (Tick(8), &b), (Tick(7), &a)];
        let mut w = Writer::new();
        InputWindow::encode(&mut w, &inputs, 300);
        let bytes = w.into_vec();
        let win = InputWindow::decode(&mut Reader::new(&bytes), &WireLimits::default()).unwrap();
        assert_eq!(win.runs.len(), 3);
        assert_eq!(win.view_delay_q8, 300);
        assert_eq!(win.oldest_tick(), Tick(7));
        let expanded: Vec<(Tick, &[u8])> = win.iter().collect();
        assert_eq!(expanded, inputs);
    }

    #[test]
    fn hostile_windows_are_rejected() {
        let limits = WireLimits::default();
        // run count 0
        let mut w = Writer::new();
        w.varu32(5);
        w.u8(0);
        assert!(InputWindow::decode(&mut Reader::new(w.as_slice()), &limits).is_err());
        // repeat longer than the tick
        let mut w = Writer::new();
        w.varu32(3);
        w.u8(1);
        w.varu16(10);
        w.bytes(&[1]);
        w.varu32(0);
        assert!(InputWindow::decode(&mut Reader::new(w.as_slice()), &limits).is_err());
        // payload too large
        let mut w = Writer::new();
        w.varu32(100);
        w.u8(1);
        w.varu16(1);
        w.bytes(&[0; 300]);
        w.varu32(0);
        assert!(InputWindow::decode(&mut Reader::new(w.as_slice()), &limits).is_err());
        // too many runs
        let mut w = Writer::new();
        w.varu32(100);
        w.u8(33);
        assert!(InputWindow::decode(&mut Reader::new(w.as_slice()), &limits).is_err());
    }

    proptest! {
        #[test]
        fn prop_window_roundtrip(newest in 64u32..1_000_000, payloads in proptest::collection::vec(0u8..3, 1..20), delay in any::<u32>()) {
            let bufs: Vec<Vec<u8>> = payloads.iter().map(|p| vec![*p; usize::from(*p) + 1]).collect();
            let inputs: Vec<(Tick, &[u8])> = bufs.iter().enumerate().map(|(i, b)| (Tick(newest - i as u32), b.as_slice())).collect();
            let mut w = Writer::new();
            InputWindow::encode(&mut w, &inputs, delay);
            let bytes = w.into_vec();
            let mut r = Reader::new(&bytes);
            let win = InputWindow::decode(&mut r, &WireLimits::default()).unwrap();
            prop_assert!(r.is_empty());
            let expanded: Vec<(Tick, &[u8])> = win.iter().collect();
            prop_assert_eq!(expanded, inputs);
            prop_assert_eq!(win.view_delay_q8, delay);
        }

        #[test]
        fn prop_decoder_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
            if let Ok(win) = InputWindow::decode(&mut Reader::new(&bytes), &WireLimits::default()) {
                prop_assert!(u64::from(win.len_ticks()) <= MAX_WINDOW_TICKS);
                let _ = win.iter().count();
            }
        }
    }
}
