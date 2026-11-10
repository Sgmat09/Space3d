//! Dynamic resolution: the scene's resolution follows what the GPU can do.
//!
//! With [`DynamicResolution::enabled`] the renderer scales [`FrameSettings::render_scale`]
//! (the Primary-target cameras' scene resolution; the UI and the window stay at full size) down
//! in steps while frames take longer than the target frame rate allows, and back up when there
//! is room again. It reads nothing but the time from one frame to the next, so it works on
//! every backend, with or without vsync:
//!
//! - Frames are averaged over [`WINDOW_MS`]. A window slower than the budget steps the scale
//!   down. If the next window is no faster, the frame was not waiting for the GPU (the CPU is
//!   the limit, or the pass that is slow does not scale with resolution): the step is taken
//!   back and no further one is tried for [`BLOCK_SECS`].
//! - After [`DynamicResolution::probe_secs`] of frames within budget the scale tries one step
//!   up. Under vsync a frame cannot show how much room it has, so going up is a probe: if the
//!   window after it is too slow the step is taken back and the next probe waits twice as
//!   long (at most [`MAX_PROBE_SECS`]).
//!
//! [`FrameSettings::render_scale`]: crate::FrameSettings::render_scale

/// The scales the resolution steps through: each step is about 0.72 of the pixels of the one
/// before, down to a quarter.
pub const STEPS: [f32; 5] = [1.0, 0.85, 0.72, 0.6, 0.5];
/// How long frames are averaged before the scale is judged (ms of frame time).
pub const WINDOW_MS: f32 = 750.0;
/// How long no step down is tried after one that gained nothing (s).
pub const BLOCK_SECS: f32 = 30.0;
/// The longest wait between two probes up (s).
pub const MAX_PROBE_SECS: f32 = 120.0;
/// A window is too slow past this share of the budget, and a step down has gained something
/// under this share of the time before it.
const SLOW: f32 = 1.12;
const GAINED: f32 = 0.95;
/// A frame longer than this is a hitch (a load, a stall), not what the scene costs: left out.
const HITCH_MS: f32 = 250.0;

