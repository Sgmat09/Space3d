//! Keyboard layouts: what the player's keys *print*, as opposed to where they are.
//!
//! # Physical and logical keys
//!
//! Every keyboard event carries two identities:
//!
//! * the **physical key** ([`KeyCode`]) — the key's *position*, named after the US QWERTY key
//!   in that place (`KeyCode::KeyW` is the key right of Tab on every keyboard, whatever it is
//!   labelled). [`Input::pressed`](crate::Input::pressed) and
//!   [`Binding::Key`](crate::Binding::Key) use physical keys;
//! * the **logical key** ([`LogicalKey`]) — what the key *means* on the player's layout: the
//!   same key is `"z"` on AZERTY, `"w"` on QWERTY, `","` on Dvorak. The runner records it on
//!   every [`KeyEvent`](crate::KeyEvent) (see [`Input::key_events`](crate::Input::key_events)).
//!
//! # Bindings
//!
//! Bind **physical** keys for movement and other position-based controls: WASD bound as
//! `KeyW/KeyA/KeyS/KeyD` stays a comfortable inverted T on AZERTY (`Z/Q/S/D`), QWERTZ and
//! Dvorak. Show the player what to press with the layout's label, not the code name:
//!
//! ```
//! use space3d_platform::*;
//! let input = Input::default();
//! // "W" on QWERTY, "Z" on AZERTY (once known — see below), never "KeyW".
//! let cap = input.key_cap(KeyCode::KeyW);
//! assert!(!cap.is_empty());
//! ```
//!
//! Text and editing shortcuts follow the layout instead: the UI (egui) receives the logical
//! key, so `Ctrl/Cmd+Z` undoes with the key *labelled* Z (physical `KeyW` on AZERTY), as in
//! native apps. Games with letter-mnemonic commands ("I" for inventory) can match
//! [`Input::logical_key`](crate::Input::logical_key) instead of the physical key.
//!
//! # Key labels
//!
//! [`Input::key_label`](crate::Input::key_label) returns what a physical key prints on the
//! current layout **without modifiers** (`"q"` for `KeyA` on AZERTY; dead keys give their base
//! character, `"^"`), and English names for non-character keys (`"Space"`, `"Enter"`,
//! `"Shift"`, `"F1"` — see [`english_key_name`]; games should localize those themselves).
//! [`Input::key_cap`](crate::Input::key_cap) turns that into key-cap text (`"Q"`, never empty).
//! Labels come, in order of preference, from:
//!
//! 1. **learned** — the layout's key reported by the OS for keys already pressed on this
//!    layout (winit's `key_without_modifiers`, or its logical key when pressed without
//!    Shift/Alt on platforms without it);
//! 2. **the OS layout table** ([`KeyLabelSource::Os`]), read by the window runner at start-up,
//!    on focus and when the layout changes, and cached (never queried per frame);
//! 3. nothing (`None`) for character keys that are still unknown; `key_cap` then shows the US
//!    QWERTY character of that position.
//!
//! | platform | OS table | layout changes |
//! |---|---|---|
//! | macOS | `TISCopyCurrentKeyboardLayoutInputSource` + `UCKeyTranslate` | polled once per second and on focus (input source id) |
//! | Windows | `MapVirtualKeyExW` + `ToUnicodeEx` for the thread's `HKL` | polled once per second and on focus (`HKL`) |
//! | Web | `navigator.keyboard.getLayoutMap()` (Chromium-based browsers; async) | re-read on focus |
//! | Linux (X11/Wayland) | none: learned only (winit reports `key_without_modifiers`) | learned labels update on the next press |
//! | Android, iOS | none: learned only (logical key of unmodified presses) | learned labels update on the next press |
//!
//! Limitations: learned labels only exist for keys pressed at least once; a layout switch that
//! the platform cannot report (Linux, mobile, Firefox/Safari) keeps the previous learned labels
//! until each key is pressed again; the first OS query on macOS initializes the Text Input
//! Services (tens of ms, once, at start-up); input methods (IME) report the layout they type
//! through (e.g. romaji for Japanese).

