//! Input state ([`Input`]), injectable for tests, plus named actions ([`ActionMap`]).
//!
//! The runner feeds window events into [`Input`] between frames; systems read it during
//! the frame; `Stage::Last` clears the per-frame edges (`just_pressed`, deltas, text).
//! Tests and replays drive the same resource directly with the `inject_*` methods:
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_platform::*;
//! let mut app = App::new();
//! app.add_plugin(InputPlugin);
//! app.resources.expect_mut::<ActionMap>().bind("jump", Binding::Key(KeyCode::Space));
//! app.resources.expect_mut::<Input>().inject_key_down(KeyCode::Space);
//! app.step_ticks(1);
//! assert!(app.resources.expect::<ActionState>().just_pressed("jump"));
//! app.step_ticks(1); // key still held, edge gone
//! assert!(app.resources.expect::<ActionState>().pressed("jump"));
//! assert!(!app.resources.expect::<ActionState>().just_pressed("jump"));
//! ```

use glam::Vec2;
use space3d_core::{App, Plugin, Stage};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

pub use winit::keyboard::KeyCode;

/// Mouse buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Primary.
    Left,
    /// Secondary.
    Right,
    /// Wheel click.
    Middle,
    /// Browser back.
    Back,
    /// Browser forward.
    Forward,
    /// Anything else.
    Other(u16),
}

impl From<winit::event::MouseButton> for MouseButton {
    fn from(b: winit::event::MouseButton) -> Self {
        use winit::event::MouseButton as W;
        match b {
            W::Left => Self::Left,
            W::Right => Self::Right,
            W::Middle => Self::Middle,
            W::Back => Self::Back,
            W::Forward => Self::Forward,
            W::Other(n) => Self::Other(n),
        }
    }
}

/// Touch lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TouchPhase {
    /// Finger down.
    Started,
    /// Finger moved.
    Moved,
    /// Finger lifted.
    Ended,
    /// System cancelled the touch.
    Cancelled,
}

impl From<winit::event::TouchPhase> for TouchPhase {
    fn from(p: winit::event::TouchPhase) -> Self {
        use winit::event::TouchPhase as W;
        match p {
            W::Started => Self::Started,
            W::Moved => Self::Moved,
            W::Ended => Self::Ended,
            W::Cancelled => Self::Cancelled,
        }
    }
}

/// One active finger.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Touch {
    /// Stable id while the finger is down.
    pub id: u64,
    /// Position in physical pixels.
    pub position: Vec2,
    /// Position when the touch started.
    pub start: Vec2,
    /// Movement since last frame.
    pub delta: Vec2,
}

/// Gamepad buttons (stub layout; no backend is wired yet — inject for tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GamepadButton {
    /// A / Cross.
    South,
    /// B / Circle.
    East,
    /// Y / Triangle.
    North,
    /// X / Square.
    West,
    /// Left bumper.
    LeftShoulder,
    /// Right bumper.
    RightShoulder,
    /// Start / Options.
    Start,
    /// Select / Share.
    Select,
    /// D-pad up.
    DPadUp,
    /// D-pad down.
    DPadDown,
    /// D-pad left.
    DPadLeft,
    /// D-pad right.
    DPadRight,
}

/// Gamepad axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GamepadAxis {
    /// Left stick horizontal.
    LeftStickX,
    /// Left stick vertical.
    LeftStickY,
    /// Right stick horizontal.
    RightStickX,
    /// Right stick vertical.
    RightStickY,
    /// Left trigger 0..1.
    LeftTrigger,
    /// Right trigger 0..1.
    RightTrigger,
}

impl GamepadAxis {
    fn index(self) -> usize {
        self as usize
    }
}

/// Pressed / just-pressed / just-released sets for one kind of button.
#[derive(Clone, Debug)]
pub struct ButtonState<T: Copy + Eq + Hash> {
    pressed: HashSet<T>,
    just_pressed: HashSet<T>,
    just_released: HashSet<T>,
}

impl<T: Copy + Eq + Hash> Default for ButtonState<T> {
    fn default() -> Self {
        Self {
            pressed: HashSet::new(),
            just_pressed: HashSet::new(),
            just_released: HashSet::new(),
        }
    }
}

