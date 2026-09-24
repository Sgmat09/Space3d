//! Server input buffering: per slot, a ring of the last 128 ticks of decoded inputs.
//!
//! For tick T the driver uses `buffer[T]`; if missing: `last.repeat()` (counted as missing);
//! if the slot is disconnected: `NetInput::neutral()`. Inputs for ticks `< T` are late
//! (dropped, counted); ticks `> T + max_input_ahead` are rejected (strike). `input_slack` =
//! how many ticks before use the input for T arrived (EWMA), reported in every snapshot.

use super::{LeaveReason, NetServer, SlotState};
use crate::sim::{SimRole, SimTime, run_sim};
use crate::{ControlledBy, FastMap, PlayerId, ReplicationRegistry, Tick};
use space3d_core::{Entity, SystemCtx, TickRate};
use std::any::Any;

/// Type-erased decoded input (the registry's input driver knows the concrete `I`).
pub type ErasedInput = Box<dyn Any + Send + Sync>;

const RING: usize = 128;

/// Per-slot input ring (internal to the replication builder).
pub struct InputBuffer {
    ring: Vec<Option<(Tick, ErasedInput)>>,
    last: Option<ErasedInput>,
    newest: Option<Tick>,
    slack_ewma: f32,
    slack_samples: u32,
    /// Newest tick consumed with a real (received) input.
    consumed_real: Option<Tick>,
    /// Last tick consumed at all.
    consumed_through: Option<Tick>,
    /// An input was missing since the last snapshot.
    missing: bool,
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self {
            ring: (0..RING).map(|_| None).collect(),
            last: None,
            newest: None,
            slack_ewma: 0.0,
            slack_samples: 0,
            consumed_real: None,
            consumed_through: None,
            missing: false,
        }
    }
}