use crate::input::KeyCode;
use std::collections::HashMap;

pub use winit::keyboard::{Key as LogicalKey, KeyLocation, NamedKey};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_os = "windows")]
mod windows;

/// The writing-system keys (W3C UI Events "writing system keys"): the keys whose character
/// depends on the layout, with their W3C `code` name and unshifted US QWERTY character.
const WRITING_KEYS: [(KeyCode, &str, &str); 50] = {
    use KeyCode as K;
    [
        (K::Backquote, "Backquote", "`"),
        (K::Digit1, "Digit1", "1"),
        (K::Digit2, "Digit2", "2"),
        (K::Digit3, "Digit3", "3"),
        (K::Digit4, "Digit4", "4"),
        (K::Digit5, "Digit5", "5"),
        (K::Digit6, "Digit6", "6"),
        (K::Digit7, "Digit7", "7"),
        (K::Digit8, "Digit8", "8"),
        (K::Digit9, "Digit9", "9"),
        (K::Digit0, "Digit0", "0"),
        (K::Minus, "Minus", "-"),
        (K::Equal, "Equal", "="),
        (K::IntlYen, "IntlYen", "¥"),
        (K::KeyQ, "KeyQ", "q"),
        (K::KeyW, "KeyW", "w"),
        (K::KeyE, "KeyE", "e"),
        (K::KeyR, "KeyR", "r"),
        (K::KeyT, "KeyT", "t"),
        (K::KeyY, "KeyY", "y"),
        (K::KeyU, "KeyU", "u"),
        (K::KeyI, "KeyI", "i"),
        (K::KeyO, "KeyO", "o"),
        (K::KeyP, "KeyP", "p"),
        (K::BracketLeft, "BracketLeft", "["),
        (K::BracketRight, "BracketRight", "]"),
        (K::Backslash, "Backslash", "\\"),
        (K::KeyA, "KeyA", "a"),
        (K::KeyS, "KeyS", "s"),
        (K::KeyD, "KeyD", "d"),
        (K::KeyF, "KeyF", "f"),
        (K::KeyG, "KeyG", "g"),
        (K::KeyH, "KeyH", "h"),
        (K::KeyJ, "KeyJ", "j"),
        (K::KeyK, "KeyK", "k"),
        (K::KeyL, "KeyL", "l"),
        (K::Semicolon, "Semicolon", ";"),
        (K::Quote, "Quote", "'"),
        (K::IntlBackslash, "IntlBackslash", "\\"),
        (K::KeyZ, "KeyZ", "z"),
        (K::KeyX, "KeyX", "x"),
        (K::KeyC, "KeyC", "c"),
        (K::KeyV, "KeyV", "v"),
        (K::KeyB, "KeyB", "b"),
        (K::KeyN, "KeyN", "n"),
        (K::KeyM, "KeyM", "m"),
        (K::Comma, "Comma", ","),
        (K::Period, "Period", "."),
        (K::Slash, "Slash", "/"),
        (K::IntlRo, "IntlRo", "\\"),
    ]
};

fn writing_key(key: KeyCode) -> Option<&'static (KeyCode, &'static str, &'static str)> {
    WRITING_KEYS.iter().find(|w| w.0 == key)
}

/// The physical keys whose character depends on the layout (letters, digits, punctuation,
/// `IntlBackslash`, `IntlRo`, `IntlYen`), in keyboard order. These are the keys
/// [`Input::key_label`](crate::Input::key_label) reads from the OS table or learns; every
/// other key has a fixed [`english_key_name`].
pub fn writing_system_keys() -> impl Iterator<Item = KeyCode> {
    WRITING_KEYS.iter().map(|w| w.0)
}

/// Whether `key` is one of the [`writing_system_keys`].
pub fn is_writing_system_key(key: KeyCode) -> bool {
    writing_key(key).is_some()
}

/// What `key` prints on a US QWERTY layout, unshifted (`"a"`, `";"`). `None` for keys that
/// are not [`writing_system_keys`].
pub fn us_qwerty_label(key: KeyCode) -> Option<&'static str> {
    writing_key(key).map(|w| w.2)
}

