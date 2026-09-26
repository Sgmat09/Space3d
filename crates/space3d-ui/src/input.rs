//! Translation of `space3d_platform::Input` state into egui events.
//!
//! `Input` is state-based (held keys + per-frame edges), so events are reconstructed from the
//! edges every frame: pointer moves, button presses/releases (a press and release in the same
//! frame become a click), wheel, keys, typed text and touches. Because tests drive the same
//! `Input` resource through `inject_*`, UI interaction is fully scriptable.
//!
//! Keys follow the player's keyboard layout like egui-winit: `Event::Key::key` is the
//! **logical** key of the press ([`egui_logical_key`] of winit's logical key, then of the
//! modifier-free key) and `physical_key` the physical one ([`egui_key`]). When the layout's key
//! has no egui equivalent (non-Latin layouts, AZERTY's digit row) or no logical key was
//! recorded (legacy `inject_key_*` events), the physical key is used for both — so `Ctrl/Cmd+Z`
//! undoes with the key labelled Z (physical `KeyW` on AZERTY), and on a Cyrillic layout with the
//! key in Z's position. A key's release reuses the egui key of its press.
//!
//! `Input` drops the OS's key repeats (a held key is simply "pressed"), but egui text fields
//! only act on key *presses*, so held editing/navigation keys ([`REPEAT_KEYS`]: Backspace,
//! Delete, arrows, Home/End, ...) get synthesized repeat presses: after [`REPEAT_DELAY`]
//! seconds, one every [`REPEAT_INTERVAL`]. The timing uses `space3d_core::Time`, so it is
//! deterministic under `App::step_ticks`.

use space3d_core::glam::Vec2;
use space3d_platform::{Input, KeyCode, LogicalKey, MouseButton, NamedKey};

/// Map a logical (layout) key to an egui key — the mapping egui-winit uses: characters by
/// [`egui::Key::from_name`] (`"z"` → `Z`, `"?"` → `Questionmark`), named keys (Enter, arrows,
/// F-keys, ...) directly; `None` for dead keys, modifiers and characters egui has no key for
/// (`"é"`, `"я"`).
pub fn egui_logical_key(key: &LogicalKey) -> Option<egui::Key> {
    match key {
        LogicalKey::Named(named) => egui_named_key(*named),
        LogicalKey::Character(s) => egui::Key::from_name(s.as_str()),
        LogicalKey::Unidentified(_) | LogicalKey::Dead(_) => None,
    }
}

fn egui_named_key(key: NamedKey) -> Option<egui::Key> {
    use NamedKey as N;
    use egui::Key as E;
    Some(match key {
        N::Enter => E::Enter,
        N::Tab => E::Tab,
        N::ArrowDown => E::ArrowDown,
        N::ArrowLeft => E::ArrowLeft,
        N::ArrowRight => E::ArrowRight,
        N::ArrowUp => E::ArrowUp,
        N::End => E::End,
        N::Home => E::Home,
        N::PageDown => E::PageDown,
        N::PageUp => E::PageUp,
        N::Backspace => E::Backspace,
        N::Delete => E::Delete,
        N::Insert => E::Insert,
        N::Escape => E::Escape,
        N::Cut => E::Cut,
        N::Copy => E::Copy,
        N::Paste => E::Paste,
        N::Space => E::Space,
        N::F1 => E::F1,
        N::F2 => E::F2,
        N::F3 => E::F3,
        N::F4 => E::F4,
        N::F5 => E::F5,
        N::F6 => E::F6,
        N::F7 => E::F7,
        N::F8 => E::F8,
        N::F9 => E::F9,
        N::F10 => E::F10,
        N::F11 => E::F11,
        N::F12 => E::F12,
        N::F13 => E::F13,
        N::F14 => E::F14,
        N::F15 => E::F15,
        N::F16 => E::F16,
        N::F17 => E::F17,
        N::F18 => E::F18,
        N::F19 => E::F19,
        N::F20 => E::F20,
        N::F21 => E::F21,
        N::F22 => E::F22,
        N::F23 => E::F23,
        N::F24 => E::F24,
        N::F25 => E::F25,
        N::F26 => E::F26,
        N::F27 => E::F27,
        N::F28 => E::F28,
        N::F29 => E::F29,
        N::F30 => E::F30,
        N::F31 => E::F31,
        N::F32 => E::F32,
        N::F33 => E::F33,
        N::F34 => E::F34,
        N::F35 => E::F35,
        N::BrowserBack => E::BrowserBack,
        _ => return None,
    })
}

