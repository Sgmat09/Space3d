//! Dynamic resolution: the scene's resolution follows what the GPU can do.
//!
//! With [`DynamicResolution::enabled`] the renderer scales [`FrameSettings::render_scale`]
//! (the scene resolution of the Primary-target cameras that take the global scale; the UI and
//! the window stay at full size) down in steps while frames take longer than the target frame
//! rate allows, and back up when there is room again. It reads nothing but the time from one
//! frame to the next, so it works on every backend, with or without vsync. The rules, in the
//! order [`DynamicResolution::frame`] applies them:
//!
//! - **Windows.** Frames are averaged over [`WINDOW_MS`]; the window after a change of scale is
//!   left out (the scene's targets are made anew: a hitch). A frame of [`HITCH_MS`] or more is
//!   a stall, not what the scene costs, and is left out, unless three in a row are that long:
//!   then they are what the scene costs, and count.
//! - **Down.** A window slower than the budget starts a *descent*: a step down each judged
//!   window until a window is within budget, or the lowest step is reached. A single step
//!   cannot be judged by itself: under vsync a frame that misses the refresh by a little and
//!   one that misses it by a lot are shown just as late, so a step that helps may show nothing
//!   until the next one crosses a refresh interval.
//! - **At the floor.** A descent that reaches the lowest step still over budget stays there if
//!   the frame is at least a quarter faster than where it started ([`WORTH`]: the resolution
//!   is what costs, and this is the best that can be done). Otherwise the resolution is not
//!   what the frame waits for (the CPU is the limit, the display cannot show the target rate,
//!   or the slow pass does not scale with the pixels): the scale goes back to where the
//!   descent started and no descent is tried for [`BLOCK_SECS`], twice as long after every
//!   further one that ends so, up to [`MAX_BLOCK_SECS`]. A descent that ends within budget, a
//!   minute of frames within budget, or a frame time a fifth away from the one the last
//!   descent failed at (the scene is another) ends the back-off.
//! - **Up, with room.** After [`DynamicResolution::probe_secs`] of windows within budget the
//!   scale tries one step up. Under vsync a frame cannot show how much room it has, so going
//!   up is a probe: if the window after it is over budget the step is taken back and the next
//!   probe waits twice as long (at most [`MAX_PROBE_SECS`]); a probe that holds halves the
//!   wait again.
//! - **Up, without room.** A reduced scale that is over budget where no descent may start
//!   (blocked, or at the floor) tries a step up after `probe_secs` of such windows: if the
//!   frame is no slower for it ([`NO_SLOWER`]) the resolution was not what cost the time and
//!   the step is kept, and the next is tried as soon; if it is slower the step is taken back
//!   and the next try waits twice as long (at most [`MAX_PROBE_SECS`]).
//!
//! Switched off, the scale is 1 and nothing of the last run is kept.
//!
//! [`FrameSettings::render_scale`]: crate::FrameSettings::render_scale

/// The scales the resolution steps through: each step is about 0.72 of the pixels of the one
/// before, down to a quarter.
pub const STEPS: [f32; 5] = [1.0, 0.85, 0.72, 0.6, 0.5];
/// How long frames are averaged before the scale is judged (ms of frame time).
pub const WINDOW_MS: f32 = 750.0;
/// How long no descent is tried after the first that showed the resolution is not what costs
/// the time (s).
pub const BLOCK_SECS: f32 = 30.0;
/// The longest that wait grows to (s): a frame rate the resolution cannot help costs one
/// descent to the lowest step and back (a few seconds at reduced resolution, a new set of
/// scene targets a step) every ten minutes at most.
pub const MAX_BLOCK_SECS: f32 = 600.0;
/// The longest wait between two steps up that were taken back (s).
pub const MAX_PROBE_SECS: f32 = 120.0;
/// A frame this long (ms) is a stall (a load), not what the scene costs, unless
/// [`HITCH_RUN`] in a row are.
pub const HITCH_MS: f32 = 250.0;
/// So many long frames in a row are what the scene costs.
const HITCH_RUN: u8 = 3;
/// A window is over budget past this share of the budget.
const SLOW: f32 = 1.12;
/// A descent that ends over budget at the lowest step was worth it if the frame takes at most
/// this share of the time it took where the descent started.
pub const WORTH: f32 = 0.75;
/// A step up from an over-budget scale is kept if the frame takes at most this share of the
/// time before it.
pub const NO_SLOWER: f32 = 1.05;
/// Seconds within budget after which a back-off is forgotten.
const FORGET_SECS: f32 = 60.0;
/// A frame time this far (a share) from the one the last descent failed at is another scene.
const ANOTHER: f32 = 0.2;