/// The W3C UI Events `code` name of a writing-system key (`"KeyA"`, `"BracketLeft"`).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn w3c_code(key: KeyCode) -> Option<&'static str> {
    writing_key(key).map(|w| w.1)
}

/// English name of a key that does not type a layout-dependent character (`"Space"`,
/// `"Enter"`, `"Shift"`, `"F1"`, `"Numpad 1"`). `None` for [`writing_system_keys`] and exotic
/// keys. Modifier names follow the platform (`"Command"`/`"Option"` on Apple platforms,
/// `"Windows"` on Windows). Games should translate these for their players.
pub fn english_key_name(key: KeyCode) -> Option<&'static str> {
    use KeyCode as K;
    let apple = cfg!(any(target_os = "macos", target_os = "ios"));
    Some(match key {
        K::Space => "Space",
        K::Enter => "Enter",
        K::Tab => "Tab",
        K::Backspace => "Backspace",
        K::Escape => "Escape",
        K::CapsLock => "Caps Lock",
        K::ShiftLeft | K::ShiftRight => "Shift",
        K::ControlLeft | K::ControlRight if apple => "Control",
        K::ControlLeft | K::ControlRight => "Ctrl",
        K::AltLeft | K::AltRight if apple => "Option",
        K::AltLeft | K::AltRight => "Alt",
        K::SuperLeft | K::SuperRight if apple => "Command",
        K::SuperLeft | K::SuperRight if cfg!(target_os = "windows") => "Windows",
        K::SuperLeft | K::SuperRight if cfg!(target_arch = "wasm32") => "Meta",
        K::SuperLeft | K::SuperRight => "Super",
        K::Fn => "Fn",
        K::FnLock => "Fn Lock",
        K::ContextMenu => "Menu",
        K::ArrowUp => "Up",
        K::ArrowDown => "Down",
        K::ArrowLeft => "Left",
        K::ArrowRight => "Right",
        K::Insert => "Insert",
        K::Delete => "Delete",
        K::Home => "Home",
        K::End => "End",
        K::PageUp => "Page Up",
        K::PageDown => "Page Down",
        K::Help => "Help",
        K::PrintScreen => "Print Screen",
        K::ScrollLock => "Scroll Lock",
        K::Pause => "Pause",
        K::NumLock => "Num Lock",
        K::Numpad0 => "Numpad 0",
        K::Numpad1 => "Numpad 1",
        K::Numpad2 => "Numpad 2",
        K::Numpad3 => "Numpad 3",
        K::Numpad4 => "Numpad 4",
        K::Numpad5 => "Numpad 5",
        K::Numpad6 => "Numpad 6",
        K::Numpad7 => "Numpad 7",
        K::Numpad8 => "Numpad 8",
        K::Numpad9 => "Numpad 9",
        K::NumpadAdd => "Numpad +",
        K::NumpadSubtract => "Numpad -",
        K::NumpadMultiply => "Numpad *",
        K::NumpadDivide => "Numpad /",
        K::NumpadDecimal => "Numpad .",
        K::NumpadComma => "Numpad ,",
        K::NumpadEqual => "Numpad =",
        K::NumpadEnter => "Numpad Enter",
        K::NumpadBackspace => "Numpad Backspace",
        K::NumpadClear => "Clear",
        K::NumpadHash => "Numpad #",
        K::NumpadStar => "Numpad *",
        K::NumpadParenLeft => "Numpad (",
        K::NumpadParenRight => "Numpad )",
        K::F1 => "F1",
        K::F2 => "F2",
        K::F3 => "F3",
        K::F4 => "F4",
        K::F5 => "F5",
        K::F6 => "F6",
        K::F7 => "F7",
        K::F8 => "F8",
        K::F9 => "F9",
        K::F10 => "F10",
        K::F11 => "F11",
        K::F12 => "F12",
        K::F13 => "F13",
        K::F14 => "F14",
        K::F15 => "F15",
        K::F16 => "F16",
        K::F17 => "F17",
        K::F18 => "F18",
        K::F19 => "F19",
        K::F20 => "F20",
        K::F21 => "F21",
        K::F22 => "F22",
        K::F23 => "F23",
        K::F24 => "F24",
        K::KanaMode => "Kana",
        K::Lang1 => "Kana/Hangul",
        K::Lang2 => "Eisu/Hanja",
        K::Convert => "Convert",
        K::NonConvert => "Non-convert",
        K::AudioVolumeUp => "Volume Up",
        K::AudioVolumeDown => "Volume Down",
        K::AudioVolumeMute => "Mute",
        K::MediaPlayPause => "Play/Pause",
        K::MediaStop => "Stop",
        K::MediaTrackNext => "Next Track",
        K::MediaTrackPrevious => "Previous Track",
        K::BrowserBack => "Back",
        K::BrowserForward => "Forward",
        K::Copy => "Copy",
        K::Cut => "Cut",
        K::Paste => "Paste",
        K::Undo => "Undo",
        _ => return None,
    })
}