impl<T: Copy + Eq + Hash> ButtonState<T> {
    /// Button went down (ignores OS key repeat).
    pub fn press(&mut self, b: T) {
        if self.pressed.insert(b) {
            self.just_pressed.insert(b);
        }
    }
    /// Button went up.
    pub fn release(&mut self, b: T) {
        if self.pressed.remove(&b) {
            self.just_released.insert(b);
        }
    }
    /// Held.
    pub fn pressed(&self, b: T) -> bool {
        self.pressed.contains(&b)
    }
    /// Went down this frame.
    pub fn just_pressed(&self, b: T) -> bool {
        self.just_pressed.contains(&b)
    }
    /// Went up this frame.
    pub fn just_released(&self, b: T) -> bool {
        self.just_released.contains(&b)
    }
    /// All held buttons.
    pub fn iter_pressed(&self) -> impl Iterator<Item = T> + '_ {
        self.pressed.iter().copied()
    }
    /// Release everything (focus loss).
    pub fn release_all(&mut self) {
        let held: Vec<T> = self.pressed.drain().collect();
        self.just_released.extend(held);
    }
    fn end_frame(&mut self) {
        self.just_pressed.clear();
        self.just_released.clear();
    }
}

/// State of one gamepad.
#[derive(Clone, Debug, Default)]
pub struct Gamepad {
    /// Buttons.
    pub buttons: ButtonState<GamepadButton>,
    axes: [f32; 6],
}

impl Gamepad {
    /// Axis value (-1..1 for sticks, 0..1 for triggers).
    pub fn axis(&self, axis: GamepadAxis) -> f32 {
        self.axes[axis.index()]
    }
}

/// Keyboard, mouse, touch and gamepad state (resource).
#[derive(Clone, Debug, Default)]
pub struct Input {
    keys: ButtonState<KeyCode>,
    mouse: ButtonState<MouseButton>,
    mouse_position: Option<Vec2>,
    mouse_delta: Vec2,
    wheel: Vec2,
    touches: Vec<Touch>,
    started_touches: Vec<u64>,
    ended_touches: Vec<Touch>,
    text: String,
    gamepads: HashMap<u32, Gamepad>,
    window_size: Vec2,
}

impl Input {
    // ---- queries ----------------------------------------------------------------------