/// A step up that is being judged.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Rise {
    /// Tried with room: it holds if the next window is within budget.
    Probe,
    /// Tried over budget: it holds if the next window is no slower than this mean (ms).
    Lift(f32),
}

/// Dynamic resolution (resource, inserted by [`crate::RenderPlugin`], off by default).
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicResolution {
    /// Follow the frame time. Off, the scale is 1.
    pub enabled: bool,
    /// The frame rate to hold. Default 60.
    pub target_fps: f32,
    /// The lowest scale to go to (the nearest of [`STEPS`] at or above it). Default 0.5.
    pub min_scale: f32,
    /// Seconds of judged windows before a step up is tried. Default 8.
    pub probe_secs: f32,
    step: usize,
    /// Frame time and frames of the window being gathered.
    window: (f32, u32),
    /// Long frames in a row, and their time while they are still held back as stalls.
    long: (u8, f32),
    /// Windows still to leave out after a change of scale.
    settle: u8,
    /// A descent in progress: the step it started from and the mean frame time there.
    descent: Option<(usize, f32)>,
    /// A step up being judged.
    rising: Option<Rise>,
    /// Seconds within budget since the last change, and the wait before the next probe.
    calm: f32,
    probe_wait: f32,
    /// Seconds over budget where no descent may start, and the wait before the next lift.
    stuck: f32,
    lift_wait: f32,
    /// Seconds left in which no descent starts, the length of the next such wait, and the mean
    /// frame time the last descent failed at.
    blocked: f32,
    block: f32,
    failed_at: f32,
}

impl Default for DynamicResolution {
    fn default() -> Self {
        Self {
            enabled: false,
            target_fps: 60.0,
            min_scale: 0.5,
            probe_secs: 8.0,
            step: 0,
            window: (0.0, 0),
            long: (0, 0.0),
            settle: 0,
            descent: None,
            rising: None,
            calm: 0.0,
            probe_wait: 0.0,
            stuck: 0.0,
            lift_wait: 0.0,
            blocked: 0.0,
            block: 0.0,
            failed_at: 0.0,
        }
    }
}

impl DynamicResolution {
    /// On, holding `target_fps`.
    pub fn on(target_fps: f32) -> Self {
        Self {
            enabled: true,
            target_fps,
            ..Self::default()
        }
    }

    /// What [`FrameSettings::render_scale`](crate::FrameSettings::render_scale) is multiplied
    /// by this frame: one of [`STEPS`], never under `min_scale`'s step; 1 while off.
    pub fn scale(&self) -> f32 {
        if self.enabled {
            STEPS[self.step.min(self.last_step())]
        } else {
            1.0
        }
    }

    /// The lowest step `min_scale` allows.
    fn last_step(&self) -> usize {
        STEPS.iter().rposition(|s| *s >= self.min_scale).unwrap_or(0)
    }

    fn change(&mut self, step: usize) {
        self.step = step;
        self.settle = 1;
        self.calm = 0.0;
    }

    /// The settings alone, as a run starts.
    fn fresh(&self) -> Self {
        Self {
            enabled: self.enabled,
            target_fps: self.target_fps,
            min_scale: self.min_scale,
            probe_secs: self.probe_secs,
            ..Self::default()
        }
    }