/// Where a [`KeyLabel`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyLabelSource {
    /// The operating system's table of the current layout (macOS, Windows, Chromium).
    Os,
    /// Learned from a key event observed on the current layout.
    Learned,
    /// A fixed English name of a non-character key ([`english_key_name`]).
    Named,
}

/// What a physical key prints on the current keyboard layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyLabel {
    /// The physical key.
    pub key: KeyCode,
    /// Character(s) typed without modifiers (`"q"`, `"é"`, dead keys: their base character
    /// `"^"`), or the English name of a non-character key (`"Space"`).
    pub text: String,
    /// Character(s) typed with Shift, when known (`"Q"`, `"2"` for AZERTY's `é` key).
    pub shifted: Option<String>,
    /// Where the label came from.
    pub source: KeyLabelSource,
}

impl KeyLabel {
    /// Text for a key cap: letters upper-cased as keyboards print them (`"Q"`), everything
    /// else as typed (`"é"`, `"-"`, `"Space"`). A letter is upper-cased when Shift is known to
    /// type its upper case, or — when the shifted character is unknown — on the letter rows
    /// (`KeyA`..`KeyZ`).
    pub fn cap(&self) -> String {
        if self.source == KeyLabelSource::Named {
            return self.text.clone();
        }
        let mut chars = self.text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return self.text.clone();
        };
        let upper: String = c.to_uppercase().collect();
        if upper.chars().count() != 1 || upper == self.text {
            return self.text.clone();
        }
        let shift_types_upper = match &self.shifted {
            Some(s) => *s == upper,
            None => c.is_alphabetic() && is_letter_position(self.key),
        };
        if shift_types_upper { upper } else { self.text.clone() }
    }
}

fn is_letter_position(key: KeyCode) -> bool {
    writing_key(key).is_some_and(|w| w.1.starts_with("Key"))
}

/// What a key event taught about one key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Learned {
    unshifted: Option<String>,
    shifted: Option<String>,
}

/// The current keyboard layout's labels: an OS table (when the platform has one) plus labels
/// learned from key events. Lives in [`Input`](crate::Input); the window runner keeps it up
/// to date. See the [module docs](self).
#[derive(Clone, Debug, Default)]
pub struct KeyboardLayout {
    os_id: Option<String>,
    os: HashMap<KeyCode, (String, Option<String>)>,
    learned: HashMap<KeyCode, Learned>,
    generation: u64,
}

impl KeyboardLayout {
    /// Label of a [writing-system key](writing_system_keys) (learned first, then the OS
    /// table). `None` for other keys and unknown ones; see
    /// [`Input::key_label_info`](crate::Input::key_label_info) for the full lookup.
    pub fn label(&self, key: KeyCode) -> Option<KeyLabel> {
        let os = self.os.get(&key);
        if let Some(learned) = self.learned.get(&key)
            && let Some(text) = &learned.unshifted
        {
            let shifted = learned
                .shifted
                .clone()
                .or_else(|| os.filter(|o| o.0 == *text).and_then(|o| o.1.clone()));
            return Some(KeyLabel {
                key,
                text: text.clone(),
                shifted,
                source: KeyLabelSource::Learned,
            });
        }
        os.map(|(text, shifted)| KeyLabel {
            key,
            text: text.clone(),
            shifted: shifted.clone(),
            source: KeyLabelSource::Os,
        })
    }

