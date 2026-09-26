//! macOS: the current keyboard layout from Text Input Sources (`TISCopyCurrentKeyboardLayoutInputSource`)
//! translated key by key with `UCKeyTranslate` (the same calls winit makes for
//! `key_without_modifiers`). Small FFI to Carbon / CoreFoundation, which winit already links.

use super::{OsLayout, normalize_label, writing_system_keys};
use std::ffi::{c_char, c_ulong, c_void};
use winit::platform::scancode::PhysicalKeyExtScancode;

type CFStringRef = *const c_void;
type CFDataRef = *const c_void;
type TISInputSourceRef = *mut c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    static kTISPropertyInputSourceID: CFStringRef;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> TISInputSourceRef;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> TISInputSourceRef;
    fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *mut c_void;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const c_void,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: c_ulong,
        actual_string_length: *mut c_ulong,
        unicode_string: *mut u16,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    fn CFStringGetCString(string: CFStringRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
}

const K_UC_KEY_ACTION_DISPLAY: u16 = 3;
const K_UC_KEY_TRANSLATE_NO_DEAD_KEYS: u32 = 1;
/// `shiftKey` (0x0200) in `UCKeyTranslate`'s `(EventModifiers >> 8) & 0xFF` form.
const SHIFT: u32 = 0x02;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// An owned (copied) input source, released on drop.
struct Source(TISInputSourceRef);

impl Source {
    fn current() -> Option<Self> {
        // SAFETY: plain HIToolbox call; the returned reference is owned (Copy rule).
        let s = unsafe { TISCopyCurrentKeyboardLayoutInputSource() };
        (!s.is_null()).then_some(Self(s))
    }

    fn ascii_capable() -> Option<Self> {
        // SAFETY: as above.
        let s = unsafe { TISCopyCurrentASCIICapableKeyboardLayoutInputSource() };
        (!s.is_null()).then_some(Self(s))
    }

    fn property(&self, key: CFStringRef) -> *mut c_void {
        // SAFETY: `self.0` is a live input source; the result follows the Get rule (not owned).
        unsafe { TISGetInputSourceProperty(self.0, key) }
    }

    fn id(&self) -> Option<String> {
        // SAFETY: reading an extern static CFString constant.
        let s = self.property(unsafe { kTISPropertyInputSourceID }) as CFStringRef;
        if s.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 256];
        // SAFETY: `s` is a CFString owned by the source; the buffer length is passed.
        let ok = unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, K_CF_STRING_ENCODING_UTF8) };
        if ok == 0 {
            return None;
        }
        // SAFETY: CFStringGetCString NUL-terminated the buffer on success.
        let id = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
        Some(id.to_string_lossy().into_owned())
    }

    /// The source's `UCKeyboardLayout` bytes (null for layouts without Unicode data).
    fn layout_data(&self) -> *const c_void {
        // SAFETY: reading an extern static CFString constant.
        let data = self.property(unsafe { kTISPropertyUnicodeKeyLayoutData }) as CFDataRef;
        if data.is_null() {
            return std::ptr::null();
        }
        // SAFETY: `data` is a CFData owned by the source, alive as long as the source.
        unsafe { CFDataGetBytePtr(data) as *const c_void }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // SAFETY: we own the reference returned by a TISCopy* function.
        unsafe { CFRelease(self.0 as *const c_void) }
    }
}

/// Input source id of the current keyboard layout (e.g. `com.apple.keylayout.French`).
pub(super) fn current_layout_id() -> Option<String> {
    Source::current()?.id()
}

/// Read the current layout: unshifted and shifted output of every writing-system key.
pub(super) fn read_layout() -> Option<OsLayout> {
    let current = Source::current()?;
    // Layouts without Unicode data (input methods, legacy layouts): use the ASCII-capable
    // layout they type through.
    let fallback;
    let mut data = current.layout_data();
    if data.is_null() {
        fallback = Source::ascii_capable()?;
        data = fallback.layout_data();
        if data.is_null() {
            return None;
        }
    }
    Some(read_keys(current.id(), data))
}

/// Translate every writing-system key of the `UCKeyboardLayout` at `layout`.
fn read_keys(id: Option<String>, layout: *const c_void) -> OsLayout {
    // SAFETY: plain Low Memory accessor (keyboard type used to pick ANSI/ISO/JIS mappings).
    let keyboard_type = u32::from(unsafe { LMGetKbdType() });
    let mut keys = Vec::new();
    for code in writing_system_keys() {
        let Some(vk) = code.to_scancode().and_then(|s| u16::try_from(s).ok()) else {
            continue;
        };
        let Some(unshifted) = translate(layout, vk, 0, keyboard_type) else {
            continue;
        };
        keys.push((code, unshifted, translate(layout, vk, SHIFT, keyboard_type)));
    }
    tracing::debug!(
        layout = id.as_deref().unwrap_or("?"),
        keys = keys.len(),
        "keyboard layout read"
    );
    OsLayout { id, keys }
}

