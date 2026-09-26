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
//! Macs) virtual key `0x0A` (`kVK_ISO_Section`) and the key right of left Shift (`<`) `0x32`
//! (`kVK_ANSI_Grave`); winit 0.30 reports both as `KeyCode::Backquote`. [`physical_key`] reads
//! the virtual key of the event AppKit is dispatching (`NSApp.currentEvent`) and reports the
//! key right of left Shift as `IntlBackslash` (its W3C position), the key left of `1` as
//! `Backquote`. Their labels are read from those virtual keys. ANSI and JIS keyboards send
//! `0x32` for the key left of `1`. The JIS Ro key (`0x5E`, `kVK_JIS_Underscore`), which winit
//! leaves unidentified, is `IntlRo`. See `super::MACOS_KEYS`.
//!
//! When winit delivers a key event late (it queues events that arrive while another one is
//! handled), `currentEvent` is another event and the ISO key right of left Shift stays
//! `Backquote`: the label of `Backquote` on ISO keyboards therefore comes from the OS table
//! only and is not learned from presses, which could flip it between the two keys.

use super::{OsLayout, macos_key_of_virtual_key, macos_virtual_key_of_key, normalize_label, writing_system_keys};
use crate::input::KeyCode;
use std::ffi::{c_char, c_int, c_ulong, c_void};
use winit::keyboard::{NativeKeyCode, PhysicalKey};
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

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    /// The shared `NSApplication` (nil until created; winit creates it).
    static mut NSApp: *mut c_void;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn sel_registerName(name: *const c_char) -> *const c_void;
    /// Called through a pointer cast to each message's signature.
    fn objc_msgSend();
}

/// `NSEventTypeKeyDown`, `NSEventTypeKeyUp`.
const NS_EVENT_TYPE_KEY_DOWN: usize = 10;
const NS_EVENT_TYPE_KEY_UP: usize = 11;

const K_UC_KEY_ACTION_DISPLAY: u16 = 3;
const K_UC_KEY_TRANSLATE_NO_DEAD_KEYS: u32 = 1;
/// `shiftKey` (0x0200) in `UCKeyTranslate`'s `(EventModifiers >> 8) & 0xFF` form.
const SHIFT: u32 = 0x02;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kKeyboardISO` (`'ISO '`), a `KBGetLayoutType` result.
const K_KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");
/// `kVK_ISO_Section`: on ISO keyboards, the key left of `1` (W3C `Backquote` position).
const VK_ISO_SECTION: u16 = 0x0A;
/// `kVK_ANSI_Grave`: on ISO keyboards, the key right of left Shift (W3C `IntlBackslash`).
const VK_ANSI_GRAVE: u16 = 0x32;

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

/// The virtual key code of the key event AppKit is dispatching (`NSApp.currentEvent`), if it
/// is one. `None` off the main thread, before `NSApplication` exists, and for other events.
fn current_key_event_code() -> Option<u16> {
    type SendId = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;
    type SendUInt = unsafe extern "C" fn(*mut c_void, *const c_void) -> usize;
    type SendU16 = unsafe extern "C" fn(*mut c_void, *const c_void) -> u16;
    if !on_main_thread() {
        return None;
    }
    // SAFETY: main thread (AppKit's requirement). `objc_msgSend` is called with the exact
    // signature of each message: `-[NSApplication currentEvent]` returns an object,
    // `-[NSEvent type]` an `NSUInteger`, `-[NSEvent keyCode]` an `unsigned short` (only sent to
    // key events: it raises for other event types).
    unsafe {
        let app = (&raw const NSApp).read();
        if app.is_null() {
            return None;
        }
        let send_id = std::mem::transmute::<unsafe extern "C" fn(), SendId>(objc_msgSend);
        let send_uint = std::mem::transmute::<unsafe extern "C" fn(), SendUInt>(objc_msgSend);
        let send_u16 = std::mem::transmute::<unsafe extern "C" fn(), SendU16>(objc_msgSend);
        let event = send_id(app, sel_registerName(c"currentEvent".as_ptr()));
        if event.is_null() {
            return None;
        }
        let kind = send_uint(event, sel_registerName(c"type".as_ptr()));
        if kind != NS_EVENT_TYPE_KEY_DOWN && kind != NS_EVENT_TYPE_KEY_UP {
            return None;
        }
        Some(send_u16(event, sel_registerName(c"keyCode".as_ptr())))
    }
}

/// winit's physical key of the key event AppKit is dispatching, corrected (module docs):
/// `Backquote` from the event's virtual key (ISO: `0x32` is `IntlBackslash`), unidentified
/// keys of `super::MACOS_KEYS` (JIS Ro) by their code. Unchanged off the main thread and
/// when the virtual key is unknown.
pub(super) fn physical_key(key: PhysicalKey) -> PhysicalKey {
    let vk = match key {
        PhysicalKey::Code(KeyCode::Backquote) => match current_key_event_code() {
            Some(vk @ (VK_ISO_SECTION | VK_ANSI_GRAVE)) => vk,
            _ => return key,
        },
        PhysicalKey::Unidentified(NativeKeyCode::MacOS(vk)) if on_main_thread() => vk,
        _ => return key,
    };
    macos_key_of_virtual_key(vk, is_iso(keyboard_type())).map_or(key, PhysicalKey::Code)
}

/// The virtual key whose output labels `code` on a keyboard of type ISO (`iso`) or other.
fn virtual_key(code: KeyCode, iso: bool) -> Option<u16> {
    macos_virtual_key_of_key(code, iso).or_else(|| code.to_scancode().and_then(|s| u16::try_from(s).ok()))
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
        let Some(vk) = virtual_key(code, iso) else { continue };
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
            let ro = PhysicalKey::Unidentified(NativeKeyCode::MacOS(0x5E));
            (
                layout_check().is_none(),
                read_layout().is_none(),
                read_installed("com.apple.keylayout.US", None).is_none(),
                current_key_event_code().is_none(),
                physical_key(ro) == ro,
            )
        })
        .join()
        .unwrap();
        assert_eq!(read, (true, true, true, true, true));
    }

    #[test]
    fn labels_are_read_from_the_keys_virtual_keys() {
        assert_eq!(virtual_key(KeyCode::Backquote, true), Some(VK_ISO_SECTION));
        assert_eq!(virtual_key(KeyCode::Backquote, false), Some(VK_ANSI_GRAVE));
        assert_eq!(virtual_key(KeyCode::IntlBackslash, true), Some(VK_ANSI_GRAVE));
        assert_eq!(virtual_key(KeyCode::IntlBackslash, false), Some(VK_ISO_SECTION));
        assert_eq!(virtual_key(KeyCode::IntlRo, true), Some(0x5E));
        assert_eq!(virtual_key(KeyCode::IntlYen, false), Some(0x5D));
        assert_eq!(virtual_key(KeyCode::KeyA, true), Some(0x00));
        // Every writing-system key has a virtual key on every keyboard type.
        for iso in [true, false] {
            for key in writing_system_keys() {
                assert!(virtual_key(key, iso).is_some(), "{key:?} (ISO: {iso})");
            }
        }
    }
}
