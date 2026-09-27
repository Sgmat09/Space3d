//! The OS keyboard-layout tables (macOS, Windows) through the public API, read on the main
//! thread as the window runner does. A custom harness (`harness = false` in Cargo.toml):
//! libtest runs tests on worker threads, and macOS Text Input Sources are main-thread only
//! (macOS 14+ aborts the process otherwise). Skips on macOS without a GUI login session and
//! on platforms without an OS table.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use space3d_platform::*;

/// `(name, check)`; a check panics on failure.
type Check = (&'static str, fn());

fn checks() -> Vec<Check> {
    let checks: &[Check] = &[
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        ("current_layout_labels_writing_keys", current_layout_labels_writing_keys),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        ("pinned_labels_survive_os_refreshes", pinned_labels_survive_os_refreshes),
        #[cfg(target_os = "macos")]
        (
            "installed_layouts_translate_as_printed",
            macos::installed_layouts_translate_as_printed,
        ),
        #[cfg(target_os = "macos")]
        (
            "iso_backquote_is_the_key_left_of_1",
            macos::iso_backquote_is_the_key_left_of_1,
        ),
        #[cfg(target_os = "macos")]
        (
            "iso_and_jis_keys_have_os_labels",
            macos::iso_and_jis_keys_have_os_labels,
        ),
        #[cfg(target_os = "macos")]
        (
            "key_events_are_mapped_by_virtual_key",
            macos::key_events_are_mapped_by_virtual_key,
        ),
        #[cfg(target_os = "macos")]
        (
            "nothing_is_read_off_the_main_thread",
            macos::nothing_is_read_off_the_main_thread,
        ),
    ];
    checks.to_vec()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let checks = checks();
    if args.iter().any(|a| a == "--list") {
        // libtest's listing format (cargo-nextest); nothing is `#[ignore]`d.
        if !args.iter().any(|a| a == "--ignored") {
            for (name, _) in &checks {
                println!("{name}: test");
            }
        }
        return;
    }
    if checks.is_empty() {
        println!("skipping: no OS keyboard-layout table on this platform");
        return;
    }
    #[cfg(target_os = "macos")]
    if !macos::gui_session() {
        println!("skipping: no GUI login session (Text Input Sources unavailable)");
        return;
    }
    println!("running {} tests", checks.len());
    for (name, check) in &checks {
        check();
        println!("test {name} ... ok");
    }
}

/// The public API reads the current layout's table (runner path: `refresh_keyboard_layout`).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn current_layout_labels_writing_keys() {
    let mut input = Input::default();
    assert!(input.refresh_keyboard_layout(), "layout table read from the OS");
    let id = input.keyboard_layout().os_layout_id().expect("layout id").to_owned();
    let label = input.key_label_info(KeyCode::KeyA).expect("KeyA labelled");
    assert_eq!(label.source, KeyLabelSource::Os);
    assert!(!label.text.is_empty());
    assert!(!input.key_cap(KeyCode::KeyA).is_empty());
    println!(
        "layout {id}: KeyA = {:?} (shifted {:?}), KeyQ = {:?}, KeyZ = {:?}, Digit2 = {:?}, Backquote = {:?}",
        label.text,
        label.shifted,
        input.key_label(KeyCode::KeyQ),
        input.key_label(KeyCode::KeyZ),
        input.key_label(KeyCode::Digit2),
        input.key_label(KeyCode::Backquote),
    );
    let count = writing_system_keys().filter(|&k| input.key_label(k).is_some()).count();
    assert!(count >= 40, "most writing-system keys labelled ({count})");
    let generation = input.keyboard_layout().generation();
    assert!(!input.refresh_keyboard_layout(), "same layout: table not re-read");
    assert_eq!(input.keyboard_layout().generation(), generation);
    // The reverse lookup agrees with the labels.
    let layout = input.keyboard_layout();
    for key in writing_system_keys() {
        if let Some(text) = layout.label_text(key) {
            let first = layout.key_for_label(text).expect("labelled key found");
            let position = |k| writing_system_keys().position(|w| w == k);
            assert!(
                position(first) <= position(key),
                "{key:?} {text:?}: {first:?} comes first"
            );
            assert_eq!(layout.label_text(first), Some(text));
        }
    }
    println!(
        "  IntlBackslash = {:?}, IntlRo = {:?}, IntlYen = {:?}; key typing \"`\": {:?}",
        input.key_label(KeyCode::IntlBackslash),
        input.key_label(KeyCode::IntlRo),
        input.key_label(KeyCode::IntlYen),
        layout.key_for_label("`"),
    );
}