/// What `vk` types with `modifiers` (dead keys: their base character).
fn translate(layout: *const c_void, vk: u16, modifiers: u32, keyboard_type: u32) -> Option<String> {
    let mut chars = [0u16; 8];
    let mut len: c_ulong = 0;
    let mut dead_key_state = 0u32;
    // SAFETY: `layout` points at the UCKeyboardLayout of a live input source; the output
    // buffer length is passed.
    let status = unsafe {
        UCKeyTranslate(
            layout,
            vk,
            K_UC_KEY_ACTION_DISPLAY,
            modifiers,
            keyboard_type,
            K_UC_KEY_TRANSLATE_NO_DEAD_KEYS,
            &mut dead_key_state,
            chars.len() as c_ulong,
            &mut len,
            chars.as_mut_ptr(),
        )
    };
    if status != 0 || len == 0 {
        return None;
    }
    let len = (len as usize).min(chars.len());
    normalize_label(&String::from_utf16_lossy(&chars[..len]))
}

#[cfg(test)]
mod tests {
    //! Reads installed layouts by id (without selecting them: the user's input source is
    //! never changed) to check the translation against known layouts.
    use super::*;
    use crate::input::KeyCode;

    type CFArrayRef = *const c_void;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        static kTISPropertyInputSourceID: CFStringRef;
        fn TISCreateInputSourceList(properties: *const c_void, include_all_installed: u8) -> CFArrayRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> CFStringRef;
        fn CFDictionaryCreate(
            alloc: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        fn CFArrayGetCount(array: CFArrayRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> *const c_void;
        fn CFRetain(cf: *const c_void) -> *const c_void;
    }

    /// The installed layout `id`, read like the current one (`None` if not installed).
    fn read_installed(id: &str) -> Option<OsLayout> {
        let cid = std::ffi::CString::new(id).ok()?;
        // SAFETY: CoreFoundation create/release pairs; the list and its elements are retained
        // while used.
        unsafe {
            let value = CFStringCreateWithCString(std::ptr::null(), cid.as_ptr(), K_CF_STRING_ENCODING_UTF8);
            let key = kTISPropertyInputSourceID;
            let filter = CFDictionaryCreate(
                std::ptr::null(),
                &key,
                &value,
                1,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            let list = TISCreateInputSourceList(filter, 1);
            CFRelease(filter);
            CFRelease(value);
            if list.is_null() {
                return None;
            }
            let source = (CFArrayGetCount(list) > 0)
                .then(|| Source(CFRetain(CFArrayGetValueAtIndex(list, 0)) as TISInputSourceRef));
            CFRelease(list);
            let source = source?;
            let data = source.layout_data();
            (!data.is_null()).then(|| read_keys(source.id(), data))
        }
    }

    fn label(layout: &OsLayout, key: KeyCode) -> (&str, Option<&str>) {
        let (_, text, shifted) = layout.keys.iter().find(|k| k.0 == key).expect("key read");
        (text, shifted.as_deref())
    }

    #[test]
    fn current_layout_is_readable() {
        let layout = read_layout().expect("current layout");
        assert!(layout.id.is_some());
        assert!(layout.keys.len() >= 40, "{} keys", layout.keys.len());
        assert_eq!(current_layout_id(), layout.id);
    }

    #[test]
    fn known_layouts_translate_as_printed_on_their_keyboards() {
        let Some(fr) = read_installed("com.apple.keylayout.French") else {
            eprintln!("skipping: French layout not installed");
            return;
        };
        assert_eq!(fr.id.as_deref(), Some("com.apple.keylayout.French"));
        assert_eq!(label(&fr, KeyCode::KeyA), ("q", Some("Q")));
        assert_eq!(label(&fr, KeyCode::KeyQ), ("a", Some("A")));
        assert_eq!(label(&fr, KeyCode::KeyW), ("z", Some("Z")));
        assert_eq!(label(&fr, KeyCode::Semicolon), ("m", Some("M")));
        assert_eq!(label(&fr, KeyCode::Digit2), ("é", Some("2")));
        assert_eq!(
            label(&fr, KeyCode::BracketLeft).0,
            "^",
            "dead circumflex: base character"
        );
        if let Some(de) = read_installed("com.apple.keylayout.German") {
            assert_eq!(label(&de, KeyCode::KeyY), ("z", Some("Z")));
            assert_eq!(label(&de, KeyCode::KeyZ), ("y", Some("Y")));
            assert_eq!(label(&de, KeyCode::Semicolon), ("ö", Some("Ö")));
            assert_eq!(label(&de, KeyCode::Minus).0, "ß");
        }
        if let Some(dv) = read_installed("com.apple.keylayout.Dvorak") {
            assert_eq!(label(&dv, KeyCode::KeyS).0, "o");
            assert_eq!(label(&dv, KeyCode::Period).0, "v");
        }
    }
}
