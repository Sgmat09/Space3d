//! A wandering, trigger-happy bot for load tests and backfill: deterministic per seed, turns
//! every couple of seconds, sprints sometimes and fires about once a second.

use arena_shared::ArenaInput;
use space3d_core::World;
use space3d_replication::Tick;
use space3d_replication::bots::Bot;

/// See the module docs.
#[derive(Clone, Debug)]
pub struct WanderBot {
    state: u64,
    input: ArenaInput,
    next_turn: u32,
    next_shot: u32,
}

impl WanderBot {
    /// Bot with its own random stream.
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
            input: ArenaInput::default(),
            next_turn: 0,
            next_shot: 0,
        }
    }

    /// SplitMix64.
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl Bot<ArenaInput> for WanderBot {
    fn think(&mut self, _world: &World, tick: Tick) -> ArenaInput {
        let t = tick.0;
        if t >= self.next_turn {
            let r = self.next();
            self.input.move_x = ((r & 0xFF) as i16 - 128).clamp(-127, 127) as i8;
            self.input.move_z = (((r >> 8) & 0xFF) as i16 - 128).clamp(-127, 127) as i8;
            self.input.yaw = (r >> 16) as u16;
            self.input.buttons = if (r >> 32).is_multiple_of(4) {
                ArenaInput::SPRINT
            } else {
                0
            };
            self.next_turn = t + 90 + ((r >> 40) % 90) as u32;
        }
        if t >= self.next_shot {
            self.input.fire_seq = self.input.fire_seq.wrapping_add(1);
            self.next_shot = t + 45 + (self.next() % 30) as u32;
        }
        self.input
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_replication::NetInput;

    #[test]
    fn deterministic_valid_inputs() {
        let world = World::new();
        let run = |seed| {
            let mut b = WanderBot::new(seed);
            (0..600).map(|t| b.think(&world, Tick(t))).collect::<Vec<_>>()
        };
        let a = run(7);
        assert_eq!(a, run(7));
        assert_ne!(a, run(8));
        assert!(a.iter().all(|i| i.clone().sanitize().is_ok()));
        assert!(a.last().unwrap().fire_seq >= 8, "fires about once a second");
    }
}
