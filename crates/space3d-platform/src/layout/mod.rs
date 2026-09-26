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
//! native apps, and an egui hotkey read (`key_pressed(egui::Key::M)`) fires on the key
//! labelled M (`KeyCode::Semicolon` on AZERTY). Code that relates such reads to physical
//! bindings (e.g. "is this key already bound?") must translate them first
//! (`space3d_ui::physical_key_for_egui_key`). Games with letter-mnemonic commands ("I" for
//! inventory) can match [`Input::logical_key`](crate::Input::logical_key) instead of the
//! physical key.
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
//! The reverse lookup — which physical key types a given text — is
//! [`KeyboardLayout::key_for_label`] (`"m"` → `KeyCode::Semicolon` on AZERTY) and
//! [`KeyboardLayout::key_for_label_matching`]; both read a map cached per
//! [`KeyboardLayout::generation`], so they do not allocate while the labels stay the same
//! (`space3d_ui::physical_key_for_egui_key` is built on them). [`KeyboardLayout::label_text`]
//! borrows a label without allocating.
//!
//! | platform | OS table | layout changes |
//! |---|---|---|
//! | macOS | `TISCopyCurrentKeyboardLayoutInputSource` + `UCKeyTranslate` (main thread), by virtual key (ISO and JIS keys: see below) | polled once per second and on focus (input source id, keyboard type) |
//! | Windows | `MapVirtualKeyExW` + `ToUnicodeEx` for the thread's `HKL` | polled once per second and on focus (`HKL`) |
//! | Web | `navigator.keyboard.getLayoutMap()` (Chromium-based browsers; async) | re-read on focus |
//! | Linux (X11/Wayland) | none: learned only (winit reports `key_without_modifiers`) | learned labels update on the next press |
//! | Android, iOS | none: learned only (logical key of unmodified presses) | learned labels update on the next press |
//!
//! Limitations:
//!
//! * learned labels only exist for keys pressed at least once; after a layout switch that the
//!   platform cannot report (Linux, mobile, Firefox/Safari) the previous learned labels stay
//!   until each key is pressed again (a key that types a new character forgets its old shifted
//!   one);
//! * without a modifier-free key (web, mobile), Caps Lock is undone for letters (`"A"` is
//!   learned as `"a"`), but where Caps Lock locks Shift on other keys (the digit row of Windows
//!   AZERTY) such a press teaches the shifted character until the key is pressed without it;
//! * macOS reads Text Input Sources on the main thread only (macOS 14+ aborts the process on
//!   other threads): [`KeyboardLayout::refresh_from_os`] reads nothing elsewhere. The first
//!   query initializes the Text Input Services (tens of ms, once, at start-up);
//! * macOS ISO keyboards (most European Macs): winit 0.30 reports both the key left of `1`
//!   (`§`, `@`, `^`; virtual key `0x0A`) and the key right of left Shift (`<`, `` ` ``; `0x32`)
//!   as [`KeyCode::Backquote`]. The window runner reads the virtual key of the event AppKit is
//!   dispatching and reports the key right of left Shift as [`KeyCode::IntlBackslash`] (its
//!   W3C position, as on Windows); each has its own OS label. When winit delivers
//!   an event late (queued behind another one) its virtual key is unknown and the key reads
//!   `Backquote`, so on ISO keyboards `Backquote`'s label comes from the OS table only (not
//!   learned from presses, which could flip it between the two keys);
//! * macOS JIS keyboards: winit 0.30 has no code for the Ro key (virtual key `0x5E`, left of
//!   right Shift); the runner reports it as [`KeyCode::IntlRo`], with its OS label (`"_"`);
//! * input methods (IME) report the layout they type through (e.g. romaji for Japanese).
//!
//! # Pinned layouts
//!
//! On macOS, Windows and the web the window runner keeps the OS table current, so a table set
//! with [`KeyboardLayout::set_os_layout`] (a fake or sample layout for screenshots, windowed
//! test runs, a "my keyboard is…" setting) is replaced within a second. Pin it with
//! [`KeyboardLayout::set_pinned`]: while pinned the OS table is not re-read and key events
//! teach nothing; unpinning lets the runner re-read the OS table on its next frame.