    /// Key held.
    pub fn pressed(&self, key: KeyCode) -> bool {
        self.keys.pressed(key)
    }
    /// Key went down this frame.
    pub fn just_pressed(&self, key: KeyCode) -> bool {
        self.keys.just_pressed(key)
    }
    /// Key went up this frame.
    pub fn just_released(&self, key: KeyCode) -> bool {
        self.keys.just_released(key)
    }
    /// Any of `keys` held.
    pub fn any_pressed(&self, keys: impl IntoIterator<Item = KeyCode>) -> bool {
        keys.into_iter().any(|k| self.pressed(k))
    }
    /// Keyboard state.
    pub fn keys(&self) -> &ButtonState<KeyCode> {
        &self.keys
    }
    /// Mouse button held.
    pub fn mouse_pressed(&self, b: MouseButton) -> bool {
        self.mouse.pressed(b)
    }
    /// Mouse button went down this frame.
    pub fn mouse_just_pressed(&self, b: MouseButton) -> bool {
        self.mouse.just_pressed(b)
    }
    /// Mouse button went up this frame.
    pub fn mouse_just_released(&self, b: MouseButton) -> bool {
        self.mouse.just_released(b)
    }
    /// Cursor position in physical pixels (None when outside the window).
    pub fn mouse_position(&self) -> Option<Vec2> {
        self.mouse_position
    }
    /// Mouse movement this frame (raw motion when available, else cursor delta).
    pub fn mouse_delta(&self) -> Vec2 {
        self.mouse_delta
    }
    /// Scroll this frame in lines (x, y).
    pub fn wheel_delta(&self) -> Vec2 {
        self.wheel
    }
    /// Active touches.
    pub fn touches(&self) -> &[Touch] {
        &self.touches
    }
    /// Active touch by id.
    pub fn touch(&self, id: u64) -> Option<&Touch> {
        self.touches.iter().find(|t| t.id == id)
    }
    /// Touch ids that started this frame.
    pub fn just_started_touches(&self) -> &[u64] {
        &self.started_touches
    }
    /// Touches that ended this frame (last known state).
    pub fn just_ended_touches(&self) -> &[Touch] {
        &self.ended_touches
    }
    /// Text typed this frame.
    pub fn text(&self) -> &str {
        &self.text
    }
    /// Gamepad by id.
    pub fn gamepad(&self, id: u32) -> Option<&Gamepad> {
        self.gamepads.get(&id)
    }
    /// Connected gamepad ids.
    pub fn gamepad_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.gamepads.keys().copied()
    }
    /// Window size in physical pixels (for normalized touch regions).
    pub fn window_size(&self) -> Vec2 {
        self.window_size
    }

    // ---- injection (runner, tests, replays) ---------------------------------------------

    /// Key down.
    pub fn inject_key_down(&mut self, key: KeyCode) {
        self.keys.press(key);
    }
    /// Key up.
    pub fn inject_key_up(&mut self, key: KeyCode) {
        self.keys.release(key);
    }
    /// Down + up within one frame (both edges visible that frame).
    pub fn inject_key_tap(&mut self, key: KeyCode) {
        self.keys.press(key);
        self.keys.release(key);
    }
    /// Mouse button down.
    pub fn inject_mouse_button_down(&mut self, b: MouseButton) {
        self.mouse.press(b);
    }
    /// Mouse button up.
    pub fn inject_mouse_button_up(&mut self, b: MouseButton) {
        self.mouse.release(b);
    }
    /// Cursor moved to `position` (physical pixels); accumulates the delta.
    pub fn inject_mouse_move(&mut self, position: Vec2) {
        if let Some(prev) = self.mouse_position {
            self.mouse_delta += position - prev;
        }
        self.mouse_position = Some(position);
    }
    /// Raw mouse motion (e.g. with a grabbed cursor); accumulates the delta.
    pub fn inject_mouse_motion(&mut self, delta: Vec2) {
        self.mouse_delta += delta;
    }
    /// Set the cursor position without producing a delta (used when raw motion is available).
    pub fn set_mouse_position(&mut self, position: Option<Vec2>) {
        self.mouse_position = position;
    }
    /// Cursor left the window.
    pub fn inject_mouse_left(&mut self) {
        self.mouse_position = None;
    }
    /// Scroll in lines.
    pub fn inject_wheel(&mut self, delta: Vec2) {
        self.wheel += delta;
    }
    /// Touch event (multi-touch: one call per finger).
    pub fn inject_touch(&mut self, id: u64, phase: TouchPhase, position: Vec2) {
        match phase {
            TouchPhase::Started => {
                self.touches.retain(|t| t.id != id);
                self.touches.push(Touch {
                    id,
                    position,
                    start: position,
                    delta: Vec2::ZERO,
                });
                self.started_touches.push(id);
            }
            TouchPhase::Moved => {
                if let Some(t) = self.touches.iter_mut().find(|t| t.id == id) {
                    t.delta += position - t.position;
                    t.position = position;
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if let Some(i) = self.touches.iter().position(|t| t.id == id) {
                    let mut t = self.touches.remove(i);
                    t.delta += position - t.position;
                    t.position = position;
                    self.ended_touches.push(t);
                }
            }
        }
    }
    /// Typed text (IME/committed characters).
    pub fn inject_text(&mut self, text: &str) {
        self.text.push_str(text);
    }
    /// Connect a gamepad.
    pub fn inject_gamepad_connected(&mut self, id: u32) {
        self.gamepads.entry(id).or_default();
    }
    /// Disconnect a gamepad.
    pub fn inject_gamepad_disconnected(&mut self, id: u32) {
        self.gamepads.remove(&id);
    }
    /// Gamepad button (connects the pad if needed).
    pub fn inject_gamepad_button(&mut self, id: u32, button: GamepadButton, pressed: bool) {
        let pad = self.gamepads.entry(id).or_default();
        if pressed {
            pad.buttons.press(button);
        } else {
            pad.buttons.release(button);
        }
    }
    /// Gamepad axis (connects the pad if needed).
    pub fn inject_gamepad_axis(&mut self, id: u32, axis: GamepadAxis, value: f32) {
        self.gamepads.entry(id).or_default().axes[axis.index()] = value.clamp(-1.0, 1.0);
    }
    /// Window resized (physical pixels).
    pub fn set_window_size(&mut self, size: Vec2) {
        self.window_size = size;
    }
    /// Release every held key/button (window focus lost).
    pub fn release_all(&mut self) {
        self.keys.release_all();
        self.mouse.release_all();
        for pad in self.gamepads.values_mut() {
            pad.buttons.release_all();
        }
    }

    /// Clear per-frame edges and deltas (runs in `Stage::Last`).
    pub fn end_frame(&mut self) {
        self.keys.end_frame();
        self.mouse.end_frame();
        self.mouse_delta = Vec2::ZERO;
        self.wheel = Vec2::ZERO;
        self.started_touches.clear();
        self.ended_touches.clear();
        self.text.clear();
        for t in &mut self.touches {
            t.delta = Vec2::ZERO;
        }
        for pad in self.gamepads.values_mut() {
            pad.buttons.end_frame();
        }
    }
}

