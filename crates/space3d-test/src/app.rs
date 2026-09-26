//! [`TestApp`]: a deterministic, headless [`App`] wrapper for engine and game tests.

use crate::gpu::{self, GpuInfo};
use crate::input::{InputEvent, InputRecorder, InputRecorderPlugin, InputScript};
use space3d_core::glam::{Vec2, Vec3};
use space3d_core::hecs::Component;
use space3d_core::{App, Entity, Events, ImageData, Name, TickRate, Time, Transform};
use space3d_platform::{GamepadAxis, GamepadButton, Input, InputPlugin, KeyCode, MouseButton};
use space3d_render::{RenderPlugin, RenderStats, Renderer};
use std::ops::{Deref, DerefMut};
use std::time::Duration;

/// Whether a [`TestApp`] gets a headless renderer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenderMode {
    /// Create a headless renderer when a GPU adapter is available, otherwise run without one
    /// (screenshot assertions then skip).
    #[default]
    Auto,
    /// Never create a renderer (simulation-only tests; fastest).
    Disabled,
    /// Panic when no renderer can be created.
    Required,
}

/// Builder for [`TestApp`].
///
/// ```
/// use space3d_test::{RenderMode, TestApp};
/// let app = TestApp::builder().size(64, 48).tickrate(30.0).render(RenderMode::Disabled).build();
/// assert_eq!(app.size(), (64, 48));
/// assert!(!app.has_renderer());
/// ```
#[derive(Clone, Debug)]
pub struct TestAppBuilder {
    size: (u32, u32),
    tickrate: Option<f64>,
    render: RenderMode,
    input: bool,
    render_on_demand: bool,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self {
            size: (256, 256),
            tickrate: None,
            render: RenderMode::Auto,
            input: true,
            render_on_demand: false,
        }
    }
}

impl TestAppBuilder {
    /// Render target size in physical pixels (default 256x256). Also used as the input
    /// window size for touch regions.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.size = (width.max(1), height.max(1));
        self
    }

    /// Fixed tick rate in Hz (default: the app's current `TickRate`, 60 Hz for a new app).
    pub fn tickrate(mut self, hz: f64) -> Self {
        self.tickrate = Some(hz);
        self
    }

    /// Renderer policy (default [`RenderMode::Auto`]).
    pub fn render(mut self, mode: RenderMode) -> Self {
        self.render = mode;
        self
    }

    /// Shorthand for `render(RenderMode::Disabled)`.
    pub fn no_render(self) -> Self {
        self.render(RenderMode::Disabled)
    }

    /// Do not add `InputPlugin` (input helpers then panic).
    pub fn without_input(mut self) -> Self {
        self.input = false;
        self
    }

    /// Keep the renderer out of the frame loop and only render in [`TestApp::screenshot`] /
    /// [`TestApp::render_frame`]. Much faster for long simulations; per-frame `RenderStats`
    /// are then only updated on demand.
    pub fn render_on_demand(mut self) -> Self {
        self.render_on_demand = true;
        self
    }

    /// Build a fresh app.
    pub fn build(self) -> TestApp {
        self.build_with(App::new())
    }

    /// Wrap an existing app (e.g. one built by the game's own setup function). Adds
    /// `InputPlugin` and `RenderPlugin` (deduplicated if already present) and the renderer.
    pub fn build_with(self, mut app: App) -> TestApp {
        if let Some(hz) = self.tickrate {
            app.set_tickrate(TickRate::new(hz));
        }
        if self.input {
            app.add_plugin(InputPlugin);
            app.resources
                .expect_mut::<Input>()
                .set_window_size(Vec2::new(self.size.0 as f32, self.size.1 as f32));
        }
        app.add_plugin(RenderPlugin::default());
        let mut gpu = None;
        let mut renderer_error = None;
        if let Some(r) = app.resources.get::<Renderer>() {
            gpu = Some(GpuInfo::from_renderer(r));
        } else if self.render == RenderMode::Disabled {
            renderer_error = Some("rendering is disabled (built with RenderMode::Disabled / no_render())".to_string());
        } else {
            match create_renderer(&mut app, self.size) {
                Ok(info) => gpu = Some(info),
                Err(reason) if self.render == RenderMode::Required => {
                    panic!("TestApp: renderer required but unavailable: {reason}")
                }
                Err(reason) => {
                    if gpu::gpu_available() {
                        // An adapter exists, so this is a renderer bug rather than a CI machine
                        // without GPU: screenshot assertions will fail with this reason.
                        tracing::error!("TestApp: renderer creation failed: {reason}");
                    } else {
                        tracing::info!("TestApp without renderer: {reason}");
                    }
                    renderer_error = Some(format!("renderer creation failed: {reason}"));
                }
            }
        }
        let stashed = if self.render_on_demand {
            app.resources.remove::<Renderer>()
        } else {
            None
        };
        TestApp {
            app,
            size: self.size,
            pending: Vec::new(),
            gpu,
            renderer_error,
            stashed,
        }
    }
}

