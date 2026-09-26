//! Measuring the Battle reference scene against the Dev gates of graphics spec §4.4 (native
//! only): shared by `tests/perf_graphics.rs` (the gates) and `benches/graphics.rs`.

use super::{BattleScene, ShowcaseContent, dress_battle, particle_settings, spawn_battle, turn_previews};
use space3d_core::prelude::*;
use space3d_particles::{ParticleStats, ParticlesPlugin};
use space3d_render::{
    FrameSettings, GpuTimings, PointLight, RenderAssets, RenderError, RenderPlugin, RenderStats, Renderer, wgpu,
};
use std::time::Instant;

/// A headless app running the Battle at `width` × `height`: `RenderPlugin` with a headless
/// renderer, `ParticlesPlugin` with the tier's settings for the device
/// ([`particle_settings`]), the scene spawned and dressed with `content`, the previews
/// turning. `Err` without a GPU adapter.
pub fn headless_battle(
    width: u32,
    height: u32,
    scene: &BattleScene,
    content: &ShowcaseContent,
) -> Result<App, RenderError> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    space3d_render::init_headless(&mut app, width, height)?;
    let settings = particle_settings(&app.resources, scene.quality);
    app.add_plugin(ParticlesPlugin::with_settings(settings));
    let mut handles = spawn_battle(&mut app, scene);
    let assets = content.upload(app.resources.init::<RenderAssets>());
    dress_battle(&mut app.world, &mut app.resources, &assets, scene, &mut handles);
    app.add_system(Stage::FixedUpdate, turn_previews);
    Ok(app)
}

/// What [`measure_battle`] found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BattleReport {
    /// Target size.
    pub size: (u32, u32),
    /// Median CPU time of `render_frame` in ticking frames back to back (a tick of simulation,
    /// then `render_frame`, as a game loop runs them; at most 3 frames in flight), ms. The CPU
    /// gate.
    pub cpu_render_ms: f64,
    /// Median CPU time of `render_frame` with the GPU idle before each call (a cold CPU: the
    /// thread slept while the GPU finished), ms. For reference.
    pub cpu_render_idle_ms: f64,
    /// Median wall time per frame of the same frozen frame rendered back to back (at most 3 in
    /// flight): the GPU's frame time when it is slower than the CPU, else the CPU's; ms.
    pub frame_throughput_ms: f64,
    /// Median GPU time of the particle passes of a ticking frame (`particles.sim`,
    /// `particles.compact`, `particles.sort`; not in the frozen frame), ms.
    pub particle_sim_ms: f64,
    /// `frame_throughput_ms + particle_sim_ms`: the gated GPU frame time, ms.
    pub gpu_frame_ms: f64,
    /// Median of `GpuTimings::total_ms` of ticking frames (a sum of pass scopes: on tile-based
    /// GPUs consecutive scopes overlap, so it overstates the frame), ms.
    pub scopes_total_ms: f64,
    /// Median GPU ms per timing label of ticking frames.
    pub scopes: Vec<(&'static str, f64)>,
    /// Median wall time of a whole ticking frame (simulation, particles tick, render), ms.
    pub ticking_frame_ms: f64,
    /// Median bytes a ticking frame uploads: all of them (`RenderStats::bytes_uploaded`) and the
    /// particles' share (`ParticleStats::bytes_uploaded`).
    pub ticking_upload_bytes: (u64, u64),
    /// Renderer counters of the last frame.
    pub stats: RenderStats,
    /// Particle counters.
    pub particles: ParticleStats,
    /// Point lights in the world (explosion flashes).
    pub point_lights: usize,
    /// GPU errors so far.
    pub gpu_errors: u64,
    /// Adapter.
    pub adapter: String,
}

impl std::fmt::Display for BattleReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "Battle {}×{} on {}: GPU frame {:.3} ms (frozen-frame throughput {:.3} + particle sim {:.3}), CPU render_frame {:.3} ms ({:.3} after an idle GPU), ticking frame {:.3} ms",
            self.size.0,
            self.size.1,
            self.adapter,
            self.gpu_frame_ms,
            self.frame_throughput_ms,
            self.particle_sim_ms,
            self.cpu_render_ms,
            self.cpu_render_idle_ms,
            self.ticking_frame_ms
        )?;
        writeln!(
            f,
            "  {} draws ({} shadow), {} render passes, {} compute passes, {} views, {} post groups, targets {:.1} MB, {} particles alive ({} draws, {} dispatches), {} point lights, {} GPU errors",
            self.stats.draw_calls,
            self.stats.shadow_draw_calls,
            self.stats.render_passes,
            self.stats.compute_passes,
            self.stats.views,
            self.stats.post_groups,
            self.stats.target_bytes as f64 / 1e6,
            self.particles.alive,
            self.particles.draw_calls,
            self.particles.dispatches,
            self.point_lights,
            self.gpu_errors
        )?;
        writeln!(
            f,
            "  a ticking frame uploads {:.1} KiB ({:.1} KiB particles)",
            self.ticking_upload_bytes.0 as f64 / 1024.0,
            self.ticking_upload_bytes.1 as f64 / 1024.0
        )?;
        write!(f, "  scopes (sum {:.3} ms):", self.scopes_total_ms)?;
        for (label, ms) in &self.scopes {
            write!(f, " {label} {ms:.3}")?;
        }
        Ok(())
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn wait_idle(renderer: &Renderer) {
    let _ = renderer.device().poll(wgpu::PollType::wait_indefinitely());
}