/// The egui key for a press of physical `code`: the logical key of the frame's last press
/// event of `code` (winit's logical key first, like egui-winit, then the modifier-free key —
/// which still names the layout's letter when Option/AltGr changed the character), else the
/// physical key.
fn press_key(input: &Input, code: KeyCode) -> Option<egui::Key> {
    let event = input
        .key_events()
        .iter()
        .rev()
        .find(|e| e.physical == code && e.pressed);
    event
        .and_then(|e| {
            egui_logical_key(&e.logical_key).or_else(|| e.key_without_modifiers.as_ref().and_then(egui_logical_key))
        })
        .or_else(|| egui_key(code))
}

/// Map a physical key to the egui key it produces (US layout for punctuation).
pub fn egui_key(code: KeyCode) -> Option<egui::Key> {
    use KeyCode as K;
    use egui::Key as E;
    Some(match code {
        K::ArrowDown => E::ArrowDown,
        K::ArrowLeft => E::ArrowLeft,
        K::ArrowRight => E::ArrowRight,
        K::ArrowUp => E::ArrowUp,
        K::Escape => E::Escape,
        K::Tab => E::Tab,
        K::Backspace => E::Backspace,
        K::Enter | K::NumpadEnter => E::Enter,
        K::Space => E::Space,
        K::Insert => E::Insert,
        K::Delete => E::Delete,
        K::Home => E::Home,
        K::End => E::End,
        K::PageUp => E::PageUp,
        K::PageDown => E::PageDown,
        K::Copy => E::Copy,
        K::Cut => E::Cut,
        K::Paste => E::Paste,
        K::Comma => E::Comma,
        K::Backslash => E::Backslash,
        K::Slash | K::NumpadDivide => E::Slash,
        K::BracketLeft => E::OpenBracket,
        K::BracketRight => E::CloseBracket,
        K::Backquote => E::Backtick,
        K::Minus | K::NumpadSubtract => E::Minus,
        K::Period | K::NumpadDecimal => E::Period,
        K::NumpadAdd => E::Plus,
        K::Equal | K::NumpadEqual => E::Equals,
        K::Semicolon => E::Semicolon,
        K::Quote => E::Quote,
        K::Digit0 | K::Numpad0 => E::Num0,
        K::Digit1 | K::Numpad1 => E::Num1,
        K::Digit2 | K::Numpad2 => E::Num2,
        K::Digit3 | K::Numpad3 => E::Num3,
        K::Digit4 | K::Numpad4 => E::Num4,
        K::Digit5 | K::Numpad5 => E::Num5,
        K::Digit6 | K::Numpad6 => E::Num6,
        K::Digit7 | K::Numpad7 => E::Num7,
        K::Digit8 | K::Numpad8 => E::Num8,
        K::Digit9 | K::Numpad9 => E::Num9,
        K::KeyA => E::A,
        K::KeyB => E::B,
        K::KeyC => E::C,
        K::KeyD => E::D,
        K::KeyE => E::E,
        K::KeyF => E::F,
        K::KeyG => E::G,
        K::KeyH => E::H,
        K::KeyI => E::I,
        K::KeyJ => E::J,
        K::KeyK => E::K,
        K::KeyL => E::L,
        K::KeyM => E::M,
        K::KeyN => E::N,
        K::KeyO => E::O,
        K::KeyP => E::P,
        K::KeyQ => E::Q,
        K::KeyR => E::R,
        K::KeyS => E::S,
        K::KeyT => E::T,
        K::KeyU => E::U,
        K::KeyV => E::V,
        K::KeyW => E::W,
        K::KeyX => E::X,
        K::KeyY => E::Y,
        K::KeyZ => E::Z,
        K::F1 => E::F1,
        K::F2 => E::F2,
        K::F3 => E::F3,
        K::F4 => E::F4,
        K::F5 => E::F5,
        K::F6 => E::F6,
        K::F7 => E::F7,
        K::F8 => E::F8,
        K::F9 => E::F9,
        K::F10 => E::F10,
        K::F11 => E::F11,
        K::F12 => E::F12,
        K::BrowserBack => E::BrowserBack,
        K::ShiftLeft => E::ShiftLeft,
        K::ShiftRight => E::ShiftRight,
        K::ControlLeft => E::ControlLeft,
        K::ControlRight => E::ControlRight,
        K::AltLeft => E::AltLeft,
        K::AltRight => E::AltRight,
        K::SuperLeft => E::SuperLeft,
        K::SuperRight => E::SuperRight,
        _ => return None,
    })
}