    /// Note a frame that took `frame_ms` from the one before it. Called by the renderer once a
    /// frame; public for tests and for an app that paces frames itself.
    pub fn frame(&mut self, frame_ms: f32) {
        if !self.enabled {
            let fresh = self.fresh();
            if *self != fresh {
                *self = fresh;
            }
            return;
        }
        let last = self.last_step();
        if self.step > last {
            // (`min_scale` was raised on the way)
            self.descent = None;
            self.rising = None;
            self.change(last);
        }
        if !frame_ms.is_finite() || frame_ms < 0.0 {
            return;
        }
        if frame_ms >= HITCH_MS {
            self.long.0 = self.long.0.saturating_add(1);
            if self.long.0 < HITCH_RUN {
                self.long.1 += frame_ms;
                return;
            }
            // the run is what the scene costs: the frames held back count, and this one
            self.window.0 += self.long.1 + frame_ms;
            self.window.1 += if self.long.1 > 0.0 { u32::from(HITCH_RUN) } else { 1 };
            self.long.1 = 0.0;
        } else {
            self.long = (0, 0.0);
            self.window.0 += frame_ms;
            self.window.1 += 1;
        }
        if self.window.0 < WINDOW_MS {
            return;
        }
        let (total, frames) = std::mem::take(&mut self.window);
        let (mean, secs) = (total / frames as f32, total / 1000.0);
        self.blocked = (self.blocked - secs).max(0.0);
        if self.settle > 0 {
            self.settle -= 1;
            return;
        }
        self.probe_wait = self.probe_wait.max(self.probe_secs);
        self.lift_wait = self.lift_wait.max(self.probe_secs);
        self.block = self.block.max(BLOCK_SECS);
        let slow = mean > 1000.0 / self.target_fps.max(1.0) * SLOW;

        // A step up taken in the window before: does it hold?
        match self.rising.take() {
            Some(Rise::Probe) if slow => {
                self.probe_wait = (self.probe_wait * 2.0).min(MAX_PROBE_SECS);
                self.change((self.step + 1).min(last));
                return;
            }
            Some(Rise::Probe) => self.probe_wait = (self.probe_wait * 0.5).max(self.probe_secs),
            Some(Rise::Lift(before)) if slow && mean > before * NO_SLOWER => {
                self.lift_wait = (self.lift_wait * 2.0).min(MAX_PROBE_SECS);
                self.stuck = 0.0;
                self.change((self.step + 1).min(last));
                return;
            }
            Some(Rise::Lift(_)) => self.lift_wait = self.probe_secs,
            None => {}
        }

        if !slow {
            if self.descent.take().is_some() {
                // the descent met the budget: the resolution was what cost the time
                self.blocked = 0.0;
                self.block = BLOCK_SECS;
            }
            self.stuck = 0.0;
            self.calm += secs;
            if self.calm >= FORGET_SECS {
                self.blocked = 0.0;
                self.block = BLOCK_SECS;
            }
            if self.step > 0 && self.calm >= self.probe_wait {
                self.rising = Some(Rise::Probe);
                self.change(self.step - 1);
            }
            return;
        }

        self.calm = 0.0;
        if let Some((start, before)) = self.descent {
            if self.step < last {
                self.change(self.step + 1);
                return;
            }
            // the floor, and still over budget
            self.descent = None;
            self.stuck = 0.0;
            if mean <= before * WORTH {
                self.blocked = 0.0;
                self.block = BLOCK_SECS;
            } else {
                self.blocked = self.block;
                self.block = (self.block * 2.0).min(MAX_BLOCK_SECS);
                self.failed_at = before;
                self.change(start);
            }
            return;
        }
        if self.blocked > 0.0 && (mean - self.failed_at).abs() > self.failed_at * ANOTHER {
            // not the scene the last descent failed in
            self.blocked = 0.0;
            self.block = BLOCK_SECS;
        }
        if self.blocked == 0.0 && self.step < last {
            self.descent = Some((self.step, mean));
            self.stuck = 0.0;
            self.change(self.step + 1);
            return;
        }
        // Over budget where no descent may start: was it the resolution? A step up tells.
        if self.step > 0 {
            self.stuck += secs;
            if self.stuck >= self.lift_wait {
                self.stuck = 0.0;
                self.rising = Some(Rise::Lift(mean));
                self.change(self.step - 1);
            }
        }
    }
}