use crate::input::KeyCode;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use winit::keyboard::PhysicalKey;

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
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// A label (custom platforms, tests): `key` types `text`, and `shifted` with Shift.
    pub fn new(key: KeyCode, text: impl Into<String>, shifted: Option<String>, source: KeyLabelSource) -> Self {
        Self {
            key,
            text: text.into(),
            shifted,
            source,
        }
    }

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
    /// What the platform's layout check returned when the OS table was read (layout id, plus
    /// the keyboard type on macOS); `refresh_from_os` re-reads the table when it changes.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    os_check: Option<String>,
    os: HashMap<KeyCode, (String, Option<String>)>,
    /// Keys that stand for more than one physical key on this keyboard (macOS ISO keyboards:
    /// `Backquote`). Their label comes from the OS table only: learning would flip it between
    /// the keys.
    ambiguous: HashSet<KeyCode>,
    learned: HashMap<KeyCode, Learned>,
    generation: u64,
    /// Automatic updates suspended ([`Self::set_pinned`]).
    pinned: bool,
    /// Reverse lookup of the labels ([`Self::key_for_label`]): built on first use and dropped
    /// by every change, so it always belongs to the current [`Self::generation`].
    by_label: OnceLock<LabelIndex>,
}

/// The labels of one [`KeyboardLayout::generation`], by text.
#[derive(Clone, Debug, Default)]
struct LabelIndex {
    /// Each distinct label text with the first writing-system key (keyboard order) that
    /// prints it, in keyboard order.
    first: Vec<(String, KeyCode)>,
    /// The same, by text.
    by_text: HashMap<String, KeyCode>,
}

impl LabelIndex {
    fn build(layout: &KeyboardLayout) -> Self {
        let mut index = Self::default();
        for key in writing_system_keys() {
            if let Some(text) = layout.label_text(key)
                && !index.by_text.contains_key(text)
            {
                index.first.push((text.to_owned(), key));
                index.by_text.insert(text.to_owned(), key);
            }
        }
        index
    }
}

impl KeyboardLayout {
    /// The text of [`Self::label`] (`label(key).text`) without allocating: what `key` types
    /// without modifiers, learned first, then from the OS table. `None` for keys that are not
    /// [`writing_system_keys`] and unknown ones.
    pub fn label_text(&self, key: KeyCode) -> Option<&str> {
        self.learned
            .get(&key)
            .and_then(|l| l.unshifted.as_deref())
            .or_else(|| self.os.get(&key).map(|o| o.0.as_str()))
    }

    /// The physical key that types `text` without modifiers on this layout (the inverse of
    /// [`Self::label_text`]): `Some(KeyCode::Semicolon)` for `"m"` on AZERTY. When several
    /// keys print the same text, the first in keyboard order ([`writing_system_keys`]) wins.
    /// `None` when no key is known to type `text`.
    ///
    /// Looks up a reverse map that is built on the first lookup after the labels change
    /// ([`Self::generation`]) and cached until the next change: lookups in between do not
    /// allocate. Safe to call from several threads.
    pub fn key_for_label(&self, text: &str) -> Option<KeyCode> {
        self.label_index().by_text.get(text).copied()
    }

    /// The first physical key in keyboard order ([`writing_system_keys`]) whose label text
    /// satisfies `matches`, e.g. the key egui reads as a given `egui::Key`. Calls `matches`
    /// once per distinct label, in keyboard order, and stops at the first match; uses the
    /// same cached reverse map as [`Self::key_for_label`] (no allocations while the labels do
    /// not change).
    pub fn key_for_label_matching(&self, mut matches: impl FnMut(&str) -> bool) -> Option<KeyCode> {
        let index = self.label_index();
        index.first.iter().find(|(text, _)| matches(text)).map(|&(_, key)| key)
    }

    fn label_index(&self) -> &LabelIndex {
        self.by_label.get_or_init(|| LabelIndex::build(self))
    }

