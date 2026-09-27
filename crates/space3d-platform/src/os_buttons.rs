//! Held mouse buttons checked against the OS once per frame (window runner).
//!
//! A button goes down with a press event and comes up with a release event or a focus loss
//! ([`Input::release_all`]). When the window gets the press but never the release and keeps the
//! focus, nothing would release it, and every move would be a drag (text selecting itself, a
//! window following the pointer). On macOS the release goes elsewhere when the system takes the
//! pointer between the press and the release (Mission Control, App Exposé, a Spaces swipe, a hot
//! corner, the screenshot overlay) while the window stays key; winit reports `mouseMoved:` and
//! `mouseDragged:` alike as `CursorMoved`, so nothing downstream can tell.
//!
//! So before each frame the runner asks the OS which buttons it holds — macOS
//! `+[NSEvent pressedMouseButtons]`, Windows `GetAsyncKeyState` (physical buttons, swapped back
//! for left-handed mice: `SM_SWAPBUTTON`) — and [`ButtonReconciler`] releases a button `Input`
//! holds when the OS showed it held during this press and has shown it up since, on at least two
//! consecutive checks spanning [`GRACE`]. The release is a cancel
//! ([`MouseButtonCause::OsReconciled`]): the UI registers neither a click nor a drop. It never
//! presses anything.
//!
//! * A press the OS never showed held (a click shorter than a frame, a synthetic or injected
//!   press, a button the OS does not report) is left to its own release event.
//! * The OS state can be ahead of the event queue (a release event still on its way): requiring
//!   two checks, with the events of a frame delivered between them, and [`GRACE`] keeps a real
//!   release a real one (a click).
//! * Linux, the web, Android and iOS have no query here: a lost release there waits for the next
//!   click or a focus loss.
//! * Cheap: no OS call while no button is held, one per frame otherwise.

use crate::input::{Input, MouseButton, MouseButtonCause};
use std::time::Duration;
use web_time::Instant;

/// The buttons checked; bit `n` of [`OsButtons`] is `BUTTONS[n]` (macOS numbers them so, and
/// winit maps macOS buttons 3 and 4 to `Back` and `Forward`).
const BUTTONS: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Right,
    MouseButton::Middle,
    MouseButton::Back,
    MouseButton::Forward,
];

/// How long the OS must show a held button up (over at least two checks) before it is released.
pub(crate) const GRACE: Duration = Duration::from_millis(50);

/// The buttons the OS holds: bit `n` held = `BUTTONS[n]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OsButtons(pub(crate) u8);

impl OsButtons {
    fn held(self, i: usize) -> bool {
        self.0 & (1 << i) != 0
    }

    /// From `+[NSEvent pressedMouseButtons]` (bit `n`: button `n`).
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    fn from_macos(bits: usize) -> Self {
        Self((bits & 0b1_1111) as u8)
    }

    /// From Windows' physical button state (`held(virtual key)`); `swapped`: left-handed mouse
    /// (`SM_SWAPBUTTON`), where the primary button (winit's `Left`) is the physical right one.
    #[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
    fn from_windows(swapped: bool, held: impl Fn(u16) -> bool) -> Self {
        let (left, right) = if swapped {
            (VK_RBUTTON, VK_LBUTTON)
        } else {
            (VK_LBUTTON, VK_RBUTTON)
        };
        let keys = [left, right, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2];
        Self(
            keys.iter()
                .enumerate()
                .filter(|&(_, &vk)| held(vk))
                .fold(0, |bits, (i, _)| bits | 1 << i),
        )
    }
}

// Windows virtual keys of the mouse buttons (`winuser.h`).
const VK_LBUTTON: u16 = 0x01;
const VK_RBUTTON: u16 = 0x02;
const VK_MBUTTON: u16 = 0x04;
const VK_XBUTTON1: u16 = 0x05;
const VK_XBUTTON2: u16 = 0x06;

#[cfg(target_os = "windows")]
const _: () = {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse as K;
    assert!(K::VK_LBUTTON == VK_LBUTTON && K::VK_RBUTTON == VK_RBUTTON && K::VK_MBUTTON == VK_MBUTTON);
    assert!(K::VK_XBUTTON1 == VK_XBUTTON1 && K::VK_XBUTTON2 == VK_XBUTTON2);
};

/// Per watched button: what the OS showed of the current press.
#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    /// The OS showed the button held during this press.
    seen_held: bool,
    /// The first of the consecutive checks that showed it up since.
    up_since: Option<Instant>,
    /// Consecutive checks that showed it up.
    up_checks: u32,
}

/// Releases held buttons the OS has let go of (see the module docs).
#[derive(Debug, Default)]
pub(crate) struct ButtonReconciler {
    slots: [Slot; BUTTONS.len()],
}

