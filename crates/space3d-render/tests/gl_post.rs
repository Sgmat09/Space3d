//! Post-processing on wgpu's GL backend — the WebGL2 / GLES 3.0 path (graphics spec §4.1 class
//! B: float color buffers through `EXT_color_buffer_float`): the `tests/post.rs` cases on a GL
//! adapter, plus bloom parity with the default backend.
//!
//! Skips when there is no GL adapter. On macOS, ANGLE (GLES 3.0, WebGL2's feature level) provides
//! one; ANGLE crashes when several GL contexts render from different threads, so run it with one
//! test thread:
//!
//! ```text
//! DYLD_FALLBACK_LIBRARY_PATH=<dir with libEGL.dylib> \
//!     cargo test -p space3d-render --features angle --test gl_post -- --test-threads=1
//! ```

mod common;
#[path = "common/post.rs"]
mod post_common;

use post_common::cases;
use space3d_core::prelude::*;
use space3d_render::*;
use space3d_test::{CompareOptions, compare_images};

fn gl_app(width: u32, height: u32) -> Option<App> {
    let renderer = pollster::block_on(Renderer::new_headless_with(RendererConfig {
        width,
        height,
        backends: Some(wgpu::Backends::GL),
        ..Default::default()
    }));
    match renderer {
        Ok(r) => {
            assert_eq!(r.capabilities().adapter.backend, wgpu::Backend::Gl);
            eprintln!(
                "GL adapter: {} ({}), bloom chain {:?}",
                r.capabilities().adapter.name,
                r.capabilities().adapter.driver_info,
                r.graphics_caps().bloom_format,
            );
            if r.graphics_caps().bloom_format.is_none() {
                eprintln!("skipping GL post test: no float render targets (class C: no bloom)");
                return None;
            }
            let mut app = App::new();
            app.add_plugin(RenderPlugin::default());
            app.insert_resource(r);
            Some(app)
        }
        Err(e) => {
            eprintln!("skipping GL test: no GL adapter ({e})");
            None
        }
    }
}

#[test]
fn gl_bloom_only_above_threshold() {
    cases::bloom_only_above_threshold(gl_app);
}

#[test]
fn gl_bloom_is_in_exposed_units() {
    cases::bloom_is_in_exposed_units(gl_app);
}

#[test]
fn gl_energy_conserving_preserves_mean() {
    cases::energy_conserving_preserves_mean(gl_app);
}

#[test]
fn gl_bloom_does_not_bleed_across_viewports() {
    cases::bloom_does_not_bleed_across_viewports(gl_app);
}

#[test]
fn gl_hdr_scene_overlays_feed_bloom() {
    cases::hdr_scene_overlays_feed_bloom(gl_app);
}

#[test]
fn gl_bloom_off_in_ldr_fallback() {
    cases::bloom_off_in_ldr_fallback(gl_app);
}

#[test]
fn gl_transparent_rt_bloom_weighted_by_alpha() {
    cases::transparent_rt_bloom_weighted_by_alpha(gl_app);
}

#[test]
fn gl_bloom_steady_state() {
    cases::bloom_steady_state(gl_app);
}

#[test]
fn gl_bloom_downlevel_parity() {
    cases::bloom_downlevel_parity(gl_app);
}

#[test]
fn gl_fxaa_softens_edges() {
    cases::fxaa_softens_edges(gl_app);
}

#[test]
fn gl_film_grain_is_seeded() {
    cases::film_grain_is_seeded(gl_app);
}

#[test]
fn gl_grain_after_fxaa() {
    cases::grain_after_fxaa(gl_app);
}

#[test]
fn gl_after_tonemap_overlays_untouched_by_grain_ca_vignette() {
    cases::after_tonemap_overlays_untouched_by_grain_ca_vignette(gl_app);
}

#[test]
fn gl_vignette_darkens_corners() {
    cases::vignette_darkens_corners(gl_app);
}

#[test]
fn gl_chromatic_aberration_splits_toward_corners() {
    cases::chromatic_aberration_splits_toward_corners(gl_app);
}

#[test]
fn gl_lens_effects_do_not_bleed_across_viewports() {
    cases::lens_effects_do_not_bleed_across_viewports(gl_app);
}

#[test]
fn gl_fxaa_ldr_fallback_and_steady_state() {
    cases::fxaa_ldr_fallback_and_steady_state(gl_app);
}

#[test]
fn gl_transparent_rt_lens_effects_keep_alpha() {
    cases::transparent_rt_lens_effects_keep_alpha(gl_app);
}

/// Emissive quads of 2 … 40 over a dark clear with the default bloom: GL (level views bound
/// with `TEXTURE_BASE_LEVEL`/`MAX_LEVEL` while rendering the next level) looks like the
/// default backend.
#[test]
fn gl_bloom_matches_default_backend() {
    let scene = |mut app: App| {
        for (i, v) in [2.0f32, 5.0, 12.0, 40.0].into_iter().enumerate() {
            let color = [Vec3::new(1.0, 0.3, 0.1), Vec3::new(0.2, 0.6, 1.0)][i % 2] * v;
            post_common::quad(
                &mut app,
                color,
                Vec2::new(-1.5 + i as f32, 0.3 - 0.2 * i as f32),
                Vec2::splat(0.15),
            );
        }
        post_common::camera(
            &mut app,
            0.0,
            Vec4::new(0.01, 0.01, 0.02, 1.0),
            Some(PostProcess::default()),
        );
        post_common::render(&mut app)
    };
    let Some(gl) = gl_app(128, 64) else { return };
    let Some(default) = common::headless_app(128, 64) else {
        return;
    };
    let (gl, default) = (scene(gl), scene(default));
    let cmp = compare_images(&default, &gl, &CompareOptions::perceptual());
    eprintln!("bloom GL vs default: {}", cmp.summary());
    assert!(cmp.passed, "{}", cmp.summary());
}