/// The renderer's clock for [`DynamicResolution::frame`]: the time from one Render stage to
/// the next.
#[derive(Default)]
pub(crate) struct FrameClock(Option<web_time::Instant>);

pub(crate) fn dynamic_resolution_system(resources: &mut space3d_core::Resources) {
    let now = web_time::Instant::now();
    let last = resources.get_mut::<FrameClock>().and_then(|clock| clock.0.replace(now));
    if let (Some(last), Some(d)) = (last, resources.get_mut::<DynamicResolution>()) {
        d.frame((now - last).as_secs_f32() * 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a run did: the scale at its end, how often the scale changed, the lowest scale it
    /// went to, and the share of its time at each of [`STEPS`].
    struct Ran {
        scale: f32,
        changes: u32,
        lowest: f32,
        share: [f32; 5],
    }

    /// Run `secs` of frames whose time `cost(scale)` gives.
    fn run(d: &mut DynamicResolution, secs: f32, cost: impl Fn(f32) -> f32) -> Ran {
        let (mut t, mut changes, mut lowest, mut at) = (0.0, 0, d.scale(), [0.0f32; 5]);
        let mut scale = d.scale();
        while t < secs * 1000.0 {
            let ms = cost(d.scale());
            at[STEPS.iter().position(|s| *s == d.scale()).unwrap()] += ms;
            d.frame(ms);
            t += ms;
            if d.scale() != scale {
                changes += 1;
                scale = d.scale();
                lowest = lowest.min(scale);
            }
        }
        Ran {
            scale: d.scale(),
            changes,
            lowest,
            share: at.map(|ms| ms / t),
        }
    }

    /// A frame that waits for the GPU: 4 ms of CPU and `gpu` ms at full resolution, by the pixels.
    fn gpu_bound(gpu: f32) -> impl Fn(f32) -> f32 {
        move |scale| 4.0 + gpu * scale * scale
    }

    /// `cost` as a display of 60 Hz with vsync shows it: at the next refresh.
    fn vsynced(cost: impl Fn(f32) -> f32) -> impl Fn(f32) -> f32 {
        move |scale| (cost(scale) / 16.667).ceil().max(1.0) * 16.667
    }

    #[test]
    fn off_it_is_full_resolution() {
        let mut d = DynamicResolution::default();
        assert_eq!(run(&mut d, 20.0, gpu_bound(60.0)).scale, 1.0);
        // switched off on the way, it is whole again at once and nothing is left of the run
        let mut d = DynamicResolution::on(60.0);
        assert!(run(&mut d, 20.0, gpu_bound(60.0)).scale < 1.0);
        d.enabled = false;
        d.frame(16.0);
        assert_eq!(d, DynamicResolution::default());
        // also from a run that left no step behind, only a back-off
        let mut d = DynamicResolution::on(50.0);
        run(&mut d, 20.0, |_| 40.0);
        assert!(d.scale() == 1.0 && d.blocked > 0.0);
        d.enabled = false;
        d.frame(16.0);
        assert_eq!(
            d,
            DynamicResolution {
                target_fps: 50.0,
                ..DynamicResolution::default()
            }
        );
    }

    #[test]
    fn a_slow_gpu_steps_down_until_the_budget_is_met() {
        // 30 ms of GPU at full resolution: 0.6 brings it to 10.8 + 4, within 16.7; 0.72 does not (19.5)
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 12.0, gpu_bound(30.0)).scale, 0.6);
        // a GPU that is fast enough stays whole, and nothing is ever tried
        let mut d = DynamicResolution::on(60.0);
        let ran = run(&mut d, 60.0, gpu_bound(9.0));
        assert_eq!((ran.scale, ran.changes), (1.0, 0));
        // one too slow even at the lowest stays there (a quarter of the pixels is four times the frames), and at
        // min_scale's step when that is higher
        let mut d = DynamicResolution::on(60.0);
        let ran = run(&mut d, 600.0, gpu_bound(200.0));
        assert!(ran.share[4] > 0.9, "{:?}", ran.share);
        let mut d = DynamicResolution {
            min_scale: 0.7,
            ..DynamicResolution::on(60.0)
        };
        let ran = run(&mut d, 600.0, gpu_bound(200.0));
        assert!(ran.share[2] > 0.9 && ran.lowest == 0.72, "{:?}", ran.share);
    }

    #[test]
    fn the_scale_is_never_under_a_min_scale_raised_on_the_way() {
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 12.0, gpu_bound(200.0)).scale, 0.5);
        d.min_scale = 0.8;
        assert_eq!(d.scale(), 0.85, "at once");
        let ran = run(&mut d, 60.0, gpu_bound(200.0));
        assert_eq!((ran.scale, ran.lowest), (0.85, 0.85));
    }

    /// A frame the resolution cannot help (the CPU is the limit; a 50 Hz display asked for 60): a descent to the lowest
    /// step shows it, the scale goes back, and the descents come less and less often.
    #[test]
    fn what_the_resolution_cannot_help_is_left_alone_longer_and_longer() {
        for cost in [25.0f32, 20.0] {
            let mut d = DynamicResolution::on(60.0);
            let first = run(&mut d, 30.0 * 60.0, |_| cost);
            // each descent is four steps down and one back: 30, 60, 120, 240, 480 and 600 s apart
            assert_eq!(first.scale, 1.0);
            assert!(
                (5..=7).contains(&(first.changes / 5)) && first.changes.is_multiple_of(5),
                "{} changes",
                first.changes
            );
            assert!(first.share[0] > 0.97, "{:?}", first.share);
            let later = run(&mut d, 60.0 * 60.0, |_| cost);
            assert!(later.changes <= 7 * 5, "{} changes in the next hour", later.changes);
            assert_eq!(d.block, MAX_BLOCK_SECS);
        }
    }

    /// A frame that depends on the resolution only a little (30 ms that do not, 4 ms that do) is not helped enough to
    /// stay down, and is not stepped up and down for ever.
    #[test]
    fn a_frame_the_resolution_helps_only_a_little_settles() {
        for (fixed, gpu) in [(30.0f32, 4.0f32), (28.0, 6.0), (22.0, 3.0), (40.0, 5.0)] {
            let mut d = DynamicResolution::on(60.0);
            let ran = run(&mut d, 30.0 * 60.0, move |s| fixed + gpu * s * s);
            assert!(
                ran.changes <= 7 * 5,
                "{fixed} + {gpu}: {} changes in half an hour",
                ran.changes
            );
            assert!(ran.share[0] > 0.95, "{fixed} + {gpu}: {:?}", ran.share);
        }
    }

    /// Under vsync a step that crosses no refresh interval shows no gain at all: the descent goes on until one does.
    #[test]
    fn a_step_that_crosses_no_refresh_interval_is_not_the_last_word() {
        // 25 ms of GPU: shown every 33.3 ms at 1.0, 0.85 and 0.72 (17 ms), every 16.7 ms at 0.6
        let mut d = DynamicResolution::on(60.0);
        let ran = run(&mut d, 5.0 * 60.0, vsynced(gpu_bound(25.0)));
        assert!(ran.share[3] > 0.9 && ran.lowest == 0.6, "{:?}", ran.share);
        // 10 ms of CPU and 20 of GPU: only the lowest step fits a refresh
        let mut d = DynamicResolution::on(60.0);
        let ran = run(&mut d, 5.0 * 60.0, vsynced(|s| 10.0 + 20.0 * s * s));
        assert!(ran.share[4] > 0.9, "{:?}", ran.share);
    }

    #[test]
    fn it_comes_back_up_when_there_is_room() {
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 12.0, gpu_bound(30.0)).scale, 0.6);
        // the scene got light (vsync holds the frame at 16.7 ms whatever the scale): a step up every probe
        assert_eq!(run(&mut d, 60.0, |_| 16.7).scale, 1.0);
        // back in the heavy scene it settles where it was, and the probes up that fail come less and less often
        let ran = run(&mut d, 5.0 * 60.0, gpu_bound(30.0));
        assert!(ran.share[3] > 0.9 && ran.scale <= 0.72, "{:?}", ran.share);
        let later = run(&mut d, 4.0 * 60.0, gpu_bound(30.0));
        assert!(
            (1..=6).contains(&(later.changes / 2)),
            "{} changes in four minutes",
            later.changes
        );
    }

    /// A reduced scale that the resolution no longer explains (the scene went from GPU-bound to CPU-bound) is given
    /// back, also after a long time at the reduced scale, whose failed probes have made the probes rare.
    #[test]
    fn it_comes_back_up_when_the_resolution_is_no_longer_what_is_slow() {
        for (before, gpu) in [(20.0f32, 30.0f32), (600.0, 30.0), (600.0, 200.0)] {
            let mut d = DynamicResolution::on(60.0);
            assert!(run(&mut d, before, gpu_bound(gpu)).scale <= 0.72);
            let ran = run(&mut d, 120.0, |_| 25.0);
            assert_eq!(ran.scale, 1.0, "after {before} s of {gpu} ms: {:?}", ran.share);
            let later = run(&mut d, 20.0 * 60.0, |_| 25.0);
            assert!(later.share[0] > 0.95, "{:?}", later.share);
        }
    }

    /// A step up that costs time is taken back, and tried less and less often.
    #[test]
    fn a_step_up_that_costs_time_is_taken_back() {
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 12.0, gpu_bound(200.0)).scale, 0.5);
        let ran = run(&mut d, 8.0 * 60.0, gpu_bound(200.0));
        assert!((3..=8).contains(&(ran.changes / 2)), "{} changes", ran.changes);
        assert!(ran.share[4] > 0.95, "{:?}", ran.share);
    }

    /// A scene that turns GPU-bound while a back-off runs is not left waiting for it.
    #[test]
    fn another_scene_ends_the_back_off() {
        let mut d = DynamicResolution::on(60.0);
        run(&mut d, 20.0 * 60.0, |_| 25.0);
        assert!(d.blocked > 60.0, "{}", d.blocked);
        let ran = run(&mut d, 30.0, gpu_bound(60.0));
        assert_eq!(ran.scale, 0.5);
        // and so does a minute within budget
        let mut d = DynamicResolution::on(60.0);
        run(&mut d, 20.0 * 60.0, |_| 25.0);
        run(&mut d, 70.0, |_| 10.0);
        assert_eq!((d.blocked, d.block), (0.0, BLOCK_SECS));
    }

    #[test]
    fn hitches_do_not_count() {
        let mut d = DynamicResolution::on(60.0);
        let mut t = 0.0;
        while t < 10_000.0 {
            // a 400 ms stall every half second among 10 ms frames, and now and then two in a row
            let stall = (t as u32 / 10).is_multiple_of(50);
            d.frame(if stall { 400.0 } else { 10.0 });
            if stall && (t as u32 / 10).is_multiple_of(200) {
                d.frame(400.0);
            }
            t += 10.0;
        }
        assert_eq!(d.scale(), 1.0);
        // a frame time that is no number, or less than none, is not one
        d.frame(f32::NAN);
        d.frame(-5.0);
        d.frame(f32::INFINITY);
        assert_eq!(d.scale(), 1.0);
    }

    /// Frames that are all long are no stalls: they are what the scene costs.
    #[test]
    fn frames_that_are_all_long_are_what_the_scene_costs() {
        // 300 ms a frame at full resolution, by the pixels
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 60.0, |s| 300.0 * s * s).scale, 0.5);
        // long at every scale: still judged (a descent, then back)
        let mut d = DynamicResolution::on(60.0);
        let ran = run(&mut d, 60.0, |_| 300.0);
        assert!(ran.lowest == 0.5 && ran.scale == 1.0);
    }
}
