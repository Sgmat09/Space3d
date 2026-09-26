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
    }

    /// Deterministically run exactly `n` fixed ticks (one frame each). For tests/replays.
    pub fn step_ticks(&mut self, n: u64) {
        let step = self.resources.expect::<TickRate>().step();
        for _ in 0..n {
            self.update(step);
        }
    }
}

/// Default runner: real-time loop without a window, until [`AppExit`].
pub fn headless_runner(mut app: App) {
    let mut last = Instant::now();
    loop {
        let now = Instant::now();
        app.update(now - last);
        last = now;
        if app.should_exit() {
            break;
        }
        let step = app.resources.expect::<TickRate>().step();
        let spent = Instant::now() - now;
        if spent < step {
            std::thread::sleep(step - spent);
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
        // 110 ms = 5 steps (+10 ms): only one runs, the fraction stays for the next frame.
        app.update(Duration::from_millis(110));
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