    /// Pin the labels (`true`) or let them follow the OS and key events again (`false`).
    ///
    /// While pinned nothing automatic changes the labels: the OS table is not re-read (the
    /// window runner's polls and focus checks, [`Self::refresh_from_os`], results of the
    /// web's asynchronous read) and key events teach nothing
    /// ([`Input::inject_key_event`](crate::Input::inject_key_event), the runner). Explicit
    /// calls still apply: [`Self::set_os_layout`], [`Self::learn`], [`Self::clear_os_layout`],
    /// [`Self::clear_learned`]. Use it to show a fake or sample layout in windowed runs,
    /// screenshots and tests on a machine with another layout:
    ///
    /// ```
    /// use space3d_platform::*;
    /// let mut input = Input::default();
    /// let layout = input.keyboard_layout_mut();
    /// layout.set_os_layout(
    ///     Some("sample-azerty".into()),
    ///     [(KeyCode::KeyA, "q".into(), Some("Q".into())), (KeyCode::KeyQ, "a".into(), Some("A".into()))],
    /// );
    /// layout.set_pinned(true); // the runner keeps this table until unpinned
    /// assert!(!input.refresh_keyboard_layout());
    /// input.inject_key_tap_logical(KeyCode::KeyA, LogicalKey::Character("a".into()));
    /// assert_eq!(input.key_cap(KeyCode::KeyA), "Q", "key events teach nothing while pinned");
    /// ```
    ///
    /// Unpinning does not change the labels by itself; the window runner re-reads the OS
    /// table on its next frame (macOS, Windows: replacing a table set with `set_os_layout`;
    /// web: a new asynchronous read), as [`Self::refresh_from_os`] does.
    pub fn set_pinned(&mut self, pinned: bool) {
        self.pinned = pinned;
    }

    /// Whether the labels are pinned ([`Self::set_pinned`]).
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// A label changed: new generation, and the reverse map is rebuilt on its next use.
    fn changed(&mut self) {
        self.generation += 1;
        self.by_label.take();
    }

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
    /// Ignored for keys that are not [`writing_system_keys`], for text that is not a printable
    /// label (control characters, private-use glyphs) and for keys the OS table marks as
    /// standing for several physical keys (macOS ISO `Backquote`). A new character that
    /// replaces a different learned one means the layout changed, so the other half learned
    /// on the old layout is dropped unless given too. Returns whether a label changed.
    pub fn learn(&mut self, key: KeyCode, unshifted: Option<&str>, shifted: Option<&str>) -> bool {
        if !is_writing_system_key(key) || (self.ambiguous.contains(&key) && self.os.contains_key(&key)) {
            return false;
        }
        let unshifted = unshifted.and_then(normalize_label);
        let shifted = shifted.and_then(normalize_label);
        let (has_unshifted, has_shifted) = (unshifted.is_some(), shifted.is_some());
        if !has_unshifted && !has_shifted {
            return false;
        }
        let entry = self.learned.entry(key).or_default();
        let before = entry.clone();
        if let Some(text) = unshifted {
            if !has_shifted && entry.unshifted.as_ref().is_some_and(|old| *old != text) {
                entry.shifted = None;
            }
            entry.unshifted = Some(text);
        }
        if let Some(text) = shifted {
            if !has_unshifted && entry.shifted.as_ref().is_some_and(|old| *old != text) {
                entry.unshifted = None;
            }
            entry.shifted = Some(text);
        }
        let changed = *entry != before;
        if changed {
            self.changed();
        }
        changed
    }

    /// Replace the OS table: layout `id` (platform-specific, e.g.
    /// `"com.apple.keylayout.French"`, a Windows `HKL`) and `(key, unshifted, shifted)`
    /// labels. A different layout (id or labels) also forgets the learned labels, which
    /// belonged to the previous layout. Returns whether anything changed. Custom platforms
    /// and tests can call this directly; on macOS, Windows and the web the window runner
    /// replaces such a table with the OS's on its next check unless the labels are pinned
    /// ([`Self::set_pinned`]).
    pub fn set_os_layout(
        &mut self,
        id: Option<String>,
        labels: impl IntoIterator<Item = (KeyCode, String, Option<String>)>,
    ) -> bool {
        self.os_check = None;
        self.set_os_table(id, labels, [])
    }

    /// [`Self::set_os_layout`] plus the keys that stand for several physical keys.
    fn set_os_table(
        &mut self,
        id: Option<String>,
        labels: impl IntoIterator<Item = (KeyCode, String, Option<String>)>,
        ambiguous: impl IntoIterator<Item = KeyCode>,
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
        let ambiguous: HashSet<KeyCode> = ambiguous.into_iter().collect();
        if id == self.os_id && table == self.os && ambiguous == self.ambiguous {
            return false;
        }
        self.os_id = id;
        self.os = table;
        self.ambiguous = ambiguous;
        self.learned.clear();
        self.changed();
        true
    }

