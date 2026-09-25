//! Post-processing effects (graphics spec §5.2 P-POST) on the default backend: bloom (PO1).
//! The bodies live in `common/post.rs` (`cases`), shared with the GL run (`tests/gl_post.rs`);
//! `WGPU_BACKEND=gl` runs this file on GL too.

mod common;
#[path = "common/post.rs"]
mod post_common;

use common::headless_app;
use post_common::cases;

#[test]
fn bloom_only_above_threshold() {
    cases::bloom_only_above_threshold(headless_app);
}

#[test]
fn bloom_is_in_exposed_units() {
    cases::bloom_is_in_exposed_units(headless_app);
}

#[test]
fn energy_conserving_preserves_mean() {
    cases::energy_conserving_preserves_mean(headless_app);
}

#[test]
fn bloom_does_not_bleed_across_viewports() {
    cases::bloom_does_not_bleed_across_viewports(headless_app);
}

#[test]
fn hdr_scene_overlays_feed_bloom() {
    cases::hdr_scene_overlays_feed_bloom(headless_app);
}

#[test]
fn bloom_off_in_ldr_fallback() {
    cases::bloom_off_in_ldr_fallback(headless_app);
}

#[test]
fn transparent_rt_bloom_weighted_by_alpha() {
    cases::transparent_rt_bloom_weighted_by_alpha(headless_app);
}

#[test]
fn bloom_steady_state() {
    cases::bloom_steady_state(headless_app);
}

#[test]
fn bloom_downlevel_parity() {
    cases::bloom_downlevel_parity(headless_app);
}