    /// Record what `key` typed: `unshifted` without modifiers, `shifted` with Shift only.
    /// Ignored for keys that are not [`writing_system_keys`] and for text that is not a
    /// printable label (control characters, private-use glyphs). Returns whether a label
    /// changed.
    pub fn learn(&mut self, key: KeyCode, unshifted: Option<&str>, shifted: Option<&str>) -> bool {
        if !is_writing_system_key(key) {
            return false;
        }
        let unshifted = unshifted.and_then(normalize_label);
        let shifted = shifted.and_then(normalize_label);
        if unshifted.is_none() && shifted.is_none() {
            return false;
        }
        let entry = self.learned.entry(key).or_default();
        let before = entry.clone();
        if unshifted.is_some() {
            entry.unshifted = unshifted;
        }
        if shifted.is_some() {
            entry.shifted = shifted;
        }
        let changed = *entry != before;
        if changed {
            self.generation += 1;
        }
        changed
    }

    /// Replace the OS table: layout `id` (platform-specific, e.g.
    /// `"com.apple.keylayout.French"`, a Windows `HKL`) and `(key, unshifted, shifted)`
    /// labels. A different layout (id or labels) also forgets the learned labels, which
    /// belonged to the previous layout. Returns whether anything changed. Custom platforms
    /// and tests can call this directly.
    pub fn set_os_layout(
        &mut self,
        id: Option<String>,
        labels: impl IntoIterator<Item = (KeyCode, String, Option<String>)>,
    ) -> bool {
        let mut table = HashMap::new();
        for (key, unshifted, shifted) in labels {
            if !is_writing_system_key(key) {
                continue;
            }
            if let Some(text) = normalize_label(&unshifted) {
                table.insert(key, (text, shifted.as_deref().and_then(normalize_label)));
            }
        }
        if id == self.os_id && table == self.os {
            return false;
        }
        self.os_id = id;
        self.os = table;
        self.learned.clear();
        self.generation += 1;
        true
    }

    /// Forget the OS table (labels fall back to learned ones).
    pub fn clear_os_layout(&mut self) {
        if self.os_id.is_some() || !self.os.is_empty() {
            self.os_id = None;
            self.os.clear();
            self.generation += 1;
        }
    }

    /// Forget every learned label.
    pub fn clear_learned(&mut self) {
        if !self.learned.is_empty() {
            self.learned.clear();
            self.generation += 1;
        }
    }

    /// Whether an OS table is loaded.
    pub fn has_os_layout(&self) -> bool {
        !self.os.is_empty()
    }

    /// Platform id of the layout the OS table was read from, if any.
    pub fn os_layout_id(&self) -> Option<&str> {
        self.os_id.as_deref()
    }

    /// Increases whenever a label may have changed: cache key-cap text and rebuild it when
    /// this moves.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Re-read the OS table if the platform has one and the layout changed (or none is
    /// loaded). Synchronous on macOS and Windows — call it on the main (event loop) thread,
    /// as the window runner does; on the web it applies the result of the previous request
    /// and starts a new asynchronous one; elsewhere it does nothing. Returns whether the
    /// labels changed.
    pub fn refresh_from_os(&mut self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let id = os::current_layout_id();
            if id.is_some() && self.has_os_layout() && id.as_deref() == self.os_layout_id() {
                return false;
            }
            match os::read_layout() {
                Some(layout) => self.set_os_layout(layout.id, layout.keys),
                None => false,
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let changed = self.apply_web_result();
            web::request();
            changed
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_arch = "wasm32")))]
        {
            false
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn apply_web_result(&mut self) -> bool {
        web::take().is_some_and(|layout| self.set_os_layout(layout.id, layout.keys))
    }
}