/// Something that can trigger an action.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Binding {
    /// Keyboard key.
    Key(KeyCode),
    /// Mouse button.
    Mouse(MouseButton),
    /// Any finger on the screen.
    TouchAny,
    /// A finger inside a normalized screen rectangle (`min`/`max` in 0..1, origin top-left):
    /// virtual buttons on mobile.
    TouchRegion {
        /// Top-left corner.
        min: Vec2,
        /// Bottom-right corner.
        max: Vec2,
    },
    /// Gamepad button on any pad.
    GamepadButton(GamepadButton),
    /// Gamepad axis beyond `threshold` (negative thresholds trigger below it).
    GamepadAxis {
        /// Axis.
        axis: GamepadAxis,
        /// Activation threshold.
        threshold: f32,
    },
}

/// Named actions → bindings (resource).
#[derive(Clone, Debug, Default)]
pub struct ActionMap {
    bindings: HashMap<String, Vec<Binding>>,
}

impl ActionMap {
    /// Add a binding to an action.
    pub fn bind(&mut self, action: impl Into<String>, binding: Binding) -> &mut Self {
        self.bindings.entry(action.into()).or_default().push(binding);
        self
    }
    /// Remove all bindings of an action.
    pub fn unbind(&mut self, action: &str) {
        self.bindings.remove(action);
    }
    /// Bindings of an action.
    pub fn bindings(&self, action: &str) -> &[Binding] {
        self.bindings.get(action).map_or(&[], Vec::as_slice)
    }
    /// All action names.
    pub fn actions(&self) -> impl Iterator<Item = &str> {
        self.bindings.keys().map(String::as_str)
    }
}

/// Per-frame state of one action.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ActionValue {
    /// Any binding held.
    pub pressed: bool,
    /// Became active this frame.
    pub just_pressed: bool,
    /// Became inactive this frame.
    pub just_released: bool,
    /// Analog strength 0..1 (1 for digital bindings).
    pub value: f32,
}

/// Evaluated actions (resource, updated in `Stage::PreUpdate`).
#[derive(Clone, Debug, Default)]
pub struct ActionState {
    values: HashMap<String, ActionValue>,
}

impl ActionState {
    /// Full state of an action.
    pub fn get(&self, action: &str) -> ActionValue {
        self.values.get(action).copied().unwrap_or_default()
    }
    /// Action held.
    pub fn pressed(&self, action: &str) -> bool {
        self.get(action).pressed
    }
    /// Action started this frame.
    pub fn just_pressed(&self, action: &str) -> bool {
        self.get(action).just_pressed
    }
    /// Action ended this frame.
    pub fn just_released(&self, action: &str) -> bool {
        self.get(action).just_released
    }
    /// Analog value.
    pub fn value(&self, action: &str) -> f32 {
        self.get(action).value
    }
    /// `value(positive) - value(negative)`.
    pub fn axis(&self, negative: &str, positive: &str) -> f32 {
        self.value(positive) - self.value(negative)
    }

