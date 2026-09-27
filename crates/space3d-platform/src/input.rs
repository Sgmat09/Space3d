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
//!
//! Mouse buttons also keep their order within a frame: [`Input::mouse_button_events`] lists
//! each press and release with the cursor position at the time and its cause
//! ([`MouseButtonCause`]). Buttons released without a release event — the window lost the focus
//! ([`Input::release_all`]), or the OS reports the button up although its release never arrived
//! (the window runner checks once per frame on macOS and Windows) — are marked as cancels
//! ([`MouseButtonEvent::cancelled`]): the UI registers no click and no drop for them, and games
//! can tell them apart too.
//!
//! Touches keep their order the same way: [`Input::touch_events`] lists each finger's start,
//! moves and end as they happened. The per-frame state ([`Input::touches`],
//! [`Input::just_started_touches`], [`Input::just_ended_touches`]) cannot tell a tap that began
//! and ended within one frame (a quick tap, or any tap during a long frame) from other orders,
//! and a finger id can be reused once its finger lifts (Android numbers fingers from 0).
//!
//! Keys are **physical** ([`KeyCode`]: the key's position). The runner also records what each
//! key means on the player's layout ([`KeyEvent`], [`Input::key_events`],
//! [`Input::logical_key`]) and what it prints ([`Input::key_label`]); see
//! [`crate::layout`]. Tests inject layout-aware events with [`Input::inject_key_event`]:
//!
//! ```
//! use space3d_platform::*;
//! let mut input = Input::default();
//! // AZERTY: the key in QWERTY's `A` position types "q".
//! input.inject_key_tap_logical(KeyCode::KeyA, LogicalKey::Character("q".into()));
//! assert!(input.just_pressed(KeyCode::KeyA));
//! assert_eq!(input.key_label(KeyCode::KeyA).as_deref(), Some("q"));
//! assert_eq!(input.key_cap(KeyCode::KeyA), "Q");
//! assert_eq!(input.text(), "q");
//! ```

use crate::layout::{KeyLabel, KeyLabelSource, KeyLocation, KeyboardLayout, LogicalKey, english_key_name};
use glam::Vec2;
use space3d_core::{App, Plugin, Stage};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

pub use winit::keyboard::KeyCode;

/// Upper bound on [`Input::key_events`] kept per frame (a frame never sees this many; it only
/// bounds memory when `end_frame` is not called).
const MAX_KEY_EVENTS: usize = 256;
/// Upper bound on [`Input::mouse_button_events`] kept per frame (same purpose).
const MAX_MOUSE_BUTTON_EVENTS: usize = 64;
/// Upper bound on [`Input::touch_events`] kept per frame (same purpose).
const MAX_TOUCH_EVENTS: usize = 256;

/// One keyboard event as the OS reported it: the physical key plus what it means on the
/// player's keyboard layout. Recorded by the runner (and [`Input::inject_key_event`]) in
/// [`Input::key_events`]; see [`crate::layout`] for physical vs logical keys. Build one with
/// [`KeyEvent::press`] / [`KeyEvent::release`] and the `with_*` methods.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyEvent {
    /// The physical key (its position; layout-independent). What [`Input::pressed`] and
    /// [`Binding::Key`] use.
    pub physical: KeyCode,
    /// What the key means on the current layout, with Shift / AltGr / Option applied (not
    /// Ctrl): winit's `logical_key`. `Dead` for dead keys.
    pub logical_key: LogicalKey,
    /// The layout's key ignoring every modifier (winit's `key_without_modifiers`; never
    /// `Dead`). Desktop only: `None` on the web, Android and iOS.
    pub key_without_modifiers: Option<LogicalKey>,
    /// Text the press produced, if any.
    pub text: Option<String>,
    /// Left/right/numpad variant of keys that exist more than once.
    pub location: KeyLocation,
    /// Key went down (`false`: up).
    pub pressed: bool,
    /// OS auto-repeat of a held key.
    pub repeat: bool,
}

impl KeyEvent {
    /// A press of `physical` that types `key` on the current layout (both
    /// [`logical_key`](Self::logical_key) and [`key_without_modifiers`](Self::key_without_modifiers);
    /// the text is the key's text, e.g. `"q"`, `" "` for Space).
    pub fn press(physical: KeyCode, key: LogicalKey) -> Self {
        Self {
            physical,
            text: key.to_text().map(str::to_owned),
            key_without_modifiers: Some(key.clone()),
            logical_key: key,
            location: default_location(physical),
            pressed: true,
            repeat: false,
        }
    }

