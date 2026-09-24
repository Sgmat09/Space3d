//! Play / pause / step control of the fixed-tick simulation ([`SimControl`]).
//!
//! The runner keeps calling `App::update(real_dt)`; while paused, a `Stage::PreUpdate` system
//! empties the fixed-step accumulator of [`Time`] so `Stage::FixedUpdate` does not run, and a
//! step request sets the accumulator to exactly `n * step` so exactly `n` ticks run in that
//! frame (at most `TickRate::max_steps_per_frame` per frame). Everything else (`Update`,
//! rendering, UI) keeps running, so the editor stays interactive.

use space3d_core::{App, Plugin, Stage, SystemCtx, TickRate, Time};
use std::collections::HashMap;
use std::time::Duration;

/// Simulation run state (resource inserted by [`SimControlPlugin`] / `EditorPlugin`).
#[derive(Clone, Debug, Default)]
pub struct SimControl {
    paused: bool,
    pending_steps: u64,
    /// `Time` templates with a given accumulator, keyed by nanoseconds.
    templates: HashMap<u128, Time>,
}

impl SimControl {
    /// Control starting paused or running.
    pub fn new(paused: bool) -> Self {
        Self {
            paused,
            ..Default::default()
        }
    }

    /// Whether `FixedUpdate` is paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Pause the simulation.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Resume the simulation (drops pending steps).
    pub fn play(&mut self) {
        self.paused = false;
        self.pending_steps = 0;
    }

    /// Toggle play/pause.
    pub fn toggle(&mut self) {
        if self.paused { self.play() } else { self.pause() }
    }

    /// Pause (if running) and run exactly `n` more fixed ticks over the next frame(s).
    pub fn step(&mut self, n: u64) {
        self.paused = true;
        self.pending_steps += n;
    }

    /// Ticks requested by [`Self::step`] that have not run yet.
    pub fn pending_steps(&self) -> u64 {
        self.pending_steps
    }

    fn template(&mut self, accumulator: Duration) -> Time {
        if accumulator.is_zero() {
            return Time::default();
        }
        self.templates
            .entry(accumulator.as_nanos())
            .or_insert_with(|| accumulated_time(accumulator))
            .clone()
    }

    /// `Stage::PreUpdate` logic (public for custom schedules).
    pub fn apply(ctx: &mut SystemCtx) {
        let Some(mut ctl) = ctx.resources.remove::<SimControl>() else {
            return;
        };
        if ctl.paused {
            let rate = ctx.resources.get::<TickRate>().copied().unwrap_or_default();
            let k = ctl.pending_steps.min(rate.max_steps_per_frame.max(1) as u64) as u32;
            ctl.pending_steps -= k as u64;
            let template = ctl.template(rate.step() * k);
            if let Some(t) = ctx.resources.get::<Time>() {
                let t = with_template(t, template);
                ctx.resources.insert(t);
            }
        }
        ctx.resources.insert(ctl);
    }
}

/// A `Time` carrying the public state of `t` and the (private) fixed-step accumulator of
/// `template`.
fn with_template(t: &Time, mut template: Time) -> Time {
    template.delta = t.delta;
    template.elapsed = t.elapsed;
    template.frame = t.frame;
    template.tick = t.tick;
    template.fixed_delta = t.fixed_delta;
    template.alpha = t.alpha;
    template.dropped_ticks = t.dropped_ticks;
    template
}

/// A `Time` whose accumulator holds exactly `total`: a scratch app with a tick rate so low
/// that no tick ever runs accumulates `total` (fed in chunks below `App::update`'s 250 ms
/// clamp).
fn accumulated_time(total: Duration) -> Time {
    let mut scratch = App::new();
    scratch.set_tickrate(TickRate::new(1e-9));
    let mut remaining = total;
    let chunk = Duration::from_millis(200);
    while !remaining.is_zero() {
        let d = remaining.min(chunk);
        scratch.update(d);
        remaining -= d;
    }
    scratch.resources.expect::<Time>().clone()
}

/// Adds [`SimControl`] and its `PreUpdate` system (included in `EditorPlugin`).
#[derive(Clone, Copy, Debug, Default)]
pub struct SimControlPlugin {
    /// Start paused.
    pub paused: bool,
}

impl Plugin for SimControlPlugin {
    fn build(&self, app: &mut App) {
        if !app.resources.contains::<SimControl>() {
            app.insert_resource(SimControl::new(self.paused));
        }
        app.add_system_named(Stage::PreUpdate, "editor::sim_control", SimControl::apply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Ticks(u64);

    fn app(paused: bool) -> App {
        let mut app = App::new();
        app.add_plugin(SimControlPlugin { paused });
        app.init_resource::<Ticks>();
        app.add_system(Stage::FixedUpdate, |c| c.resources.expect_mut::<Ticks>().0 += 1);
        app
    }

    fn ticks(app: &App) -> u64 {
        app.resources.expect::<Ticks>().0
    }

    #[test]
    fn paused_simulation_does_not_tick_but_frames_run() {
        let mut app = app(true);
        for _ in 0..20 {
            app.update(Duration::from_millis(16));
        }
        assert_eq!(ticks(&app), 0);
        assert_eq!(app.resources.expect::<Time>().frame, 20);
        assert_eq!(app.resources.expect::<Time>().tick, 0);
        app.resources.expect_mut::<SimControl>().play();
        for _ in 0..6 {
            app.update(Duration::from_millis(50));
        }
        // 300 ms at 60 Hz (the step is rounded to whole nanoseconds, so 17 or 18 ticks).
        assert!((17..=18).contains(&ticks(&app)), "{}", ticks(&app));
    }

    #[test]
    fn step_runs_exactly_n_ticks() {
        let mut app = app(true);
        app.resources.expect_mut::<SimControl>().step(3);
        app.update(Duration::from_millis(1));
        assert_eq!(ticks(&app), 3);
        for _ in 0..5 {
            app.update(Duration::from_millis(100));
        }
        assert_eq!(ticks(&app), 3, "paused again after the step");
        // More than max_steps_per_frame (8) spread over frames, independent of frame time.
        app.resources.expect_mut::<SimControl>().step(20);
        for _ in 0..3 {
            app.update(Duration::ZERO);
        }
        assert_eq!(ticks(&app), 23);
        assert_eq!(app.resources.expect::<SimControl>().pending_steps(), 0);
        assert_eq!(app.resources.expect::<Time>().tick, 23);
        assert_eq!(app.resources.expect::<Time>().dropped_ticks, 0);
    }

    #[test]
    fn step_works_with_other_tick_rates() {
        let mut app = app(false);
        app.set_tickrate(TickRate::new(2.0));
        app.resources.expect_mut::<SimControl>().step(2);
        app.update(Duration::ZERO);
        assert_eq!(ticks(&app), 2);
        app.update(Duration::from_secs(2));
        assert_eq!(ticks(&app), 2);
    }
}
