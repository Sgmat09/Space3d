//! Windows: the calling thread's keyboard layout (`GetKeyboardLayout`), each writing-system
//! key's scancode mapped to its virtual key (`MapVirtualKeyExW`) and translated with
//! `ToUnicodeEx` (unshifted and with Shift). Cached per `HKL` by [`super::KeyboardLayout`].

use super::{OsLayout, normalize_label, writing_system_keys};
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

/// The current `HKL` as hex (e.g. `04070407` for German).
pub(super) fn current_layout_id() -> Option<String> {
    let hkl = current_hkl();
    (!hkl.is_null()).then(|| format!("{:08x}", hkl as usize))
}

/// Read the current layout: unshifted and shifted output of every writing-system key.
pub(super) fn read_layout() -> Option<OsLayout> {
    let hkl = current_hkl();
    if hkl.is_null() {
        return None;
    }
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
    tracing::debug!(
        layout = format!("{:08x}", hkl as usize),
        keys = keys.len(),
        "keyboard layout read"
    );
    Some(OsLayout {
        id: Some(format!("{:08x}", hkl as usize)),
        keys,
    })
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