    /// A release of `physical` (see [`KeyEvent::press`]; no text).
    pub fn release(physical: KeyCode, key: LogicalKey) -> Self {
        Self {
            pressed: false,
            text: None,
            ..Self::press(physical, key)
        }
    }

    /// Replace the logical key (e.g. the shifted character, or `Dead`).
    pub fn with_logical_key(mut self, key: LogicalKey) -> Self {
        self.logical_key = key;
        self
    }

    /// Replace the modifier-free key (`None`: platform without it).
    pub fn with_key_without_modifiers(mut self, key: Option<LogicalKey>) -> Self {
        self.key_without_modifiers = key;
        self
    }

    /// Replace the produced text.
    pub fn with_text(mut self, text: Option<&str>) -> Self {
        self.text = text.map(str::to_owned);
        self
    }

    /// Replace the key location.
    pub fn with_location(mut self, location: KeyLocation) -> Self {
        self.location = location;
        self
    }

    /// Mark as an OS auto-repeat.
    pub fn with_repeat(mut self, repeat: bool) -> Self {
        self.repeat = repeat;
        self
    }

    /// The layout's key for bindings and shortcuts: [`key_without_modifiers`](Self::key_without_modifiers)
    /// where the platform reports it, else [`logical_key`](Self::logical_key).
    pub fn key(&self) -> &LogicalKey {
        self.key_without_modifiers.as_ref().unwrap_or(&self.logical_key)
    }
}

fn default_location(key: KeyCode) -> KeyLocation {
    use KeyCode as K;
    match key {
        K::ShiftLeft | K::ControlLeft | K::AltLeft | K::SuperLeft => KeyLocation::Left,
        K::ShiftRight | K::ControlRight | K::AltRight | K::SuperRight => KeyLocation::Right,
        K::NumLock
        | K::Numpad0
        | K::Numpad1
        | K::Numpad2
        | K::Numpad3
        | K::Numpad4
        | K::Numpad5
        | K::Numpad6
        | K::Numpad7
        | K::Numpad8
        | K::Numpad9
        | K::NumpadAdd
        | K::NumpadSubtract
        | K::NumpadMultiply
        | K::NumpadDivide
        | K::NumpadDecimal
        | K::NumpadComma
        | K::NumpadEqual
        | K::NumpadEnter => KeyLocation::Numpad,
        _ => KeyLocation::Standard,
    }
}

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

impl MouseButton {
    /// Order of the buttons in [`Input::release_all`]'s events: `Left`, `Right`, `Middle`,
    /// `Back`, `Forward`, then `Other` by number.
    fn order(self) -> (u8, u16) {
        match self {
            Self::Left => (0, 0),
            Self::Right => (1, 0),
            Self::Middle => (2, 0),
            Self::Back => (3, 0),
            Self::Forward => (4, 0),
            Self::Other(n) => (5, n),
        }
    }
}

/// Why a mouse button went down or up ([`MouseButtonEvent::cause`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MouseButtonCause {
    /// The device: the OS delivered the press or release (the window runner), or a test or
    /// replay injected it ([`Input::inject_mouse_button_down`], [`Input::inject_mouse_button_up`]).
    #[default]
    Device,
    /// A release because the window lost the focus (or the app was suspended) while the button
    /// was held ([`Input::release_all`]): the OS sends the release elsewhere, if at all.
    FocusLost,
    /// A release because the OS reports the button up although its release event never
    /// arrived (e.g. macOS Mission Control or a Spaces swipe took the pointer mid-press while
    /// the window kept the focus). The window runner checks the OS's buttons once per frame on
    /// macOS and Windows.
    OsReconciled,
}

impl MouseButtonCause {
    /// A synthetic release that cancels what the press started (every cause but
    /// [`Device`](Self::Device)): like switching away from a native app mid-drag, it is neither
    /// a click nor a drop. The engine's UI treats it so; game code can too.
    pub fn is_cancel(self) -> bool {
        self != Self::Device
    }
}

