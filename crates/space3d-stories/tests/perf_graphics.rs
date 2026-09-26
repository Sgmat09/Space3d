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
    // Six seconds: the battle's particle load (explosions, missiles, their smoke) builds up for
    // about five before it is stationary; the growth of the pools and scratch buffers it causes
    // until then is warm-up.
    app.step_ticks(360);
    let before = app.resources.expect::<Renderer>().stats().clone();
    let mut reallocations = 0;
    let mut textures = 0;
    let mut particle_reallocations = 0;
    for _ in 0..30 {
        app.step_ticks(1);
        let s = app.resources.expect::<Renderer>().stats();
        reallocations += s.cpu_reallocations;
        textures += s.textures_created;
        particle_reallocations += app
            .resources
            .expect::<space3d_particles::ParticleStats>()
            .cpu_reallocations;
    }
    let report = measure_battle(&mut app, 0, 12);
    eprintln!("{report}");
    assert_eq!(report.gpu_errors, 0);
    assert_eq!((reallocations, textures), (0, 0), "steady state: {before:?}");
    assert_eq!(
        particle_reallocations, 0,
        "ParticleStats::cpu_reallocations in the steady state"
    );
    let s = &report.stats;
    // VRAM (graphics spec §4.4, T1/T2 ≤ 220 MB of graphics-owned textures): the frame and post
    // targets (S, its MSAA copy, depth, bloom, distortion, previews) and the shadow map
    // (`target_bytes`: 146 MB at High, 1080p), the sky cubemap and the baked image-based
    // lighting (`lighting_bytes`: the 1024² HDR sky is `Rg11b10Ufloat` with mips where that
    // renders, 33.5 MB, the 512² specular cube 17 MB), and the particle pool (≈ 23 MB at
    // 262 144). The driver memory each render pass holds while its frame is in flight comes
    // on top (spec §4.4, M1.1 amendment). Without `Rg11b10Ufloat` (GL) the sky is
    // `Rgba16Float`, 67 MB: the parts are pinned instead.
    let compact = app
        .resources
        .expect::<Renderer>()
        .graphics_caps()
        .compact_hdr_format
        .is_some();
    eprintln!(
        "VRAM textures: targets + shadow {:.1} MB, sky + IBL {:.1} MB",
        s.target_bytes as f64 / 1e6,
        s.lighting_bytes as f64 / 1e6
    );
    assert!(
        s.target_bytes >= 16 << 20,
        "the 2048² shadow map is counted: {}",
        s.target_bytes
    );
    assert!(
        s.target_bytes <= 150_000_000,
        "frame, post and shadow targets: {}",
        s.target_bytes
    );
    let lighting = if compact { 55_000_000 } else { 90_000_000 };
    assert!(
        s.lighting_bytes > 0 && s.lighting_bytes <= lighting,
        "sky + IBL: {}",
        s.lighting_bytes
    );
    if compact {
        const POOL: u64 = 23_000_000;
        assert!(
            s.target_bytes + s.lighting_bytes + POOL <= 220_000_000,
            "T1/T2 VRAM line: {} + {} + {POOL}",
            s.target_bytes,
            s.lighting_bytes
        );
    }
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

/// The steady state of the tiers below High, which run the CPU particle backend (Low, and
/// every tier without compute: ANGLE / WebGL2) or a smaller load: after ten seconds nothing
/// grows for fifteen more. The Battle's load is random (hits, bolts, missiles by tick hash),
/// so it keeps setting small new peaks of effect instances; the collections sized by those
/// (free lists, finish events, the entity map, the sort buffers) used to grow at them 10–20 s
/// in (Low: at 7 frames of 1500), and now grow only with the instance and entry tables.
#[test]
fn battle_steady_state_low_and_medium() {
    let small = ShowcaseContent::generate(ContentOptions {
        texture_size: 256,
        sky_face_size: 256,
    });
    for quality in [GraphicsQuality::Low, GraphicsQuality::Medium] {
        let scene = BattleScene {
            quality,
            hud: false,
            ..BattleScene::default()
        };
        let mut app = match headless_battle(960, 540, &scene, &small) {
            Ok(app) => app,
            Err(e) => {
                eprintln!("skipping: no GPU adapter ({e})");
                return;
            }
        };
        app.step_ticks(600);
        let (mut particles, mut render, mut textures) = (Vec::new(), 0, 0);
        for frame in 0..900 {
            app.step_ticks(1);
            let s = app.resources.expect::<Renderer>().stats();
            render += s.cpu_reallocations;
            textures += s.textures_created;
            let p = app
                .resources
                .expect::<space3d_particles::ParticleStats>()
                .cpu_reallocations;
            if p > 0 {
                particles.push(frame);
            }
        }
        let stats = app.resources.expect::<space3d_particles::ParticleStats>();
        eprintln!(
            "{quality}: {:?} backend, {} effects, {} layer instances; particle reallocations at frames {particles:?}, render {render}, textures {textures}",
            stats.backend, stats.effects, stats.layer_instances
        );
        assert!(
            particles.is_empty(),
            "{quality}: ParticleStats::cpu_reallocations at frames {particles:?}"
        );
        assert_eq!((render, textures), (0, 0), "{quality}: RenderStats");
    }
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
