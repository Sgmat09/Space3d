//! Steady-state heap allocations of the render stage with every material feature of PM1/PM2
//! in use: normal / ORM / emissive maps, UV transforms, the second UV set and vertex tangents
//! (extension stream), alpha masks, additive and premultiplied blending, and the per-instance
//! transparent sort of a moving alpha-blended batch. The materials area's per-frame work
//! allocates nothing (`RenderStats::cpu_reallocations == 0`, texture sets and their bind
//! groups built once); what remains is wgpu's bookkeeping, pinned by a measured bound and
//! checked for growth, as in `tests/alloc.rs`.
//!
//! It is the only test in this binary: the counter is global.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);

// SAFETY: forwards to the system allocator; the counters are plain atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE_BYTES.fetch_add(l.size() as i64, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE_BYTES.fetch_add(l.size() as i64, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE_BYTES.fetch_add(n as i64 - l.size() as i64, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE_BYTES.fetch_sub(l.size() as i64, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Upper bound of allocations per steady-state frame of this scene: measured on Apple M4 Max /
/// Metal (wgpu 30.0.1, test profile) plus ~10 %: 100 on the CPU path, 195 on the GPU culling
/// paths, the same every frame (wgpu and wgpu-hal bookkeeping: see `tests/alloc.rs`). Other
/// backends get twice the headroom; the no-growth checks are backend-independent.
fn bound(path: DrawPath, backend: wgpu::Backend) -> u64 {
    let metal = if path.gpu_culling() { 215 } else { 110 };
    if backend == wgpu::Backend::Metal {
        metal
    } else {
        2 * metal
    }
}

const WARM_UP: usize = 30;
const FRAMES: usize = 90;