/// A layout table read from the OS.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_arch = "wasm32")),
    allow(dead_code)
)]
pub(crate) struct OsLayout {
    pub id: Option<String>,
    pub keys: Vec<(KeyCode, String, Option<String>)>,
}

#[cfg(target_os = "macos")]
use macos as os;
#[cfg(target_os = "windows")]
use windows as os;

/// The printable label of a logical key: the character(s) of `Character`, the base character
/// of `Dead(Some(_))`; `None` otherwise.
pub(crate) fn logical_key_label(key: &LogicalKey) -> Option<String> {
    match key {
        LogicalKey::Character(s) => normalize_label(s),
        LogicalKey::Dead(Some(c)) => normalize_label(c.encode_utf8(&mut [0; 4])),
        _ => None,
    }
}

/// A lone combining mark (what some platforms report for dead keys) as its spacing form.
fn spacing_form(c: char) -> Option<char> {
    Some(match c {
        '\u{300}' => '`',
        '\u{301}' => '´',
        '\u{302}' => '^',
        '\u{303}' => '~',
        '\u{304}' => '¯',
        '\u{306}' => '˘',
        '\u{307}' => '˙',
        '\u{308}' => '¨',
        '\u{30a}' => '˚',
        '\u{30b}' => '˝',
        '\u{30c}' => 'ˇ',
        '\u{327}' => '¸',
        '\u{328}' => '˛',
        _ => return None,
    })
}

/// Clean OS/event text into a label: lone combining marks become their spacing form; empty,
/// control and private-use text (e.g. macOS function-key glyphs) is rejected.
pub(crate) fn normalize_label(s: &str) -> Option<String> {
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && let Some(spacing) = spacing_form(c)
    {
        return Some(spacing.to_string());
    }
    let private_use = |c: char| {
        ('\u{e000}'..='\u{f8ff}').contains(&c)
            || ('\u{f0000}'..='\u{ffffd}').contains(&c)
            || ('\u{100000}'..='\u{10fffd}').contains(&c)
    };
    let ok = !s.is_empty() && s.chars().all(|c| !c.is_control() && !private_use(c));
    ok.then(|| s.to_owned())
}

/// Runner hook: keeps [`KeyboardLayout`]'s OS table current without per-frame OS calls.
#[derive(Debug, Default)]
pub(crate) struct LayoutWatch {
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    last_check: Option<web_time::Instant>,
}