fn create_renderer(app: &mut App, size: (u32, u32)) -> Result<GpuInfo, String> {
    gpu::gpu_info().map_err(str::to_string)?;
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, size);
        Err("headless rendering is not available on wasm32".to_string())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        space3d_render::init_headless(app, size.0, size.1).map_err(|e| e.to_string())?;
        Ok(GpuInfo::from_renderer(app.resources.expect::<Renderer>()))
    }
}

/// Longest frame delta `App::update` accepts; longer frames are clamped.
const MAX_FRAME_DELTA: Duration = Duration::from_millis(250);

/// `run_until` gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("condition not met within {max_ticks} ticks")]
pub struct RunUntilTimeout {
    /// The tick budget that was exhausted.
    pub max_ticks: u64,
}

/// A deterministic headless [`App`] for tests.
///
/// * fixed timestep: every [`step`](Self::step) runs exactly one frame containing exactly one
///   `FixedUpdate` tick — never wall-clock driven (below 4 Hz a step spans several frames, see
///   [`step_ticks`](Self::step_ticks));
/// * `InputPlugin` + injectable input with scripted playback ([`play`](Self::play));
/// * `RenderPlugin` + a headless renderer when a GPU is available ([`RenderMode`]);
/// * world/resource helpers and screenshots ([`screenshot`](Self::screenshot),
///   [`assert_screenshot!`](crate::assert_screenshot)).
///
/// Derefs to [`App`], so `app.world`, `app.resources`, `app.add_system(..)` work directly.
///
/// ```
/// use space3d_core::prelude::*;
/// use space3d_platform::KeyCode;
/// use space3d_test::TestApp;
///
/// #[derive(Default)]
/// struct Jumps(u32);
///
/// let mut app = TestApp::builder().no_render().build();
/// app.init_resource::<Jumps>();
/// app.add_system(Stage::FixedUpdate, |ctx| {
///     if ctx.resources.expect::<space3d_platform::Input>().just_pressed(KeyCode::Space) {
///         ctx.resources.expect_mut::<Jumps>().0 += 1;
///     }
/// });
/// app.tap_key(KeyCode::Space);
/// app.step_ticks(5);
/// assert_eq!(app.resource::<Jumps>().0, 1);
/// assert_eq!(app.tick(), 6);
/// ```
pub struct TestApp {
    app: App,
    size: (u32, u32),
    /// Absolute tick → event, sorted by tick.
    pending: Vec<(u64, InputEvent)>,
    gpu: Option<GpuInfo>,
    /// Why there is no renderer (`None` when there is one).
    renderer_error: Option<String>,
    stashed: Option<Renderer>,
}

impl Default for TestApp {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for TestApp {
    type Target = App;
    fn deref(&self) -> &App {
        &self.app
    }
}

impl DerefMut for TestApp {
    fn deref_mut(&mut self) -> &mut App {
        &mut self.app
    }
}

impl std::fmt::Debug for TestApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestApp")
            .field("tick", &self.tick())
            .field("size", &self.size)
            .field("entities", &self.app.world.len())
            .field("gpu", &self.gpu)
            .field("pending_input", &self.pending.len())
            .finish()
    }
}

