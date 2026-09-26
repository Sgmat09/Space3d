//! Screen-space distortion (graphics spec §1.8, §5.2 PO5) on the default backend: the composite's
//! apply (offset, chromatic split, bloom lookup, rect clamp), `DistortionMaterial` meshes through
//! the built-in `"space3d/distortion-meshes"` node, occlusion by scene depth on every depth path,
//! and the WebGL2 downlevel path. The bodies live in `common/post.rs` (`distortion`), shared with
//! the GL run (`tests/gl_post.rs`).

mod common;
#[path = "common/post.rs"]
mod post_common;

use common::headless_app;
use post_common::distortion as cases;
use space3d_core::prelude::*;
use space3d_render::*;

fn msaa_app(width: u32, height: u32) -> Option<App> {
    let renderer = pollster::block_on(Renderer::new_headless_with(RendererConfig {
        width,
        height,
        msaa_samples: 4,
        ..Default::default()
    }));
    match renderer {
        Ok(r) if r.capabilities().max_msaa >= 4 => {
            let mut app = App::new();
            app.add_plugin(RenderPlugin::default());
            app.insert_resource(r);
            Some(app)
        }
        Ok(_) => {
            eprintln!("skipping MSAA test: no 4× MSAA");
            None
        }
        Err(e) => {
            eprintln!("skipping GPU test: no adapter available ({e})");
            None
        }
    }
}

fn none(_: &mut App) {}

fn webgl2(app: &mut App) {
    app.resources.expect_mut::<FrameSettings>().downlevel = DownlevelOverride::webgl2();
}

fn no_read_only_depth(app: &mut App) {
    app.resources.expect_mut::<FrameSettings>().downlevel.no_read_only_depth = true;
}

fn half_scale(app: &mut App) {
    app.resources.expect_mut::<FrameSettings>().render_scale = 0.5;
}

#[test]
fn distortion_shifts_the_background() {
    cases::distortion_shifts_the_background(headless_app, none);
}

/// The WebGL2 path on the default backend: the Ldr fallback, no read-only depth, no compute.
#[test]
fn distortion_shifts_the_background_webgl2() {
    cases::distortion_shifts_the_background(headless_app, webgl2);
}

/// The buffer follows the render scale (the rect in `S`); offsets are in view heights, so the
/// shift in display pixels is the same.
#[test]
fn distortion_shifts_the_background_at_half_render_scale() {
    cases::distortion_shifts_the_background(headless_app, half_scale);
}

#[test]
fn distortion_shifts_the_background_msaa() {
    cases::distortion_shifts_the_background(msaa_app, none);
}

#[test]
fn distortion_chromatic_split() {
    cases::distortion_chromatic_split(headless_app);
}

#[test]
fn distortion_occluded_by_closer_geometry() {
    cases::distortion_occluded_by_closer_geometry(headless_app, none, true);
}

/// The GL depth path forced (`no_read_only_depth`): depth sampled without an attachment.
#[test]
fn distortion_occluded_by_closer_geometry_forced_gl() {
    cases::distortion_occluded_by_closer_geometry(headless_app, no_read_only_depth, true);
}

/// MSAA with a depth resolve (read-only depth and multisampled depth sampling).
#[test]
fn distortion_occluded_by_closer_geometry_msaa() {
    let Some(app) = msaa_app(4, 4) else { return };
    let resolve = app.resources.expect::<Renderer>().graphics_caps().msaa_depth_resolve;
    drop(app);
    cases::distortion_occluded_by_closer_geometry(msaa_app, none, resolve);
}

#[test]
fn foreground_is_not_dragged_over_the_background() {
    cases::foreground_is_not_dragged_over_the_background(headless_app);
}

#[test]
fn hdr_scene_overlays_are_distorted() {
    cases::hdr_scene_overlays_are_distorted(headless_app);
}

#[test]
fn no_distortion_pass_without_writers() {
    cases::no_distortion_pass_without_writers(headless_app);
}

#[test]
fn radial_distortion_pushes_outward() {
    cases::radial_distortion_pushes_outward(headless_app);
}

#[test]
fn normal_writers_are_capped_by_their_size() {
    cases::normal_writers_are_capped_by_their_size(headless_app);
}

#[test]
fn distortion_does_not_bleed_across_viewports() {
    cases::distortion_does_not_bleed_across_viewports(headless_app);
}

#[test]
fn distortion_steady_state() {
    cases::distortion_steady_state(headless_app);
}