#[test]
fn material_features_allocate_nothing_per_frame() {
    let Some(mut app) = headless_app(256, 256) else { return };
    let backend = app.resources.expect::<Renderer>().capabilities().adapter.backend;
    app.resources.expect_mut::<LightingSettings>().transparent_sort =
        TransparentSort::PerInstance { max_instances: 1024 };
    let a = app.resources.expect_mut::<RenderAssets>();
    let cube = a.add_mesh(MeshData::cube(0.05));
    let tangents: Vec<[f32; 4]> = (0..24).map(|_| [1.0, 0.0, 0.0, 1.0]).collect();
    let ext_cube = a.add_mesh_with(
        MeshData::cube(0.05),
        MeshExtras::new()
            .with_tangents(tangents)
            .with_uvs1(vec![[0.5, 0.5]; 24]),
    );
    let quad = a.add_mesh(MeshData::plane(0.05));
    let normal = a.add_image_linear(ImageData::filled(8, 8, [140, 120, 240, 255]));
    let orm = a.add_image_linear(ImageData::filled(8, 8, [200, 128, 255, 255]));
    let glow = a.add_image(ImageData::checker(8, 2, [255, 128, 0, 255], [0, 0, 0, 255]));
    let mask = a.add_image(ImageData::checker(8, 2, [255, 255, 255, 255], [255, 255, 255, 0]));
    let pbr = Material::srgb(0.6, 0.6, 0.65)
        .with_normal_map(normal, 1.0)
        .with_orm_map(orm)
        .with_emissive_map(glow, Vec3::splat(2.0))
        .with_uv_transform(TextureTransform {
            scale: Vec2::splat(2.0),
            ..TextureTransform::IDENTITY
        });
    let materials = [
        pbr.clone(),
        pbr.clone().with_uv1_maps(MapMask::EMISSIVE),
        Material::srgb(0.9, 0.9, 0.9).with_texture(mask).with_alpha_mask(0.5),
        Material::additive(Vec4::new(1.0, 0.5, 0.2, 0.6)),
        Material::premultiplied(Vec4::new(0.2, 0.3, 0.4, 0.5)),
    ];
    let mut movers = Vec::new();
    for i in 0..10_000u32 {
        let (mesh, material) = match i % 5 {
            1 => (ext_cube, &materials[1]),
            k => (cube, &materials[k as usize]),
        };
        let p = Vec3::new((i % 100) as f32 * 0.1 - 5.0, (i / 100) as f32 * 0.1 - 5.0, 0.0);
        movers.push(
            app.world
                .spawn((Transform::from_xyz(p.x, p.y, p.z), Mesh3d(mesh), material.clone())),
        );
    }
    // A sorted alpha-blended batch at scattered depths.
    let alpha = Material::srgb(0.3, 0.7, 0.9).with_blend(BlendMode::AlphaBlend);
    for i in 0..1000u32 {
        let z = ((i.wrapping_mul(2_654_435_761) >> 16) % 50) as f32 * -0.05;
        movers.push(app.world.spawn((
            Transform::from_xyz((i % 40) as f32 * 0.12 - 2.4, (i / 40) as f32 * 0.12 - 1.5, z),
            Mesh3d(quad),
            alpha.clone(),
        )));
    }
    // Explicit instances with a normal-mapped material (ext stream, instance layout).
    let data = (0..2000)
        .map(|i| InstanceCompact::new(Vec3::new(i as f32 * 0.003 - 3.0, 2.0, -1.0), Quat::IDENTITY, 1.0))
        .collect();
    let instances = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, BufferUsage::Static));
    app.world.spawn((InstancedMesh {
        mesh: ext_cube,
        material: pbr.clone(),
        instances,
    },));
    let camera = spawn_camera(&mut app, 12.0);
    spawn_sun(&mut app);

    let mut frame = 0u32;
    let mut step = |app: &mut App| -> (u64, u32) {
        frame += 1;
        let dy = if frame.is_multiple_of(2) { 0.01 } else { -0.01 };
        for &e in &movers {
            app.world.get::<&mut Transform>(e).unwrap().translation.y += dy;
        }
        // The camera moves too: the sort order changes.
        app.world.get::<&mut Transform>(camera).unwrap().translation.x = dy * 30.0;
        space3d_core::transform::propagate_transforms(&mut app.world);
        let a0 = ALLOCS.load(Relaxed);
        app.run_stage(Stage::Render);
        let counted = (
            ALLOCS.load(Relaxed) - a0,
            app.resources.expect::<RenderStats>().staging_chunks_created,
        );
        let _ = app
            .resources
            .expect::<Renderer>()
            .device()
            .poll(wgpu::PollType::wait_indefinitely());
        counted
    };

    let mut per_frame = Vec::with_capacity(FRAMES);
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        for _ in 0..WARM_UP {
            step(&mut app);
        }
        let live_before = LIVE_BYTES.load(Relaxed);
        per_frame.clear();
        let mut chunks = 0;
        let mut textures = 0;
        for _ in 0..FRAMES {
            let (allocs, created) = step(&mut app);
            per_frame.push(allocs);
            chunks += created;
            textures += stats(&app).textures_created;
        }
        let live_growth = LIVE_BYTES.load(Relaxed) - live_before;
        let s = stats(&app);
        assert_eq!((s.cpu_reallocations, s.buffer_reallocations), (0, 0), "{path:?}: {s:?}");
        assert_eq!(textures, 0, "{path:?}: textures created in the steady state");
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
        let max = *per_frame.iter().max().unwrap();
        let (first, second) = per_frame.split_at(FRAMES / 2);
        let mean = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
        eprintln!(
            "{path:?} on {backend:?}: allocations per frame min {} max {max} mean {:.1}; live bytes \
             {live_growth:+} over {FRAMES} frames ({} batches, {} draws, {} pipeline binds)",
            per_frame.iter().min().unwrap(),
            mean(&per_frame),
            s.batches,
            s.draw_calls,
            s.pipeline_switches,
        );
        assert_eq!(chunks, 0, "{path:?}: the upload ring created chunks in a paced loop");
        let bound = bound(path, backend);
        assert!(
            max <= bound,
            "{path:?}: up to {max} allocations per frame (bound {bound})"
        );
        assert!(
            mean(second) <= mean(first) + 1.0,
            "{path:?}: allocations grow over time: {:.1} → {:.1}",
            mean(first),
            mean(second)
        );
        assert!(
            live_growth < 256 << 10,
            "{path:?}: {live_growth} bytes still allocated after {FRAMES} frames"
        );
    }
}