/// A fake table survives the OS refreshes while pinned, and the OS table returns once
/// unpinned (the runner's refresh path, on the main thread).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn pinned_labels_survive_os_refreshes() {
    let mut input = Input::default();
    assert!(input.refresh_keyboard_layout());
    let os_id = input.keyboard_layout().os_layout_id().map(str::to_owned);
    // What the OS table itself says ("ф" is KeyA on a Russian layout, nothing on Latin ones).
    let os_ef = input.keyboard_layout().key_for_label("ф");
    let fake = [(KeyCode::KeyA, "ф".to_owned(), Some("Ф".to_owned()))];
    let layout = input.keyboard_layout_mut();
    // Unpinned, the OS replaces a table set by the app.
    assert!(layout.set_os_layout(Some("fake".into()), fake.clone()));
    assert!(input.refresh_keyboard_layout(), "OS table back");
    assert_eq!(input.keyboard_layout().os_layout_id().map(str::to_owned), os_id);
    // Pinned, it does not.
    let layout = input.keyboard_layout_mut();
    layout.set_os_layout(Some("fake".into()), fake);
    layout.set_pinned(true);
    assert!(!input.refresh_keyboard_layout());
    assert_eq!(input.keyboard_layout().os_layout_id(), Some("fake"));
    assert_eq!(input.key_cap(KeyCode::KeyA), "Ф");
    assert_eq!(input.keyboard_layout().key_for_label("ф"), Some(KeyCode::KeyA));
    input.keyboard_layout_mut().set_pinned(false);
    assert!(input.refresh_keyboard_layout(), "unpinned: OS table back");
    assert_eq!(input.keyboard_layout().os_layout_id().map(str::to_owned), os_id);
    assert_eq!(input.keyboard_layout().key_for_label("ф"), os_ef);
}

