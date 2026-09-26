//! `physical_key_for_egui_key` does not allocate in the steady state: a counting global
//! allocator (per thread, since libtest runs tests and its own bookkeeping on several threads)
//! sees no allocation while the keyboard layout's labels do not change.

use space3d_platform::{Input, KeyCode, LogicalKey};
use space3d_ui::physical_key_for_egui_key;
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
