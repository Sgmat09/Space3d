//! Windows: the calling thread's keyboard layout (`GetKeyboardLayout`), each writing-system
//! key's scancode mapped to its virtual key (`MapVirtualKeyExW`) and translated with
//! `ToUnicodeEx` (unshifted and with Shift). Cached per `HKL` by [`super::KeyboardLayout`].

use super::{OsLayout, normalize_label, writing_system_keys};
#[cfg(test)]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayoutList;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, HKL, MAPVK_VK_TO_VSC_EX, MAPVK_VSC_TO_VK_EX, MapVirtualKeyExW, ToUnicodeEx, VK_SHIFT, VK_SPACE,
};
use winit::platform::scancode::PhysicalKeyExtScancode;

/// `ToUnicodeEx` flag: do not change the keyboard (dead key) state (Windows 10 1607+).
const NO_STATE_CHANGE: u32 = 1 << 2;

fn current_hkl() -> HKL {
    // SAFETY: plain user32 call; 0 = the calling thread.
    unsafe { GetKeyboardLayout(0) }
}

fn hkl_id(hkl: HKL) -> String {
    format!("{:08x}", hkl as usize)
}

/// The current `HKL` as hex (e.g. `04070407` for German): the table is re-read when it
/// changes.
pub(super) fn layout_check() -> Option<String> {
    let hkl = current_hkl();
    (!hkl.is_null()).then(|| hkl_id(hkl))
}

/// Read the current layout: unshifted and shifted output of every writing-system key.
pub(super) fn read_layout() -> Option<OsLayout> {
    let hkl = current_hkl();
    (!hkl.is_null()).then(|| read_hkl(hkl))
}

/// Read the layout `hkl`.
fn read_hkl(hkl: HKL) -> OsLayout {
    let mut keys = Vec::new();
    for code in writing_system_keys() {
        let Some(scancode) = code.to_scancode() else { continue };
        // SAFETY: plain user32 call.
        let vk = unsafe { MapVirtualKeyExW(scancode, MAPVK_VSC_TO_VK_EX, hkl) };
        if vk == 0 {
            continue;
        }
        let Some(unshifted) = translate(hkl, vk, scancode, false) else {
            continue;
        };
        keys.push((code, unshifted, translate(hkl, vk, scancode, true)));
    }
    let id = hkl_id(hkl);
    tracing::debug!(layout = id, keys = keys.len(), "keyboard layout read");
    OsLayout {
        check: Some(id.clone()),
        id: Some(id),
        keys,
        ambiguous: Vec::new(),
    }
}

/// What `vk` types (with Shift when `shift`); dead keys give their base character.
fn translate(hkl: HKL, vk: u32, scancode: u32, shift: bool) -> Option<String> {
    let mut state = [0u8; 256];
    if shift {
        state[usize::from(VK_SHIFT)] = 0x80;
    }
    let mut buf = [0u16; 8];
    // SAFETY: `state` has the 256 entries ToUnicodeEx reads; the buffer length is passed.
    let n = unsafe {
        ToUnicodeEx(
            vk,
            scancode,
            state.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as i32,
            NO_STATE_CHANGE,
            hkl,
        )
    };
    if n < 0 {
        // Dead key: the buffer holds its spacing character. Windows before 1607 ignores
        // NO_STATE_CHANGE and keeps the dead key pending: feed it a space to clear it.
        let base = normalize_label(&String::from_utf16_lossy(&buf[..1]));
        clear_dead_key(hkl);
        return base;
    }
    let n = usize::try_from(n).ok()?.min(buf.len());
    if n == 0 {
        return None;
    }
    normalize_label(&String::from_utf16_lossy(&buf[..n]))
}

fn clear_dead_key(hkl: HKL) {
    let state = [0u8; 256];
    let mut buf = [0u16; 8];
    // SAFETY: as in `translate`.
    unsafe {
        let space_scancode = MapVirtualKeyExW(u32::from(VK_SPACE), MAPVK_VK_TO_VSC_EX, hkl);
        ToUnicodeEx(
            u32::from(VK_SPACE),
            space_scancode,
            state.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as i32,
            NO_STATE_CHANGE,
            hkl,
        );
    }
}

#[cfg(test)]
mod tests {
    //! Runs on `windows-latest` in CI. Reads the loaded layouts (`GetKeyboardLayoutList`)
    //! without activating any: the user's input language is never changed.
    use super::*;
    use crate::input::KeyCode;

    fn label(layout: &OsLayout, key: KeyCode) -> (&str, Option<&str>) {
        let (_, text, shifted) = layout
            .keys
            .iter()
            .find(|k| k.0 == key)
            .unwrap_or_else(|| panic!("{key:?} read"));
        (text, shifted.as_deref())
    }

    #[test]
    fn current_layout_is_readable() {
        let layout = read_layout().expect("thread keyboard layout");
        assert_eq!(layout.id, layout_check());
        assert!(layout.keys.len() >= 40, "{} keys", layout.keys.len());
        for key in [KeyCode::KeyA, KeyCode::KeyZ, KeyCode::Digit1, KeyCode::Comma] {
            assert!(!label(&layout, key).0.is_empty(), "{key:?}");
        }
    }

    #[test]
    fn loaded_layouts_translate_as_printed_on_their_keyboards() {
        let mut list = [std::ptr::null_mut(); 64];
        // SAFETY: the buffer length is passed.
        let n = unsafe { GetKeyboardLayoutList(list.len() as i32, list.as_mut_ptr()) };
        let loaded = &list[..usize::try_from(n).unwrap_or(0).min(list.len())];
        assert!(!loaded.is_empty(), "at least one layout loaded");
        for &hkl in loaded {
            let layout = read_hkl(hkl);
            let id = layout.id.clone().unwrap();
            assert!(layout.keys.len() >= 40, "{id}: {} keys", layout.keys.len());
            // Only primary layouts (device id = language id) have a known key map.
            match id.as_str() {
                "04090409" => {
                    assert_eq!(label(&layout, KeyCode::KeyA), ("a", Some("A")), "{id}");
                    assert_eq!(label(&layout, KeyCode::Semicolon), (";", Some(":")), "{id}");
                    assert_eq!(label(&layout, KeyCode::Digit2), ("2", Some("@")), "{id}");
                }
                "040c040c" => {
                    assert_eq!(label(&layout, KeyCode::KeyA), ("q", Some("Q")), "{id}");
                    assert_eq!(label(&layout, KeyCode::KeyW), ("z", Some("Z")), "{id}");
                    assert_eq!(label(&layout, KeyCode::Digit2), ("é", Some("2")), "{id}");
                    assert_eq!(label(&layout, KeyCode::BracketLeft).0, "^", "{id}: dead circumflex");
                }
                "04070407" => {
                    assert_eq!(label(&layout, KeyCode::KeyY), ("z", Some("Z")), "{id}");
                    assert_eq!(label(&layout, KeyCode::Semicolon), ("ö", Some("Ö")), "{id}");
                    assert_eq!(label(&layout, KeyCode::Equal).0, "´", "{id}: dead acute");
                }
                _ => {}
            }
        }
    }
}
