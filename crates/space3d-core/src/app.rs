use crate::events::Events;
use crate::resources::Resources;
use crate::schedule::{NamedSystem, Schedule, Stage, SystemCtx};
use crate::time::{TickRate, Time};
use hecs::World;
use std::collections::HashSet;
use std::time::Duration;
use web_time::Instant;

/// Reusable unit of engine/game functionality.
pub trait Plugin: 'static {
    fn build(&self, app: &mut App);
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// Insert (or send `AppExit` event) to stop the runner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppExit;

pub type Runner = Box<dyn FnOnce(App)>;

/// The engine application: world + resources + schedule + runner.
pub struct App {
    pub world: World,
    pub resources: Resources,
    schedule: Schedule,
    plugins: HashSet<&'static str>,
    started: bool,
    runner: Option<Runner>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            world: World::new(),
            resources: Resources::default(),
            schedule: Schedule::default(),
            plugins: HashSet::new(),
            started: false,
            runner: Some(Box::new(headless_runner)),
        };
        app.resources.insert(TickRate::default());
        app.resources.insert(Time {
            fixed_delta: TickRate::default().step(),
            ..Default::default()
        });
        app.add_event::<AppExit>();
        app.add_system_named(Stage::PostUpdate, "core::propagate_transforms", |ctx| {
            crate::transform::propagate_transforms(ctx.world)
        });
        app
    }

    pub fn with_tickrate(mut self, hz: f64) -> Self {
        self.set_tickrate(TickRate::new(hz));
        self
    }

    pub fn set_tickrate(&mut self, rate: TickRate) {
        self.resources.insert(rate);
        self.resources.expect_mut::<Time>().fixed_delta = rate.step();
    }

    pub fn add_plugin(&mut self, plugin: impl Plugin) -> &mut Self {
        if self.plugins.insert(plugin.name()) {
            tracing::debug!(plugin = plugin.name(), "adding plugin");
            plugin.build(self);
        }
        self
    }

    pub fn has_plugin(&self, name: &str) -> bool {
        self.plugins.contains(name)
    }

    pub fn insert_resource<T: Send + Sync + 'static>(&mut self, value: T) -> &mut Self {
        self.resources.insert(value);
        self
    }

    pub fn init_resource<T: Default + Send + Sync + 'static>(&mut self) -> &mut Self {
        self.resources.init::<T>();
        self
    }

    /// Register an event type so its buffers are swapped every frame.
    pub fn add_event<T: Send + Sync + 'static>(&mut self) -> &mut Self {
        if !self.resources.contains::<Events<T>>() {
            self.resources.insert(Events::<T>::default());
            self.add_system_named(Stage::Last, std::any::type_name::<Events<T>>(), |ctx| {
                ctx.resources.expect_mut::<Events<T>>().update()
            });
        }
        self
    }

    pub fn add_system(&mut self, stage: Stage, system: impl FnMut(&mut SystemCtx) + Send + 'static) -> &mut Self {
        self.add_system_named(stage, std::any::type_name_of_val(&system), system)
    }

    pub fn add_system_named(
        &mut self,
        stage: Stage,
        name: &'static str,
        system: impl FnMut(&mut SystemCtx) + Send + 'static,
    ) -> &mut Self {
        self.add_system_ordered(stage, 0, name, system)
    }

    /// Register `system` in `stage` at position `order`: the systems of a stage run by
    /// ascending `order`, ties in registration order. [`Self::add_system`] and
    /// [`Self::add_system_named`] register at order 0, so their relative order is the
    /// registration order.
    ///
    /// Plugins use a positive order for systems that must see what the stage's gameplay systems
    /// did this tick, whenever those were added (the particle tick follows emitters to where
    /// gameplay moved them: `space3d_particles::ParticlesPlugin::TICK_ORDER`), and a negative one
    /// for systems that feed them.
    pub fn add_system_ordered(
        &mut self,
        stage: Stage,
        order: i32,
        name: &'static str,
        system: impl FnMut(&mut SystemCtx) + Send + 'static,
    ) -> &mut Self {
        let systems = &mut self.schedule.stages[stage.index()];
        let at = systems.partition_point(|s| s.order <= order);
        systems.insert(
            at,
            NamedSystem {
                name,
                order,
                run: Box::new(system),
            },
        );
        self
    }

    /// Names of the systems of `stage`, in run order.
    pub fn system_names(&self, stage: Stage) -> Vec<&'static str> {
        self.schedule.stages[stage.index()].iter().map(|s| s.name).collect()
    }

    pub fn set_runner(&mut self, runner: impl FnOnce(App) + 'static) -> &mut Self {
        self.runner = Some(Box::new(runner));
        self
    }

    /// Hand control to the runner (headless by default; platform plugin swaps in winit/web).
    pub fn run(mut self) {
        let runner = self.runner.take().expect("runner already taken");
        runner(self)
    }

    pub fn should_exit(&self) -> bool {
        self.resources.get::<Events<AppExit>>().is_some_and(|e| !e.is_empty())
    }

    pub fn exit(&mut self) {
        self.resources.expect_mut::<Events<AppExit>>().send(AppExit);
    }

    pub fn run_stage(&mut self, stage: Stage) {
        self.schedule.run_stage(stage, &mut self.world, &mut self.resources);
    }

    /// Advance one frame by real `dt`, running as many fixed ticks as fit.
    pub fn update(&mut self, dt: Duration) {
        let _span = tracing::trace_span!("frame").entered();
        let work_start = Instant::now();
        if !self.started {
            self.started = true;
            self.run_stage(Stage::Startup);
        }
        let rate = *self.resources.expect::<TickRate>();
        let step = rate.step();
        {
            let t = self.resources.expect_mut::<Time>();
            let dt = dt.min(Duration::from_millis(250));
            t.delta = dt;
            t.elapsed += dt;
            t.frame += 1;
            t.fixed_delta = step;
            t.accumulator += dt;
        }
        self.run_stage(Stage::PreUpdate);
        let mut steps = 0;
        while self.resources.expect::<Time>().accumulator >= step {
            if steps == rate.max_steps_per_frame {
                let t = self.resources.expect_mut::<Time>();
                let dropped = (t.accumulator.as_nanos() / step.as_nanos()) as u64;
                t.dropped_ticks += dropped;
                t.accumulator = Duration::from_nanos((t.accumulator.as_nanos() % step.as_nanos()) as u64);
                tracing::warn!(dropped, "simulation falling behind; dropping fixed ticks");
                break;
            }
            self.run_stage(Stage::FixedUpdate);
            let t = self.resources.expect_mut::<Time>();
            t.accumulator -= step;
            t.tick += 1;
            steps += 1;
        }
        {
            let t = self.resources.expect_mut::<Time>();
            t.alpha = (t.accumulator.as_secs_f64() / step.as_secs_f64()) as f32;
        }
        self.run_stage(Stage::Update);
        self.run_stage(Stage::PostUpdate);
        self.run_stage(Stage::Render);
        self.run_stage(Stage::Last);
        self.resources.expect_mut::<Time>().update_duration = work_start.elapsed();
    }

    /// Deterministically run exactly `n` fixed ticks (one frame each). For tests/replays.
    pub fn step_ticks(&mut self, n: u64) {
        let step = self.resources.expect::<TickRate>().step();
        for _ in 0..n {
            self.update(step);
        }
    }
}