    /// Forget the OS table (labels fall back to learned ones).
    pub fn clear_os_layout(&mut self) {
        self.os_check = None;
        if self.os_id.is_some() || !self.os.is_empty() || !self.ambiguous.is_empty() {
            self.os_id = None;
            self.os.clear();
            self.ambiguous.clear();
            self.changed();
        }
    }

    /// Forget every learned label.
    pub fn clear_learned(&mut self) {
        if !self.learned.is_empty() {
            self.learned.clear();
            self.changed();
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
    /// as the window runner does; on macOS it reads nothing off the main thread (Text Input
    /// Sources are main-thread only: macOS 14+ aborts the process otherwise). On the web it
    /// applies the result of the previous request and starts a new asynchronous one;
    /// elsewhere, and while the labels are pinned ([`Self::set_pinned`]), it does nothing.
    /// Returns whether the labels changed.
    pub fn refresh_from_os(&mut self) -> bool {
        if self.pinned {
            return false;
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let check = os::layout_check();
            if check.is_some() && self.has_os_layout() && check == self.os_check {
                return false;
            }
            match os::read_layout() {
                Some(layout) => self.apply_os_layout(layout),
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

    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows", target_arch = "wasm32")),
        allow(dead_code)
    )]
    fn apply_os_layout(&mut self, layout: OsLayout) -> bool {
        let changed = self.set_os_table(layout.id, layout.keys, layout.ambiguous);
        self.os_check = layout.check;
        changed
    }

    /// Apply the web's finished read, unless pinned (then it waits for the unpinning).
    #[cfg(target_arch = "wasm32")]
    fn apply_web_result(&mut self) -> bool {
        !self.pinned && web::take().is_some_and(|layout| self.apply_os_layout(layout))
    }
}

/// A layout table read from the OS.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_arch = "wasm32")),
    allow(dead_code)
)]
pub(crate) struct OsLayout {
    pub id: Option<String>,
    /// The platform's layout check at the time of reading (see `KeyboardLayout::os_check`).
    pub check: Option<String>,
    pub keys: Vec<(KeyCode, String, Option<String>)>,
    /// Keys that stand for several physical keys on this keyboard.
    pub ambiguous: Vec<KeyCode>,
}

/// macOS virtual keys whose physical key winit 0.30 leaves unidentified or conflates:
/// `(virtual key, key on ISO keyboards, key on ANSI and JIS keyboards)`. ISO keyboards send
/// `kVK_ISO_Section` (0x0A) for the key left of `1` and `kVK_ANSI_Grave` (0x32) for the extra
/// key right of left Shift, which winit both reports as `Backquote`; other keyboard types send
/// `kVK_ANSI_Grave` for the key left of `1` (and `kVK_ISO_Section` for the extra key, when the
/// OS types an ISO keyboard as ANSI). winit has no code for the JIS Ro key
/// (`kVK_JIS_Underscore`, 0x5E, left of right Shift).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_KEYS: [(u16, KeyCode, KeyCode); 3] = [
    (0x0A, KeyCode::Backquote, KeyCode::IntlBackslash),
    (0x32, KeyCode::IntlBackslash, KeyCode::Backquote),
    (0x5E, KeyCode::IntlRo, KeyCode::IntlRo),
];

/// The physical key of macOS virtual key `vk` on an ISO (`iso`) or ANSI/JIS keyboard, for the
/// virtual keys winit 0.30 gets wrong ([`MACOS_KEYS`]); `None` for the others.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_key_of_virtual_key(vk: u16, iso: bool) -> Option<KeyCode> {
    MACOS_KEYS
        .iter()
        .find(|k| k.0 == vk)
        .map(|k| if iso { k.1 } else { k.2 })
}

/// The macOS virtual key of physical `key` on an ISO (`iso`) or ANSI/JIS keyboard, for the
/// keys winit 0.30 gets wrong ([`MACOS_KEYS`]); `None` for the others.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_virtual_key_of_key(key: KeyCode, iso: bool) -> Option<u16> {
    MACOS_KEYS
        .iter()
        .find(|k| (if iso { k.1 } else { k.2 }) == key)
        .map(|k| k.0)
}

/// The window runner's physical key of a winit key event: winit's, corrected where winit 0.30
/// leaves a key unidentified or reports two keys as one (macOS ISO and JIS keys, see
/// [`MACOS_KEYS`]). Call on the event-loop thread while the event is dispatched.
pub(crate) fn physical_key(key: PhysicalKey) -> PhysicalKey {
    #[cfg(target_os = "macos")]
    {
        macos::physical_key(key)
    }
    #[cfg(not(target_os = "macos"))]
    {
        key
    }
}