/// Dynamic resolution (resource, inserted by [`crate::RenderPlugin`], off by default).
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicResolution {
    /// Follow the frame time. Off, the scale is 1.
    pub enabled: bool,
    /// The frame rate to hold. Default 60.
    pub target_fps: f32,
    /// The lowest scale to go to (the nearest of [`STEPS`] at or above it). Default 0.5.
    pub min_scale: f32,
    /// Seconds of frames within budget before a step up is tried. Default 8.
    pub probe_secs: f32,
    step: usize,
    /// Frame time and frames of the window being gathered.
    window: (f32, u32),
    /// Windows still to leave out after a change of scale (its targets are made anew: a hitch).
    settle: u8,
    /// The mean frame time before the last step down, while that step is judged.
    judged: Option<f32>,
    /// The last change was a probe up that is still to be confirmed.
    probing: bool,
    /// Seconds within budget since the last change, the wait before the next probe, and the
    /// seconds left in which no step down is tried.
    calm: f32,
    probe_wait: f32,
    blocked: f32,
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
            settle: 0,
            judged: None,
            probing: false,
            calm: 0.0,
            probe_wait: 0.0,
            blocked: 0.0,
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
    /// by this frame: one of [`STEPS`], 1 while off.
    pub fn scale(&self) -> f32 {
        if self.enabled { STEPS[self.step] } else { 1.0 }
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

    /// Note a frame that took `frame_ms` from the one before it. Called by the renderer once a
    /// frame; public for tests and for an app that paces frames itself.
    pub fn frame(&mut self, frame_ms: f32) {
        if !self.enabled {
            let keep = (self.target_fps, self.min_scale, self.probe_secs);
            if self.step != 0 || self.window.1 != 0 {
                *self = Self::default();
                (self.target_fps, self.min_scale, self.probe_secs) = keep;
            }
            return;
        }
        if !(0.0..HITCH_MS).contains(&frame_ms) {
            return;
        }
        self.window.0 += frame_ms;
        self.window.1 += 1;
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
        if self.probe_wait < self.probe_secs {
            self.probe_wait = self.probe_secs;
        }
        let budget = 1000.0 / self.target_fps.max(1.0);
        if mean > budget * SLOW {
            if self.probing {
                // the step up was too much: back, and the next probe waits longer
                self.probing = false;
                self.probe_wait = (self.probe_wait * 2.0).min(MAX_PROBE_SECS);
                self.change((self.step + 1).min(self.last_step()));
            } else if let Some(before) = self.judged.take()
                && mean > before * GAINED
            {
                // the step down gained nothing: the GPU is not what the frame waits for
                self.blocked = BLOCK_SECS;
                self.change(self.step.saturating_sub(1));
            } else if self.blocked == 0.0 && self.step < self.last_step() {
                self.judged = Some(mean);
                self.change(self.step + 1);
            }
            self.calm = 0.0;
            return;
        }
        self.judged = None;
        if self.probing {
            // the step up holds: probes come sooner again
            self.probing = false;
            self.probe_wait = (self.probe_wait * 0.5).max(self.probe_secs);
        }
        self.calm += secs;
        if self.step > 0 && self.calm >= self.probe_wait {
            self.probing = true;
            self.change(self.step - 1);
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

    /// Run `secs` of frames whose time `cost(scale)` gives; returns the scale at the end.
    fn run(d: &mut DynamicResolution, secs: f32, cost: impl Fn(f32) -> f32) -> f32 {
        let mut t = 0.0;
        while t < secs * 1000.0 {
            let ms = cost(d.scale());
            d.frame(ms);
            t += ms;
        }
        d.scale()
    }

    /// A frame that waits for the GPU: 4 ms of CPU and `gpu` ms at full resolution, by the pixels.
    fn gpu_bound(gpu: f32) -> impl Fn(f32) -> f32 {
        move |scale| 4.0 + gpu * scale * scale
    }

    #[test]
    fn off_it_is_full_resolution() {
        let mut d = DynamicResolution::default();
        assert_eq!(run(&mut d, 20.0, gpu_bound(60.0)), 1.0);
        // switched off on the way, it is whole again at once and starts afresh
        let mut d = DynamicResolution::on(60.0);
        assert!(run(&mut d, 20.0, gpu_bound(60.0)) < 1.0);
        d.enabled = false;
        d.frame(16.0);
        assert_eq!((d.scale(), d.step, d.target_fps), (1.0, 0, 60.0));
    }

    #[test]
    fn a_slow_gpu_steps_down_until_the_budget_is_met() {
        // 30 ms of GPU at full resolution: 0.6 brings it to 10.8 + 4, within 16.7; 0.72 does not (19.5)
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 20.0, gpu_bound(30.0)), 0.6);
        // a GPU that fast enough stays whole
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 20.0, gpu_bound(9.0)), 1.0);
        // one too slow even at the lowest stops there, and at min_scale when that is higher
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 30.0, gpu_bound(200.0)), 0.5);
        let mut d = DynamicResolution {
            min_scale: 0.7,
            ..DynamicResolution::on(60.0)
        };
        assert_eq!(run(&mut d, 30.0, gpu_bound(200.0)), 0.72);
    }

    #[test]
    fn a_slow_cpu_keeps_the_resolution() {
        // 25 ms whatever the scale: one step is tried, gains nothing and is taken back
        let mut d = DynamicResolution::on(60.0);
        let mut lowest = 1.0f32;
        let mut t = 0.0;
        while t < 20_000.0 {
            d.frame(25.0);
            lowest = lowest.min(d.scale());
            t += 25.0;
        }
        assert_eq!((d.scale(), lowest), (1.0, 0.85));
        // and nothing more is tried while it is blocked, then once again
        assert!(d.blocked > 0.0);
        assert_eq!(run(&mut d, 5.0, |_| 25.0), 1.0);
    }

    #[test]
    fn it_comes_back_up_when_there_is_room() {
        let mut d = DynamicResolution::on(60.0);
        assert_eq!(run(&mut d, 20.0, gpu_bound(30.0)), 0.6);
        // the scene got light (vsync holds the frame at 16.7 ms whatever the scale): a step up every probe
        assert_eq!(run(&mut d, 60.0, |_| 16.7), 1.0);
        // back in the heavy scene it settles where it was, and the probes up that fail come less and less often
        assert_eq!(run(&mut d, 30.0, gpu_bound(30.0)), 0.6);
        let mut ups = 0;
        let (mut t, mut last) = (0.0, d.scale());
        while t < 240_000.0 {
            let ms = gpu_bound(30.0)(d.scale());
            d.frame(ms);
            if d.scale() > last {
                ups += 1;
            }
            last = d.scale();
            t += ms;
        }
        assert!((2..=6).contains(&ups), "{ups} probes up in four minutes");
        assert!(d.scale() <= 0.72);
    }

    #[test]
    fn hitches_do_not_count() {
        let mut d = DynamicResolution::on(60.0);
        let mut t = 0.0;
        while t < 10_000.0 {
            // a 400 ms stall every half second among 10 ms frames
            d.frame(if (t as u32 / 10) % 50 == 0 { 400.0 } else { 10.0 });
            t += 10.0;
        }
        assert_eq!(d.scale(), 1.0);
    }
}
