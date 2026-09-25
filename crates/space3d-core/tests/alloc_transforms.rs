//! Steady-state heap allocations of `propagate_transforms` (graphics spec §1.1 invariant 2: the
//! frame allocates nothing engine-side): with a parent hierarchy that neither grows nor gains
//! entities, propagating moving transforms allocates nothing.
//!
//! The only test in this binary: the counter is global.

use space3d_core::prelude::*;
use space3d_core::transform::propagate_transforms;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator; the counter is a plain atomic.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn propagating_a_steady_hierarchy_allocates_nothing() {
    let mut world = hecs::World::new();
    // 64 ships, each with a hull, two engines under the hull and a turret: depth 3.
    let mut ships = Vec::new();
    let mut engines = Vec::new();
    for i in 0..64 {
        let ship = world.spawn((Transform::from_xyz(i as f32, 0.0, 0.0),));
        let hull = world.spawn((Transform::from_xyz(0.0, 0.5, 0.0), Parent(ship)));
        for x in [-1.0, 1.0] {
            engines.push(world.spawn((Transform::from_xyz(x, 0.0, 2.0), Parent(hull))));
        }
        world.spawn((Transform::from_xyz(0.0, 1.0, 0.0), Parent(ship)));
        ships.push(ship);
    }
    // Warm-up: every entity gains its `GlobalTransform`, the scratch reaches its size.
    propagate_transforms(&mut world);
    propagate_transforms(&mut world);

    let before = ALLOCS.load(Relaxed);
    for frame in 1..=120 {
        for (i, &ship) in ships.iter().enumerate() {
            let mut t = world.get::<&mut Transform>(ship).unwrap();
            t.translation.z = frame as f32 * 0.1 + i as f32;
            t.rotation = Quat::from_rotation_y(frame as f32 * 0.01);
        }
        propagate_transforms(&mut world);
    }
    let allocs = ALLOCS.load(Relaxed) - before;

    // The hierarchy resolved: an engine sits at ship + hull + engine offsets.
    let ship = world.get::<&GlobalTransform>(ships[3]).unwrap().0;
    let engine = world.get::<&GlobalTransform>(engines[3 * 2 + 1]).unwrap().0;
    let expected = ship.transform_point3(Vec3::new(1.0, 0.5, 2.0));
    assert!(
        engine.w_axis.truncate().distance(expected) < 1e-4,
        "{engine:?} vs {expected:?}"
    );
    assert_eq!(allocs, 0, "propagate_transforms allocated {allocs} times in 120 frames");
}