impl ButtonReconciler {
    /// One check, before the frame's update: `os` reads the OS's buttons (`None`: unknown) and
    /// is only called while `input` holds a watched button. Returns how many were released.
    pub(crate) fn reconcile(
        &mut self,
        input: &mut Input,
        now: Instant,
        os: impl FnOnce() -> Option<OsButtons>,
    ) -> usize {
        let mut os = Some(os);
        let mut state = None;
        let mut released = 0;
        for (i, &button) in BUTTONS.iter().enumerate() {
            let slot = &mut self.slots[i];
            let held = input.mouse_pressed(button);
            if !held || input.mouse_just_pressed(button) {
                // Up, or a new press since the last check: nothing seen of it yet.
                *slot = Slot::default();
                if !held {
                    continue;
                }
            }
            let state = *state.get_or_insert_with(|| os.take().and_then(|query| query()));
            let Some(state) = state else {
                // Unknown: no evidence either way.
                slot.up_since = None;
                slot.up_checks = 0;
                continue;
            };
            if state.held(i) {
                slot.seen_held = true;
                slot.up_since = None;
                slot.up_checks = 0;
            } else if slot.seen_held {
                let since = *slot.up_since.get_or_insert(now);
                slot.up_checks += 1;
                if slot.up_checks >= 2 && now.duration_since(since) >= GRACE {
                    *slot = Slot::default();
                    tracing::info!(
                        target: "space3d_platform::input",
                        "{button:?} held with no release event, the OS shows it up: released (cancelled)"
                    );
                    input.release_mouse_button(button, MouseButtonCause::OsReconciled);
                    released += 1;
                }
            }
        }
        released
    }
}

/// The buttons the OS holds right now.
#[cfg(target_os = "macos")]
pub(crate) fn query() -> Option<OsButtons> {
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *const c_void;
        fn sel_registerName(name: *const c_char) -> *const c_void;
        /// Called through a pointer cast to the message's signature.
        fn objc_msgSend();
    }
    type SendUInt = unsafe extern "C" fn(*const c_void, *const c_void) -> usize;

    // `NSEvent` and the selector, looked up once (addresses: plain integers are `Sync`).
    static TARGET: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let &(class, selector) = TARGET
        .get_or_init(|| {
            // SAFETY: plain Objective-C runtime lookups with NUL-terminated names.
            let (class, selector) = unsafe {
                (
                    objc_getClass(c"NSEvent".as_ptr()),
                    sel_registerName(c"pressedMouseButtons".as_ptr()),
                )
            };
            (!class.is_null() && !selector.is_null()).then_some((class as usize, selector as usize))
        })
        .as_ref()?;
    // SAFETY: `+[NSEvent pressedMouseButtons]` is a class property returning an `NSUInteger`
    // (the window server's state when called, not an event's); `objc_msgSend` is called with
    // exactly that signature.
    let bits = unsafe {
        let send = std::mem::transmute::<unsafe extern "C" fn(), SendUInt>(objc_msgSend);
        send(class as *const c_void, selector as *const c_void)
    };
    Some(OsButtons::from_macos(bits))
}

/// The buttons the OS holds right now.
#[cfg(target_os = "windows")]
pub(crate) fn query() -> Option<OsButtons> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
    // `GetAsyncKeyState` reads the physical buttons; window messages (and winit's buttons) are
    // the logical ones, swapped for left-handed use.
    // SAFETY: plain user32 queries.
    let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
    // The most significant bit: held now.
    // SAFETY: as above.
    Some(OsButtons::from_windows(swapped, |vk| unsafe {
        GetAsyncKeyState(i32::from(vk)) < 0
    }))
}