/// Environment variable selecting [`HeadlessPacing::Unpaced`] for [`headless_runner`]
/// (`1`/`true`/`yes`/`on`).
pub const ENV_HEADLESS_UNPACED: &str = "SPACE3D_HEADLESS_UNPACED";

/// How [`headless_runner`] paces frames. Insert as a resource before [`App::run`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeadlessPacing {
    /// Real time: one frame per fixed step, paced against deadlines so OS sleep overshoot
    /// (4–5 ms on macOS) does not accumulate. Frame-to-frame `Time::delta` reflects wall
    /// time; use `Time::update_duration` for the actual CPU work of a frame.
    #[default]
    Realtime,
    /// As fast as possible: no sleeping, and every frame advances time by exactly one fixed
    /// step (deterministic; for soak tests, profiling and headless benchmarks). Also selected
    /// by [`ENV_HEADLESS_UNPACED`].
    Unpaced,
}

impl HeadlessPacing {
    /// The pacing for `app`: the resource if present, else [`ENV_HEADLESS_UNPACED`].
    pub fn for_app(app: &App) -> Self {
        if let Some(p) = app.resources.get::<HeadlessPacing>() {
            return *p;
        }
        let unpaced = std::env::var(ENV_HEADLESS_UNPACED)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        if unpaced { Self::Unpaced } else { Self::Realtime }
    }
}

/// Deadline pacing for real-time loops: frame `n` is due at `start + n × step`, so a late
/// wake-up shortens the next sleep instead of delaying every later frame.
#[derive(Clone, Copy, Debug)]
pub struct Pacer {
    deadline: Instant,
}

