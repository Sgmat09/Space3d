//! Graphics benchmarks (graphics spec §4.4, §5.3 I2/I6): the Battle reference scene
//! (`space3d_stories::showcase`) at 1920 × 1080 on every `GraphicsQuality` tier. Skipped (with
//! a message) without a GPU adapter; the GPU figures need `TIMESTAMP_QUERY`.
//!
//! * `graphics/cpu/<tier>` (criterion): `render_frame` on the CPU, the GPU idle before each
//!   frame (a tick of simulation between frames: the particles replay it in `prepare`).
//! * `graphics/report` (printed after the criterion groups; run alone with
//!   `-- graphics/report`): per tier the GPU frame (the frozen frame's throughput plus the
//!   particle simulation passes), `render_frame`'s CPU time, the per-pass GPU timestamps of
//!   ticking frames, draws, passes and `RenderStats::target_bytes`. Asserts the T1/T2 VRAM
//!   figure of the frame and post targets (≤ 220 MB) at High, and prints the Dev gates
//!   (GPU ≤ 3.0 ms, CPU ≤ 1.5 ms at High; enforced by `tests/perf_graphics.rs`).
//!
//! ```text
//! cargo bench -p space3d-stories --bench graphics
//! cargo bench -p space3d-stories --bench graphics -- graphics/report
//! ```

use criterion::{Criterion, criterion_group};
use space3d_render::{GraphicsQuality, Renderer, wgpu};
use space3d_stories::showcase::{BattleScene, ContentOptions, ShowcaseContent, headless_battle, measure_battle};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The T1/T2 VRAM line for the frame and post targets (spec §4.4).
const VRAM_BYTES: u64 = 220_000_000;

fn content() -> &'static ShowcaseContent {
    static C: OnceLock<ShowcaseContent> = OnceLock::new();
    C.get_or_init(|| {
        ShowcaseContent::generate(ContentOptions {
            texture_size: 1024,
            sky_face_size: 1024,
        })
    })
}

fn selected(name: &str) -> bool {
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    filter.is_none_or(|f| name.contains(f.as_str()))
}

fn cpu(c: &mut Criterion) {
    let mut group = c.benchmark_group("graphics");
    group.sample_size(30);
    for quality in GraphicsQuality::ALL {
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
        app.step_ticks(120);
        group.bench_function(format!("cpu/{quality}"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let mut renderer = app.resources.remove::<Renderer>().expect("renderer");
                    app.step_ticks(1);
                    let _ = renderer.device().poll(wgpu::PollType::wait_indefinitely());
                    let t = Instant::now();
                    renderer.render_frame(&app.world, &mut app.resources);
                    total += t.elapsed();
                    app.resources.insert(renderer);
                }
                total
            })
        });
    }
    group.finish();
}

/// `graphics/report`: every tier's `BattleReport`, the VRAM assertion and the Dev gates.
fn report() {
    if !selected("graphics/report") {
        return;
    }
    for quality in GraphicsQuality::ALL {
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
        let r = measure_battle(&mut app, 240, 60);
        println!("graphics/report {quality}: {r}");
        assert_eq!(r.gpu_errors, 0, "{quality}");
        if quality == GraphicsQuality::High {
            assert!(
                r.stats.target_bytes <= VRAM_BYTES,
                "frame and post targets {} B > {VRAM_BYTES} B",
                r.stats.target_bytes
            );
            println!(
                "graphics/report gates (High, Dev: GPU ≤ 3.0 ms, CPU ≤ 1.5 ms): GPU {:.3} ms {}, CPU {:.3} ms {}, targets {:.1} MB ≤ 220",
                r.gpu_frame_ms,
                if r.gpu_frame_ms <= 3.0 { "ok" } else { "OVER" },
                r.cpu_render_ms,
                if r.cpu_render_ms <= 1.5 { "ok" } else { "OVER" },
                r.stats.target_bytes as f64 / 1e6
            );
        }
    }
}

criterion_group!(benches, cpu);

fn main() {
    if selected("graphics/cpu") {
        benches();
    }
    report();
    Criterion::default().configure_from_args().final_summary();
}