/// Every key [`egui_key`] maps (scanned for edges each frame).
const KEYS: &[KeyCode] = {
    use KeyCode as K;
    &[
        K::ArrowDown,
        K::ArrowLeft,
        K::ArrowRight,
        K::ArrowUp,
        K::Escape,
        K::Tab,
        K::Backspace,
        K::Enter,
        K::NumpadEnter,
        K::Space,
        K::Insert,
        K::Delete,
        K::Home,
        K::End,
        K::PageUp,
        K::PageDown,
        K::Copy,
        K::Cut,
        K::Paste,
        K::Comma,
        K::Backslash,
        K::Slash,
        K::NumpadDivide,
        K::BracketLeft,
        K::BracketRight,
        K::Backquote,
        K::Minus,
        K::NumpadSubtract,
        K::Period,
        K::NumpadDecimal,
        K::NumpadAdd,
        K::Equal,
        K::NumpadEqual,
        K::Semicolon,
        K::Quote,
        K::Digit0,
        K::Digit1,
        K::Digit2,
        K::Digit3,
        K::Digit4,
        K::Digit5,
        K::Digit6,
        K::Digit7,
        K::Digit8,
        K::Digit9,
        K::Numpad0,
        K::Numpad1,
        K::Numpad2,
        K::Numpad3,
        K::Numpad4,
        K::Numpad5,
        K::Numpad6,
        K::Numpad7,
        K::Numpad8,
        K::Numpad9,
        K::KeyA,
        K::KeyB,
        K::KeyC,
        K::KeyD,
        K::KeyE,
        K::KeyF,
        K::KeyG,
        K::KeyH,
        K::KeyI,
        K::KeyJ,
        K::KeyK,
        K::KeyL,
        K::KeyM,
        K::KeyN,
        K::KeyO,
        K::KeyP,
        K::KeyQ,
        K::KeyR,
        K::KeyS,
        K::KeyT,
        K::KeyU,
        K::KeyV,
        K::KeyW,
        K::KeyX,
        K::KeyY,
        K::KeyZ,
        K::F1,
        K::F2,
        K::F3,
        K::F4,
        K::F5,
        K::F6,
        K::F7,
        K::F8,
        K::F9,
        K::F10,
        K::F11,
        K::F12,
        K::BrowserBack,
        K::ShiftLeft,
        K::ShiftRight,
        K::ControlLeft,
        K::ControlRight,
        K::AltLeft,
        K::AltRight,
        K::SuperLeft,
        K::SuperRight,
    ]
};

/// Held keys that auto-repeat (editing and navigation keys; printable characters repeat
/// through the OS text events instead).
pub const REPEAT_KEYS: &[KeyCode] = {
    use KeyCode as K;
    &[
        K::Backspace,
        K::Delete,
        K::ArrowLeft,
        K::ArrowRight,
        K::ArrowUp,
        K::ArrowDown,
        K::Home,
        K::End,
        K::PageUp,
        K::PageDown,
        K::Enter,
        K::NumpadEnter,
        K::Tab,
    ]
};
/// Seconds a key must be held before it starts repeating.
pub const REPEAT_DELAY: f64 = 0.5;
/// Seconds between synthesized repeats.
pub const REPEAT_INTERVAL: f64 = 1.0 / 30.0;
/// Upper bound on repeats emitted per key and frame (long frames do not burst).
const MAX_REPEATS_PER_FRAME: u32 = 4;

const BUTTONS: [(MouseButton, egui::PointerButton); 5] = [
    (MouseButton::Left, egui::PointerButton::Primary),
    (MouseButton::Right, egui::PointerButton::Secondary),
    (MouseButton::Middle, egui::PointerButton::Middle),
    (MouseButton::Back, egui::PointerButton::Extra1),
    (MouseButton::Forward, egui::PointerButton::Extra2),
];

/// Modifier state derived from held keys.
pub(crate) fn modifiers(input: &Input) -> egui::Modifiers {
    use KeyCode as K;
    let shift = input.any_pressed([K::ShiftLeft, K::ShiftRight]);
    let ctrl = input.any_pressed([K::ControlLeft, K::ControlRight]);
    let alt = input.any_pressed([K::AltLeft, K::AltRight]);
    let sup = input.any_pressed([K::SuperLeft, K::SuperRight]);
    let mac = cfg!(any(target_os = "macos", target_os = "ios"));
    egui::Modifiers {
        alt,
        ctrl,
        shift,
        mac_cmd: mac && sup,
        command: if mac {
            sup
        } else if cfg!(target_arch = "wasm32") {
            ctrl || sup
        } else {
            ctrl
        },
    }
}