impl TestApp {
    /// Default app: 256x256, 60 Hz, input, renderer if a GPU is available.
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Default app with a `width`x`height` render target.
    pub fn headless(width: u32, height: u32) -> Self {
        Self::builder().size(width, height).build()
    }

    /// Default app without renderer.
    pub fn no_render() -> Self {
        Self::builder().no_render().build()
    }

    /// Builder.
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder::default()
    }

    /// Wrap an existing app with default settings (see [`TestAppBuilder::build_with`]).
    pub fn from_app(app: App) -> Self {
        Self::builder().build_with(app)
    }

    /// The wrapped app.
    pub fn app(&self) -> &App {
        &self.app
    }

    /// The wrapped app, mutably.
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Unwrap (pending scripted input is dropped; a stashed renderer is re-inserted).
    pub fn into_app(mut self) -> App {
        if let Some(r) = self.stashed.take() {
            self.app.insert_resource(r);
        }
        self.app
    }

    /// Render target size.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// A headless renderer exists.
    pub fn has_renderer(&self) -> bool {
        self.stashed.is_some() || self.app.resources.contains::<Renderer>()
    }

    /// Why the app has no renderer: rendering disabled, or the GPU probe / renderer creation
    /// error. `None` when a renderer exists.
    pub fn renderer_error(&self) -> Option<&str> {
        if self.has_renderer() {
            None
        } else {
            Some(self.renderer_error.as_deref().unwrap_or("the renderer was removed"))
        }
    }

    /// Adapter used by this app's renderer.
    pub fn gpu(&self) -> Option<&GpuInfo> {
        self.gpu.as_ref()
    }

    /// Backend name of the renderer (`metal`, `vulkan`, ...), used for golden variants.
    pub fn backend(&self) -> Option<&str> {
        self.gpu.as_ref().map(|g| g.backend.as_str())
    }

    // ---- time ----------------------------------------------------------------------------

    /// Fixed ticks executed so far.
    pub fn tick(&self) -> u64 {
        self.app.resources.expect::<Time>().tick
    }

    /// Run one frame with exactly one fixed tick.
    pub fn step(&mut self) {
        self.step_ticks(1);
    }

    /// Run `n` fixed ticks, one frame each, injecting scheduled input before each frame.
    ///
    /// `App::update` clamps a frame to 250 ms, so below 4 Hz one frame
    /// cannot carry a whole tick: the tick's duration is then fed as several frames, filler
    /// frames first and the ticking frame last (scheduled input is injected right before it).
    /// Every step still advances exactly one fixed tick.
    pub fn step_ticks(&mut self, n: u64) {
        for _ in 0..n {
            let step = self.app.resources.expect::<TickRate>().step();
            let mut left = step;
            while left > MAX_FRAME_DELTA {
                self.app.update(MAX_FRAME_DELTA);
                left -= MAX_FRAME_DELTA;
            }
            self.inject_due();
            self.app.update(left);
        }
    }

    /// Step until `condition` holds (checked before the first step and after every step).
    /// Returns the number of ticks stepped.
    pub fn run_until(
        &mut self,
        mut condition: impl FnMut(&TestApp) -> bool,
        max_ticks: u64,
    ) -> Result<u64, RunUntilTimeout> {
        for stepped in 0..=max_ticks {
            if condition(self) {
                return Ok(stepped);
            }
            if stepped < max_ticks {
                self.step();
            }
        }
        Err(RunUntilTimeout { max_ticks })
    }

    // ---- world & resources ----------------------------------------------------------------

    /// Clone of a component; panics with a clear message when missing.
    #[track_caller]
    pub fn get<C: Component + Clone>(&self, entity: Entity) -> C {
        self.try_get(entity).unwrap_or_else(|| {
            panic!(
                "entity {entity:?} has no {} (alive: {})",
                std::any::type_name::<C>(),
                self.app.world.contains(entity)
            )
        })
    }

    /// Clone of a component, if present.
    pub fn try_get<C: Component + Clone>(&self, entity: Entity) -> Option<C> {
        self.app.world.get::<&C>(entity).ok().map(|c| (*c).clone())
    }

    /// Entity has component `C`.
    pub fn has<C: Component>(&self, entity: Entity) -> bool {
        self.app.world.satisfies::<&C>(entity)
    }

    /// `Transform::translation` of an entity (panics if missing).
    #[track_caller]
    pub fn translation(&self, entity: Entity) -> Vec3 {
        self.get::<Transform>(entity).translation
    }

    /// All `(entity, C)` pairs, sorted by entity id (deterministic order).
    pub fn query_all<C: Component + Clone>(&self) -> Vec<(Entity, C)> {
        let mut v: Vec<(Entity, C)> = self
            .app
            .world
            .query::<(Entity, &C)>()
            .iter()
            .map(|(e, c)| (e, c.clone()))
            .collect();
        v.sort_by_key(|(e, _)| e.to_bits());
        v
    }

    /// Entities with component `C`, sorted by id.
    pub fn entities_with<C: Component>(&self) -> Vec<Entity> {
        let mut v: Vec<Entity> = self.app.world.query::<(Entity, &C)>().iter().map(|(e, _)| e).collect();
        v.sort_by_key(|e| e.to_bits());
        v
    }

    /// The only entity with component `C`; panics if there are zero or several.
    #[track_caller]
    pub fn single<C: Component + Clone>(&self) -> (Entity, C) {
        let mut all = self.query_all::<C>();
        match all.len() {
            1 => all.pop().expect("one"),
            n => panic!(
                "expected exactly one entity with {}, found {n}",
                std::any::type_name::<C>()
            ),
        }
    }

    /// Number of entities with component `C`.
    pub fn count<C: Component>(&self) -> usize {
        self.app.world.query::<&C>().iter().count()
    }

    /// First entity (lowest id) with `Name(name)`.
    pub fn find_by_name(&self, name: &str) -> Option<Entity> {
        self.app
            .world
            .query::<(Entity, &Name)>()
            .iter()
            .filter(|(_, n)| n.0 == name)
            .map(|(e, _)| e)
            .min_by_key(|e| e.to_bits())
    }

    /// Resource (panics with the type name if missing).
    #[track_caller]
    pub fn resource<T: 'static>(&self) -> &T {
        self.app.resources.expect::<T>()
    }

    /// Mutable resource (panics with the type name if missing).
    #[track_caller]
    pub fn resource_mut<T: 'static>(&mut self) -> &mut T {
        self.app.resources.expect_mut::<T>()
    }

    /// Resource, if present.
    pub fn try_resource<T: 'static>(&self) -> Option<&T> {
        self.app.resources.get::<T>()
    }

    /// Clones of all live events of type `E` (current and previous frame).
    pub fn events<E: Clone + 'static>(&self) -> Vec<E> {
        self.app
            .resources
            .get::<Events<E>>()
            .map(|ev| ev.iter().cloned().collect())
            .unwrap_or_default()
    }

    // ---- input ---------------------------------------------------------------------------

    /// The `Input` resource for direct injection (visible to the next step).
    #[track_caller]
    pub fn input(&mut self) -> &mut Input {
        self.app
            .resources
            .get_mut::<Input>()
            .expect("TestApp has no Input resource (built with `without_input()`?)")
    }

    /// Queue `script` relative to the current tick without stepping.
    pub fn schedule(&mut self, script: &InputScript) {
        let now = self.tick();
        for e in script.events() {
            let tick = now + e.tick;
            let at = self.pending.partition_point(|(t, _)| *t <= tick);
            self.pending.insert(at, (tick, e.event.clone()));
        }
    }

    /// Queue `script` and step [`InputScript::duration`] ticks.
    pub fn play(&mut self, script: &InputScript) {
        self.schedule(script);
        self.step_ticks(script.duration());
    }

    /// Scheduled events not yet injected.
    pub fn pending_input(&self) -> usize {
        self.pending.len()
    }

    fn inject_due(&mut self) {
        let now = self.tick();
        let due = self.pending.partition_point(|(t, _)| *t <= now);
        if due == 0 {
            return;
        }
        let events: Vec<(u64, InputEvent)> = self.pending.drain(..due).collect();
        let input = self.input();
        for (_, e) in events {
            e.apply(input);
        }
    }

    /// Key down now (visible from the next step; stays held).
    pub fn press_key(&mut self, key: KeyCode) {
        self.input().inject_key_down(key);
    }

    /// Key up now (visible from the next step).
    pub fn release_key(&mut self, key: KeyCode) {
        self.input().inject_key_up(key);
    }

    /// Hold `key` for exactly one tick: press, step 1, release (release visible next step).
    pub fn tap_key(&mut self, key: KeyCode) {
        self.hold_key(key, 1);
    }

    /// Hold `key` for `ticks` ticks: press, step `ticks`, release (visible next step).
    pub fn hold_key(&mut self, key: KeyCode, ticks: u64) {
        self.press_key(key);
        self.step_ticks(ticks);
        self.release_key(key);
    }

    /// Committed text (visible next step).
    pub fn type_text(&mut self, text: &str) {
        self.input().inject_text(text);
    }

    /// Move the cursor (visible next step).
    pub fn mouse_move(&mut self, position: Vec2) {
        self.input().inject_mouse_move(position);
    }

    /// Mouse button down now.
    pub fn mouse_press(&mut self, button: MouseButton) {
        self.input().inject_mouse_button_down(button);
    }

    /// Mouse button up now.
    pub fn mouse_release(&mut self, button: MouseButton) {
        self.input().inject_mouse_button_up(button);
    }

    /// Move to `position`, press for one tick, release (visible next step).
    pub fn click(&mut self, button: MouseButton, position: Vec2) {
        self.mouse_move(position);
        self.mouse_press(button);
        self.step();
        self.mouse_release(button);
    }

    /// Mouse drag over `ticks` ticks (see [`InputScript::drag`]); steps `ticks + 2`.
    pub fn drag(&mut self, button: MouseButton, from: Vec2, to: Vec2, ticks: u64) {
        self.play(&InputScript::new().drag(0, button, from, to, ticks));
    }

    /// Scroll now.
    pub fn wheel(&mut self, delta: Vec2) {
        self.input().inject_wheel(delta);
    }

    /// Touch tap with a fresh finger id; steps 2 ticks.
    pub fn touch_tap(&mut self, position: Vec2) {
        let id = self.next_touch_id();
        self.play(&InputScript::new().touch_tap(0, id, position));
    }

    /// One-finger swipe over `ticks` ticks; steps `ticks + 2`.
    pub fn swipe(&mut self, from: Vec2, to: Vec2, ticks: u64) {
        let id = self.next_touch_id();
        self.play(&InputScript::new().swipe(0, id, from, to, ticks));
    }

    /// Two-finger pinch around `center` from `start_distance` to `end_distance` pixels apart
    /// over `ticks` ticks; steps `ticks + 2`.
    pub fn pinch(&mut self, center: Vec2, start_distance: f32, end_distance: f32, ticks: u64) {
        let id = self.next_touch_id();
        self.play(&InputScript::new().pinch(0, id, center, start_distance, end_distance, ticks));
    }

    fn next_touch_id(&self) -> u64 {
        let input = self.app.resources.get::<Input>();
        input
            .and_then(|i| i.touches().iter().map(|t| t.id).max())
            .map_or(1, |m| m + 2)
    }

    /// Hold a gamepad button for `ticks` ticks, then release (release visible next step).
    pub fn gamepad_press(&mut self, pad: u32, button: GamepadButton, ticks: u64) {
        self.input().inject_gamepad_button(pad, button, true);
        self.step_ticks(ticks);
        self.input().inject_gamepad_button(pad, button, false);
    }

    /// Set a gamepad axis now (stays until changed).
    pub fn set_gamepad_axis(&mut self, pad: u32, axis: GamepadAxis, value: f32) {
        self.input().inject_gamepad_axis(pad, axis, value);
    }

    /// Start recording input into an [`InputScript`] (adds [`InputRecorderPlugin`]).
    pub fn start_recording(&mut self) {
        self.app.add_plugin(InputRecorderPlugin::default());
        let tick = self.tick();
        self.app.resources.expect_mut::<InputRecorder>().start(tick);
    }

    /// Stop recording and return the script (empty if recording never started).
    pub fn stop_recording(&mut self) -> InputScript {
        self.app
            .resources
            .get_mut::<InputRecorder>()
            .map(InputRecorder::stop)
            .unwrap_or_default()
    }

    // ---- rendering -----------------------------------------------------------------------

    /// Run `f` with the renderer in the app's resources, also when it is kept out of the frame
    /// loop ([`TestAppBuilder::render_on_demand`]): setup code that sizes things from
    /// `Renderer::size` or reads its capabilities sees the renderer the screenshots use.
    pub fn with_renderer<R>(&mut self, f: impl FnOnce(&mut App) -> R) -> R {
        if let Some(r) = self.stashed.take() {
            self.app.insert_resource(r);
            let out = f(&mut self.app);
            self.stashed = self.app.resources.remove::<Renderer>();
            out
        } else {
            f(&mut self.app)
        }
    }

    /// Render the current state (without advancing time) and read it back. `None` without a
    /// renderer (no GPU, `RenderMode::Disabled`, wasm32).
    pub fn screenshot(&mut self) -> Option<ImageData> {
        self.with_renderer(space3d_render::render_to_image)
    }

    /// Render one frame without advancing time and return its stats (`None` without renderer).
    pub fn render_frame(&mut self) -> Option<RenderStats> {
        self.with_renderer(|app| {
            space3d_core::transform::propagate_transforms(&mut app.world);
            let mut renderer = app.resources.remove::<Renderer>()?;
            renderer.render_frame(&app.world, &mut app.resources);
            app.resources.insert(renderer);
            app.resources.get::<RenderStats>().cloned()
        })
    }

    /// Stats of the most recently rendered frame.
    pub fn render_stats(&self) -> Option<&RenderStats> {
        self.app.resources.get::<RenderStats>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::Stage;
    use space3d_platform::{ActionMap, ActionState, Binding};

    #[derive(Default)]
    struct Log(Vec<(u64, &'static str)>);

    fn logging_app() -> TestApp {
        let mut app = TestApp::no_render();
        app.init_resource::<Log>();
        app.resources
            .expect_mut::<ActionMap>()
            .bind("jump", Binding::Key(KeyCode::Space))
            .bind("fire", Binding::Mouse(MouseButton::Left));
        app.add_system(Stage::FixedUpdate, |ctx| {
            let tick = ctx.time().tick;
            let a = ctx.resources.expect::<ActionState>().clone();
            let log = ctx.resources.expect_mut::<Log>();
            if a.just_pressed("jump") {
                log.0.push((tick, "jump"));
            }
            if a.just_released("jump") {
                log.0.push((tick, "jump-up"));
            }
            if a.just_pressed("fire") {
                log.0.push((tick, "fire"));
            }
        });
        app
    }

    #[test]
    fn scripted_input_lands_on_exact_ticks() {
        let mut app = logging_app();
        app.step_ticks(3);
        app.play(
            &InputScript::new()
                .hold_key(2, KeyCode::Space, 3)
                .click(1, MouseButton::Left, Vec2::ONE),
        );
        assert_eq!(app.tick(), 3 + 6);
        assert_eq!(app.resource::<Log>().0, vec![(4, "fire"), (5, "jump"), (8, "jump-up")]);
        assert_eq!(app.pending_input(), 0);
    }

    #[test]
    fn schedule_does_not_step() {
        let mut app = logging_app();
        app.schedule(&InputScript::new().tap_key(4, KeyCode::Space));
        assert_eq!((app.tick(), app.pending_input()), (0, 2));
        let ticks = app.run_until(|a| !a.resource::<Log>().0.is_empty(), 100).unwrap();
        assert_eq!(ticks, 5);
        assert_eq!(app.resource::<Log>().0, vec![(4, "jump")]);
        assert_eq!(app.run_until(|_| false, 3), Err(RunUntilTimeout { max_ticks: 3 }));
        assert_eq!(app.run_until(|_| true, 0), Ok(0));
    }

    #[test]
    fn immediate_helpers() {
        let mut app = logging_app();
        app.tap_key(KeyCode::Space);
        app.step();
        app.click(MouseButton::Left, Vec2::new(3.0, 4.0));
        assert_eq!(app.resource::<Log>().0, vec![(0, "jump"), (1, "jump-up"), (2, "fire")]);
        assert_eq!(app.resource::<Input>().mouse_position(), Some(Vec2::new(3.0, 4.0)));
        app.hold_key(KeyCode::KeyA, 2);
        app.step();
        assert!(!app.resource::<Input>().pressed(KeyCode::KeyA));
    }

    #[test]
    fn touch_gestures_reach_input() {
        #[derive(Default)]
        struct Touches {
            max_fingers: usize,
            ended: Vec<(Vec2, Vec2)>,
        }
        let mut app = TestApp::no_render();
        app.init_resource::<Touches>();
        app.add_system(Stage::Update, |ctx| {
            let (n, ended): (usize, Vec<(Vec2, Vec2)>) = {
                let i = ctx.resources.expect::<Input>();
                (
                    i.touches().len(),
                    i.just_ended_touches().iter().map(|t| (t.start, t.position)).collect(),
                )
            };
            let t = ctx.resources.expect_mut::<Touches>();
            t.max_fingers = t.max_fingers.max(n);
            t.ended.extend(ended);
        });
        app.swipe(Vec2::new(10.0, 10.0), Vec2::new(110.0, 10.0), 5);
        assert_eq!(app.tick(), 7);
        app.pinch(Vec2::new(100.0, 100.0), 20.0, 80.0, 4);
        app.touch_tap(Vec2::new(5.0, 5.0));
        let t = app.resource::<Touches>();
        assert_eq!(t.max_fingers, 2);
        assert_eq!(t.ended[0], (Vec2::new(10.0, 10.0), Vec2::new(110.0, 10.0)));
        assert_eq!(t.ended[1], (Vec2::new(90.0, 100.0), Vec2::new(60.0, 100.0)));
        assert_eq!(t.ended[2], (Vec2::new(110.0, 100.0), Vec2::new(140.0, 100.0)));
        assert_eq!(t.ended.len(), 4);
        assert!(app.resource::<Input>().touches().is_empty());
    }

    #[test]
    fn gamepad_and_recording_roundtrip() {
        let script = InputScript::new()
            .hold_key(1, KeyCode::ArrowUp, 4)
            .key_tap_same_frame(2, KeyCode::Enter)
            .gamepad_press(3, 0, GamepadButton::South, 2)
            .gamepad_axis(3, 0, GamepadAxis::LeftStickY, -0.5)
            .swipe(4, 9, Vec2::ZERO, Vec2::new(8.0, 0.0), 2)
            .drag(6, MouseButton::Right, Vec2::ONE, Vec2::new(9.0, 1.0), 2)
            .wheel(7, Vec2::new(0.0, 1.0))
            .type_text(8, "ok");
        let mut app = TestApp::no_render();
        app.step_ticks(2);
        app.start_recording();
        app.play(&script);
        let recorded = app.stop_recording();
        // The recorder captures the initial window size and makes the (implicit on first use)
        // gamepad connection explicit.
        let expected = script
            .clone()
            .at(0, InputEvent::WindowSize(Vec2::new(256.0, 256.0)))
            .at(3, InputEvent::GamepadConnected(0));
        let norm = |s: &InputScript| {
            let mut v: Vec<String> = s.events().iter().map(|e| format!("{}:{:?}", e.tick, e.event)).collect();
            v.sort();
            v
        };
        assert_eq!(
            norm(&recorded),
            norm(&expected),
            "recorded:\n{}",
            recorded.to_ron().unwrap()
        );

        // Replaying the recording reproduces the same input state history.
        let mut replay = TestApp::no_render();
        replay.step_ticks(2);
        replay.play(&recorded);
        assert_eq!(replay.tick(), app.tick());
        let pad = replay
            .resource::<Input>()
            .gamepad(0)
            .unwrap()
            .axis(GamepadAxis::LeftStickY);
        assert_eq!(pad, -0.5);
    }

    #[test]
    fn world_helpers() {
        let mut app = TestApp::no_render();
        let a = app.world.spawn((Name("a".into()), Transform::from_xyz(1.0, 2.0, 3.0)));
        let b = app.world.spawn((Name("b".into()), 5u32));
        assert_eq!(app.find_by_name("b"), Some(b));
        assert_eq!(app.find_by_name("zzz"), None);
        assert_eq!(app.translation(a), Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(app.single::<u32>(), (b, 5));
        assert_eq!(app.count::<Name>(), 2);
        assert_eq!(app.entities_with::<Name>(), vec![a, b]);
        assert!(app.has::<u32>(b) && !app.has::<u32>(a));
        assert_eq!(app.try_get::<u32>(a), None);
        assert_eq!(app.query_all::<Name>()[1].1, Name("b".into()));
        app.add_event::<u8>();
        app.resource_mut::<Events<u8>>().send(7);
        assert_eq!(app.events::<u8>(), vec![7]);
        assert!(app.try_resource::<String>().is_none());
        assert!(!app.has_renderer());
        assert!(app.screenshot().is_none());
        assert!(app.render_frame().is_none());
        let s = format!("{app:?}");
        assert!(s.contains("TestApp"), "{s}");
    }

    #[test]
    fn slow_tickrates_still_run_one_tick_per_step() {
        for hz in [60.0, 5.0, 4.0, 3.0, 1.0, 0.25] {
            let mut app = logging_app();
            app.set_tickrate(TickRate::new(hz));
            app.step_ticks(10);
            assert_eq!(app.tick(), 10, "{hz} Hz");
            assert_eq!(app.run_until(|a| a.tick() >= 20, 10), Ok(10), "{hz} Hz");
            let before = app.resource::<Time>().elapsed;
            app.step();
            let dt = app.resource::<Time>().elapsed - before;
            assert_eq!(
                dt,
                TickRate::new(hz).step(),
                "a step advances time by one tick at {hz} Hz"
            );
            // Scripted input reaches FixedUpdate on the exact tick.
            app.play(&InputScript::new().tap_key(2, KeyCode::Space));
            assert_eq!(app.tick(), 21 + 4, "{hz} Hz");
            assert_eq!(app.resource::<Log>().0, vec![(23, "jump"), (24, "jump-up")], "{hz} Hz");
        }
        let app = TestApp::builder().tickrate(1.0).no_render().build();
        assert_eq!(app.resource::<TickRate>().hz, 1.0);
    }

    #[test]
    #[should_panic(expected = "has no u64")]
    fn get_missing_component_panics_clearly() {
        let mut app = TestApp::no_render();
        let e = app.world.spawn((1u32,));
        let _ = app.get::<u64>(e);
    }

    #[test]
    fn tickrate_and_wrapping() {
        let mut game = App::new();
        game.insert_resource(41u32);
        let mut app = TestApp::builder().tickrate(30.0).no_render().build_with(game);
        assert_eq!(app.resource::<TickRate>().hz, 30.0);
        app.add_system(Stage::FixedUpdate, |ctx| *ctx.resources.expect_mut::<u32>() += 1);
        app.step();
        assert_eq!(*app.resource::<u32>(), 42);
        let inner = app.into_app();
        assert_eq!(inner.resources.expect::<Time>().tick, 1);
    }
}