#[cfg(target_os = "macos")]
mod macos {
    use space3d_platform::layout::{__installed_macos_layout, __macos_physical_key};
    use space3d_platform::*;
    use std::ffi::{c_char, c_void};
    use winit::keyboard::{NativeKeyCode, PhysicalKey};

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn KBGetLayoutType(keyboard_type: i16) -> u32;
        fn LMGetKbdType() -> u8;
    }

    /// Whether this process runs in a GUI login session (else there are no input sources).
    pub fn gui_session() -> bool {
        // SAFETY: CoreGraphics create/release pair.
        unsafe {
            let session = CGSessionCopyCurrentDictionary();
            if session.is_null() {
                return false;
            }
            CFRelease(session);
            true
        }
    }

    /// A keyboard type of layout type `kind` (`b"ISO "`, `b"ANSI"`).
    fn keyboard_type(kind: &[u8; 4]) -> u8 {
        (0..=u8::MAX)
            // SAFETY: plain HIToolbox lookup.
            .find(|&t| unsafe { KBGetLayoutType(i16::from(t)) } == u32::from_be_bytes(*kind))
            .expect("keyboard type")
    }

    fn label(layout: &KeyboardLayout, key: KeyCode) -> (String, Option<String>) {
        let l = layout.label(key).unwrap_or_else(|| panic!("{key:?} labelled"));
        (l.text, l.shifted)
    }

    fn pair(text: &str, shifted: &str) -> (String, Option<String>) {
        (text.into(), Some(shifted.into()))
    }

    /// Installed layouts read by id (never selected: the user's input source is untouched).
    pub fn installed_layouts_translate_as_printed() {
        let Some(fr) = __installed_macos_layout("com.apple.keylayout.French", None) else {
            println!("skipping: French layout not installed");
            return;
        };
        assert_eq!(fr.os_layout_id(), Some("com.apple.keylayout.French"));
        assert_eq!(label(&fr, KeyCode::KeyA), pair("q", "Q"));
        assert_eq!(label(&fr, KeyCode::KeyQ), pair("a", "A"));
        assert_eq!(label(&fr, KeyCode::KeyW), pair("z", "Z"));
        assert_eq!(label(&fr, KeyCode::Semicolon), pair("m", "M"));
        assert_eq!(label(&fr, KeyCode::Digit2), pair("é", "2"));
        assert_eq!(
            label(&fr, KeyCode::BracketLeft).0,
            "^",
            "dead circumflex: base character"
        );
        if let Some(de) = __installed_macos_layout("com.apple.keylayout.German", None) {
            assert_eq!(label(&de, KeyCode::KeyY), pair("z", "Z"));
            assert_eq!(label(&de, KeyCode::KeyZ), pair("y", "Y"));
            assert_eq!(label(&de, KeyCode::Semicolon), pair("ö", "Ö"));
            assert_eq!(label(&de, KeyCode::Minus).0, "ß");
        }
        if let Some(dv) = __installed_macos_layout("com.apple.keylayout.Dvorak", None) {
            assert_eq!(label(&dv, KeyCode::KeyS).0, "o");
            assert_eq!(label(&dv, KeyCode::Period).0, "v");
        }
    }

    /// ISO Macs: winit reports the key left of `1` (virtual key 0x0A) and the key right of
    /// left Shift (0x32) both as `Backquote`. The runner tells them apart by virtual key
    /// (`key_events_are_mapped_by_virtual_key`), but falls back to `Backquote` for either when
    /// an event's virtual key is unknown: `Backquote`'s label is the one left of `1` and is not
    /// learned from presses.
    pub fn iso_backquote_is_the_key_left_of_1() {
        let (iso, ansi) = (keyboard_type(b"ISO "), keyboard_type(b"ANSI"));
        let cases = [
            ("com.apple.keylayout.US", pair("§", "±"), pair("`", "~")),
            ("com.apple.keylayout.French", pair("@", "#"), pair("<", ">")),
            ("com.apple.keylayout.German", pair("^", "°"), pair("<", ">")),
        ];
        for (id, iso_label, ansi_label) in cases {
            let Some(mut on_iso) = __installed_macos_layout(id, Some(iso)) else {
                println!("skipping {id}: not installed");
                continue;
            };
            let mut on_ansi = __installed_macos_layout(id, Some(ansi)).expect("installed");
            assert_eq!(label(&on_iso, KeyCode::Backquote), iso_label, "{id} ISO");
            assert_eq!(label(&on_ansi, KeyCode::Backquote), ansi_label, "{id} ANSI");
            // A press of the other ISO key (right of left Shift) does not relabel it.
            let other = ansi_label.0.as_str();
            assert!(!on_iso.learn(KeyCode::Backquote, Some(other), None), "{id}");
            assert_eq!(label(&on_iso, KeyCode::Backquote), iso_label, "{id} ISO after a press");
            assert_eq!(on_iso.label(KeyCode::Backquote).unwrap().source, KeyLabelSource::Os);
            // ANSI has one such key: presses are learned as usual.
            assert!(on_ansi.learn(KeyCode::Backquote, Some("x"), None), "{id}");
            // Other keys are the same on both.
            assert_eq!(label(&on_iso, KeyCode::KeyA), label(&on_ansi, KeyCode::KeyA), "{id}");
        }
    }

    /// The keys winit 0.30 misses or conflates get OS labels from their virtual keys: on ISO
    /// keyboards `IntlBackslash` is the key right of left Shift (`0x32`), on ANSI ones the
    /// extra key an ISO keyboard typed as ANSI sends (`0x0A`); JIS Ro (`0x5E`) is `IntlRo`.
    /// Installed layouts are read by id (never selected).
    pub fn iso_and_jis_keys_have_os_labels() {
        let (iso, ansi, jis) = (keyboard_type(b"ISO "), keyboard_type(b"ANSI"), keyboard_type(b"JIS "));
        // (layout, ISO Backquote, ISO IntlBackslash): as printed on the keys of Apple's ISO
        // keyboards (left of 1, right of left Shift).
        let cases = [
            ("com.apple.keylayout.British", pair("§", "±"), pair("`", "~")),
            ("com.apple.keylayout.German", pair("^", "°"), pair("<", ">")),
            ("com.apple.keylayout.French", pair("@", "#"), pair("<", ">")),
            ("com.apple.keylayout.US", pair("§", "±"), pair("`", "~")),
        ];
        let mut checked = 0;
        for (id, backquote, intl_backslash) in cases {
            let Some(on_iso) = __installed_macos_layout(id, Some(iso)) else {
                println!("skipping {id}: not installed");
                continue;
            };
            assert_eq!(label(&on_iso, KeyCode::Backquote), backquote, "{id} ISO");
            assert_eq!(label(&on_iso, KeyCode::IntlBackslash), intl_backslash, "{id} ISO");
            let on_ansi = __installed_macos_layout(id, Some(ansi)).expect("installed");
            assert_eq!(label(&on_ansi, KeyCode::Backquote), intl_backslash, "{id} ANSI: 0x32");
            assert_eq!(label(&on_ansi, KeyCode::IntlBackslash), backquote, "{id} ANSI: 0x0A");
            // The key right of left Shift is found by what it types.
            let text = intl_backslash.0.as_str();
            assert_eq!(on_iso.key_for_label(text), Some(KeyCode::IntlBackslash), "{id} ISO");
            println!("  {id} (ISO): Backquote {backquote:?}, IntlBackslash {intl_backslash:?}");
            checked += 1;
        }
        assert!(checked > 0, "no ISO layout installed");
        // JIS: the Yen and Ro keys of the ASCII layouts Japanese input methods type through.
        let mut jis_checked = 0;
        for id in ["com.apple.keylayout.ABC", "com.apple.keylayout.US"] {
            let Some(on_jis) = __installed_macos_layout(id, Some(jis)) else {
                println!("skipping {id}: not installed");
                continue;
            };
            assert_eq!(label(&on_jis, KeyCode::IntlRo).0, "_", "{id} JIS Ro");
            assert_eq!(label(&on_jis, KeyCode::IntlYen).0, "¥", "{id} JIS Yen");
            assert_eq!(on_jis.key_for_label("_"), Some(KeyCode::IntlRo), "{id}");
            println!(
                "  {id} (JIS): IntlRo {:?}, IntlYen {:?}",
                label(&on_jis, KeyCode::IntlRo),
                label(&on_jis, KeyCode::IntlYen)
            );
            jis_checked += 1;
        }
        assert!(jis_checked > 0, "no ASCII layout installed");
        for id in ["com.apple.keylayout.Japanese", "com.apple.keylayout.JIS"] {
            if __installed_macos_layout(id, None).is_some() {
                println!("  note: {id} is installed but not checked");
            }
        }
    }

    type Id = *mut c_void;
    type Sel = *const c_void;

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsPoint {
        x: f64,
        y: f64,
    }

    fn sel(name: &std::ffi::CStr) -> Sel {
        // SAFETY: registering a selector name.
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// `objc_msgSend` with signature `F`.
    ///
    /// # Safety
    /// `F` must be an `extern "C"` fn pointer type matching the message sent.
    unsafe fn msg<F: Copy>() -> F {
        assert_eq!(size_of::<F>(), size_of::<unsafe extern "C" fn()>());
        let f: unsafe extern "C" fn() = objc_msgSend;
        // SAFETY: per the contract, `F` is a fn pointer type.
        unsafe { std::mem::transmute_copy(&f) }
    }

    /// Make `event` AppKit's current event (`NSApp.currentEvent`), as when AppKit dispatches
    /// it, without a window: post it to this process's own event queue and take it out again.
    /// Nothing reaches the window server or other apps.
    fn make_current(app: Id, event: Id) {
        // SAFETY: AppKit messages with their exact signatures, on the main thread.
        unsafe {
            let post = msg::<unsafe extern "C" fn(Id, Sel, Id, bool)>();
            post(app, sel(c"postEvent:atStart:"), event, true);
            let date =
                msg::<unsafe extern "C" fn(Id, Sel) -> Id>()(objc_getClass(c"NSDate".as_ptr()), sel(c"distantPast"));
            let next = msg::<unsafe extern "C" fn(Id, Sel, u64, Id, *const c_void, bool) -> Id>();
            let got = next(
                app,
                sel(c"nextEventMatchingMask:untilDate:inMode:dequeue:"),
                u64::MAX,
                date,
                kCFRunLoopDefaultMode,
                true,
            );
            assert!(!got.is_null(), "posted event dequeued");
            let current = msg::<unsafe extern "C" fn(Id, Sel) -> Id>()(app, sel(c"currentEvent"));
            assert!(!current.is_null(), "current event set");
        }
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreateKeyboardEvent(source: *const c_void, virtual_key: u16, key_down: bool) -> *const c_void;
        fn CGEventSetIntegerValueField(event: *const c_void, field: u32, value: i64);
    }

    /// `kCGKeyboardEventKeyboardType`.
    const KEYBOARD_TYPE_FIELD: u32 = 10;

    /// A key-down `NSEvent` of virtual key `vk` from a keyboard of type `keyboard_type`
    /// (created only: nothing is posted to the system).
    fn key_event(vk: u16, keyboard_type: u8) -> Id {
        // SAFETY: CoreGraphics/AppKit calls with their exact signatures, on the main thread.
        unsafe {
            let cg = CGEventCreateKeyboardEvent(std::ptr::null(), vk, true);
            assert!(!cg.is_null(), "CGEvent created");
            CGEventSetIntegerValueField(cg, KEYBOARD_TYPE_FIELD, i64::from(keyboard_type));
            let event = msg::<unsafe extern "C" fn(Id, Sel, *const c_void) -> Id>()(
                objc_getClass(c"NSEvent".as_ptr()),
                sel(c"eventWithCGEvent:"),
                cg,
            );
            CFRelease(cg);
            assert!(!event.is_null(), "key event created");
            event
        }
    }

    fn app_defined_event() -> Id {
        // SAFETY: as above.
        unsafe {
            let make =
                msg::<unsafe extern "C" fn(Id, Sel, u64, NsPoint, u64, f64, isize, Id, i16, isize, isize) -> Id>();
            let event = make(
                objc_getClass(c"NSEvent".as_ptr()),
                sel(c"otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:"),
                15, // NSEventTypeApplicationDefined
                NsPoint { x: 0.0, y: 0.0 },
                0,
                0.0,
                0,
                std::ptr::null_mut(),
                0,
                0,
                0,
            );
            assert!(!event.is_null(), "event created");
            event
        }
    }

    /// The runner's physical key comes from the virtual key of the event AppKit dispatches
    /// (`NSApp.currentEvent`) and its keyboard type: `0x0A` and `0x32` (both `Backquote` for
    /// winit) are told apart, JIS Ro is `IntlRo`. Uses this process's `NSApplication` with
    /// activation prohibited (no Dock icon, no focus change) and events created in and posted
    /// to this process only (the user's input sources and other apps are untouched).
    pub fn key_events_are_mapped_by_virtual_key() {
        let backquote = PhysicalKey::Code(KeyCode::Backquote);
        let ro = PhysicalKey::Unidentified(NativeKeyCode::MacOS(0x5E));
        assert_eq!(__macos_physical_key(backquote), backquote, "no NSApplication yet");
        assert_eq!(__macos_physical_key(ro), PhysicalKey::Code(KeyCode::IntlRo));
        // SAFETY: AppKit messages with their exact signatures, on the main thread.
        let app = unsafe {
            let app = msg::<unsafe extern "C" fn(Id, Sel) -> Id>()(
                objc_getClass(c"NSApplication".as_ptr()),
                sel(c"sharedApplication"),
            );
            assert!(!app.is_null());
            // NSApplicationActivationPolicyProhibited: never in the Dock, never active (the
            // default of a process without an app bundle; `setActivationPolicy:` returns NO
            // when the policy does not change).
            msg::<unsafe extern "C" fn(Id, Sel, isize) -> bool>()(app, sel(c"setActivationPolicy:"), 2);
            let policy = msg::<unsafe extern "C" fn(Id, Sel) -> isize>()(app, sel(c"activationPolicy"));
            assert_eq!(policy, 2, "activation prohibited");
            app
        };
        // ISO keyboards send 0x0A left of 1 and 0x32 right of left Shift; ANSI ones 0x32 left
        // of 1 (and 0x0A for the extra key of an ISO keyboard typed as ANSI).
        let (iso, ansi) = (keyboard_type(b"ISO "), keyboard_type(b"ANSI"));
        let cases = [
            (iso, 0x0A, KeyCode::Backquote),
            (iso, 0x32, KeyCode::IntlBackslash),
            (ansi, 0x32, KeyCode::Backquote),
            (ansi, 0x0A, KeyCode::IntlBackslash),
        ];
        for (kind, vk, want) in cases {
            make_current(app, key_event(vk, kind));
            // SAFETY: plain Low Memory accessor on the main thread.
            assert_eq!(unsafe { LMGetKbdType() }, kind, "the event's keyboard type is current");
            assert_eq!(
                __macos_physical_key(backquote),
                PhysicalKey::Code(want),
                "type {kind}, key {vk:#04x}"
            );
            println!("  keyboard type {kind}, virtual key {vk:#04x}: {want:?}");
        }
        // Other keys, unidentified keys and non-key events are not affected.
        make_current(app, key_event(0x00, iso));
        assert_eq!(__macos_physical_key(backquote), backquote, "another key's event");
        assert_eq!(__macos_physical_key(ro), PhysicalKey::Code(KeyCode::IntlRo));
        let other = PhysicalKey::Unidentified(NativeKeyCode::MacOS(0x6E));
        assert_eq!(__macos_physical_key(other), other);
        make_current(app, app_defined_event());
        assert_eq!(__macos_physical_key(backquote), backquote, "not a key event");
    }

    /// Off the main thread nothing touches Text Input Sources (macOS 14+ would abort).
    pub fn nothing_is_read_off_the_main_thread() {
        let (refreshed, loaded, installed) = std::thread::spawn(|| {
            let mut input = Input::default();
            let refreshed = input.refresh_keyboard_layout();
            (
                refreshed,
                input.keyboard_layout().has_os_layout(),
                __installed_macos_layout("com.apple.keylayout.US", None).is_some(),
            )
        })
        .join()
        .unwrap();
        assert_eq!((refreshed, loaded, installed), (false, false, false));
    }
}