fn printable(c: char) -> bool {
    let private_use = ('\u{e000}'..='\u{f8ff}').contains(&c)
        || ('\u{f0000}'..='\u{ffffd}').contains(&c)
        || ('\u{100000}'..='\u{10fffd}').contains(&c);
    !private_use && !c.is_control()
}

/// Per-frame translator (keeps the last pointer position, modifiers and key-repeat timers).
#[derive(Default)]
pub(crate) struct InputTranslator {
    last_pos: Option<egui::Pos2>,
    last_modifiers: egui::Modifiers,
    /// Touch that emulates the mouse pointer (first finger down).
    pointer_touch: Option<u64>,
    /// Held [`REPEAT_KEYS`] and the time of their next synthesized repeat.
    repeats: Vec<(KeyCode, f64)>,
    /// Held keys and the egui key their press was sent as (the release and repeats reuse it).
    held: Vec<(KeyCode, egui::Key)>,
}

impl InputTranslator {
    /// Append this frame's egui events for `input` (physical pixels → points with `ppp`).
    /// `time` is the app time in seconds (drives key repeat).
    pub(crate) fn translate(
        &mut self,
        input: &Input,
        ppp: f32,
        time: f64,
        clipboard: &str,
        out: &mut Vec<egui::Event>,
    ) {
        let to_points = |p: Vec2| egui::pos2(p.x / ppp, p.y / ppp);
        let mods = modifiers(input);
        if mods != self.last_modifiers {
            out.push(egui::Event::ModifiersChanged(mods));
            self.last_modifiers = mods;
        }

        // Pointer.
        let pos = input.mouse_position().map(to_points);
        match pos {
            Some(p) if self.last_pos != Some(p) => {
                out.push(egui::Event::PointerMoved(p));
                self.last_pos = Some(p);
            }
            None if self.last_pos.is_some() && self.pointer_touch.is_none() => {
                out.push(egui::Event::PointerGone);
                self.last_pos = None;
            }
            _ => {}
        }
        let delta = input.mouse_delta();
        if delta != Vec2::ZERO {
            out.push(egui::Event::MouseMoved(egui::vec2(delta.x / ppp, delta.y / ppp)));
        }
        let at = self.last_pos.unwrap_or(egui::Pos2::ZERO);
        for (b, pb) in BUTTONS {
            let (down, up) = (input.mouse_just_pressed(b), input.mouse_just_released(b));
            if down {
                out.push(egui::Event::PointerButton {
                    pos: at,
                    button: pb,
                    pressed: true,
                    modifiers: mods,
                });
            }
            if up {
                out.push(egui::Event::PointerButton {
                    pos: at,
                    button: pb,
                    pressed: false,
                    modifiers: mods,
                });
            }
        }

        // Wheel (Input reports lines).
        let wheel = input.wheel_delta();
        if wheel != Vec2::ZERO {
            out.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(wheel.x, wheel.y),
                phase: egui::TouchPhase::Move,
                modifiers: mods,
            });
        }

        // Keys (+ clipboard shortcuts): the physical keys egui knows, then any other key that
        // produced an event this frame (its logical key may still be one egui knows).
        let mut extra: Vec<KeyCode> = Vec::new();
        for e in input.key_events() {
            if !KEYS.contains(&e.physical) && !extra.contains(&e.physical) {
                extra.push(e.physical);
            }
        }
        for code in KEYS.iter().copied().chain(extra) {
            let (down, up) = (input.just_pressed(code), input.just_released(code));
            if !down && !up {
                continue;
            }
            let physical_key = egui_key(code);
            let held = self.held.iter().position(|(k, _)| *k == code);
            let key = if down {
                let key = press_key(input, code);
                if let Some(i) = held {
                    self.held.remove(i);
                }
                if let Some(key) = key
                    && !up
                {
                    self.held.push((code, key));
                }
                key
            } else {
                held.map(|i| self.held.remove(i).1).or_else(|| press_key(input, code))
            };
            let Some(key) = key else { continue };
            if down && mods.command {
                match key {
                    egui::Key::C => {
                        out.push(egui::Event::Copy);
                        continue;
                    }
                    egui::Key::X => {
                        out.push(egui::Event::Cut);
                        continue;
                    }
                    egui::Key::V => {
                        if !clipboard.is_empty() {
                            out.push(egui::Event::Paste(clipboard.to_owned()));
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            for pressed in [true, false] {
                if (pressed && down) || (!pressed && up) {
                    out.push(egui::Event::Key {
                        key,
                        physical_key,
                        pressed,
                        repeat: false,
                        modifiers: mods,
                    });
                }
            }
        }

        self.key_repeats(input, time, mods, out);

        // Typed text (not while a command shortcut is held).
        let text: String = input.text().chars().filter(|&c| printable(c)).collect();
        if !text.is_empty() && !(mods.command || (mods.ctrl && !mods.alt)) {
            out.push(egui::Event::Text(text));
        }

        // Touches (the first finger also drives the pointer, like egui-winit).
        let device_id = egui::TouchDeviceId(0);
        for &id in input.just_started_touches() {
            if let Some(t) = input.touch(id) {
                let p = to_points(t.position);
                out.push(egui::Event::Touch {
                    device_id,
                    id: egui::TouchId(id),
                    phase: egui::TouchPhase::Start,
                    pos: p,
                    force: None,
                });
                if self.pointer_touch.is_none() && input.mouse_position().is_none() {
                    self.pointer_touch = Some(id);
                    out.push(egui::Event::PointerMoved(p));
                    out.push(egui::Event::PointerButton {
                        pos: p,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: mods,
                    });
                    self.last_pos = Some(p);
                }
            }
        }
        for t in input.touches() {
            if t.delta != Vec2::ZERO && !input.just_started_touches().contains(&t.id) {
                let p = to_points(t.position);
                out.push(egui::Event::Touch {
                    device_id,
                    id: egui::TouchId(t.id),
                    phase: egui::TouchPhase::Move,
                    pos: p,
                    force: None,
                });
                if self.pointer_touch == Some(t.id) {
                    out.push(egui::Event::PointerMoved(p));
                    self.last_pos = Some(p);
                }
            }
        }
        for t in input.just_ended_touches() {
            let p = to_points(t.position);
            out.push(egui::Event::Touch {
                device_id,
                id: egui::TouchId(t.id),
                phase: egui::TouchPhase::End,
                pos: p,
                force: None,
            });
            if self.pointer_touch == Some(t.id) {
                self.pointer_touch = None;
                out.push(egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: mods,
                });
                out.push(egui::Event::PointerGone);
                self.last_pos = None;
            }
        }
    }
}

impl InputTranslator {
    /// Synthesized repeat presses for held [`REPEAT_KEYS`].
    fn key_repeats(&mut self, input: &Input, time: f64, mods: egui::Modifiers, out: &mut Vec<egui::Event>) {
        self.repeats
            .retain(|(k, _)| input.pressed(*k) && !input.just_pressed(*k));
        for &code in REPEAT_KEYS {
            if !input.pressed(code) {
                continue;
            }
            let physical_key = egui_key(code);
            let held = self.held.iter().find(|(k, _)| *k == code).map(|h| h.1);
            let Some(key) = held.or(physical_key) else { continue };
            let Some(i) = self.repeats.iter().position(|(k, _)| *k == code) else {
                // Newly held (or held before the UI started): repeat after the delay.
                self.repeats.push((code, time + REPEAT_DELAY));
                continue;
            };
            let next = &mut self.repeats[i].1;
            let mut n = 0;
            while time >= *next && n < MAX_REPEATS_PER_FRAME {
                out.push(egui::Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    repeat: true,
                    modifiers: mods,
                });
                *next += REPEAT_INTERVAL;
                n += 1;
            }
            if time >= *next {
                // Too far behind (long frame): skip the backlog instead of bursting.
                *next = time + REPEAT_INTERVAL;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_platform::NamedKey;

    fn repeats(ev: &[egui::Event], key: egui::Key) -> usize {
        ev.iter()
            .filter(|e| matches!(e, egui::Event::Key { key: k, pressed: true, repeat: true, .. } if *k == key))
            .count()
    }

    #[test]
    fn held_editing_keys_repeat_after_a_delay() {
        let mut t = InputTranslator::default();
        let mut input = Input::default();
        input.inject_key_down(KeyCode::Backspace);
        input.inject_key_down(KeyCode::KeyA);
        let mut ev = Vec::new();
        t.translate(&input, 1.0, 10.0, "", &mut ev);
        assert_eq!(repeats(&ev, egui::Key::Backspace), 0, "first frame: only the press");
        assert!(ev.iter().any(|e| matches!(
            e,
            egui::Event::Key {
                key: egui::Key::Backspace,
                pressed: true,
                repeat: false,
                ..
            }
        )));
        let mut total = 0;
        let mut frame_time = 10.0;
        for _ in 0..60 {
            input.end_frame();
            frame_time += 1.0 / 60.0;
            ev.clear();
            t.translate(&input, 1.0, frame_time, "", &mut ev);
            if frame_time - 10.0 < REPEAT_DELAY - 1e-9 {
                assert_eq!(repeats(&ev, egui::Key::Backspace), 0, "no repeat before the delay");
            }
            total += repeats(&ev, egui::Key::Backspace);
            assert_eq!(repeats(&ev, egui::Key::A), 0, "letters repeat through text events");
        }
        // 1 s held: repeats during the last 0.5 s at 30 Hz.
        assert!((13..=17).contains(&total), "{total} repeats");
        // A long frame does not burst.
        input.end_frame();
        ev.clear();
        t.translate(&input, 1.0, frame_time + 5.0, "", &mut ev);
        assert!(repeats(&ev, egui::Key::Backspace) <= MAX_REPEATS_PER_FRAME as usize);
        // Released: no more repeats; pressed again: delay starts over.
        input.end_frame();
        input.inject_key_up(KeyCode::Backspace);
        ev.clear();
        t.translate(&input, 1.0, frame_time + 6.0, "", &mut ev);
        assert_eq!(repeats(&ev, egui::Key::Backspace), 0);
        input.end_frame();
        input.inject_key_down(KeyCode::Backspace);
        ev.clear();
        t.translate(&input, 1.0, frame_time + 7.0, "", &mut ev);
        input.end_frame();
        ev.clear();
        t.translate(&input, 1.0, frame_time + 7.1, "", &mut ev);
        assert_eq!(repeats(&ev, egui::Key::Backspace), 0);
    }

    fn ch(s: &str) -> LogicalKey {
        LogicalKey::Character(s.into())
    }

    /// `(key, physical_key, pressed)` of every `Event::Key` in `ev`.
    fn keys(ev: &[egui::Event]) -> Vec<(egui::Key, Option<egui::Key>, bool)> {
        ev.iter()
            .filter_map(|e| match e {
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    ..
                } => Some((*key, *physical_key, *pressed)),
                _ => None,
            })
            .collect()
    }

    fn translate_frame(t: &mut InputTranslator, input: &mut Input, clipboard: &str) -> Vec<egui::Event> {
        let mut ev = Vec::new();
        t.translate(input, 1.0, 0.0, clipboard, &mut ev);
        input.end_frame();
        ev
    }

    #[test]
    fn logical_key_mapping_matches_egui_winit() {
        use egui::Key as E;
        assert_eq!(egui_logical_key(&ch("z")), Some(E::Z));
        assert_eq!(egui_logical_key(&ch("Z")), Some(E::Z));
        assert_eq!(egui_logical_key(&ch("?")), Some(E::Questionmark));
        assert_eq!(egui_logical_key(&ch(":")), Some(E::Colon));
        assert_eq!(egui_logical_key(&ch("+")), Some(E::Plus));
        assert_eq!(egui_logical_key(&ch("é")), None);
        assert_eq!(egui_logical_key(&ch("я")), None);
        assert_eq!(egui_logical_key(&LogicalKey::Named(NamedKey::Enter)), Some(E::Enter));
        assert_eq!(egui_logical_key(&LogicalKey::Named(NamedKey::F5)), Some(E::F5));
        assert_eq!(egui_logical_key(&LogicalKey::Named(NamedKey::Shift)), None);
        assert_eq!(egui_logical_key(&LogicalKey::Dead(Some('^'))), None);
    }

    #[test]
    fn egui_gets_the_logical_key_and_the_physical_key() {
        use egui::Key as E;
        let mut t = InputTranslator::default();
        let mut input = Input::default();
        // (physical key, modifier-free key, logical key if different, egui key, egui physical key)
        type Case = (KeyCode, LogicalKey, Option<LogicalKey>, E, Option<E>);
        let cases: [Case; 9] = [
            // AZERTY: QWERTY's W position is labelled Z.
            (KeyCode::KeyW, ch("z"), None, E::Z, Some(E::W)),
            // QWERTZ: QWERTY's Y position is labelled Z.
            (KeyCode::KeyY, ch("z"), None, E::Z, Some(E::Y)),
            // Dvorak: QWERTY's period position types "v".
            (KeyCode::Period, ch("v"), None, E::V, Some(E::Period)),
            // JIS: QWERTY's quote position types ":".
            (KeyCode::Quote, ch(":"), None, E::Colon, Some(E::Quote)),
            // AZERTY digit row types "&": egui has no such key, the physical key is used.
            (KeyCode::Digit1, ch("&"), None, E::Num1, Some(E::Num1)),
            // Cyrillic: non-Latin letters fall back to the physical key (Ctrl+Z still works).
            (KeyCode::KeyZ, ch("я"), None, E::Z, Some(E::Z)),
            // US Shift+/: the shifted logical key, like egui-winit.
            (KeyCode::Slash, ch("/"), Some(ch("?")), E::Questionmark, Some(E::Slash)),
            // macOS AZERTY Option+(labelled W): logical "Ω", the modifier-free key names W.
            (KeyCode::KeyZ, ch("w"), Some(ch("Ω")), E::W, Some(E::Z)),
            // A key egui has no physical key for, whose character it knows (JIS ろ).
            (KeyCode::IntlRo, ch("\\"), None, E::Backslash, None),
        ];
        for (code, key, logical, want, want_physical) in cases {
            let mut press = space3d_platform::KeyEvent::press(code, key.clone());
            if let Some(l) = logical {
                press = press.with_logical_key(l);
            }
            input.inject_key_event(press);
            input.inject_key_up_logical(code, key);
            let ev = translate_frame(&mut t, &mut input, "");
            assert_eq!(
                keys(&ev),
                vec![(want, want_physical, true), (want, want_physical, false)],
                "{code:?}"
            );
        }
        // Legacy injection (no logical key): physical for both.
        input.inject_key_tap(KeyCode::KeyW);
        let ev = translate_frame(&mut t, &mut input, "");
        assert_eq!(keys(&ev), vec![(E::W, Some(E::W), true), (E::W, Some(E::W), false)]);
    }

    #[test]
    fn release_matches_its_press() {
        let mut t = InputTranslator::default();
        let mut input = Input::default();
        // US: Shift, then "/" pressed as "?"; Shift released before the key.
        input.inject_key_down(KeyCode::ShiftLeft);
        input.inject_key_event(space3d_platform::KeyEvent::press(KeyCode::Slash, ch("/")).with_logical_key(ch("?")));
        let ev = translate_frame(&mut t, &mut input, "");
        assert_eq!(
            keys(&ev),
            vec![
                (egui::Key::Questionmark, Some(egui::Key::Slash), true),
                (egui::Key::ShiftLeft, Some(egui::Key::ShiftLeft), true)
            ]
        );
        input.inject_key_up(KeyCode::ShiftLeft);
        translate_frame(&mut t, &mut input, "");
        input.inject_key_up_logical(KeyCode::Slash, ch("/"));
        let ev = translate_frame(&mut t, &mut input, "");
        assert_eq!(
            keys(&ev),
            vec![(egui::Key::Questionmark, Some(egui::Key::Slash), false)],
            "egui sees the key it saw pressed go up"
        );
    }

    #[test]
    fn clipboard_shortcuts_follow_the_layout() {
        let cmd = if cfg!(any(target_os = "macos", target_os = "ios")) {
            KeyCode::SuperLeft
        } else {
            KeyCode::ControlLeft
        };
        let mut t = InputTranslator::default();
        let mut input = Input::default();
        input.inject_key_down(cmd);
        // Dvorak: "v" is on QWERTY's period position; QWERTY's V position types "k".
        input.inject_key_tap_logical(KeyCode::Period, ch("v"));
        let ev = translate_frame(&mut t, &mut input, "clip");
        assert!(ev.contains(&egui::Event::Paste("clip".into())), "{ev:?}");
        assert!(
            !ev.iter().any(|e| matches!(e, egui::Event::Text(_))),
            "no text under Cmd"
        );
        input.inject_key_tap_logical(KeyCode::KeyV, ch("k"));
        let ev = translate_frame(&mut t, &mut input, "clip");
        assert!(!ev.iter().any(|e| matches!(e, egui::Event::Paste(_))));
        assert_eq!(keys(&ev)[0].0, egui::Key::K);
        // AZERTY: "c" and "x" keep their QWERTY positions.
        input.inject_key_tap_logical(KeyCode::KeyC, ch("c"));
        input.inject_key_tap_logical(KeyCode::KeyX, ch("x"));
        let ev = translate_frame(&mut t, &mut input, "");
        assert!(ev.contains(&egui::Event::Copy) && ev.contains(&egui::Event::Cut));
    }

    #[test]
    fn keys_table_is_fully_mapped() {
        for &k in KEYS {
            assert!(egui_key(k).is_some(), "{k:?} unmapped");
        }
        assert_eq!(egui_key(KeyCode::KeyA), Some(egui::Key::A));
        assert_eq!(egui_key(KeyCode::CapsLock), None);
    }

    #[test]
    fn mouse_click_text_and_wheel() {
        let mut t = InputTranslator::default();
        let mut input = Input::default();
        input.inject_mouse_move(Vec2::new(20.0, 40.0));
        input.inject_mouse_button_down(MouseButton::Left);
        input.inject_mouse_button_up(MouseButton::Left);
        input.inject_text("hi\u{8}");
        input.inject_wheel(Vec2::new(0.0, -1.0));
        let mut ev = Vec::new();
        t.translate(&input, 2.0, 0.0, "", &mut ev);
        assert!(ev.contains(&egui::Event::PointerMoved(egui::pos2(10.0, 20.0))));
        let presses: Vec<bool> = ev
            .iter()
            .filter_map(|e| match e {
                egui::Event::PointerButton { pressed, .. } => Some(*pressed),
                _ => None,
            })
            .collect();
        assert_eq!(presses, vec![true, false], "tap = press then release");
        assert!(ev.contains(&egui::Event::Text("hi".into())), "control chars filtered");
        assert!(ev.iter().any(|e| matches!(e, egui::Event::MouseWheel { .. })));
        // Unchanged position: no new move event.
        input.end_frame();
        ev.clear();
        t.translate(&input, 2.0, 0.0, "", &mut ev);
        assert!(ev.is_empty(), "{ev:?}");
        input.inject_mouse_left();
        t.translate(&input, 2.0, 0.0, "", &mut ev);
        assert_eq!(ev, vec![egui::Event::PointerGone]);
    }

    #[test]
    fn keys_modifiers_and_clipboard() {
        let mut t = InputTranslator::default();
        let mut input = Input::default();
        let cmd = if cfg!(any(target_os = "macos", target_os = "ios")) {
            KeyCode::SuperLeft
        } else {
            KeyCode::ControlLeft
        };
        input.inject_key_down(cmd);
        input.inject_key_tap(KeyCode::KeyV);
        let mut ev = Vec::new();
        t.translate(&input, 1.0, 0.0, "pasted", &mut ev);
        assert!(
            ev.iter()
                .any(|e| matches!(e, egui::Event::ModifiersChanged(m) if m.command))
        );
        assert!(ev.contains(&egui::Event::Paste("pasted".into())));
        assert!(
            !ev.iter()
                .any(|e| matches!(e, egui::Event::Key { key: egui::Key::V, .. }))
        );
        input.end_frame();
        input.inject_key_up(cmd);
        input.inject_key_tap(KeyCode::Enter);
        ev.clear();
        t.translate(&input, 1.0, 0.0, "", &mut ev);
        let enter: Vec<bool> = ev
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key {
                    key: egui::Key::Enter,
                    pressed,
                    ..
                } => Some(*pressed),
                _ => None,
            })
            .collect();
        assert_eq!(enter, vec![true, false]);
    }

    #[test]
    fn touch_drives_pointer() {
        let mut t = InputTranslator::default();
        let mut input = Input::default();
        input.inject_touch(7, space3d_platform::TouchPhase::Started, Vec2::new(10.0, 10.0));
        let mut ev = Vec::new();
        t.translate(&input, 1.0, 0.0, "", &mut ev);
        assert!(ev.iter().any(|e| matches!(
            e,
            egui::Event::Touch {
                phase: egui::TouchPhase::Start,
                ..
            }
        )));
        assert!(
            ev.iter()
                .any(|e| matches!(e, egui::Event::PointerButton { pressed: true, .. }))
        );
        input.end_frame();
        input.inject_touch(7, space3d_platform::TouchPhase::Ended, Vec2::new(12.0, 10.0));
        ev.clear();
        t.translate(&input, 1.0, 0.0, "", &mut ev);
        assert!(
            ev.iter()
                .any(|e| matches!(e, egui::Event::PointerButton { pressed: false, .. }))
        );
        assert_eq!(ev.last(), Some(&egui::Event::PointerGone));
    }
}
