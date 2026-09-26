//! macOS: the current keyboard layout from Text Input Sources (`TISCopyCurrentKeyboardLayoutInputSource`)
//! translated key by key with `UCKeyTranslate` (the same calls winit makes for
//! `key_without_modifiers`). Small FFI to Carbon / CoreFoundation, which winit already links.
//!
//! Text Input Sources are main-thread only (macOS 14+ aborts the process on other threads, and
//! winit runs `LMGetKbdType` on the main thread), so every entry point reads nothing off the
//! main thread. The window runner calls them on the event-loop thread, which is the main
//! thread on macOS.
//!
//! ISO keyboards: macOS gives the key left of `1` (`§` on US, `@` on French, `^` on German
//! Macs) virtual key `0x0A` and the key right of left Shift (`<`) `0x32`; winit reports both
//! as `KeyCode::Backquote` (and never `IntlBackslash`). The label of `Backquote` is then read
//! from `0x0A` — the W3C `Backquote` position — and not learned from presses, which would
//! flip it between the two keys. ANSI and JIS keyboards have no `0x0A` key: `0x32` is left of
//! `1`.

use super::{OsLayout, normalize_label, writing_system_keys};
use crate::input::KeyCode;
use std::ffi::{c_char, c_int, c_ulong, c_void};
use winit::platform::scancode::PhysicalKeyExtScancode;

type CFStringRef = *const c_void;
type CFDataRef = *const c_void;
type TISInputSourceRef = *mut c_void;
type CFArrayRef = *const c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    static kTISPropertyInputSourceID: CFStringRef;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> TISInputSourceRef;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> TISInputSourceRef;
    fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *mut c_void;
    fn TISCreateInputSourceList(properties: *const c_void, include_all_installed: u8) -> CFArrayRef;
    fn LMGetKbdType() -> u8;
    fn KBGetLayoutType(keyboard_type: i16) -> u32;
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

unsafe extern "C" {
    /// libSystem: non-zero on the process's main thread.
    fn pthread_main_np() -> c_int;
}

const K_UC_KEY_ACTION_DISPLAY: u16 = 3;
const K_UC_KEY_TRANSLATE_NO_DEAD_KEYS: u32 = 1;
/// `shiftKey` (0x0200) in `UCKeyTranslate`'s `(EventModifiers >> 8) & 0xFF` form.
const SHIFT: u32 = 0x02;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kKeyboardISO` (`'ISO '`), a `KBGetLayoutType` result.
const K_KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");
/// `kVK_ISO_Section`: on ISO keyboards, the key left of `1` (W3C `Backquote` position).
const VK_ISO_SECTION: u16 = 0x0A;

/// Whether the caller may use Text Input Sources.
fn on_main_thread() -> bool {
    // SAFETY: plain libSystem query.
    unsafe { pthread_main_np() != 0 }
}

/// The current keyboard type (`LMGetKbdType`; ANSI/ISO/JIS model of the last keyboard used).
fn keyboard_type() -> u8 {
    // SAFETY: plain Low Memory accessor; only called on the main thread (see module docs).
    unsafe { LMGetKbdType() }
}

fn is_iso(keyboard_type: u8) -> bool {
    // SAFETY: plain HIToolbox lookup.
    unsafe { KBGetLayoutType(i16::from(keyboard_type)) == K_KEYBOARD_ISO }
}

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

/// Input source id of the current keyboard layout (e.g. `com.apple.keylayout.French`) and the
/// keyboard type: the table is re-read when either changes. `None` off the main thread.
pub(super) fn layout_check() -> Option<String> {
    if !on_main_thread() {
        return None;
    }
    let id = Source::current()?.id()?;
    Some(format!("{id}#{}", keyboard_type()))
}

/// Read the current layout: unshifted and shifted output of every writing-system key. `None`
/// off the main thread.
pub(super) fn read_layout() -> Option<OsLayout> {
    if !on_main_thread() {
        tracing::debug!("keyboard layout not read: Text Input Sources are main-thread only");
        return None;
    }
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
    Some(read_keys(current.id(), data, keyboard_type()))
}

/// The installed layout `id`, read like the current one without selecting it (`None` if not
/// installed or off the main thread), for keyboard type `keyboard_type` (default: current).
pub(super) fn read_installed(id: &str, keyboard_type: Option<u8>) -> Option<OsLayout> {
    if !on_main_thread() {
        return None;
    }
    let cid = std::ffi::CString::new(id).ok()?;
    // SAFETY: CoreFoundation create/release pairs; the list and its first element are
    // retained while used.
    let source = unsafe {
        let value = CFStringCreateWithCString(std::ptr::null(), cid.as_ptr(), K_CF_STRING_ENCODING_UTF8);
        if value.is_null() {
            return None;
        }
        let key = kTISPropertyInputSourceID;
        let filter = CFDictionaryCreate(
            std::ptr::null(),
            &key,
            &value,
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        );
        CFRelease(value);
        if filter.is_null() {
            return None;
        }
        let list = TISCreateInputSourceList(filter, 1);
        CFRelease(filter);
        if list.is_null() {
            return None;
        }
        let source =
            (CFArrayGetCount(list) > 0).then(|| Source(CFRetain(CFArrayGetValueAtIndex(list, 0)) as TISInputSourceRef));
        CFRelease(list);
        source?
    };
    let data = source.layout_data();
    (!data.is_null()).then(|| read_keys(source.id(), data, keyboard_type.unwrap_or_else(self::keyboard_type)))
}

/// Translate every writing-system key of the `UCKeyboardLayout` at `layout` for keyboard type
/// `keyboard_type` (which picks the layout's ANSI/ISO/JIS mappings).
fn read_keys(id: Option<String>, layout: *const c_void, keyboard_type: u8) -> OsLayout {
    let iso = is_iso(keyboard_type);
    let kt = u32::from(keyboard_type);
    let mut keys = Vec::new();
    for code in writing_system_keys() {
        let vk = match code {
            KeyCode::Backquote if iso => Some(VK_ISO_SECTION),
            _ => code.to_scancode().and_then(|s| u16::try_from(s).ok()),
        };
        let Some(vk) = vk else { continue };
        let Some(unshifted) = translate(layout, vk, 0, kt) else {
            continue;
        };
        keys.push((code, unshifted, translate(layout, vk, SHIFT, kt)));
    }
    tracing::debug!(
        layout = id.as_deref().unwrap_or("?"),
        keyboard_type,
        iso,
        keys = keys.len(),
        "keyboard layout read"
    );
    let check = id.as_ref().map(|id| format!("{id}#{keyboard_type}"));
    OsLayout {
        id,
        check,
        keys,
        ambiguous: if iso { vec![KeyCode::Backquote] } else { Vec::new() },
    }
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
    //! Text Input Sources are main-thread only and libtest runs tests on worker threads: the
    //! OS tables are tested on the main thread by `tests/os_layout.rs` (custom harness).
    use super::*;

    #[test]
    fn nothing_is_read_off_the_main_thread() {
        let read = std::thread::spawn(|| {
            (
                layout_check().is_none(),
                read_layout().is_none(),
                read_installed("com.apple.keylayout.US", None).is_none(),
            )
        })
        .join()
        .unwrap();
        assert_eq!(read, (true, true, true));
    }
}