/// One mouse button press or release, in the order of the frame's events
/// ([`Input::mouse_button_events`]).
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct MouseButtonEvent {
    /// The button.
    pub button: MouseButton,
    /// Went down (`false`: up).
    pub pressed: bool,
    /// Cursor position in physical pixels when it happened (`None`: outside the window).
    pub position: Option<Vec2>,
    /// Why (device event, focus loss, OS state).
    pub cause: MouseButtonCause,
}

impl MouseButtonEvent {
    /// A release that cancels the press ([`MouseButtonCause::is_cancel`]): no click, no drop.
    pub fn cancelled(&self) -> bool {
        !self.pressed && self.cause.is_cancel()
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

/// One touch change, in the order of the frame's events ([`Input::touch_events`]).
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct TouchEvent {
    /// Finger id (reused once the finger lifts on some platforms).
    pub id: u64,
    /// Started, moved, ended or cancelled by the system.
    pub phase: TouchPhase,
    /// Position in physical pixels.
    pub position: Vec2,
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
    touch_events: Vec<TouchEvent>,
    text: String,
    gamepads: HashMap<u32, Gamepad>,
    window_size: Vec2,
    key_events: Vec<KeyEvent>,
    logical_keys: HashMap<KeyCode, LogicalKey>,
    layout: KeyboardLayout,
    mouse_button_events: Vec<MouseButtonEvent>,
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
    /// This frame's mouse button presses and releases in order, each with the cursor position
    /// at the time and its cause (a release the focus loss or the OS state caused is a cancel:
    /// [`MouseButtonEvent::cancelled`]). Only changes are recorded: a press of a held button or
    /// a release of a button that is up adds nothing. Cleared in `end_frame`.
    ///
    /// The per-frame edges ([`Self::mouse_just_pressed`], [`Self::mouse_just_released`]) lose
    /// the order: a release and a new press in one frame (a fast double click) show both edges
    /// with the button held; the events say it went up first.
    pub fn mouse_button_events(&self) -> &[MouseButtonEvent] {
        &self.mouse_button_events
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
    /// Touches that ended (or the system cancelled) this frame (last known state).
    pub fn just_ended_touches(&self) -> &[Touch] {
        &self.ended_touches
    }
    /// This frame's touch starts, moves, ends and cancels in order. Only changes are recorded:
    /// a move to the same position, or a move or end of a finger that is not down, adds
    /// nothing. Cleared in `end_frame`.
    ///
    /// The per-frame state loses the order: a finger that went down and up within one frame is
    /// in [`Self::just_started_touches`] and [`Self::just_ended_touches`] but no longer in
    /// [`Self::touches`], and when a lifted finger's id comes back in the same frame, the
    /// events say which came first.
    pub fn touch_events(&self) -> &[TouchEvent] {
        &self.touch_events
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

    // ---- keyboard layout ----------------------------------------------------------------

    /// This frame's keyboard events in order, with their logical keys (runner and
    /// [`Self::inject_key_event`]; the plain `inject_key_*` methods add none). Cleared in
    /// `end_frame`.
    pub fn key_events(&self) -> &[KeyEvent] {
        &self.key_events
    }

    /// The layout's key ([`KeyEvent::key`]) of the most recent event of physical `key`, e.g.
    /// `Character("z")` for `KeyCode::KeyW` on AZERTY. Kept after release; `None` until the
    /// key produced a [`KeyEvent`].
    pub fn logical_key(&self, key: KeyCode) -> Option<&LogicalKey> {
        self.logical_keys.get(&key)
    }

    /// What physical `key` prints on the current layout without modifiers (`"q"` for
    /// `KeyA` on AZERTY; dead keys: the base character), or the English name of a
    /// non-character key (`"Space"`, `"Enter"`, `"Shift"`, `"F1"` — localize those). `None`
    /// while a character key's label is unknown (no OS table and not pressed yet). See
    /// [`crate::layout`].
    pub fn key_label(&self, key: KeyCode) -> Option<String> {
        self.key_label_info(key).map(|l| l.text)
    }

    /// [`Self::key_label`] with the shifted character and the label's source.
    pub fn key_label_info(&self, key: KeyCode) -> Option<KeyLabel> {
        if crate::layout::is_writing_system_key(key) {
            return self.layout.label(key);
        }
        english_key_name(key).map(|name| KeyLabel {
            key,
            text: name.to_owned(),
            shifted: None,
            source: KeyLabelSource::Named,
        })
    }

    /// Key-cap text for physical `key`, never empty: [`KeyLabel::cap`] of the layout label
    /// (`"Q"` for `KeyA` on AZERTY), else the US QWERTY character of that position (`"A"`),
    /// else the key code name.
    pub fn key_cap(&self, key: KeyCode) -> String {
        if let Some(label) = self.key_label_info(key) {
            return label.cap();
        }
        match crate::layout::us_qwerty_label(key) {
            Some(us) => us.to_uppercase(),
            None => format!("{key:?}"),
        }
    }

    /// The keyboard layout labels (OS table + learned).
    pub fn keyboard_layout(&self) -> &KeyboardLayout {
        &self.layout
    }

    /// Mutable keyboard layout labels (custom platforms, tests).
    pub fn keyboard_layout_mut(&mut self) -> &mut KeyboardLayout {
        &mut self.layout
    }

    /// Re-read the OS layout table now (see [`KeyboardLayout::refresh_from_os`]; the window
    /// runner already does this at start-up, on focus and on layout changes). Returns whether
    /// the labels changed.
    pub fn refresh_keyboard_layout(&mut self) -> bool {
        self.layout.refresh_from_os()
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
    /// A keyboard event with its logical key (what the runner feeds): presses/releases the
    /// physical key, appends the text of presses to [`Self::text`], records the event in
    /// [`Self::key_events`] and learns the key's label (see [`crate::layout`]; not while the
    /// labels are pinned: [`KeyboardLayout::set_pinned`]).
    pub fn inject_key_event(&mut self, event: KeyEvent) {
        if event.pressed {
            self.keys.press(event.physical);
            if let Some(text) = &event.text {
                self.text.push_str(text);
            }
            self.learn_label(&event);
        } else {
            self.keys.release(event.physical);
        }
        self.logical_keys.insert(event.physical, event.key().clone());
        if self.key_events.len() >= MAX_KEY_EVENTS {
            self.key_events.remove(0);
        }
        self.key_events.push(event);
    }
    /// Key down on a layout where it types `logical` ([`KeyEvent::press`]).
    pub fn inject_key_down_logical(&mut self, key: KeyCode, logical: LogicalKey) {
        self.inject_key_event(KeyEvent::press(key, logical));
    }
    /// Key up ([`KeyEvent::release`]).
    pub fn inject_key_up_logical(&mut self, key: KeyCode, logical: LogicalKey) {
        self.inject_key_event(KeyEvent::release(key, logical));
    }
    /// Down + up within one frame, typing `logical`.
    pub fn inject_key_tap_logical(&mut self, key: KeyCode, logical: LogicalKey) {
        self.inject_key_down_logical(key, logical.clone());
        self.inject_key_up_logical(key, logical);
    }

    /// Learn what a pressed key types: the modifier-free key where the platform reports it,
    /// else the logical key when neither Shift nor Alt/AltGr/Option is held; plus the
    /// shifted character when only Shift is held. Caps Lock is undone for letters (the
    /// logical key of the fallback is lowered, the shifted character raised). Nothing is
    /// learned while the labels are pinned ([`KeyboardLayout::set_pinned`]).
    fn learn_label(&mut self, event: &KeyEvent) {
        use crate::layout::{logical_key_label, shifted_case, unshifted_case};
        use KeyCode as K;
        if self.layout.is_pinned() {
            return;
        }
        let shift = self.any_pressed([K::ShiftLeft, K::ShiftRight]);
        let alt = self.any_pressed([K::AltLeft, K::AltRight]);
        let other = self.any_pressed([K::ControlLeft, K::ControlRight, K::SuperLeft, K::SuperRight]);
        let unshifted = match &event.key_without_modifiers {
            Some(key) => logical_key_label(key),
            None if !shift && !alt => logical_key_label(&event.logical_key).map(|s| unshifted_case(&s)),
            None => None,
        };
        let shifted = (shift && !alt && !other)
            .then(|| logical_key_label(&event.logical_key))
            .flatten()
            .map(|s| shifted_case(&s));
        self.layout
            .learn(event.physical, unshifted.as_deref(), shifted.as_deref());
    }
    /// Mouse button down (recorded in [`Self::mouse_button_events`] at the cursor position).
    pub fn inject_mouse_button_down(&mut self, b: MouseButton) {
        if !self.mouse.pressed(b) {
            self.mouse.press(b);
            self.record_mouse_button(b, true, MouseButtonCause::Device);
        }
    }
    /// Mouse button up (recorded in [`Self::mouse_button_events`] at the cursor position).
    pub fn inject_mouse_button_up(&mut self, b: MouseButton) {
        self.release_mouse_button(b, MouseButtonCause::Device);
    }
    /// Mouse button up for `cause`: [`MouseButtonCause::Device`] is
    /// [`Self::inject_mouse_button_up`]; the others release it as a cancel (no click, no drop in
    /// the UI), for a release that no device event reported — what the window runner does on a
    /// focus loss ([`Self::release_all`]) and when the OS shows the button up with no release
    /// event. Nothing happens while `b` is up.
    pub fn release_mouse_button(&mut self, b: MouseButton, cause: MouseButtonCause) {
        if self.mouse.pressed(b) {
            self.mouse.release(b);
            self.record_mouse_button(b, false, cause);
        }
    }
    fn record_mouse_button(&mut self, button: MouseButton, pressed: bool, cause: MouseButtonCause) {
        if self.mouse_button_events.len() >= MAX_MOUSE_BUTTON_EVENTS {
            self.mouse_button_events.remove(0);
        }
        self.mouse_button_events.push(MouseButtonEvent {
            button,
            pressed,
            position: self.mouse_position,
            cause,
        });
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
    /// Touch event (multi-touch: one call per finger), recorded in [`Self::touch_events`]. A
    /// start with the id of a finger that is still down (its end was lost) cancels that finger
    /// first.
    pub fn inject_touch(&mut self, id: u64, phase: TouchPhase, position: Vec2) {
        match phase {
            TouchPhase::Started => {
                if let Some(t) = self.touch(id) {
                    let at = t.position;
                    self.inject_touch(id, TouchPhase::Cancelled, at);
                }
                self.touches.push(Touch {
                    id,
                    position,
                    start: position,
                    delta: Vec2::ZERO,
                });
                self.started_touches.push(id);
            }
            TouchPhase::Moved => {
                let Some(t) = self.touches.iter_mut().find(|t| t.id == id) else {
                    return;
                };
                if t.position == position {
                    return;
                }
                t.delta += position - t.position;
                t.position = position;
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                let Some(i) = self.touches.iter().position(|t| t.id == id) else {
                    return;
                };
                let mut t = self.touches.remove(i);
                t.delta += position - t.position;
                t.position = position;
                self.ended_touches.push(t);
            }
        }
        if self.touch_events.len() >= MAX_TOUCH_EVENTS {
            self.touch_events.remove(0);
        }
        self.touch_events.push(TouchEvent { id, phase, position });
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
    /// Release every held key/button (window focus lost, app suspended). The mouse buttons'
    /// releases are cancels ([`MouseButtonCause::FocusLost`] in [`Self::mouse_button_events`]).
    pub fn release_all(&mut self) {
        self.keys.release_all();
        let mut held: Vec<MouseButton> = self.mouse.iter_pressed().collect();
        held.sort_by_key(|b| b.order());
        for b in held {
            self.release_mouse_button(b, MouseButtonCause::FocusLost);
        }
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
        self.touch_events.clear();
        self.text.clear();
        self.key_events.clear();
        self.mouse_button_events.clear();
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
    use crate::layout::NamedKey;

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

    fn ch(s: &str) -> LogicalKey {
        LogicalKey::Character(s.into())
    }

    #[test]
    fn key_events_press_record_and_learn() {
        let mut i = Input::default();
        // AZERTY: QWERTY's A position types "q"; the physical key is what is held.
        i.inject_key_down_logical(KeyCode::KeyA, ch("q"));
        assert!(i.pressed(KeyCode::KeyA) && i.just_pressed(KeyCode::KeyA));
        assert_eq!(i.text(), "q");
        assert_eq!(i.key_events().len(), 1);
        assert_eq!(i.key_events()[0].physical, KeyCode::KeyA);
        assert_eq!(i.logical_key(KeyCode::KeyA), Some(&ch("q")));
        assert_eq!(i.key_label(KeyCode::KeyA).as_deref(), Some("q"));
        assert_eq!(i.key_label_info(KeyCode::KeyA).unwrap().source, KeyLabelSource::Learned);
        i.inject_key_event(KeyEvent::press(KeyCode::KeyA, ch("q")).with_repeat(true));
        assert_eq!(i.text(), "qq", "OS repeats type");
        i.end_frame();
        assert!(i.key_events().is_empty() && i.text().is_empty());
        assert!(i.pressed(KeyCode::KeyA), "held");
        i.inject_key_up_logical(KeyCode::KeyA, ch("q"));
        assert!(i.just_released(KeyCode::KeyA));
        assert!(!i.key_events()[0].pressed);
        assert_eq!(i.text(), "", "releases type nothing");
        assert_eq!(i.logical_key(KeyCode::KeyA), Some(&ch("q")), "kept after release");
        // Legacy injection adds no event and keeps the learned label.
        i.end_frame();
        i.inject_key_tap(KeyCode::KeyA);
        assert!(i.key_events().is_empty());
        assert_eq!(i.key_cap(KeyCode::KeyA), "Q");
    }

    #[test]
    fn shifted_and_modified_presses() {
        let mut i = Input::default();
        // Desktop AZERTY, Shift held: the modifier-free key is still "é", the logical key "2".
        i.inject_key_down(KeyCode::ShiftLeft);
        i.inject_key_event(KeyEvent::press(KeyCode::Digit2, ch("é")).with_logical_key(ch("2")));
        let l = i.key_label_info(KeyCode::Digit2).unwrap();
        assert_eq!((l.text.as_str(), l.shifted.as_deref()), ("é", Some("2")));
        assert_eq!(i.key_cap(KeyCode::Digit2), "é", "shift types a digit, not É");
        // Web/mobile (no modifier-free key): a shifted press only teaches the shifted value.
        i.inject_key_event(
            KeyEvent::press(KeyCode::KeyQ, ch("A"))
                .with_key_without_modifiers(None)
                .with_text(Some("A")),
        );
        assert_eq!(i.key_label(KeyCode::KeyQ), None, "unshifted still unknown");
        assert_eq!(i.key_cap(KeyCode::KeyQ), "Q", "US fallback for the position");
        i.inject_key_up(KeyCode::ShiftLeft);
        i.inject_key_event(KeyEvent::press(KeyCode::KeyQ, ch("a")).with_key_without_modifiers(None));
        let l = i.key_label_info(KeyCode::KeyQ).unwrap();
        assert_eq!((l.text.as_str(), l.shifted.as_deref()), ("a", Some("A")));
        // Option/AltGr characters are not labels (macOS Option+Z = "Ω").
        i.inject_key_down(KeyCode::AltLeft);
        i.inject_key_event(KeyEvent::press(KeyCode::KeyZ, ch("Ω")).with_key_without_modifiers(None));
        assert_eq!(i.key_label(KeyCode::KeyZ), None);
        // Dead keys learn their base character.
        i.inject_key_up(KeyCode::AltLeft);
        i.inject_key_event(
            KeyEvent::press(KeyCode::BracketLeft, LogicalKey::Dead(Some('^')))
                .with_key_without_modifiers(None)
                .with_text(None),
        );
        assert_eq!(i.key_label(KeyCode::BracketLeft).as_deref(), Some("^"));
    }

    #[test]
    fn caps_lock_does_not_teach_upper_case_labels() {
        let mut i = Input::default();
        // Web/mobile (no modifier-free key), Caps Lock on: "A" is typed, "a" is the label.
        i.inject_key_event(KeyEvent::press(KeyCode::KeyA, ch("A")).with_key_without_modifiers(None));
        assert_eq!(i.text(), "A");
        assert_eq!(i.key_label(KeyCode::KeyA).as_deref(), Some("a"));
        assert_eq!(i.key_cap(KeyCode::KeyA), "A");
        // AZERTY on the web, Caps Lock on: "É" on the digit row is still the "é" key.
        i.inject_key_event(KeyEvent::press(KeyCode::Digit2, ch("É")).with_key_without_modifiers(None));
        assert_eq!(i.key_label(KeyCode::Digit2).as_deref(), Some("é"));
        // Windows/Linux/web, Caps Lock + Shift types "q": Shift's character is "Q".
        i.inject_key_down(KeyCode::ShiftLeft);
        i.inject_key_event(KeyEvent::press(KeyCode::KeyQ, ch("q")).with_logical_key(ch("q")));
        let q = i.key_label_info(KeyCode::KeyQ).unwrap();
        assert_eq!((q.text.as_str(), q.shifted.as_deref()), ("q", Some("Q")));
        assert_eq!(i.key_cap(KeyCode::KeyQ), "Q");
        // Desktop reports the modifier-free key: taken as is.
        i.inject_key_up(KeyCode::ShiftLeft);
        i.inject_key_event(KeyEvent::press(KeyCode::KeyW, ch("w")).with_logical_key(ch("W")));
        assert_eq!(i.key_label(KeyCode::KeyW).as_deref(), Some("w"));
    }

    #[test]
    fn named_keys_and_fallbacks() {
        let mut i = Input::default();
        assert_eq!(i.key_label(KeyCode::Space).as_deref(), Some("Space"));
        assert_eq!(i.key_label(KeyCode::F1).as_deref(), Some("F1"));
        assert_eq!(i.key_label(KeyCode::Enter).as_deref(), Some("Enter"));
        assert_eq!(i.key_label(KeyCode::KeyW), None, "unknown layout");
        assert_eq!(i.key_cap(KeyCode::KeyW), "W");
        assert_eq!(i.key_cap(KeyCode::Semicolon), ";");
        assert_eq!(i.key_cap(KeyCode::Lang5), "Lang5");
        // Named keys are never learned (Space types " ").
        i.inject_key_tap_logical(KeyCode::Space, LogicalKey::Named(NamedKey::Space));
        assert_eq!(i.text(), " ");
        assert_eq!(i.key_label(KeyCode::Space).as_deref(), Some("Space"));
        assert_eq!(i.key_events()[0].location, KeyLocation::Standard);
        i.inject_key_tap_logical(KeyCode::Numpad1, ch("1"));
        assert_eq!(i.key_events()[2].location, KeyLocation::Numpad);
        assert_eq!(i.key_label(KeyCode::Numpad1).as_deref(), Some("Numpad 1"));
    }

    #[test]
    fn key_event_list_is_bounded() {
        let mut i = Input::default();
        for _ in 0..(MAX_KEY_EVENTS + 10) {
            i.inject_key_tap_logical(KeyCode::KeyB, ch("b"));
        }
        assert_eq!(i.key_events().len(), MAX_KEY_EVENTS);
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
    fn mouse_button_events_keep_order_position_and_cause() {
        let mut i = Input::default();
        let (a, b) = (Vec2::new(10.0, 10.0), Vec2::new(20.0, 20.0));
        i.inject_mouse_move(a);
        i.inject_mouse_button_down(MouseButton::Left);
        i.end_frame();
        // A fast double click's second click: up, then down again, then a drag.
        i.inject_mouse_move(b);
        i.inject_mouse_button_up(MouseButton::Left);
        i.inject_mouse_button_down(MouseButton::Left);
        i.inject_mouse_button_down(MouseButton::Left); // held: nothing
        i.inject_mouse_button_up(MouseButton::Right); // up: nothing
        let ev = |button, pressed, position, cause| MouseButtonEvent {
            button,
            pressed,
            position,
            cause,
        };
        let device = MouseButtonCause::Device;
        assert_eq!(
            i.mouse_button_events(),
            [
                ev(MouseButton::Left, false, Some(b), device),
                ev(MouseButton::Left, true, Some(b), device),
            ]
        );
        assert!(i.mouse_just_pressed(MouseButton::Left) && i.mouse_just_released(MouseButton::Left));
        assert!(i.mouse_pressed(MouseButton::Left), "the edges alone lose the order");
        assert!(!i.mouse_button_events().iter().any(MouseButtonEvent::cancelled));
        i.end_frame();
        assert!(i.mouse_button_events().is_empty());

        // Focus lost with buttons held, the cursor outside: cancels, in button order.
        i.inject_mouse_button_down(MouseButton::Other(7));
        i.inject_mouse_button_down(MouseButton::Right);
        i.inject_mouse_left();
        i.end_frame();
        i.release_all();
        let focus = MouseButtonCause::FocusLost;
        assert_eq!(
            i.mouse_button_events(),
            [
                ev(MouseButton::Left, false, None, focus),
                ev(MouseButton::Right, false, None, focus),
                ev(MouseButton::Other(7), false, None, focus),
            ]
        );
        assert!(i.mouse_button_events().iter().all(MouseButtonEvent::cancelled));
        assert!(i.mouse_just_released(MouseButton::Right) && !i.mouse_pressed(MouseButton::Right));
        i.release_all();
        assert_eq!(i.mouse_button_events().len(), 3, "nothing left to release");

        // A release only the OS saw.
        i.end_frame();
        i.inject_mouse_move(a);
        i.inject_mouse_button_down(MouseButton::Middle);
        i.release_mouse_button(MouseButton::Middle, MouseButtonCause::OsReconciled);
        i.release_mouse_button(MouseButton::Middle, MouseButtonCause::OsReconciled);
        assert_eq!(
            i.mouse_button_events(),
            [
                ev(MouseButton::Middle, true, Some(a), device),
                ev(MouseButton::Middle, false, Some(a), MouseButtonCause::OsReconciled),
            ]
        );
        assert!(!i.mouse_button_events()[0].cancelled(), "presses never are");
        assert!(i.mouse_button_events()[1].cancelled());
        assert!(!MouseButtonCause::Device.is_cancel());
        // `Device` is a plain release.
        i.inject_mouse_button_down(MouseButton::Middle);
        i.release_mouse_button(MouseButton::Middle, device);
        assert!(!i.mouse_button_events()[3].cancelled());

        // Bounded when `end_frame` never runs.
        for _ in 0..(MAX_MOUSE_BUTTON_EVENTS + 10) {
            i.inject_mouse_button_down(MouseButton::Left);
            i.inject_mouse_button_up(MouseButton::Left);
        }
        assert_eq!(i.mouse_button_events().len(), MAX_MOUSE_BUTTON_EVENTS);
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
    fn touch_events_keep_order() {
        use TouchPhase::*;
        let ev = |id, phase, x: f32| TouchEvent {
            id,
            phase,
            position: Vec2::new(x, 0.0),
        };
        let mut i = Input::default();
        i.inject_touch(0, Started, Vec2::new(1.0, 0.0));
        i.end_frame();
        assert!(i.touch_events().is_empty(), "cleared in end_frame");
        // In one frame: finger 0 lifts, a quick tap reuses id 0, finger 0 comes back again.
        // Moves to the same place, and moves or ends of fingers that are not down, add nothing.
        i.inject_touch(0, Moved, Vec2::new(2.0, 0.0));
        i.inject_touch(0, Moved, Vec2::new(2.0, 0.0));
        i.inject_touch(0, Ended, Vec2::new(3.0, 0.0));
        i.inject_touch(0, Moved, Vec2::new(4.0, 0.0));
        i.inject_touch(0, Started, Vec2::new(10.0, 0.0));
        i.inject_touch(0, Ended, Vec2::new(11.0, 0.0));
        i.inject_touch(9, Ended, Vec2::new(11.0, 0.0));
        i.inject_touch(0, Started, Vec2::new(20.0, 0.0));
        assert_eq!(
            i.touch_events(),
            [
                ev(0, Moved, 2.0),
                ev(0, Ended, 3.0),
                ev(0, Started, 10.0),
                ev(0, Ended, 11.0),
                ev(0, Started, 20.0),
            ]
        );
        // The per-frame state keeps the last finger and both ended ones.
        assert_eq!(i.touches().len(), 1);
        assert_eq!(i.touch(0).unwrap().start, Vec2::new(20.0, 0.0));
        assert_eq!(i.just_started_touches(), [0, 0]);
        let ended: Vec<_> = i
            .just_ended_touches()
            .iter()
            .map(|t| (t.start.x, t.position.x))
            .collect();
        assert_eq!(ended, [(1.0, 3.0), (10.0, 11.0)]);
        i.end_frame();
        // A start while the id is still down (its end was lost) cancels that finger first.
        i.inject_touch(0, Moved, Vec2::new(21.0, 0.0));
        i.inject_touch(0, Started, Vec2::new(30.0, 0.0));
        assert_eq!(
            i.touch_events(),
            [ev(0, Moved, 21.0), ev(0, Cancelled, 21.0), ev(0, Started, 30.0)]
        );
        assert_eq!(i.touches().len(), 1);
        assert_eq!(i.just_ended_touches().len(), 1);
        // Bounded when `end_frame` never runs.
        for n in 0..(MAX_TOUCH_EVENTS + 10) {
            i.inject_touch(0, Moved, Vec2::new(n as f32, 1.0));
        }
        assert_eq!(i.touch_events().len(), MAX_TOUCH_EVENTS);
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