/// Not known here (Linux, the web, Android, iOS).
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn query() -> Option<OsButtons> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec2;
    use std::cell::Cell;

    const LEFT: OsButtons = OsButtons(0b1);
    const NONE: OsButtons = OsButtons(0);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A frame: the check, then the app's frame end (edges cleared).
    fn frame(r: &mut ButtonReconciler, input: &mut Input, now: Instant, os: Option<OsButtons>) -> usize {
        let n = r.reconcile(input, now, || os);
        input.end_frame();
        n
    }

    #[test]
    fn a_release_only_the_os_saw_releases_after_the_grace() {
        let t0 = Instant::now();
        let mut r = ButtonReconciler::default();
        let mut input = Input::default();
        input.inject_mouse_move(Vec2::new(5.0, 5.0));
        input.inject_mouse_button_down(MouseButton::Left);
        assert_eq!(frame(&mut r, &mut input, t0, Some(LEFT)), 0);
        assert_eq!(frame(&mut r, &mut input, t0 + ms(16), Some(LEFT)), 0);
        // The release went elsewhere: up for the OS, no event.
        assert_eq!(frame(&mut r, &mut input, t0 + ms(33), Some(NONE)), 0, "one check");
        assert_eq!(
            frame(&mut r, &mut input, t0 + ms(50), Some(NONE)),
            0,
            "within the grace"
        );
        assert!(input.mouse_pressed(MouseButton::Left));
        let released = r.reconcile(&mut input, t0 + ms(83), || Some(NONE));
        assert_eq!(released, 1);
        assert!(!input.mouse_pressed(MouseButton::Left) && input.mouse_just_released(MouseButton::Left));
        let e = input.mouse_button_events()[0];
        assert_eq!((e.button, e.pressed), (MouseButton::Left, false));
        assert_eq!(e.cause, MouseButtonCause::OsReconciled);
        assert!(e.cancelled());
        input.end_frame();
        assert_eq!(frame(&mut r, &mut input, t0 + ms(100), Some(NONE)), 0, "released once");
    }

    #[test]
    fn a_long_frame_still_needs_two_checks() {
        let t0 = Instant::now();
        let mut r = ButtonReconciler::default();
        let mut input = Input::default();
        input.inject_mouse_button_down(MouseButton::Right);
        frame(&mut r, &mut input, t0, Some(OsButtons(0b10)));
        // A long frame: the OS shows the button up before its release event was delivered.
        assert_eq!(frame(&mut r, &mut input, t0 + ms(500), Some(NONE)), 0);
        // The event arrives before the next check: a real release.
        input.inject_mouse_button_up(MouseButton::Right);
        assert_eq!(frame(&mut r, &mut input, t0 + ms(516), Some(NONE)), 0);
        assert!(!input.mouse_pressed(MouseButton::Right));
        // Held again after being up: the up streak starts over.
        input.inject_mouse_button_down(MouseButton::Right);
        frame(&mut r, &mut input, t0 + ms(600), Some(OsButtons(0b10)));
        frame(&mut r, &mut input, t0 + ms(700), Some(NONE));
        frame(&mut r, &mut input, t0 + ms(716), Some(OsButtons(0b10)));
        assert_eq!(frame(&mut r, &mut input, t0 + ms(800), Some(NONE)), 0);
        assert_eq!(frame(&mut r, &mut input, t0 + ms(900), Some(NONE)), 1);
    }

    #[test]
    fn a_press_the_os_never_showed_held_is_left_alone() {
        let t0 = Instant::now();
        let mut r = ButtonReconciler::default();
        let mut input = Input::default();
        // Injected (or synthetic, or shorter than a frame): the OS never holds it.
        input.inject_mouse_button_down(MouseButton::Left);
        for k in 0..20 {
            assert_eq!(frame(&mut r, &mut input, t0 + ms(100 * k), Some(NONE)), 0);
        }
        assert!(input.mouse_pressed(MouseButton::Left));
        // Unknown OS state: nothing either way.
        let mut r = ButtonReconciler::default();
        frame(&mut r, &mut input, t0, Some(LEFT));
        for k in 1..20 {
            assert_eq!(frame(&mut r, &mut input, t0 + ms(100 * k), None), 0);
        }
        assert!(input.mouse_pressed(MouseButton::Left));
        // A new press forgets what the OS showed of the previous one.
        let mut r = ButtonReconciler::default();
        frame(&mut r, &mut input, t0, Some(LEFT));
        input.inject_mouse_button_up(MouseButton::Left);
        input.inject_mouse_button_down(MouseButton::Left);
        for k in 1..20 {
            assert_eq!(frame(&mut r, &mut input, t0 + ms(100 * k), Some(NONE)), 0);
        }
    }

    #[test]
    fn the_os_is_asked_only_while_a_button_is_held_and_nothing_is_pressed() {
        let t0 = Instant::now();
        let mut r = ButtonReconciler::default();
        let mut input = Input::default();
        let asked = Cell::new(0);
        let ask = |os| {
            asked.set(asked.get() + 1);
            Some(os)
        };
        r.reconcile(&mut input, t0, || ask(OsButtons(0b1_1111)));
        assert_eq!(asked.get(), 0, "nothing held: no OS call");
        assert!(input.mouse_button_events().is_empty(), "never presses");
        input.inject_mouse_button_down(MouseButton::Other(9));
        r.reconcile(&mut input, t0, || ask(NONE));
        assert_eq!(asked.get(), 0, "unwatched button");
        input.inject_mouse_button_down(MouseButton::Middle);
        input.inject_mouse_button_down(MouseButton::Forward);
        r.reconcile(&mut input, t0, || ask(OsButtons(0b1_0100)));
        assert_eq!(asked.get(), 1, "once per check");
    }

    /// The real query answers (reads the window server's state; synthesizes nothing).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_query_answers() {
        assert!(query().is_some());
    }

    #[test]
    fn os_bit_layouts() {
        assert_eq!(OsButtons::from_macos(0b110_0101), OsButtons(0b0_0101));
        let held = |keys: &'static [u16]| move |vk| keys.contains(&vk);
        assert_eq!(
            OsButtons::from_windows(false, held(&[VK_LBUTTON, VK_XBUTTON2])),
            OsButtons(0b1_0001)
        );
        // Left-handed: the physical right button is the primary one.
        assert_eq!(OsButtons::from_windows(true, held(&[VK_RBUTTON])), OsButtons(0b01));
        assert_eq!(
            OsButtons::from_windows(true, held(&[VK_LBUTTON, VK_MBUTTON])),
            OsButtons(0b110)
        );
        assert_eq!(
            OsButtons::from_windows(false, held(&[VK_XBUTTON1])),
            OsButtons(0b0_1000)
        );
    }
}
