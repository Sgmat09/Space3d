//! Process memory of the frame modes (M1.1 review: bloom's render passes hold Metal driver
//! memory that `RenderStats::target_bytes` does not count). macOS only (the process's
//! `phys_footprint`, which includes the GPU driver's IOAccelerator memory); ignored by default —
//! run alone:
//!
//! ```sh
//! cargo test -p space3d-render --release --test footprint -- --ignored --nocapture
//! ```
//!
//! Every scene runs in its own child process (the driver keeps caches across scenes): 1080p,
//! 600 frames, paced (one frame in flight: the GPU is waited for after each) or unpaced (the
//! renderer's cap of `MAX_FRAMES_IN_FLIGHT` frames in flight), then the footprint is read.
//! Raw wgpu scenes (no engine code) measure one empty render pass per frame against 11.
#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]

use space3d_core::prelude::*;
use space3d_render::*;

#[cfg(target_os = "macos")]
fn footprint_mb() -> f64 {
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
    }
    let mut b = [0u64; 64];
    // SAFETY: `b` is larger than `rusage_info_v4` (flavor 4); `ri_phys_footprint` is its 10th
    // 64-bit field (after the 16-byte UUID and 7 counters).
    let rc = unsafe { proc_pid_rusage(std::process::id() as i32, 4, b.as_mut_ptr()) };
    assert_eq!(rc, 0);
    b[9] as f64 / 1e6
}

const SCENES: [&str; 8] = [
    "direct",
    "hdr_minimal",
    "hdr_bloom",
    "hdr_bloom_unpaced",
    "hdr_minimal_unpaced",
    "depth_view",
    "raw_1_pass",
    "raw_11_passes",
];

fn scene_app(name: &str) -> Option<App> {
    let renderer = pollster::block_on(Renderer::new_headless(1920, 1080)).ok()?;
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    app.insert_resource(renderer);
    let (cube, plane) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(0.3)), a.add_mesh(MeshData::plane(40.0)))
    };
    app.world.spawn((
        Transform::from_xyz(0.0, -1.0, 0.0),
        Mesh3d(plane),
        Material::srgb(0.4, 0.42, 0.45),
    ));
    for i in 0..400u32 {
        app.world.spawn((
            Transform::from_xyz((i % 20) as f32 * 0.5 - 5.0, -0.7, (i / 20) as f32 * -0.5),
            Mesh3d(cube),
            Material::srgb(0.8, 0.4, 0.2).with_emissive(Vec3::splat((i % 7) as f32)),
        ));
    }
    let cam = app.world.spawn((
        Transform::from_xyz(0.0, 1.5, 4.0).looking_at(Vec3::new(0.0, -1.0, -4.0), Vec3::Y),
        Camera3d::default(),
    ));
    if name.starts_with("hdr_minimal") {
        app.world
            .insert_one(cam, PostProcess::default().without_bloom())
            .unwrap();
    } else if name.starts_with("hdr_bloom") {
        app.world.insert_one(cam, PostProcess::default()).unwrap();
    } else if name == "depth_view" {
        let view = glam::camera::rh::view::look_at_mat4(Vec3::new(3.0, 10.0, 6.0), Vec3::new(0.0, -1.0, -5.0), Vec3::Y);
        let proj = glam::camera::rh::proj::directx::orthographic(-12.0, 12.0, -12.0, 12.0, 0.0, 40.0);
        space3d_render::testing::add_depth_view(
            app.resources.expect_mut::<Renderer>(),
            proj * view,
            2048,
            Default::default(),
        );
    }
    space3d_core::transform::propagate_transforms(&mut app.world);
    Some(app)
}

fn wait(device: &wgpu::Device) {
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
}

/// Raw wgpu: `passes` empty clear passes into a 32×32 `Rgba16Float` texture per command
/// buffer, 600 frames, one frame in flight.
#[cfg(target_os = "macos")]
fn raw(passes: usize) -> String {
    let r = pollster::block_on(Renderer::new_headless(32, 32)).expect("adapter");
    let device = r.device().clone();
    let queue = r.queue().clone();
    drop(r);
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: 32,
            height: 32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    for _ in 0..600 {
        let mut encoder = device.create_command_encoder(&Default::default());
        for _ in 0..passes {
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        queue.submit([encoder.finish()]);
        wait(&device);
    }
    format!("footprint {:.1} MB", footprint_mb())
}

/// One scene (the child process's job): its footprint after 600 frames.
#[cfg(target_os = "macos")]
fn measure(name: &str) -> String {
    if let Some(n) = name.strip_prefix("raw_") {
        return raw(if n.starts_with('1') && !n.starts_with("11") {
            1
        } else {
            11
        });
    }
    let mut app = scene_app(name).expect("adapter");
    let unpaced = name.ends_with("_unpaced");
    let t = std::time::Instant::now();
    for _ in 0..600 {
        app.resources.expect_mut::<Time>().frame += 1;
        app.run_stage(Stage::Render);
        if !unpaced {
            wait(app.resources.expect::<Renderer>().device());
        }
    }
    let s = app.resources.expect::<RenderStats>().clone();
    format!(
        "footprint {:.1} MB, target_bytes {:.1} MB, render passes {}, {} frames in {:.2} s",
        footprint_mb(),
        s.target_bytes as f64 / 1e6,
        s.render_passes,
        s.frame,
        t.elapsed().as_secs_f64()
    )
}

#[test]
#[ignore = "process memory: run alone with --ignored --nocapture (macOS)"]
fn frame_mode_footprints() {
    #[cfg(target_os = "macos")]
    {
        if let Ok(scene) = std::env::var("SPACE3D_FOOTPRINT_SCENE") {
            println!("\nRESULT {}", measure(&scene));
            return;
        }
        if pollster::block_on(Renderer::new_headless(4, 4)).is_err() {
            eprintln!("skipping: no adapter");
            return;
        }
        let exe = std::env::current_exe().expect("test binary");
        let mut results = Vec::new();
        for scene in SCENES {
            let out = std::process::Command::new(&exe)
                .args([
                    "frame_mode_footprints",
                    "--exact",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("SPACE3D_FOOTPRINT_SCENE", scene)
                .output()
                .expect("child test process");
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text
                .lines()
                .find_map(|l| l.split_once("RESULT ").map(|(_, r)| r))
                .unwrap_or("failed")
                .to_string();
            println!("{scene:<20} {line}");
            results.push((scene, line));
        }
        let mb = |scene: &str| -> f64 {
            let line = &results.iter().find(|(s, _)| *s == scene).expect("scene").1;
            line.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(f64::NAN)
        };
        let per_pass = (mb("raw_11_passes") - mb("raw_1_pass")) / 10.0;
        let bloom = mb("hdr_bloom") - mb("hdr_minimal");
        let bloom_unpaced = mb("hdr_bloom_unpaced") - mb("hdr_minimal_unpaced");
        println!(
            "per render pass (raw wgpu, 1 frame in flight): {per_pass:.2} MB; bloom over the composite: \
             {bloom:.1} MB paced, {bloom_unpaced:.1} MB unpaced; depth view over Direct: {:.1} MB",
            mb("depth_view") - mb("direct")
        );
        // The footprint check (graphics spec §4.4 VRAM, amended at M1.1): the default bloom
        // chain of one 1080p post group within 80 MB paced and 200 MB with three frames in
        // flight (measured 56–61 / 151 at M1 on the dev machine).
        assert!(bloom < 80.0, "bloom holds {bloom:.1} MB paced");
        assert!(bloom_unpaced < 200.0, "bloom holds {bloom_unpaced:.1} MB unpaced");
    }
}
