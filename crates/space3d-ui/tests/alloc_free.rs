//! `physical_key_for_egui_key` and the theme sync do not allocate in the steady state: a
//! counting global allocator (per thread, since libtest runs tests and its own bookkeeping on
//! several threads) sees no allocation while the keyboard layout's labels do not change, and
//! none while the `UiTheme` resource is unchanged or absent. The console's work per frame does
//! not grow with its number of lines.

use space3d_core::App;
use space3d_platform::{Input, KeyCode, LogicalKey};
use space3d_ui::{Console, LogLevel, UiPlugin, UiTheme, UiThemeSync, physical_key_for_egui_key};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; the counter is a const-initialized thread-local
// `Cell` (no allocation, no destructor).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|n| n.set(n.get() + 1));
        // SAFETY: same contract as ours.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as ours.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|n| n.set(n.get() + 1));
        // SAFETY: same contract as ours.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations made by `f` on this thread.
fn allocations(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}

#[test]
fn physical_key_for_egui_key_does_not_allocate_in_the_steady_state() {
    use egui::Key as E;
    let mut input = Input::default();
    // AZERTY, as learned from key presses.
    for (code, c) in [
        (KeyCode::KeyQ, "a"),
        (KeyCode::KeyW, "z"),
        (KeyCode::KeyA, "q"),
        (KeyCode::KeyZ, "w"),
        (KeyCode::Semicolon, "m"),
        (KeyCode::KeyM, ","),
        (KeyCode::Digit2, "é"),
    ] {
        input.inject_key_tap_logical(code, LogicalKey::Character(c.into()));
    }
    input.end_frame();
    let queries = [
        (E::M, Some(KeyCode::Semicolon)),
        (E::Z, Some(KeyCode::KeyW)),
        (E::Comma, Some(KeyCode::KeyM)),
        (E::L, Some(KeyCode::KeyL)),
        (E::Enter, Some(KeyCode::Enter)),
        (E::F5, Some(KeyCode::F5)),
        (E::Num2, Some(KeyCode::Digit2)), // "é" has no egui key: the physical key sends Num2
    ];
    // The first lookup after a change builds the layout's reverse map.
    let first = allocations(|| {
        physical_key_for_egui_key(&input, E::M);
    });
    assert!(first > 0, "the reverse map is built on first use");
    let mut results = Vec::with_capacity(queries.len() * 100);
    let steady = allocations(|| {
        for _ in 0..100 {
            for (key, _) in queries {
                results.push(physical_key_for_egui_key(&input, key));
            }
        }
    });
    assert_eq!(steady, 0, "no allocation while the labels do not change");
    for (i, got) in results.iter().enumerate() {
        let (key, want) = queries[i % queries.len()];
        assert_eq!(*got, want, "{key:?}");
    }
    // A label change rebuilds the map once, then lookups are free again.
    input.inject_key_tap_logical(KeyCode::Digit2, LogicalKey::Character("2".into()));
    input.end_frame();
    assert_eq!(physical_key_for_egui_key(&input, E::Num2), Some(KeyCode::Digit2));
    let again = allocations(|| {
        for (key, _) in queries {
            physical_key_for_egui_key(&input, key);
        }
    });
    assert_eq!(again, 0);
}

#[test]
fn theme_sync_does_not_allocate_in_the_steady_state() {
    let ctx = egui::Context::default();
    let theme = UiTheme::compact();
    let mut sync = UiThemeSync::default();
    let first = allocations(|| assert!(sync.sync(&ctx, Some(&theme))));
    assert!(first > 0, "applying a theme restyles egui once");
    let mut absent = UiThemeSync::default();
    let steady = allocations(|| {
        for _ in 0..1000 {
            assert!(!sync.sync(&ctx, Some(&theme)), "unchanged theme");
            assert!(!absent.sync(&ctx, None), "no theme");
        }
    });
    assert_eq!(steady, 0, "no allocation while the theme is unchanged or absent");
    assert_eq!(sync.applications(), 1);
}

/// Allocations per frame of a UI-only app with the console open, holding `lines` lines (some
/// long enough to wrap), after it settled.
fn console_allocations_per_frame(lines: usize) -> usize {
    let mut app = App::new();
    app.add_plugin(UiPlugin {
        install_tracing: false,
        fallback_size: (1280, 720),
        ..UiPlugin::deterministic()
    });
    let console = app.resources.expect_mut::<Console>();
    console.visible = true;
    console.capture_logs = false;
    for i in 0..lines {
        let level = [LogLevel::Info, LogLevel::Warn, LogLevel::Error][i % 3];
        let tail = if i % 5 == 0 {
            " (a long line: it wraps under its message in the console)"
        } else {
            ""
        };
        console.print(level, format!("line {i}: 12 meshes loaded in 0.84 s{}", tail.repeat(4)));
    }
    app.step_ticks(30);
    let frames = 20;
    allocations(|| app.step_ticks(frames)) / frames as usize
}

/// Only the console lines in view are laid out: a console holding 1000 lines allocates per frame
/// what one holding 40 does (it used to lay out every line, every frame).
#[test]
fn console_allocations_do_not_grow_with_its_lines() {
    let few = console_allocations_per_frame(40);
    let many = console_allocations_per_frame(1000);
    assert!(
        many <= few + few / 20,
        "{many} allocations per frame with 1000 lines, {few} with 40"
    );
}
