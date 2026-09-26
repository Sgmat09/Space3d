//! The Battle reference scene (graphics spec §4.4; `space3d_stories::showcase`) at 1080p:
//! * `battle_steady_state` (always): no GPU errors, the steady-state invariants (no CPU
//!   reallocations, no textures created after warm-up), the VRAM figure of the frame and post
//!   targets (T1/T2 ≤ 220 MB), the draw and pass counts, every feature present;
//! * `battle_dev_gates` (ignored: a perf gate, run alone on an idle Dev machine with
//!   `--ignored --nocapture`; `SPACE3D_PERF_BUDGET_SCALE` relaxes it elsewhere): the GPU frame
//!   ≤ 3.0 ms and `render_frame` ≤ 1.5 ms CPU at `GraphicsQuality::High`, and the other tiers
//!   printed for reference.
//!
//! Both skip without a GPU adapter; the gates also need `TIMESTAMP_QUERY`.
#![cfg(feature = "engine")]

use space3d_render::{GraphicsQuality, Renderer};
use space3d_stories::showcase::{BattleScene, ContentOptions, ShowcaseContent, headless_battle, measure_battle};
use std::sync::OnceLock;

/// High's content: 1024² hull maps, 1024² sky faces.
fn content() -> &'static ShowcaseContent {
    static C: OnceLock<ShowcaseContent> = OnceLock::new();
    C.get_or_init(|| {
        ShowcaseContent::generate(ContentOptions {
            texture_size: 1024,
            sky_face_size: 1024,
        })
    })
}

fn budget_scale() -> f64 {
    std::env::var("SPACE3D_PERF_BUDGET_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0)
}

/// Everything the reference scene lists is there, and the steady state allocates nothing.
#[test]
fn battle_steady_state() {
    let scene = BattleScene {
        hud: false,
        ..BattleScene::default()
    };
    let mut app = match headless_battle(1920, 1080, &scene, content()) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("skipping: no GPU adapter ({e})");
            return;
        }
    };
    let gpu_particles = app.resources.expect::<Renderer>().graphics_caps().compute;
    app.step_ticks(120);
    let before = app.resources.expect::<Renderer>().stats().clone();
    let mut reallocations = 0;
    let mut textures = 0;
    for _ in 0..30 {
        app.step_ticks(1);
        let s = app.resources.expect::<Renderer>().stats();
        reallocations += s.cpu_reallocations;
        textures += s.textures_created;
    }
    let report = measure_battle(&mut app, 0, 12);
    eprintln!("{report}");
    assert_eq!(report.gpu_errors, 0);
    assert_eq!((reallocations, textures), (0, 0), "steady state: {before:?}");
    let s = &report.stats;
    // The frame and post targets (S, its MSAA copy, depth, bloom, distortion, shadow map,
    // previews): the T1/T2 VRAM line.
    assert!(s.target_bytes <= 220_000_000, "{} bytes", s.target_bytes);
    assert_eq!(s.depth_views, 1, "one shadowed sun");
    assert_eq!(s.post_groups, 1, "the battle camera (the previews render without post)");
    assert!(s.views >= 3, "the battle camera and two previews: {}", s.views);
    assert!(s.draw_calls <= 80, "{} draws", s.draw_calls);
    assert!(s.bytes_uploaded < 4 << 20, "{} bytes uploaded", s.bytes_uploaded);
    assert!(
        report.point_lights >= 5,
        "explosion light flashes: {}",
        report.point_lights
    );
    if gpu_particles {
        assert!(report.particles.alive >= 80_000, "{}", report.particles.alive);
    }
    assert_eq!(report.particles.refused, 0);
}

/// The Dev gates: GPU ≤ 3.0 ms, CPU `render_frame` ≤ 1.5 ms at High (1080p, loaded GPU).
#[test]
#[ignore = "perf gate: run alone with --ignored --nocapture"]
fn battle_dev_gates() {
    let scale = budget_scale();
    let mut high = None;
    for quality in [
        GraphicsQuality::High,
        GraphicsQuality::Medium,
        GraphicsQuality::Low,
        GraphicsQuality::Ultra,
    ] {
        let scene = BattleScene {
            quality,
            hud: false,
            ..BattleScene::default()
        };
        let mut app = match headless_battle(1920, 1080, &scene, content()) {
            Ok(app) => app,
            Err(e) => {
                eprintln!("skipping: no GPU adapter ({e})");
                return;
            }
        };
        if !app.resources.expect::<Renderer>().graphics_caps().timestamps {
            eprintln!("skipping: no timestamp queries");
            return;
        }
        let report = measure_battle(&mut app, 240, 60);
        eprintln!("{quality}: {report}");
        assert_eq!(report.gpu_errors, 0);
        if quality == GraphicsQuality::High {
            high = Some(report);
        }
    }
    let high = high.expect("High measured");
    assert!(
        high.gpu_frame_ms <= 3.0 * scale,
        "GPU frame {:.3} ms > {:.3} ms",
        high.gpu_frame_ms,
        3.0 * scale
    );
    assert!(
        high.cpu_render_ms <= 1.5 * scale,
        "render_frame {:.3} ms > {:.3} ms",
        high.cpu_render_ms,
        1.5 * scale
    );
}