impl std::fmt::Debug for InputBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputBuffer")
            .field("newest", &self.newest)
            .field("slack", &self.slack_ewma)
            .finish_non_exhaustive()
    }
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

    /// Newest tick simulated with a received input (`input_ack`).
    pub fn consumed_real(&self) -> Option<Tick> {
        self.consumed_real
    }

    pub(crate) fn reset(&mut self) {
        let last = self.last.take();
        *self = Self::default();
        self.last = last;
    }

    pub(crate) fn contains(&self, t: Tick) -> bool {
        matches!(&self.ring[t.0 as usize % RING], Some((tt, _)) if *tt == t)
    }

    pub(crate) fn has_consumed(&self, t: Tick) -> bool {
        self.consumed_through.is_some_and(|c| t <= c)
    }

    pub(crate) fn insert(&mut self, t: Tick, v: ErasedInput) {
        self.ring[t.0 as usize % RING] = Some((t, v));
        if self.newest.is_none_or(|n| t > n) {
            self.newest = Some(t);
        }
    }

    pub(crate) fn observe_slack(&mut self, slack: f32) {
        if self.slack_samples == 0 {
            self.slack_ewma = slack;
        } else {
            self.slack_ewma += 0.1 * (slack - self.slack_ewma);
        }
        self.slack_samples = self.slack_samples.saturating_add(1);
    }

    /// Input for `t` (received or repeated). `None` before the client sent anything.
    pub(crate) fn take(&mut self, t: Tick, ops: &dyn crate::registry::InputOps) -> (Option<ErasedInput>, bool) {
        self.consumed_through = Some(t);
        let slot = &mut self.ring[t.0 as usize % RING];
        if let Some((tt, _)) = slot
            && *tt == t
        {
            let (_, v) = slot.take().expect("checked");
            self.last = Some(ops.clone_value(&v));
            self.consumed_real = Some(t);
            return (Some(v), true);
        }
        if self.newest.is_none() {
            return (None, false);
        }
        self.missing = true;
        (self.last.as_ref().map(|l| ops.repeat(l)), false)
    }

    /// Slack for the snapshot header in quarter ticks (`i8::MIN` = an input was missing).
    pub(crate) fn take_slack_q2(&mut self) -> i8 {
        if std::mem::take(&mut self.missing) {
            return i8::MIN;
        }
        if self.slack_samples == 0 {
            return 0;
        }
        (self.slack_ewma * 4.0).round().clamp(-127.0, 127.0) as i8
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

/// The `replication::server_tick` system (FixedUpdate): advance the room tick, write
/// `PlayerInput<I>`, run the shared simulation.
pub(crate) fn server_tick(ctx: &mut SystemCtx) {
    let now = ctx.resources.init::<crate::time::NetClockHandle>().now();
    let step = ctx.resources.get::<TickRate>().map(|r| r.step()).unwrap_or_default();
    let world = &mut *ctx.world;
    let tick = ctx.resources.scope::<NetServer, _>(|res, server| {
        server.tick = server.tick.next();
        server.tick_started_at = now;
        let tick = server.tick;
        // Resume windows.
        let expired: Vec<_> = server
            .slots
            .iter()
            .filter(|s| matches!(s.state, SlotState::Disconnected { until } if tick > until))
            .map(|s| s.client)
            .collect();
        for c in expired {
            server.free_slot(c, LeaveReason::Timeout);
        }
        if let Some(input) = res.get::<ReplicationRegistry>().and_then(|r| r.input()) {
            let ops = &*input.ops;
            let mut per_player: FastMap<PlayerId, ErasedInput> = FastMap::default();
            let mut ok = 0u64;
            let mut missing = 0u64;
            for slot in server.slots.iter_mut() {
                let value = if let Some(b) = slot.brain.as_mut() {
                    b.think(world, tick)
                } else if slot.link.is_none() {
                    ops.neutral()
                } else {
                    match slot.inputs.take(tick, ops) {
                        (Some(v), true) => {
                            ok += 1;
                            v
                        }
                        (Some(v), false) => {
                            missing += 1;
                            slot.stats.input_missing += 1;
                            v
                        }
                        (None, _) => ops.neutral(),
                    }
                };
                per_player.insert(slot.identity.player, value);
            }
            server.stats.inputs_ok += ok;
            server.stats.inputs_missing += missing;
            let controlled: Vec<(Entity, PlayerId)> = world
                .query::<(Entity, &ControlledBy)>()
                .iter()
                .map(|(e, c)| (e, c.0))
                .collect();
            let neutral = ops.neutral();
            for (e, player) in controlled {
                ops.write(world, e, per_player.get(&player).unwrap_or(&neutral));
            }
        }
        server.flush_events(res);
        tick
    });
    run_sim(
        ctx,
        SimTime {
            tick,
            dt: step,
            role: SimRole::Server,
            resimulating: false,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ReplicationRegistry;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Move(i8);
    impl crate::NetInput for Move {}

    #[test]
    fn buffer_uses_received_then_repeats() {
        let mut r = ReplicationRegistry::default();
        r.register_input::<Move>("m");
        r.freeze();
        let ops = &*r.input().unwrap().ops;
        let mut b = InputBuffer::default();
        assert!(b.take(Tick(1), ops).0.is_none());
        b.insert(Tick(2), Box::new(Move(5)));
        b.insert(Tick(3), Box::new(Move(6)));
        assert!(b.contains(Tick(3)));
        let (v, real) = b.take(Tick(2), ops);
        assert!(real);
        assert_eq!(v.unwrap().downcast_ref::<Move>(), Some(&Move(5)));
        b.take(Tick(3), ops);
        let (v, real) = b.take(Tick(4), ops);
        assert!(!real);
        assert_eq!(v.unwrap().downcast_ref::<Move>(), Some(&Move(6)));
        assert_eq!(b.take_slack_q2(), i8::MIN);
        assert_eq!(b.consumed_real(), Some(Tick(3)));
        assert!(b.has_consumed(Tick(4)));
        b.observe_slack(2.0);
        assert_eq!(b.take_slack_q2(), 8);
    }
}