    /// Recompute from input.
    pub fn update(&mut self, map: &ActionMap, input: &Input) {
        for (name, bindings) in &map.bindings {
            let mut value = 0.0f32;
            let mut edge_down = false;
            let mut edge_up = false;
            for b in bindings {
                let (v, down, up) = eval(b, input);
                value = value.max(v);
                edge_down |= down;
                edge_up |= up;
            }
            let prev = self.values.get(name).copied().unwrap_or_default();
            let pressed = value > 0.0;
            let state = ActionValue {
                pressed,
                just_pressed: edge_down || (pressed && !prev.pressed),
                just_released: edge_up || (!pressed && prev.pressed),
                value,
            };
            match self.values.get_mut(name) {
                Some(v) => *v = state,
                None => {
                    self.values.insert(name.clone(), state);
                }
            }
        }
        self.values.retain(|k, _| map.bindings.contains_key(k));
    }
}

/// (value, just-down edge, just-up edge) of one binding.
fn eval(b: &Binding, input: &Input) -> (f32, bool, bool) {
    let digital = |p: bool| if p { 1.0 } else { 0.0 };
    match *b {
        Binding::Key(k) => (digital(input.pressed(k)), input.just_pressed(k), input.just_released(k)),
        Binding::Mouse(m) => (
            digital(input.mouse_pressed(m)),
            input.mouse_just_pressed(m),
            input.mouse_just_released(m),
        ),
        Binding::TouchAny => (
            digital(!input.touches().is_empty()),
            !input.just_started_touches().is_empty(),
            !input.just_ended_touches().is_empty(),
        ),
        Binding::TouchRegion { min, max } => {
            let size = input.window_size().max(Vec2::ONE);
            let inside = |p: Vec2| {
                let n = p / size;
                n.cmpge(min).all() && n.cmple(max).all()
            };
            let held = input.touches().iter().any(|t| inside(t.position));
            let started = input
                .touches()
                .iter()
                .any(|t| inside(t.position) && input.just_started_touches().contains(&t.id));
            let ended = input.just_ended_touches().iter().any(|t| inside(t.position));
            (digital(held), started, ended)
        }
        Binding::GamepadButton(button) => {
            let mut r = (0.0, false, false);
            for id in input.gamepad_ids() {
                let pad = input.gamepad(id).expect("listed");
                r.0 = f32::max(r.0, digital(pad.buttons.pressed(button)));
                r.1 |= pad.buttons.just_pressed(button);
                r.2 |= pad.buttons.just_released(button);
            }
            r
        }
        Binding::GamepadAxis { axis, threshold } => {
            let mut v = 0.0f32;
            for id in input.gamepad_ids() {
                let a = input.gamepad(id).expect("listed").axis(axis);
                let active = if threshold >= 0.0 { a > threshold } else { a < threshold };
                if active {
                    v = v.max(a.abs());
                }
            }
            (v, false, false)
        }
    }
}

