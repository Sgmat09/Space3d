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
}

#[cfg(target_os = "macos")]
mod macos {
    use space3d_platform::layout::__installed_macos_layout;
    use space3d_platform::*;
    use std::ffi::c_void;

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
    /// left Shift (0x32) both as `Backquote`; its label is the one left of `1` and is not
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