/// The key a release of `code` releases on macOS. The ISO key right of left Shift is
/// `IntlBackslash`, but [`physical_key`] reports it as `Backquote` when the event's virtual
/// key is unknown, so a press and its release may disagree: a release of one of the two keys
/// that is not held releases the other one when that is held.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_released_key(code: KeyCode, pressed: impl Fn(KeyCode) -> bool) -> KeyCode {
    let other = match code {
        KeyCode::Backquote => KeyCode::IntlBackslash,
        KeyCode::IntlBackslash => KeyCode::Backquote,
        _ => return code,
    };
    if !pressed(code) && pressed(other) { other } else { code }
}

/// Test support (macOS; call on the main thread while AppKit dispatches a key event): the
/// window runner's physical key for winit's `key` (see `MACOS_KEYS`). Not a stable API.
#[doc(hidden)]
#[cfg(target_os = "macos")]
pub fn __macos_physical_key(key: PhysicalKey) -> PhysicalKey {
    physical_key(key)
}

/// Test support (macOS; call on the main thread): the OS table of the installed layout `id`
/// (e.g. `"com.apple.keylayout.French"`) without selecting it, as read for keyboard type
/// `keyboard_type` (`None`: the current keyboard's). Not a stable API.
#[doc(hidden)]
#[cfg(target_os = "macos")]
pub fn __installed_macos_layout(id: &str, keyboard_type: Option<u8>) -> Option<KeyboardLayout> {
    let layout = macos::read_installed(id, keyboard_type)?;
    let mut labels = KeyboardLayout::default();
    labels.apply_os_layout(layout);
    Some(labels)
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

/// `s` in the other case when it is one cased letter that round-trips (`"A"` ↔ `"a"`; not
/// `"µ"`, `"ß"`, `"ı"`), else unchanged. `upper`: to upper case.
fn letter_case(s: &str, upper: bool) -> String {
    fn one(s: String) -> Option<char> {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    }
    let convert = |c: char, upper: bool| -> String {
        if upper {
            c.to_uppercase().collect()
        } else {
            c.to_lowercase().collect()
        }
    };
    let Some(c) = one(s.to_owned()) else {
        return s.to_owned();
    };
    let in_other_case = if upper { c.is_lowercase() } else { c.is_uppercase() };
    if !in_other_case {
        return s.to_owned();
    }
    match one(convert(c, upper)) {
        Some(other) if one(convert(other, !upper)) == Some(c) => other.to_string(),
        _ => s.to_owned(),
    }
}

/// An unshifted label seen through Caps Lock (web and mobile report no modifier-free key):
/// unshifted letters are lower case, so an upper-case letter is lowered.
pub(crate) fn unshifted_case(s: &str) -> String {
    letter_case(s, false)
}

/// A shifted label seen through Caps Lock (Shift + Caps Lock types lower case on Windows,
/// Linux and the web): Shift types upper-case letters, so a lower-case letter is raised.
pub(crate) fn shifted_case(s: &str) -> String {
    letter_case(s, true)
}

/// Runner hook: keeps [`KeyboardLayout`]'s OS table current without per-frame OS calls.
#[derive(Debug, Default)]
pub(crate) struct LayoutWatch {
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    last_check: Option<web_time::Instant>,
    /// The labels were pinned at the last poll: unpinning checks the OS at once.
    was_pinned: bool,
}

impl LayoutWatch {
    /// Minimum time between two layout-id checks (macOS, Windows).
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    /// Called by the runner on the event-loop thread: every frame (`force = false`) and on
    /// start-up / focus gain (`force = true`). Does nothing while the labels are pinned, and
    /// forces a check on the first poll after they are unpinned.
    pub(crate) fn poll(&mut self, now: web_time::Instant, force: bool, layout: &mut KeyboardLayout) {
        let unpinned = std::mem::replace(&mut self.was_pinned, layout.is_pinned()) && !layout.is_pinned();
        if layout.is_pinned() {
            return;
        }
        let force = force || unpinned;
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
    fn a_new_character_drops_the_other_half_learned_on_the_old_layout() {
        let mut l = KeyboardLayout::default();
        // QWERTY: "a" / "A".
        assert!(l.learn(KeyCode::KeyA, Some("a"), Some("A")));
        // Undetected switch to AZERTY (Linux, Firefox, mobile), unshifted press only.
        assert!(l.learn(KeyCode::KeyA, Some("q"), None));
        let a = l.label(KeyCode::KeyA).unwrap();
        assert_eq!(
            (a.text.as_str(), a.shifted.as_deref()),
            ("q", None),
            "stale \"A\" dropped"
        );
        assert_eq!(a.cap(), "Q");
        assert!(l.learn(KeyCode::KeyA, None, Some("Q")));
        assert_eq!(l.label(KeyCode::KeyA).unwrap().shifted.as_deref(), Some("Q"));
        // The same character again keeps the other half.
        assert!(!l.learn(KeyCode::KeyA, Some("q"), None));
        assert_eq!(l.label(KeyCode::KeyA).unwrap().shifted.as_deref(), Some("Q"));
        // Web/mobile back on QWERTY, shifted press first: the old unshifted "q" is dropped.
        assert!(l.learn(KeyCode::KeyA, None, Some("A")));
        assert_eq!(l.label(KeyCode::KeyA), None, "unshifted unknown again");
        assert!(l.learn(KeyCode::KeyA, Some("a"), None));
        let a = l.label(KeyCode::KeyA).unwrap();
        assert_eq!(
            (a.text.as_str(), a.shifted.as_deref()),
            ("a", Some("A")),
            "first unshifted keeps shifted"
        );
    }

    #[test]
    fn ambiguous_keys_keep_their_os_label() {
        let mut l = KeyboardLayout::default();
        // macOS ISO French: Backquote is both the key left of 1 ("@") and right of Shift ("<").
        assert!(l.set_os_table(
            Some("fr".into()),
            [
                (KeyCode::Backquote, "@".into(), Some("#".into())),
                (KeyCode::KeyA, "q".into(), Some("Q".into())),
            ],
            [KeyCode::Backquote],
        ));
        let g = l.generation();
        assert!(!l.learn(KeyCode::Backquote, Some("<"), Some(">")));
        assert_eq!(l.generation(), g);
        let b = l.label(KeyCode::Backquote).unwrap();
        assert_eq!((b.text.as_str(), b.source), ("@", KeyLabelSource::Os));
        assert!(l.learn(KeyCode::KeyA, Some("a"), None), "other keys still learn");
        // Same table, other keyboard type (ANSI): no longer ambiguous, labels change.
        assert!(l.set_os_table(
            Some("fr".into()),
            [
                (KeyCode::Backquote, "<".into(), Some(">".into())),
                (KeyCode::KeyA, "q".into(), Some("Q".into())),
            ],
            [],
        ));
        assert!(l.learn(KeyCode::Backquote, Some("x"), None));
        // A table set through the public API (custom platforms) has no ambiguous keys.
        l.set_os_layout(Some("fr".into()), [(KeyCode::Backquote, "@".into(), None)]);
        assert!(l.learn(KeyCode::Backquote, Some("<"), None));
        // Clearing the table forgets the ambiguity with it.
        l.set_os_table(
            Some("fr".into()),
            [(KeyCode::Backquote, "@".into(), None)],
            [KeyCode::Backquote],
        );
        l.clear_os_layout();
        assert!(l.learn(KeyCode::Backquote, Some("<"), None), "no OS table: learned");
    }

    #[test]
    fn pinned_labels_ignore_the_os_until_unpinned() {
        let mut l = KeyboardLayout::default();
        assert!(!l.is_pinned());
        l.set_os_layout(Some("fake".into()), [(KeyCode::KeyA, "q".into(), Some("Q".into()))]);
        l.set_pinned(true);
        assert!(l.is_pinned());
        let g = l.generation();
        assert!(!l.refresh_from_os(), "pinned: the OS table is not read");
        let mut watch = LayoutWatch::default();
        let now = web_time::Instant::now();
        watch.poll(now, true, &mut l);
        assert_eq!(l.os_layout_id(), Some("fake"));
        assert_eq!(l.generation(), g);
        // Explicit calls still apply.
        assert!(l.learn(KeyCode::KeyZ, Some("w"), None));
        assert!(l.set_os_layout(Some("fake2".into()), [(KeyCode::KeyA, "a".into(), None)]));
        assert!(l.is_pinned(), "set_os_layout keeps the pin");
        // Unpinned: the first poll checks the OS at once, however recent the last check.
        l.set_pinned(false);
        watch.poll(now, false, &mut l);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(watch.last_check, Some(now), "forced check");
        assert!(!watch.was_pinned);
    }

    #[test]
    fn macos_virtual_keys_map_to_their_w3c_positions() {
        use KeyCode as K;
        // ISO: 0x0A left of 1, 0x32 right of left Shift. ANSI/JIS: 0x32 left of 1.
        assert_eq!(macos_key_of_virtual_key(0x0A, true), Some(K::Backquote));
        assert_eq!(macos_key_of_virtual_key(0x32, true), Some(K::IntlBackslash));
        assert_eq!(macos_key_of_virtual_key(0x0A, false), Some(K::IntlBackslash));
        assert_eq!(macos_key_of_virtual_key(0x32, false), Some(K::Backquote));
        assert_eq!(macos_key_of_virtual_key(0x5E, true), Some(K::IntlRo), "JIS Ro");
        assert_eq!(macos_key_of_virtual_key(0x5E, false), Some(K::IntlRo));
        assert_eq!(macos_key_of_virtual_key(0x00, true), None, "winit's KeyA: unchanged");
        assert_eq!(
            macos_key_of_virtual_key(0x5D, false),
            None,
            "winit's IntlYen: unchanged"
        );
        for iso in [true, false] {
            for (vk, ..) in MACOS_KEYS {
                let key = macos_key_of_virtual_key(vk, iso).unwrap();
                assert!(is_writing_system_key(key));
                assert_eq!(macos_virtual_key_of_key(key, iso), Some(vk), "{key:?} (ISO: {iso})");
            }
        }
        assert_eq!(macos_virtual_key_of_key(K::KeyA, true), None);
        // Off macOS (and for other keys) the runner keeps winit's key.
        #[cfg(not(target_os = "macos"))]
        {
            use winit::keyboard::NativeKeyCode;
            let ro = PhysicalKey::Unidentified(NativeKeyCode::MacOS(0x5E));
            assert_eq!(physical_key(ro), ro);
        }
        assert_eq!(physical_key(PhysicalKey::Code(K::KeyA)), PhysicalKey::Code(K::KeyA));
    }

    #[test]
    fn a_release_finds_the_iso_key_its_press_named() {
        use KeyCode as K;
        let held = |keys: &'static [KeyCode]| move |k: KeyCode| keys.contains(&k);
        // Pressed as IntlBackslash (virtual key known), released as Backquote (unknown).
        assert_eq!(
            macos_released_key(K::Backquote, held(&[K::IntlBackslash])),
            K::IntlBackslash
        );
        assert_eq!(
            macos_released_key(K::IntlBackslash, held(&[K::Backquote])),
            K::Backquote
        );
        assert_eq!(
            macos_released_key(K::Backquote, held(&[K::Backquote, K::IntlBackslash])),
            K::Backquote
        );
        assert_eq!(macos_released_key(K::Backquote, held(&[])), K::Backquote);
        assert_eq!(macos_released_key(K::KeyA, held(&[K::KeyB])), K::KeyA);
    }

    #[test]
    fn reverse_lookup_finds_the_first_key_and_follows_changes() {
        let mut l = KeyboardLayout::default();
        assert_eq!(l.key_for_label("a"), None);
        l.set_os_layout(
            Some("fr".into()),
            [
                (KeyCode::KeyA, "q".into(), Some("Q".into())),
                (KeyCode::KeyQ, "a".into(), Some("A".into())),
                (KeyCode::Semicolon, "m".into(), Some("M".into())),
                (KeyCode::Backquote, "<".into(), None),
                (KeyCode::IntlBackslash, "<".into(), None),
            ],
        );
        assert_eq!(l.key_for_label("m"), Some(KeyCode::Semicolon));
        assert_eq!(l.key_for_label("a"), Some(KeyCode::KeyQ));
        assert_eq!(
            l.key_for_label("<"),
            Some(KeyCode::Backquote),
            "first in keyboard order"
        );
        assert_eq!(l.key_for_label("M"), None, "unshifted labels only");
        assert!(l.by_label.get().is_some(), "cached");
        let upper = |t: &str| t.chars().all(char::is_alphabetic) && t != "q";
        assert_eq!(
            l.key_for_label_matching(upper),
            Some(KeyCode::KeyQ),
            "keyboard order: Q row first"
        );
        let mut seen = Vec::new();
        assert_eq!(
            l.key_for_label_matching(|t| {
                seen.push(t.to_owned());
                false
            }),
            None
        );
        assert_eq!(
            seen,
            ["<", "a", "q", "m"],
            "each distinct label once, in keyboard order"
        );
        // Learned labels win, and every change rebuilds the map.
        assert!(l.learn(KeyCode::Semicolon, Some("ñ"), None));
        assert!(l.by_label.get().is_none(), "dropped by the change");
        assert_eq!(l.key_for_label("m"), None);
        assert_eq!(l.key_for_label("ñ"), Some(KeyCode::Semicolon));
        for key in writing_system_keys() {
            assert_eq!(l.label_text(key), l.label(key).map(|x| x.text).as_deref(), "{key:?}");
        }
        assert_eq!(l.label_text(KeyCode::F1), None);
        l.clear_learned();
        assert_eq!(l.key_for_label("m"), Some(KeyCode::Semicolon));
        l.clear_os_layout();
        assert_eq!(l.key_for_label("m"), None);
        // A clone carries its own map.
        l.learn(KeyCode::KeyA, Some("x"), None);
        let copy = l.clone();
        l.learn(KeyCode::KeyA, Some("y"), None);
        assert_eq!(copy.key_for_label("x"), Some(KeyCode::KeyA));
        assert_eq!(l.key_for_label("x"), None);
    }

    #[test]
    fn reverse_lookup_is_shared_across_threads() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<KeyboardLayout>();
        let mut l = KeyboardLayout::default();
        for (key, us) in WRITING_KEYS.iter().map(|w| (w.0, w.2)) {
            l.learn(key, Some(us), None);
        }
        let l = &l;
        std::thread::scope(|s| {
            let threads: Vec<_> = (0..8)
                .map(|_| s.spawn(move || (l.key_for_label("m"), l.key_for_label("\\"))))
                .collect();
            for t in threads {
                // US labels: `\` is printed by Backslash, IntlBackslash and IntlRo; Backslash first.
                assert_eq!(t.join().unwrap(), (Some(KeyCode::KeyM), Some(KeyCode::Backslash)));
            }
        });
    }

    #[test]
    fn caps_lock_case_is_undone_for_letters_only() {
        assert_eq!(unshifted_case("A"), "a");
        assert_eq!(unshifted_case("É"), "é");
        assert_eq!(unshifted_case("Я"), "я");
        assert_eq!(unshifted_case("a"), "a");
        assert_eq!(unshifted_case("2"), "2");
        assert_eq!(unshifted_case("AB"), "AB", "not one letter");
        assert_eq!(shifted_case("a"), "A");
        assert_eq!(shifted_case("ö"), "Ö");
        assert_eq!(
            shifted_case("µ"),
            "µ",
            "micro sign (AZERTY Shift+*) has no own upper case"
        );
        assert_eq!(shifted_case("ß"), "ß", "upper case is two letters");
        assert_eq!(shifted_case("ı"), "ı", "dotless i does not round-trip");
        assert_eq!(shifted_case("?"), "?");
    }

    #[test]
    fn key_caps() {
        let label = |key, text: &str, shifted: Option<&str>| {
            KeyLabel::new(key, text, shifted.map(Into::into), KeyLabelSource::Learned)
        };
        assert_eq!(label(KeyCode::KeyA, "q", None).cap(), "Q");
        assert_eq!(label(KeyCode::KeyA, "q", Some("Q")).cap(), "Q");
        assert_eq!(label(KeyCode::Digit2, "é", Some("2")).cap(), "é", "AZERTY digit row");
        assert_eq!(label(KeyCode::Digit2, "é", None).cap(), "é", "not a letter row");
        assert_eq!(label(KeyCode::Semicolon, "ö", Some("Ö")).cap(), "Ö", "German ö");
        assert_eq!(label(KeyCode::Minus, "ß", Some("?")).cap(), "ß");
        assert_eq!(label(KeyCode::KeyZ, "я", None).cap(), "Я", "Cyrillic");
        assert_eq!(label(KeyCode::Comma, ",", None).cap(), ",");
        assert_eq!(
            KeyLabel::new(KeyCode::Space, "Space", None, KeyLabelSource::Named).cap(),
            "Space"
        );
    }
}
