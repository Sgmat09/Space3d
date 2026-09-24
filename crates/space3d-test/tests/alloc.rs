//! Allocation counting: this test binary opts into the counting global allocator.

use space3d_core::prelude::*;
use space3d_test::perf::AllocStats;
use space3d_test::{TestApp, allocation_counting_enabled, assert_max_allocations, count_allocations};

space3d_test::install_counting_allocator!();

#[test]
fn counts_allocations_of_a_closure() {
    assert!(allocation_counting_enabled());
    let (v, stats) = count_allocations(|| vec![7u8; 100]);
    assert_eq!(v.len(), 100);
    assert_eq!(stats.allocations, 1, "{stats:?}");
    assert!(stats.bytes_allocated >= 100);
    let (_, grow) = count_allocations(|| {
        let mut s: Vec<u32> = Vec::new();
        s.extend(0..1000);
        s
    });
    assert!(grow.total_calls() >= 1);
    assert_ne!(AllocStats::current_thread(), AllocStats::default());
}

#[test]
fn hot_loop_with_preallocated_buffer_does_not_allocate() {
    let mut buf: Vec<u64> = Vec::with_capacity(256);
    assert_max_allocations(0, || {
        for i in 0..256 {
            buf.push(i * i);
        }
    });
    assert_eq!(buf.len(), 256);
}

#[test]
#[should_panic(expected = "expected at most 0 allocations")]
fn allocating_code_fails_the_assertion() {
    assert_max_allocations(0, || Box::new(1u64));
}

#[test]
fn steady_state_ticks_do_not_allocate_more_over_time() {
    let mut app = TestApp::no_render();
    for i in 0..100 {
        app.world.spawn((Transform::from_xyz(i as f32, 0.0, 0.0),));
    }
    app.add_system(Stage::FixedUpdate, |ctx| {
        for t in ctx.world.query_mut::<&mut Transform>() {
            t.translation.y += 0.01;
        }
    });
    app.step_ticks(10); // warm-up: first-frame resource and buffer growth
    let (_, first) = count_allocations(|| app.step_ticks(50));
    let (_, second) = count_allocations(|| app.step_ticks(50));
    assert!(
        second.total_calls() <= first.total_calls(),
        "allocations grow over time: {first:?} then {second:?}"
    );
}