/// Registers [`Input`], [`ActionMap`], [`ActionState`] and their per-frame systems.
pub struct InputPlugin;

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Input>()
            .init_resource::<ActionMap>()
            .init_resource::<ActionState>();
        app.add_system_named(Stage::PreUpdate, "input::update_actions", |ctx| {
            ctx.resources.scope::<ActionState, _>(|res, state| {
                state.update(res.expect::<ActionMap>(), res.expect::<Input>());
            });
        });
        app.add_system_named(Stage::Last, "input::end_frame", |ctx| {
            ctx.resources.expect_mut::<Input>().end_frame();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_edges_and_end_frame() {
        let mut i = Input::default();
        i.inject_key_down(KeyCode::KeyW);
        i.inject_key_down(KeyCode::KeyW); // OS repeat ignored
        assert!(i.pressed(KeyCode::KeyW) && i.just_pressed(KeyCode::KeyW));
        i.end_frame();
        assert!(i.pressed(KeyCode::KeyW) && !i.just_pressed(KeyCode::KeyW));
        i.inject_key_up(KeyCode::KeyW);
        assert!(!i.pressed(KeyCode::KeyW) && i.just_released(KeyCode::KeyW));
        i.inject_key_tap(KeyCode::Space);
        assert!(i.just_pressed(KeyCode::Space) && i.just_released(KeyCode::Space) && !i.pressed(KeyCode::Space));
    }

    #[test]
    fn mouse_and_wheel() {
        let mut i = Input::default();
        i.inject_mouse_move(Vec2::new(10.0, 10.0));
        i.inject_mouse_move(Vec2::new(15.0, 7.0));
        i.inject_mouse_motion(Vec2::new(1.0, 1.0));
        i.inject_wheel(Vec2::new(0.0, 2.0));
        i.inject_mouse_button_down(MouseButton::Left);
        assert_eq!(i.mouse_delta(), Vec2::new(6.0, -2.0));
        assert_eq!(i.wheel_delta(), Vec2::new(0.0, 2.0));
        assert!(i.mouse_just_pressed(MouseButton::Left));
        i.end_frame();
        assert_eq!(i.mouse_delta(), Vec2::ZERO);
        assert_eq!(i.mouse_position(), Some(Vec2::new(15.0, 7.0)));
        i.release_all();
        assert!(i.mouse_just_released(MouseButton::Left));
    }

    #[test]
    fn multi_touch() {
        let mut i = Input::default();
        i.inject_touch(1, TouchPhase::Started, Vec2::new(0.0, 0.0));
        i.inject_touch(2, TouchPhase::Started, Vec2::new(100.0, 0.0));
        i.inject_touch(1, TouchPhase::Moved, Vec2::new(5.0, 5.0));
        assert_eq!(i.touches().len(), 2);
        assert_eq!(i.touch(1).unwrap().delta, Vec2::new(5.0, 5.0));
        assert_eq!(i.just_started_touches(), &[1, 2]);
        i.end_frame();
        i.inject_touch(2, TouchPhase::Ended, Vec2::new(100.0, 0.0));
        assert_eq!(i.touches().len(), 1);
        assert_eq!(i.just_ended_touches()[0].id, 2);
    }

    #[test]
    fn actions_combine_bindings() {
        let mut map = ActionMap::default();
        map.bind("fire", Binding::Key(KeyCode::KeyF))
            .bind("fire", Binding::Mouse(MouseButton::Left))
            .bind("fire", Binding::GamepadButton(GamepadButton::South))
            .bind(
                "left",
                Binding::GamepadAxis {
                    axis: GamepadAxis::LeftStickX,
                    threshold: -0.2,
                },
            )
            .bind(
                "button",
                Binding::TouchRegion {
                    min: Vec2::new(0.5, 0.5),
                    max: Vec2::ONE,
                },
            );
        let mut input = Input::default();
        input.set_window_size(Vec2::new(200.0, 100.0));
        let mut state = ActionState::default();

        input.inject_mouse_button_down(MouseButton::Left);
        input.inject_gamepad_axis(0, GamepadAxis::LeftStickX, -0.8);
        input.inject_touch(7, TouchPhase::Started, Vec2::new(150.0, 80.0));
        state.update(&map, &input);
        assert!(state.just_pressed("fire") && state.pressed("fire"));
        assert!((state.value("left") - 0.8).abs() < 1e-6);
        assert!(state.just_pressed("button"));
        input.end_frame();
        state.update(&map, &input);
        assert!(state.pressed("fire") && !state.just_pressed("fire"));
        input.inject_mouse_button_up(MouseButton::Left);
        input.inject_gamepad_button(0, GamepadButton::South, true);
        state.update(&map, &input);
        assert!(state.pressed("fire"), "gamepad keeps it held");
        assert!(!state.just_released("fire") || state.pressed("fire"));
        assert_eq!(state.axis("left", "fire"), 1.0 - 0.8);
    }
}