impl LayoutWatch {
    /// Minimum time between two layout-id checks (macOS, Windows).
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    /// Called by the runner on the event-loop thread: every frame (`force = false`) and on
    /// start-up / focus gain (`force = true`).
    pub(crate) fn poll(&mut self, now: web_time::Instant, force: bool, layout: &mut KeyboardLayout) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            if !force
                && self
                    .last_check
                    .is_some_and(|last| now.duration_since(last) < Self::INTERVAL)
            {
                return;
            }
            self.last_check = Some(now);
            layout.refresh_from_os();
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = now;
            layout.apply_web_result();
            if force {
                web::request();
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_arch = "wasm32")))]
        {
            let _ = (now, force, layout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_keys_and_names_do_not_overlap() {
        let keys: Vec<KeyCode> = writing_system_keys().collect();
        assert_eq!(keys.len(), 50);
        for k in &keys {
            assert!(english_key_name(*k).is_none(), "{k:?} has a name");
            assert!(us_qwerty_label(*k).is_some());
            assert_eq!(w3c_code(*k), Some(format!("{k:?}").as_str()), "W3C code of {k:?}");
        }
        assert!(!is_writing_system_key(KeyCode::Space));
        assert_eq!(english_key_name(KeyCode::Space), Some("Space"));
        assert_eq!(english_key_name(KeyCode::F1), Some("F1"));
        assert_eq!(english_key_name(KeyCode::ShiftRight), Some("Shift"));
        assert_eq!(english_key_name(KeyCode::Numpad7), Some("Numpad 7"));
        assert_eq!(us_qwerty_label(KeyCode::Semicolon), Some(";"));
    }

    #[test]
    fn labels_are_normalized() {
        assert_eq!(normalize_label("q").as_deref(), Some("q"));
        assert_eq!(normalize_label("\u{302}").as_deref(), Some("^"), "dead circumflex");
        assert_eq!(normalize_label("\u{308}").as_deref(), Some("¨"));
        assert_eq!(normalize_label(""), None);
        assert_eq!(normalize_label("\r"), None);
        assert_eq!(normalize_label("\u{f704}"), None, "macOS F1 glyph");
        assert_eq!(logical_key_label(&LogicalKey::Dead(Some('´'))).as_deref(), Some("´"));
        assert_eq!(logical_key_label(&LogicalKey::Dead(None)), None);
        assert_eq!(logical_key_label(&LogicalKey::Named(NamedKey::Enter)), None);
    }

    #[test]
    fn learned_labels_win_and_layout_changes_forget_them() {
        let mut l = KeyboardLayout::default();
        assert!(l.set_os_layout(
            Some("us".into()),
            [
                (KeyCode::KeyA, "a".into(), Some("A".into())),
                (KeyCode::Digit2, "2".into(), Some("@".into())),
                (KeyCode::F1, "x".into(), None), // not a writing-system key: ignored
            ],
        ));
        assert!(!l.set_os_layout(
            Some("us".into()),
            [
                (KeyCode::KeyA, "a".into(), Some("A".into())),
                (KeyCode::Digit2, "2".into(), Some("@".into())),
            ],
        ));
        let g = l.generation();
        assert_eq!(l.label(KeyCode::KeyA).unwrap().source, KeyLabelSource::Os);
        assert!(l.learn(KeyCode::KeyA, Some("q"), None));
        assert!(!l.learn(KeyCode::KeyA, Some("q"), None), "unchanged");
        assert!(!l.learn(KeyCode::Enter, Some("\r"), None));
        assert!(l.generation() > g);
        let a = l.label(KeyCode::KeyA).unwrap();
        assert_eq!((a.text.as_str(), a.source), ("q", KeyLabelSource::Learned));
        assert_eq!(a.shifted, None, "OS shifted value belongs to another character");
        assert_eq!(l.label(KeyCode::F1), None);
        // Another layout: learned labels are dropped.
        assert!(l.set_os_layout(Some("fr".into()), [(KeyCode::KeyA, "q".into(), Some("Q".into()))]));
        let a = l.label(KeyCode::KeyA).unwrap();
        assert_eq!((a.text.as_str(), a.source), ("q", KeyLabelSource::Os));
        assert_eq!(l.label(KeyCode::Digit2), None);
        l.clear_os_layout();
        assert!(!l.has_os_layout() && l.os_layout_id().is_none());
    }

    #[test]
    fn key_caps() {
        let label = |key, text: &str, shifted: Option<&str>| KeyLabel {
            key,
            text: text.into(),
            shifted: shifted.map(Into::into),
            source: KeyLabelSource::Learned,
        };
        assert_eq!(label(KeyCode::KeyA, "q", None).cap(), "Q");
        assert_eq!(label(KeyCode::KeyA, "q", Some("Q")).cap(), "Q");
        assert_eq!(label(KeyCode::Digit2, "é", Some("2")).cap(), "é", "AZERTY digit row");
        assert_eq!(label(KeyCode::Digit2, "é", None).cap(), "é", "not a letter row");
        assert_eq!(label(KeyCode::Semicolon, "ö", Some("Ö")).cap(), "Ö", "German ö");
        assert_eq!(label(KeyCode::Minus, "ß", Some("?")).cap(), "ß");
        assert_eq!(label(KeyCode::KeyZ, "я", None).cap(), "Я", "Cyrillic");
        assert_eq!(label(KeyCode::Comma, ",", None).cap(), ",");
        let named = KeyLabel {
            key: KeyCode::Space,
            text: "Space".into(),
            shifted: None,
            source: KeyLabelSource::Named,
        };
        assert_eq!(named.cap(), "Space");
    }
}
