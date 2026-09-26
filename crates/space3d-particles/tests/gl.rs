//! The GL backend — wgpu's WebGL2 / GLES 3.0 path, where particles run on the CPU backend and
//! scene depth is sampled without a depth attachment — renders particles like the default
//! backend: exact blend modes, the manual depth test and soft fade, and every built-in effect
//! within a perceptual tolerance.
//!
//! Skips without a GL adapter. On macOS, build wgpu's ANGLE support and put ANGLE's
//! `libEGL.dylib`/`libGLESv2.dylib` (shipped with Chrome and Electron apps) on the dyld path:
//!
//! ```text
//! DYLD_FALLBACK_LIBRARY_PATH=<dir with libEGL.dylib> \
//!     cargo test -p space3d-particles --features space3d-render/angle --test gl -- --test-threads=1
//! ```

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::ParticleBackend;
use space3d_particles::prelude::*;
use space3d_render::*;
use space3d_test::{CompareOptions, compare_images};

const W: u32 = 320;
const H: u32 = 180;

fn gl_app(settings: ParticleSettings) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    app.add_plugin(ParticlesPlugin::with_settings(settings));
    let renderer = pollster::block_on(Renderer::new_headless_with(RendererConfig {
        width: W,
        height: H,
        backends: Some(wgpu::Backends::GL),
        ..Default::default()
    }));
    match renderer {
        Ok(r) => {
            assert_eq!(r.capabilities().adapter.backend, wgpu::Backend::Gl);
            eprintln!(
                "GL adapter: {} ({})",
                r.capabilities().adapter.name,
                r.capabilities().adapter.driver_info
            );
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
fn gl_blend_modes_composite_exactly() {
    let Some(mut app) = gl_app(settings()) else { return };
    ortho_camera(&mut app, 10.0, Vec4::new(0.2, 0.2, 0.2, 1.0));
    let expected = blend_scene(&mut app);
    app.step_ticks(2);
    let img = render(&mut app);
    dump(&img, "gl_blend");
    for ((x, linear), what) in expected.into_iter().zip(["additive", "alpha", "premultiplied"]) {
        assert_rgb(at(&img, 0.5 + x * 18.0 / W as f32, 0.5), linear, 2, what);
    }
    assert_eq!(stats(&app).backend, Some(ParticleBackend::Cpu));
    assert_eq!(gpu_errors(&app), 0);
}

/// GLES 3.0 has no read-only depth attachments: the particles' pass samples depth without one,
/// tests it manually and fades like the other backends.
#[test]
fn gl_soft_particles_sample_depth_without_an_attachment() {
    let Some(mut app) = gl_app(settings()) else { return };
    ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let img = soft_scene(&mut app);
    dump(&img, "gl_soft");
    assert!(!app.resources.expect::<Renderer>().graphics_caps().read_only_depth);
    let (front, open, behind) = soft_probe(&img);
    assert_rgb(front, Vec3::splat(0.35), 3, "in front of the wall: faded");
    assert_rgb(open, Vec3::ONE, 1, "over the open background");
    assert_rgb(
        behind,
        Vec3::splat(0.1),
        1,
        "behind the wall: hidden by the manual test",
    );
    assert_eq!(gpu_errors(&app), 0);
}

/// Every built-in effect, 20 ticks in, looks the same on GL and on the default backend (beams
/// reach 400 units along the emitter's −Z, here +X; trails and engines move).
#[test]
fn gl_builtins_match_the_default_backend() {
    let scene = |app: &mut App, fx: EffectHandle| {
        camera(
            app,
            Vec3::new(0.0, 90.0, 120.0),
            Vec3::ZERO,
            Vec4::new(0.01, 0.012, 0.02, 1.0),
        );
        let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(11).facing(Vec3::X).keep());
        if [
            builtin::LASER_TRAIL,
            builtin::ENGINE_EXHAUST,
            builtin::MISSILE_TRAIL,
            builtin::ENGINE_AFTERBURNER,
        ]
        .contains(&fx)
        {
            app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
                let t = ctx.time().tick as f32;
                if let Ok(mut tr) = ctx.world.get::<&mut Transform>(e) {
                    tr.translation = Vec3::new(-60.0 + t * 4.0, 0.0, 0.0);
                }
            });
        }
        app.step_ticks(20);
        render(app)
    };
    for fx in builtin::ALL {
        let name = builtin::SOURCES[fx.index() as usize].0.trim_start_matches("space3d/");
        let Some(mut gl) = gl_app(ParticleSettings::default()) else {
            return;
        };
        let a = scene(&mut gl, fx);
        assert_eq!(gpu_errors(&gl), 0, "{name}");
        drop(gl);
        let Some(mut native) = app(W, H) else { return };
        let b = scene(&mut native, fx);
        dump(&a, &format!("gl_{name}"));
        dump(&b, &format!("native_{name}"));
        let diff = compare_images(&a, &b, &CompareOptions::perceptual());
        assert!(diff.passed, "{name}: {}", diff.summary());
    }
}