impl Pacer {
    /// Frames beyond this many steps behind are dropped (the loop resynchronises instead of
    /// running a burst of catch-up frames).
    pub const MAX_LAG_STEPS: u32 = 4;

    pub fn new(start: Instant) -> Self {
        Self { deadline: start }
    }

    /// Call after a frame finished at `now`: how long to sleep before the next one.
    pub fn next_sleep(&mut self, now: Instant, step: Duration) -> Option<Duration> {
        self.deadline += step;
        if self.deadline > now {
            Some(self.deadline - now)
        } else {
            if now - self.deadline > step * Self::MAX_LAG_STEPS {
                self.deadline = now;
            }
            None
        }
    }
}

/// Default runner: loop without a window until [`AppExit`], paced per [`HeadlessPacing`]
/// (real time by default).
pub fn headless_runner(mut app: App) {
    let pacing = HeadlessPacing::for_app(&app);
    let mut last = Instant::now();
    let mut pacer = Pacer::new(last);
    loop {
        let step = app.resources.expect::<TickRate>().step();
        if pacing == HeadlessPacing::Unpaced {
            app.update(step);
            if app.should_exit() {
                break;
            }
            continue;
        }
        let now = Instant::now();
        app.update(now - last);
        last = now;
        if app.should_exit() {
            break;
        }
        if let Some(sleep) = pacer.next_sleep(Instant::now(), step) {
            std::thread::sleep(sleep);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GlobalTransform, Transform};

    #[derive(Default)]
    struct Counter {
        fixed: u32,
        frames: u32,
    }

    #[test]
    fn ordered_systems_run_by_order_then_registration() {
        #[derive(Default)]
        struct Log(Vec<&'static str>);
        let mut app = App::new();
        app.init_resource::<Log>();
        let push = |name: &'static str| move |c: &mut SystemCtx| c.resources.expect_mut::<Log>().0.push(name);
        app.add_system_ordered(Stage::FixedUpdate, 10, "late", push("late"));
        app.add_system_named(Stage::FixedUpdate, "a", push("a"));
        app.add_system_ordered(Stage::FixedUpdate, -5, "early", push("early"));
        app.add_system_named(Stage::FixedUpdate, "b", push("b"));
        app.add_system_ordered(Stage::FixedUpdate, 10, "later", push("later"));
        app.add_system_ordered(Stage::FixedUpdate, 0, "c", push("c"));
        let order = ["early", "a", "b", "c", "late", "later"];
        assert_eq!(app.system_names(Stage::FixedUpdate), order);
        app.step_ticks(1);
        assert_eq!(app.resources.expect::<Log>().0, order);
    }

    #[test]
    fn fixed_ticks_follow_tickrate() {
        let mut app = App::new().with_tickrate(50.0);
        app.init_resource::<Counter>();
        app.add_system(Stage::FixedUpdate, |c| c.resources.expect_mut::<Counter>().fixed += 1);
        app.add_system(Stage::Update, |c| c.resources.expect_mut::<Counter>().frames += 1);
        // 10 frames of 10ms = 100ms = 5 ticks at 50Hz
        for _ in 0..10 {
            app.update(Duration::from_millis(10));
        }
        let c = app.resources.expect::<Counter>();
        assert_eq!((c.fixed, c.frames), (5, 10));
        assert_eq!(app.resources.expect::<Time>().tick, 5);
    }

    #[test]
    fn spiral_of_death_is_capped() {
        let mut app = App::new().with_tickrate(1000.0);
        app.init_resource::<Counter>();
        app.add_system(Stage::FixedUpdate, |c| c.resources.expect_mut::<Counter>().fixed += 1);
        app.update(Duration::from_millis(200));
        assert_eq!(app.resources.expect::<Counter>().fixed, 8);
        assert!(app.resources.expect::<Time>().dropped_ticks > 0);
    }

    #[test]
    fn a_long_frame_backlog_can_be_discarded_from_pre_update() {
        let mut app = App::new().with_tickrate(50.0);
        app.init_resource::<Counter>();
        app.add_system(Stage::FixedUpdate, |c| c.resources.expect_mut::<Counter>().fixed += 1);
        app.add_system(Stage::PreUpdate, |c| {
            let t = c.resources.expect_mut::<Time>();
            let pending = t.pending_steps();
            if pending > 2 {
                assert_eq!(t.discard_backlog(1), pending - 1);
                assert_eq!(t.pending_steps(), 1);
            }
        });
        // Inside a fixed run the accumulator still holds that run's step.
        app.add_system(Stage::FixedUpdate, |c| {
            let t = c.resources.expect::<Time>();
            assert!(t.accumulated() >= t.fixed_delta, "{:?}", t.accumulated());
        });
        // 110 ms = 5 steps (+10 ms): only one runs, the fraction stays for the next frame.
        app.update(Duration::from_millis(110));
        assert_eq!(app.resources.expect::<Time>().accumulated(), Duration::from_millis(10));
        assert_eq!(app.resources.expect::<Counter>().fixed, 1);
        assert!((app.resources.expect::<Time>().alpha - 0.5).abs() < 1e-3);
        app.update(Duration::from_millis(10));
        assert_eq!(app.resources.expect::<Counter>().fixed, 2);
        assert_eq!(app.resources.expect::<Time>().dropped_ticks, 0);
    }

    #[test]
    fn startup_once_and_exit() {
        let mut app = App::new();
        app.init_resource::<Counter>();
        app.add_system(Stage::Startup, |c| c.resources.expect_mut::<Counter>().frames += 100);
        app.add_system(Stage::Update, |c| c.send(AppExit));
        app.step_ticks(3);
        assert_eq!(app.resources.expect::<Counter>().frames, 100);
        assert!(app.should_exit());
    }

    #[test]
    fn transforms_propagate_to_children() {
        let mut app = App::new();
        let parent = app.world.spawn((Transform::from_xyz(1.0, 0.0, 0.0),));
        let child = app
            .world
            .spawn((Transform::from_xyz(0.0, 2.0, 0.0), crate::transform::Parent(parent)));
        app.step_ticks(1);
        let g = app.world.get::<&GlobalTransform>(child).unwrap().0;
        assert_eq!(g.w_axis.truncate(), glam::Vec3::new(1.0, 2.0, 0.0));
    }

    #[test]
    fn update_duration_measures_the_frame_work() {
        let mut app = App::new();
        app.add_system(Stage::Update, |_| std::thread::sleep(Duration::from_millis(3)));
        app.step_ticks(2);
        assert!(app.resources.expect::<Time>().update_duration >= Duration::from_millis(3));
    }

    #[test]
    fn pacer_absorbs_oversleep_and_resyncs_when_far_behind() {
        let step = Duration::from_millis(10);
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        // Frame took 2 ms: sleep the remaining 8.
        assert_eq!(
            p.next_sleep(t0 + Duration::from_millis(2), step),
            Some(Duration::from_millis(8))
        );
        // Woke 5 ms late (overslept) and worked 2 ms: the next sleep is shortened to 3 ms,
        // so frames stay on the 10 ms grid instead of drifting to 15 ms.
        assert_eq!(
            p.next_sleep(t0 + Duration::from_millis(17), step),
            Some(Duration::from_millis(3))
        );
        // Way behind (a 200 ms hitch): no sleep, and the grid restarts from now.
        assert_eq!(p.next_sleep(t0 + Duration::from_millis(230), step), None);
        assert_eq!(
            p.next_sleep(t0 + Duration::from_millis(231), step),
            Some(Duration::from_millis(9))
        );
    }

    #[test]
    fn unpaced_runner_is_fast_and_steps_exactly() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Option<(u64, Duration)>>> = Arc::default();
        let out = seen.clone();
        let mut app = App::new().with_tickrate(60.0);
        app.insert_resource(HeadlessPacing::Unpaced);
        app.init_resource::<Counter>();
        app.add_system(Stage::Update, move |c| {
            let k = c.resources.expect_mut::<Counter>();
            k.frames += 1;
            if k.frames == 600 {
                let t = c.resources.expect::<Time>();
                *out.lock().unwrap() = Some((t.tick, t.elapsed));
                c.send(AppExit);
            }
        });
        let started = Instant::now();
        headless_runner(app); // the real runner
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "unpaced run must not sleep 10 s of game time"
        );
        let (tick, elapsed) = seen.lock().unwrap().expect("ran 600 frames");
        assert_eq!(tick, 600);
        assert_eq!(elapsed, TickRate::new(60.0).step() * 600);
    }

    struct P;
    impl Plugin for P {
        fn build(&self, app: &mut App) {
            app.resources.init::<Counter>().frames += 1;
        }
    }
    #[test]
    fn plugins_are_deduplicated() {
        let mut app = App::new();
        app.add_plugin(P).add_plugin(P);
        assert_eq!(app.resources.expect::<Counter>().frames, 1);
    }
}