/// Warm the battle up (`warmup` ticking frames), then measure `frames` frames of each kind:
/// ticking frames back to back with GPU timestamps (the loaded GPU), `render_frame`'s CPU time
/// in ticking frames back to back and after an idle GPU, and the frozen frame back to back
/// (its throughput).
pub fn measure_battle(app: &mut App, warmup: u32, frames: u32) -> BattleReport {
    let mut report = BattleReport::default();
    {
        let r = app.resources.expect::<Renderer>();
        report.size = r.size();
        report.adapter = format!(
            "{} ({:?})",
            r.capabilities().adapter.name,
            r.capabilities().adapter.backend
        );
    }
    app.step_ticks(warmup as u64);
    // Ticking frames, back to back, with timestamps.
    let timestamps = app.resources.expect::<Renderer>().graphics_caps().timestamps;
    app.resources.expect_mut::<FrameSettings>().gpu_timing = timestamps;
    let mut labels: Vec<&'static str> = Vec::new();
    let mut samples: Vec<Vec<f64>> = Vec::new();
    let mut totals = Vec::new();
    let mut ticking = Vec::new();
    let mut sims = Vec::new();
    let mut uploads = Vec::new();
    for _ in 0..frames {
        let t = Instant::now();
        app.step_ticks(1);
        ticking.push(t.elapsed().as_secs_f64() * 1e3);
        uploads.push((
            app.resources.expect::<RenderStats>().bytes_uploaded,
            app.resources.expect::<ParticleStats>().bytes_uploaded,
        ));
        let timings = app.resources.expect::<GpuTimings>();
        if timings.frame.is_some() {
            totals.push(timings.total_ms as f64);
            sims.push(
                ["particles.sim", "particles.compact", "particles.sort"]
                    .iter()
                    .map(|l| timings.scope_ms(l) as f64)
                    .sum::<f64>(),
            );
            for s in &timings.scopes {
                let i = match labels.iter().position(|l| *l == s.label) {
                    Some(i) => i,
                    None => {
                        labels.push(s.label);
                        samples.push(Vec::new());
                        labels.len() - 1
                    }
                };
                samples[i].push(s.ms as f64);
            }
        }
    }
    app.resources.expect_mut::<FrameSettings>().gpu_timing = false;
    report.ticking_frame_ms = median(ticking);
    uploads.sort_unstable();
    report.ticking_upload_bytes = uploads.get(uploads.len() / 2).copied().unwrap_or_default();
    report.scopes_total_ms = median(totals);
    report.particle_sim_ms = median(sims);
    report.scopes = labels.into_iter().zip(samples).map(|(l, s)| (l, median(s))).collect();
    // `render_frame`'s CPU cost: ticking frames back to back (a tick of simulation between
    // frames, as in a game loop), and each after an idle GPU.
    for idle in [false, true] {
        let mut cpu = Vec::new();
        for _ in 0..frames {
            let mut renderer = app.resources.remove::<Renderer>().expect("renderer");
            app.step_ticks(1);
            if idle {
                wait_idle(&renderer);
            }
            let t = Instant::now();
            renderer.render_frame(&app.world, &mut app.resources);
            cpu.push(t.elapsed().as_secs_f64() * 1e3);
            app.resources.insert(renderer);
        }
        if idle {
            report.cpu_render_idle_ms = median(cpu);
        } else {
            report.cpu_render_ms = median(cpu);
        }
    }
    // The frozen frame back to back: throughput.
    let mut renderer = app.resources.remove::<Renderer>().expect("renderer");
    let mut rounds = Vec::new();
    for _ in 0..5 {
        wait_idle(&renderer);
        let n = frames.max(8);
        let t = Instant::now();
        for _ in 0..n {
            renderer.render_frame(&app.world, &mut app.resources);
        }
        wait_idle(&renderer);
        rounds.push(t.elapsed().as_secs_f64() * 1e3 / n as f64);
    }
    report.frame_throughput_ms = median(rounds);
    report.gpu_frame_ms = report.frame_throughput_ms + report.particle_sim_ms;
    report.stats = renderer.stats().clone();
    report.gpu_errors = renderer.gpu_error_count();
    app.resources.insert(renderer);
    report.particles = app.resources.expect::<ParticleStats>().clone();
    report.point_lights = app.world.query::<&PointLight>().iter().count();
    report
}
