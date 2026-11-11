//! Frame tests (moved from `frame.rs`).

use super::draws::*;
use super::extract::*;
use super::plan::*;
use super::targets::*;
use super::views::*;
use super::*;
use crate::arena::ArenaAlloc;
use crate::assets::{GpuMesh, RenderAssets};
use crate::components::Material;
use crate::culling::Sphere;
use crate::gpu::{TrackedBuffer, align_up};
use crate::handles::MeshHandle;
use crate::instance::{CullSource, InstanceBuffers};
use crate::instance::{InstanceCompact, InstanceData};
use crate::pipeline::{CLASS_BLEND, GpuCompactParams, OFFSCREEN_FORMAT};
use crate::renderer::Renderer;
use crate::stats::DrawPath;
use glam::{Mat4, Vec3, Vec4};

#[test]
fn material_keys_intern() {
    let a = MaterialKey::new(&Material::default());
    let b = MaterialKey::new(&Material::default());
    let c = MaterialKey::new(&Material::srgb(1.0, 0.0, 0.0));
    assert!(a == b && a != c);
}

#[test]
fn surface_lost_is_recreated_not_reconfigured() {
    use wgpu::CurrentSurfaceTexture as C;
    assert!(matches!(
        SurfaceAcquire::from_current(C::Lost),
        SurfaceAcquire::Recreate
    ));
    assert!(matches!(
        SurfaceAcquire::from_current(C::Outdated),
        SurfaceAcquire::Reconfigure
    ));
    assert!(matches!(SurfaceAcquire::from_current(C::Timeout), SurfaceAcquire::Skip));
    assert!(matches!(
        SurfaceAcquire::from_current(C::Occluded),
        SurfaceAcquire::Skip
    ));
}

fn headless_app(w: u32, h: u32) -> Option<space3d_core::App> {
    let mut app = space3d_core::App::new();
    app.add_plugin(crate::RenderPlugin::default());
    match pollster::block_on(Renderer::new_headless(w, h)) {
        Ok(r) => {
            app.insert_resource(r);
            Some(app)
        }
        Err(e) => {
            eprintln!("skipping GPU test: no adapter available ({e})");
            None
        }
    }
}

fn set_path(app: &mut space3d_core::App, path: DrawPath) {
    app.resources.expect_mut::<RenderSettings>().draw_path = Some(path);
}

fn differing_px(a: &space3d_core::ImageData, b: &space3d_core::ImageData) -> usize {
    a.pixels
        .chunks(4)
        .zip(b.pixels.chunks(4))
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 2))
        .count()
}

/// Regression: `cull_out` accumulates every GPU-culled source of every view but is bound as
/// one storage binding; regions past `max_storage_buffer_binding_size` (128 MiB on many
/// mobile GPUs and WebGPU) were silently dropped while the draws still counted them, and
/// the ECS source was never checked. Such sources now fall back to CPU culling.
#[test]
fn sources_past_the_storage_binding_limit_fall_back_to_cpu_culling() {
    use crate::{Camera3d, InstanceBuffer, InstanceTransform, InstancedMesh, Mesh3d};
    use space3d_core::{MeshData, Transform};
    let Some(mut app) = headless_app(96, 64) else { return };
    let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
    if best == DrawPath::CpuDirect {
        return;
    }
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.3));
    // ECS: 1000 matrices = 64000 B.
    for i in 0..1000 {
        let p = Vec3::new((i % 40) as f32 * 0.25 - 5.0, (i / 40) as f32 * 0.25 - 3.0, 0.0);
        app.world.spawn((
            Transform::from_xyz(p.x, p.y, p.z),
            Mesh3d(mesh),
            Material::srgb(0.9, 0.4, 0.1),
        ));
    }
    // Two explicit buffers of 400 × 64 B = 25600 B each.
    for (k, y) in [(0, 1.0), (1, -1.5)] {
        let data = (0..400)
            .map(|i| {
                InstanceTransform::from_trs(
                    Vec3::new((i % 40) as f32 * 0.25 + 0.5, y + (i / 40) as f32 * 0.2, -1.0),
                    glam::Quat::from_rotation_y(i as f32),
                    Vec3::ONE,
                )
            })
            .collect();
        let instances = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, crate::BufferUsage::Static));
        app.world.spawn((InstancedMesh {
            mesh,
            material: Material::srgb(0.2, 0.5 + 0.3 * k as f32, 0.9),
            instances,
        },));
    }
    // Two views of the same scene (left and right half).
    for (x, order) in [(0.0, 0), (0.5, 1)] {
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 12.0),
            Camera3d {
                viewport: Some([x, 0.0, 0.5, 1.0]),
                order,
                ..Camera3d::default().with_clear_color(glam::Vec4::new(0.0, 0.0, 1.0, 1.0))
            },
        ));
    }
    set_path(&mut app, DrawPath::CpuDirect);
    let cpu = crate::render_to_image(&mut app).expect("capture");
    // Only 32 KiB per storage binding: each explicit buffer fits on its own, the ECS
    // batch does not, and neither does the sum of all regions across the two views.
    app.resources.expect_mut::<Renderer>().frame.max_storage_binding = 32 << 10;
    set_path(&mut app, best);
    let limited = crate::render_to_image(&mut app).expect("capture");
    let d = differing_px(&cpu, &limited);
    assert_eq!(d, 0, "{best:?} with a 32 KiB binding differs in {d} px");
    let f = &app.resources.expect::<Renderer>().frame;
    assert_eq!(
        f.jobs.len(),
        1,
        "only the first explicit buffer of view 1 is GPU-culled"
    );
    assert!(app.resources.expect::<crate::RenderStats>().compute_dispatches > 0);
}

/// Regression: culling ran one mark and one scatter dispatch per (view, source buffer), so
/// a scene with 120 small buffers and 13 views took 3121 dispatches (and wgpu-hal's Metal
/// backend allocates in each). The jobs of all views reading one buffer now share a
/// dispatch, each job selecting its view's frustum: 70 views (two windows of frustums) ×
/// 74 buffers take 2 × 2 × 74 + 1 dispatches (not 70 × 74 × 2).
///
/// Every one of the 70 viewports is checked against what its own camera sees — its own
/// clear color, its ECS cube, its own uniquely colored instance in a shared explicit buffer,
/// its own marker cube in a single-instance explicit buffer, and nothing else — on every
/// draw path. A view culled with another view's frustum (on the CPU or the GPU), drawn with
/// another view's matrix or wiped by a later viewport fails.
///
/// The marker buffers are what catches a wrong frustum in the CPU culling of *explicit*
/// buffers: that path culls whole blocks of `CULL_BLOCK` instances, and each shared
/// buffer's single block spans every view, so it is drawn in every view whatever frustum
/// tests it. A marker buffer's block holds only its view's cube: tested against another
/// view's frustum it is culled (the marker vanishes), and skipping the test altogether
/// changes the exact `culled` count checked on `CpuDirect`.
#[test]
fn culling_dispatches_scale_with_buffers_not_views() {
    use crate::components::srgb_to_linear;
    use crate::{Camera3d, InstanceBuffer, InstanceTransformColor, InstancedMesh, Mesh3d, RenderStats};
    use space3d_core::{ImageData, MeshData, Transform};
    const VIEWS: usize = 70;
    const ECS: usize = 300;
    const W: u32 = 140;
    const H: u32 = 100;
    let Some(mut app) = headless_app(W, H) else { return };
    let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
    if best == DrawPath::CpuDirect {
        return;
    }
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.2));
    // View v's camera looks at `center(v)` from 4 units away (its frustum spans ±2.3 units
    // there); the next view's point is 10 units away, far outside it.
    let center = |v: usize| Vec3::new((v % 10) as f32 * 10.0, (v / 10) as f32 * 10.0, 0.0);
    // Seen by no view.
    let nowhere = |i: usize| Vec3::new(-1000.0 - i as f32 * 2.0, 0.0, 0.0);
    // sRGB colors of view v: its clear color, its shared-buffer instance and its marker.
    let clear_rgb = |v: usize| [(v % 10) as u8 * 6, (v / 10) as u8 * 6, 60];
    let own_rgb = |v: usize| [40 + (v % 10) as u8 * 20, 40 + (v / 10) as u8 * 30, 200];
    let mark_rgb = |v: usize| [255, 120 + (v % 10) as u8 * 12, (v / 10) as u8 * 20];
    let linear = |c: [u8; 3]| c.map(|x| srgb_to_linear(x as f32 / 255.0));
    let linear4 = |c: [u8; 3]| Vec3::from(linear(c)).extend(1.0);
    // ECS: a white cube left of each view's point, and cubes no view sees.
    for i in 0..ECS {
        let p = if i < VIEWS {
            center(i) - Vec3::X * 0.9
        } else {
            nowhere(i)
        };
        app.world.spawn((
            Transform::from_xyz(p.x, p.y, p.z),
            Mesh3d(mesh),
            Material::unlit(Vec4::ONE),
        ));
    }
    let instanced = |app: &mut space3d_core::App, data: Vec<InstanceTransformColor>| {
        let instances = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, crate::BufferUsage::Static));
        app.world.spawn((InstancedMesh {
            mesh,
            material: Material::unlit(Vec4::ONE),
            instances,
        },));
    };
    // Three explicit buffers of 200 instances: view v's cube, right of its point and in its
    // own color, is instance 8 × (v / 3) of buffer v % 3 (in several culling workgroups);
    // the other instances are seen by no view.
    for k in 0..3 {
        let data = (0..200)
            .map(|i| {
                let v = i / 8 * 3 + k;
                let (p, rgb) = if i % 8 == 0 && v < VIEWS {
                    (center(v) + Vec3::X * 0.9, own_rgb(v))
                } else {
                    (nowhere(ECS + 200 * k + i), [255, 255, 255])
                };
                InstanceTransformColor::new(Mat4::from_translation(p), linear4(rgb))
            })
            .collect();
        instanced(&mut app, data);
    }
    // One single-instance buffer per view: its marker cube, above its point (clear of the
    // other two cubes).
    for v in 0..VIEWS {
        let p = center(v) + Vec3::Y * 1.25;
        instanced(
            &mut app,
            vec![InstanceTransformColor::new(
                Mat4::from_translation(p),
                linear4(mark_rgb(v)),
            )],
        );
    }
    // A 10 × 7 grid of viewports (14 × 14.3 px), each with its own clear color.
    for v in 0..VIEWS {
        let (col, row) = ((v % 10) as f32, (v / 10) as f32);
        let c = center(v);
        app.world.spawn((
            Transform::from_xyz(c.x, c.y, 4.0),
            Camera3d {
                viewport: Some([col / 10.0, row / 7.0, 0.1, 1.0 / 7.0]),
                order: v as i32,
                ..Camera3d::default().with_clear_color(linear4(clear_rgb(v)))
            },
        ));
    }
    // Inside each viewport (1 px margin), every pixel is the view's clear color, its white
    // ECS cube, its own instance or its marker, and each of the four is visible.
    let check = |img: &ImageData, path: DrawPath| {
        for v in 0..VIEWS {
            let (col, row) = ((v % 10) as u32, (v / 10) as u32);
            let cell_h = H as f32 / 7.0;
            let ys = (row as f32 * cell_h).ceil() as u32 + 1..((row + 1) as f32 * cell_h).floor() as u32 - 1;
            let expected = [clear_rgb(v), [255, 255, 255], own_rgb(v), mark_rgb(v)];
            let mut seen = [0usize; 4];
            for y in ys {
                for x in col * 14 + 1..col * 14 + 13 {
                    let p = img.get(x, y);
                    let is = |c: &[u8; 3]| p.iter().zip(c).all(|(a, b)| a.abs_diff(*b) <= 2);
                    match expected.iter().position(is) {
                        Some(i) => seen[i] += 1,
                        None => {
                            panic!("{path:?}: viewport {v} shows {p:?} at ({x}, {y}), expected one of {expected:?}")
                        }
                    }
                }
            }
            assert!(
                seen[0] >= 40 && seen[1] >= 6 && seen[2] >= 6 && seen[3] >= 6,
                "{path:?}: viewport {v}: clear color / ECS cube / own instance / marker cover {seen:?} px"
            );
        }
    };
    set_path(&mut app, DrawPath::CpuDirect);
    let cpu = crate::render_to_image(&mut app).expect("capture");
    check(&cpu, DrawPath::CpuDirect);
    // Each view sees one ECS cube and one marker buffer; the shared buffers' blocks span
    // every view and are never culled.
    let stats = app.resources.expect::<RenderStats>();
    assert_eq!(
        stats.culled,
        (VIEWS * (ECS - 1) + VIEWS * (VIEWS - 1)) as u64,
        "CPU culling of every view with its own frustum"
    );
    assert_eq!(stats.instances_drawn, (VIEWS * (1 + 3 * 200 + 1)) as u64);
    for path in paths_up_to(best).skip(1) {
        set_path(&mut app, path);
        let img = crate::render_to_image(&mut app).expect("capture");
        check(&img, path);
        let d = differing_px(&cpu, &img);
        assert_eq!(d, 0, "{path:?} differs from CpuDirect in {d} px");
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.gpu_error_count(), 0, "{path:?}");
        let buffers = 1 + 3 + VIEWS;
        assert_eq!(r.frame.jobs.len(), VIEWS * buffers, "{path:?}: every source GPU-culled");
        let compact = u32::from(path == DrawPath::GpuMultiDrawCount);
        assert_eq!(
            app.resources.expect::<RenderStats>().compute_dispatches,
            2 * 2 * buffers as u32 + 1 + compact,
            "{path:?}: mark + scatter per buffer and frustum window, one scan"
        );
    }
}

/// Viewport rects are whole pixels with rounded edges (neighbours share edges exactly);
/// scissors are their part within the target, at least one pixel.
#[test]
fn viewport_rects_and_scissors() {
    let rect = |v, size| {
        let r = viewport_rect(v, size);
        (r.x, r.y, r.w, r.h)
    };
    assert_eq!(rect([0.25, 0.25, 0.375, 0.5], (8, 4)), (2, 1, 3, 2));
    // Thirds of 100 px: 0..33, 33..67, 67..100.
    let thirds: Vec<_> = (0..3)
        .map(|i| rect([i as f32 / 3.0, 0.0, 1.0 / 3.0, 1.0], (100, 10)))
        .collect();
    assert_eq!(thirds, [(0, 0, 33, 10), (33, 0, 34, 10), (67, 0, 33, 10)]);
    // Sevenths of 100 px (the rows of `culling_dispatches_scale_with_buffers_not_views`).
    let rows: Vec<_> = (0..7)
        .map(|i| rect([0.0, i as f32 / 7.0, 0.1, 1.0 / 7.0], (140, 100)))
        .map(|(_, y, _, h)| (y, h))
        .collect();
    assert_eq!(
        rows,
        [(0, 14), (14, 15), (29, 14), (43, 14), (57, 14), (71, 15), (86, 14)]
    );
    let r = viewport_rect([0.25, 0.25, 0.375, 0.5], (8, 4));
    assert_eq!(r.scissor((8, 4)), [2, 1, 3, 2]);
    assert!(!r.covers((8, 4)));
    assert!(viewport_rect([0.0, 0.0, 1.0, 1.0], (8, 4)).covers((8, 4)));
    assert!(viewport_rect([-0.1, 0.0, 1.2, 1.0], (8, 4)).covers((8, 4)));
    // Degenerate and out-of-range viewports: at least one pixel, scissor within the target.
    assert_eq!(rect([0.5, 0.5, 0.0, 0.0], (10, 10)), (5, 5, 1, 1));
    // Thinner than a pixel: the pixel holding the center, inside the target at its edges
    // (the far edge used to grow past the target: x = 97, nothing drawn).
    assert_eq!(rect([0.999, 0.0, 0.001, 1.0], (97, 61)), (96, 0, 1, 61));
    assert_eq!(rect([0.0, 0.999, 1.0, 0.001], (97, 61)), (0, 60, 97, 1));
    assert_eq!(rect([0.0, 0.0, 1e-6, 1e-6], (97, 61)), (0, 0, 1, 1));
    assert_eq!(rect([0.9999, 0.0, 0.001, 1.0], (97, 61)), (96, 0, 1, 61));
    assert_eq!(rect([0.52, 0.0, 0.001, 1.0], (10, 10)), (5, 0, 1, 10));
    assert_eq!(rect([0.3, 0.0, -0.1, 1.0], (10, 10)), (2, 0, 1, 10));
    // Entirely outside the target, it stays outside (nothing is drawn).
    assert_eq!(rect([1.0, 1.0, 0.0, 0.0], (10, 10)), (10, 10, 1, 1));
    assert_eq!(rect([-0.5, 0.0, 0.01, 1.0], (10, 10)), (-5, 0, 1, 10));
    // Viewports within 0..1 stay within the target.
    for size in [(1, 1), (2, 3), (97, 61), (1920, 1080)] {
        for x in [0.0f32, 0.1, 0.5, 0.97, 0.999, 0.9999, 1.0 - f32::EPSILON] {
            for w in [0.0f32, 1e-6, 1e-4, 1e-3, 0.01, 0.5, 1.0]
                .into_iter()
                .filter(|w| x + w <= 1.0)
            {
                let r = viewport_rect([x, 0.0, w, 1.0], size);
                let [sx, _, sw, _] = r.scissor(size);
                assert!(r.x >= 0 && r.x as u32 + r.w <= size.0, "{x} {w} {size:?}: {r:?}");
                assert_eq!((sx, sw), (r.x as u32, r.w), "{x} {w} {size:?}");
            }
        }
    }
    let outside = viewport_rect([1.0, 1.0, 0.5, 0.5], (10, 10));
    assert_eq!((outside.x, outside.w), (10, 5));
    assert_eq!(outside.scissor((10, 10)), [9, 9, 1, 1]);
    let wide = viewport_rect([-0.5, 0.0, 2.0, 1.0], (10, 10));
    assert_eq!((wide.x, wide.w), (-5, 20));
    assert_eq!(wide.scissor((10, 10)), [0, 0, 10, 10]);
}

/// Split screen for the surface-loss tests: a green cube in the left camera's view (red
/// clear color), a magenta one in the right camera's (blue).
fn split_screen_app(w: u32, h: u32, samples: u32) -> Option<space3d_core::App> {
    use crate::{Camera3d, Mesh3d};
    use space3d_core::{MeshData, Transform};
    let mut app = headless_app(w, h)?;
    app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
    let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(2.0));
    for (order, (x, color, vp, clear)) in [
        (-50.0, Vec4::new(0.0, 1.0, 0.0, 1.0), 0.0, Vec4::new(1.0, 0.0, 0.0, 1.0)),
        (50.0, Vec4::new(1.0, 0.0, 1.0, 1.0), 0.5, Vec4::new(0.0, 0.0, 1.0, 1.0)),
    ]
    .into_iter()
    .enumerate()
    {
        app.world
            .spawn((Transform::from_xyz(x, 0.0, 0.0), Mesh3d(cube), Material::unlit(color)));
        app.world.spawn((
            Transform::from_xyz(x, 0.0, 6.0),
            Camera3d {
                order: order as i32,
                viewport: Some([vp, 0.0, 0.5, 1.0]),
                clear_color: Some(clear),
                ..Default::default()
            },
        ));
    }
    Some(app)
}

/// Regression: when the presentation surface was lost after a frame's views had been
/// prepared for it (`SurfaceAcquire::Recreate`), that frame drew its primary views into
/// the offscreen target, which a windowed renderer sizes only for captures — it could
/// still have the window's first size. After the window had grown, the right viewport's
/// scissor rect lay outside it: a validation error that lost the frame's command encoder,
/// uploads included (a panic in debug builds). The frame now renders offscreen at the size
/// its views were prepared for, on every draw path, like the frames after it.
#[test]
fn a_surface_lost_mid_frame_renders_offscreen_at_the_prepared_size() {
    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];
    const MAGENTA: [u8; 3] = [255, 0, 255];
    for samples in [1, 4] {
        let Some(mut app) = split_screen_app(64, 48, samples) else {
            return;
        };
        let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
        let mut reference = None;
        for path in [
            DrawPath::CpuDirect,
            DrawPath::GpuIndirect,
            DrawPath::GpuMultiDraw,
            DrawPath::GpuMultiDrawCount,
        ] {
            if path > best {
                continue;
            }
            set_path(&mut app, path);
            {
                let r = app.resources.expect_mut::<Renderer>();
                r.frame.simulate_surface_loss = false;
                r.resize(64, 48);
            }
            let img = crate::render_to_image(&mut app).expect("capture");
            assert_eq!((img.width, img.height), (64, 48));
            {
                // The window grows; its surface is lost at the next frame's acquire.
                let r = app.resources.expect_mut::<Renderer>();
                r.frame.simulate_surface_loss = true;
                r.resize(128, 96);
                assert_eq!((r.offscreen.width, r.offscreen.height), (64, 48));
            }
            let img = crate::render_to_image(&mut app).expect("capture");
            let r = app.resources.expect::<Renderer>();
            assert!(!r.frame.simulate_surface_loss, "the lost-surface path ran");
            assert_eq!(r.gpu_error_count(), 0, "{path:?} ×{samples}");
            assert_eq!((img.width, img.height), (128, 96), "{path:?} ×{samples}");
            let n = |xs: std::ops::Range<u32>, rgb: [u8; 3]| {
                xs.flat_map(|x| (0..96).map(move |y| (x, y)))
                    .filter(|&(x, y)| img.get(x, y)[..3] == rgb)
                    .count()
            };
            assert!(
                n(0..64, RED) > 3000 && n(0..64, GREEN) > 300,
                "{path:?} ×{samples}: left"
            );
            assert!(
                n(64..128, BLUE) > 3000 && n(64..128, MAGENTA) > 300,
                "{path:?} ×{samples}: right"
            );
            assert_eq!(
                n(0..64, BLUE) + n(0..64, MAGENTA) + n(64..128, RED) + n(64..128, GREEN),
                0
            );
            match &reference {
                None => reference = Some(img),
                Some(cpu) => assert_eq!(differing_px(cpu, &img), 0, "{path:?} ×{samples} differs from CpuDirect"),
            }
        }
    }
}

/// A pass's scissor rects stay within its attachment even if the attachment does not have
/// the size the view was prepared for (which `surface_lost` prevents): a scissor rect
/// outside it would fail validation and lose the frame's commands.
#[test]
fn scissor_rects_stay_within_the_attachment() {
    for samples in [1, 4] {
        let Some(mut app) = split_screen_app(64, 48, samples) else {
            return;
        };
        crate::render_to_image(&mut app).expect("capture");
        let r = app.resources.expect_mut::<Renderer>();
        assert_eq!(r.frame.last_primary_size, (64, 48));
        // Re-record the frame's primary views into a smaller target.
        let samples = r.config.msaa_samples;
        r.offscreen = crate::renderer::Attachments::new(&r.device, 40, 30, samples, OFFSCREEN_FORMAT, true, false);
        let scope = r.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut encoder = r.device.create_command_encoder(&Default::default());
        r.encode_views(&mut encoder, None, true);
        r.queue.submit([encoder.finish()]);
        let error = pollster::block_on(scope.pop());
        assert!(error.is_none(), "×{samples}: {error:?}");
        assert_eq!(r.gpu_error_count(), 0);
    }
}

/// A yellow rectangle overlay (fullscreen triangle + scissor) recording the kinds of the
/// targets it was prepared for.
struct RectOverlay {
    rect: [u32; 4],
    pipeline: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    kinds: std::sync::Arc<std::sync::Mutex<Vec<crate::OverlayTargetKind>>>,
    stage: crate::OverlayStage,
}

impl crate::RenderOverlay for RectOverlay {
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        _: &mut wgpu::CommandEncoder,
        target: &crate::OverlayTarget,
    ) {
        self.kinds.lock().unwrap().push(target.kind);
        if self.pipeline.as_ref().is_some_and(|(f, _)| *f == target.format) {
            return;
        }
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rect overlay"),
            source: wgpu::ShaderSource::Wgsl(
                "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
                     let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
                     return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
                 }
                 @fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(1.0, 1.0, 0.0, 1.0); }"
                    .into(),
            ),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rect overlay"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(target.format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipeline = Some((target.format, pipeline));
    }

    fn stage(&self) -> crate::OverlayStage {
        self.stage
    }

    fn render(&self, pass: &mut wgpu::RenderPass<'static>, _: &crate::OverlayTarget) {
        let Some((_, pipeline)) = &self.pipeline else { return };
        let [x, y, w, h] = self.rect;
        pass.set_scissor_rect(x, y, w, h);
        pass.set_pipeline(pipeline);
        pass.draw(0..3, 0..1);
    }
}

/// A windowed capture re-records the last frame's primary views into the offscreen target
/// and runs the overlays after them (kind `Capture`): the capture is the frame, overlays on
/// top — with and without MSAA. (Recorded the other way round, the replayed views would
/// paint over the overlays.)
#[test]
fn capture_replay_draws_overlays_after_the_views() {
    for samples in [1, 4] {
        let Some(mut app) = split_screen_app(64, 48, samples) else {
            return;
        };
        let kinds = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        app.resources.expect_mut::<crate::RenderOverlays>().insert(
            "rect",
            0,
            RectOverlay {
                rect: [24, 16, 16, 16],
                pipeline: None,
                kinds: kinds.clone(),
                stage: crate::OverlayStage::AfterTonemap,
            },
        );
        let frame = crate::render_to_image(&mut app).expect("capture");
        assert_eq!(
            frame.get(30, 20)[..3],
            [255, 255, 0],
            "overlay over the scene ×{samples}"
        );
        assert_eq!(frame.get(4, 4)[..3], [255, 0, 0], "left view ×{samples}");
        let r = app.resources.expect_mut::<Renderer>();
        r.frame.force_capture_replay = true;
        let replayed = r.capture_blocking().expect("replayed capture");
        assert_eq!(r.gpu_error_count(), 0);
        assert!(
            frame.pixels == replayed.pixels,
            "×{samples}: the replay differs from the frame"
        );
        use crate::OverlayTargetKind as K;
        assert_eq!(*kinds.lock().unwrap(), [K::Offscreen, K::Capture], "×{samples}");
    }
}

/// Viewports within wgpu's limits are drawn as they are; others through their part within
/// the target, remapped so each pixel shows what the whole viewport would.
#[test]
fn out_of_range_viewport_rects_are_fitted_to_the_target() {
    // In range (incl. partly or entirely outside the target): unchanged, no correction.
    for (v, size) in [
        ([0.25, 0.25, 0.375, 0.5], (8, 4)),
        ([-0.5, 0.0, 2.0, 1.0], (10, 10)),
        ([1.0, 1.0, 0.5, 0.5], (10, 10)),
        ([0.0, 0.0, 1.0, 1.0], (8192, 8192)),
    ] {
        let r = viewport_rect(v, size);
        assert_eq!(r.fitted(size, 8192), (r, Mat4::IDENTITY), "{v:?}");
    }
    // 101× the target, centered (6464 × 4848 px > 2048): the target itself, remapped.
    let size = (64, 48);
    let big = viewport_rect([-50.0, -50.0, 101.0, 101.0], size);
    assert_eq!((big.x, big.y, big.w, big.h), (-3200, -2400, 6464, 4848));
    let (fit_rect, fit) = big.fitted(size, 2048);
    assert_eq!((fit_rect.x, fit_rect.y, fit_rect.w, fit_rect.h), (0, 0, 64, 48));
    // A clip-space point lands on the same pixel either way.
    let pixel = |r: ViewportRect, clip: Vec4| {
        let n = clip / clip.w;
        (
            r.x as f64 + (n.x as f64 + 1.0) / 2.0 * r.w as f64,
            r.y as f64 + (1.0 - n.y as f64) / 2.0 * r.h as f64,
        )
    };
    for clip in [
        Vec4::new(0.0, 0.0, 0.5, 1.0),
        Vec4::new(0.004, -0.007, 0.5, 1.0),
        Vec4::new(-0.018, 0.012, 0.4, 2.0),
    ] {
        let (a, b) = (pixel(big, clip), pixel(fit_rect, fit * clip));
        assert!(
            (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3,
            "{clip}: {a:?} vs {b:?}"
        );
    }
    // Off-center and partly outside: the visible part only.
    let wide = viewport_rect([0.5, 0.0, 1000.0, 1.0], size);
    let (r, fit) = wide.fitted(size, 2048);
    assert_eq!((r.x, r.y, r.w, r.h), (32, 0, 32, 48));
    let clip = Vec4::new(-0.9999, 0.25, 0.5, 1.0);
    let (a, b) = (pixel(wide, clip), pixel(r, fit * clip));
    assert!((a.0 - b.0).abs() < 1e-2 && (a.1 - b.1).abs() < 1e-3, "{a:?} vs {b:?}");
    // Entirely outside or non-finite: whatever is drawn is valid for wgpu.
    for v in [
        [2e5, 0.0, 1.0, 1.0],
        [-2e5, -2e5, 1.0, 1.0],
        [f32::INFINITY, 0.0, 1.0, 1.0],
        [f32::NEG_INFINITY, 0.0, 1.0, 1.0],
        [0.0, 0.0, f32::INFINITY, 1.0],
        [0.0, f32::NEG_INFINITY, 1.0, f32::INFINITY],
        [f32::NAN; 4],
    ] {
        let (r, fit) = viewport_rect(v, size).fitted(size, 2048);
        let (x, y, w, h) = (r.x as i64, r.y as i64, r.w as i64, r.h as i64);
        assert!(
            w <= 2048 && h <= 2048 && x >= -4096 && y >= -4096 && x + w < 4096 && y + h < 4096,
            "{v:?}: {r:?}"
        );
        assert!(fit.is_finite(), "{v:?}: {fit}");
    }
}

/// Regression: a camera viewport past wgpu's viewport limits (larger than the device's
/// `max_texture_dimension_2d`, or far outside the target, e.g. from non-finite values)
/// failed `set_viewport` validation, which loses the frame's whole command buffer (a panic
/// in debug builds). Such viewports now draw their part within the target: a viewport 101
/// times the target's size centered on it shows the central 1/101 of its image — what a
/// full-target camera with a 101 times narrower field of view shows — on every draw path,
/// with and without MSAA.
#[test]
fn out_of_range_viewports_draw_their_visible_part() {
    use crate::{Camera3d, Mesh3d, Projection};
    use space3d_core::{MeshData, Transform};
    let narrow = 30f32.to_radians();
    let wide = 2.0 * (101.0 * (narrow / 2.0).tan()).atan();
    let camera = |fov_y: f32, viewport: Option<[f32; 4]>| Camera3d {
        viewport,
        clear_color: Some(Vec4::new(0.0, 0.0, 1.0, 1.0)),
        ..Camera3d::default().with_projection(Projection::Perspective {
            fov_y,
            near: 0.1,
            far: 100.0,
        })
    };
    for samples in [1, 4] {
        let Some(mut app) = headless_app(64, 48) else { return };
        app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
        let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
        let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
        app.world.spawn((
            Transform::from_xyz(0.3, -0.2, 0.0).with_rotation(glam::Quat::from_rotation_y(0.6)),
            Mesh3d(cube),
            Material::unlit(Vec4::new(1.0, 0.5, 0.0, 1.0)),
        ));
        let cam = app
            .world
            .spawn((Transform::from_xyz(0.0, 0.0, 4.0), camera(narrow, None)));
        let reference = crate::render_to_image(&mut app).expect("capture");
        let orange = reference.pixels.chunks(4).filter(|p| p[0] == 255 && p[2] == 0).count();
        assert!(orange > 200, "the cube fills part of the view: {orange} px");
        for path in [DrawPath::CpuDirect, best] {
            set_path(&mut app, path);
            *app.world.get::<&mut Camera3d>(cam).unwrap() = camera(wide, Some([-50.0, -50.0, 101.0, 101.0]));
            let zoomed = crate::render_to_image(&mut app).expect("capture");
            let d = differing_px(&reference, &zoomed);
            assert!(d <= 16, "{path:?} ×{samples}: {d} px differ from the narrow camera");
            // Far outside, or not finite: nothing drawn (the first pass clears to black),
            // and no validation error.
            for v in [
                [2e5, 0.0, 1.0, 1.0],
                [f32::INFINITY, 0.0, 1.0, 1.0],
                [0.0, 0.0, f32::INFINITY, 1.0],
            ] {
                *app.world.get::<&mut Camera3d>(cam).unwrap() = camera(narrow, Some(v));
                let img = crate::render_to_image(&mut app).expect("capture");
                let black = img.pixels.chunks(4).filter(|p| p[..3] == [0, 0, 0]).count();
                if v[2].is_finite() {
                    assert_eq!(black, 64 * 48, "{v:?}: {path:?} ×{samples}");
                }
            }
            assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
        }
    }
}

/// The viewport clear writes its color — the pass's blend constant — into every color
/// format a view can render to: sRGB (encoded on store), linear 8- and 10-bit, and HDR
/// `Rgba16Float` (not clamped to 1), with 1 and 4 samples (resolved), only inside the
/// viewport and its scissor rect.
#[test]
fn viewport_clear_writes_every_target_format() {
    use crate::components::linear_to_srgb;
    use crate::instance::f16_to_f32;
    use crate::pipeline::DEPTH_FORMAT;
    use wgpu::TextureFormat as F;
    const W: u32 = 8;
    const H: u32 = 4;
    let Some(mut app) = headless_app(8, 8) else { return };
    let r = app.resources.expect_mut::<Renderer>();
    let size = wgpu::Extent3d {
        width: W,
        height: H,
        depth_or_array_layers: 1,
    };
    let rect = viewport_rect([0.25, 0.25, 0.375, 0.5], (W, H));
    let mut tested = 0;
    for format in [
        F::Rgba8UnormSrgb,
        F::Bgra8UnormSrgb,
        F::Rgba8Unorm,
        F::Bgra8Unorm,
        F::Rgb10a2Unorm,
        F::Rgba16Float,
    ] {
        let features = r.adapter.get_texture_format_features(format);
        let color = if format == F::Rgba16Float {
            [2.5, 0.5, 0.125, 1.0]
        } else {
            [0.25, 0.5, 0.75, 1.0]
        };
        for samples in [1, 4] {
            let depth_samples = r.adapter.get_texture_format_features(DEPTH_FORMAT).flags;
            if !features.allowed_usages.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
                || !features.flags.contains(wgpu::TextureFormatFeatureFlags::BLENDABLE)
                || !features.flags.sample_count_supported(samples)
                || !depth_samples.sample_count_supported(samples)
            {
                eprintln!("{format:?} ×{samples}: not a blendable render target here, skipped");
                continue;
            }
            let texture = |format, samples, usage| {
                r.device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: None,
                        size,
                        mip_level_count: 1,
                        sample_count: samples,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage,
                        view_formats: &[],
                    })
                    .create_view(&Default::default())
            };
            use wgpu::TextureUsages as U;
            let target = texture(format, 1, U::RENDER_ATTACHMENT | U::COPY_SRC);
            let msaa = (samples > 1).then(|| texture(format, samples, U::RENDER_ATTACHMENT));
            let depth = texture(DEPTH_FORMAT, samples, U::RENDER_ATTACHMENT);
            let mut encoder = r.device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: msaa.as_ref().unwrap_or(&target),
                        depth_slice: None,
                        resolve_target: msaa.is_some().then_some(&target),
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &depth,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(1.0),
                            store: wgpu::StoreOp::Discard,
                        }),
                        stencil_ops: None,
                    }),
                    ..Default::default()
                });
                pass.set_viewport(rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32, 0.0, 1.0);
                let [x, y, w, h] = rect.scissor((W, H));
                pass.set_scissor_rect(x, y, w, h);
                pass.set_pipeline(r.pipelines.viewport_clear(&r.device, format, samples));
                let [cr, cg, cb, ca] = color.map(f64::from);
                pass.set_blend_constant(wgpu::Color {
                    r: cr,
                    g: cg,
                    b: cb,
                    a: ca,
                });
                pass.draw(0..3, 0..1);
                // As `encode_pass` does: GL clips the MSAA resolve to the scissor rect.
                pass.set_scissor_rect(0, 0, W, H);
            }
            let texel = format.block_copy_size(None).expect("color format") as usize;
            let row = align_up((W as usize * texel) as u64, 256) as usize;
            let buffer = r.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: (row * H as usize) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                target.texture().as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row as u32),
                        rows_per_image: None,
                    },
                },
                size,
            );
            r.queue.submit([encoder.finish()]);
            buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            r.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            assert_eq!(r.gpu_error_count(), 0, "{format:?} ×{samples}");
            let data = buffer.slice(..).get_mapped_range().expect("mapped");
            // Stored values, normalized (sRGB formats store encoded values).
            let pixel = |x: usize, y: usize| -> [f32; 4] {
                let p = &data[y * row + x * texel..][..texel];
                let unorm8 = |i: usize| p[i] as f32 / 255.0;
                match format {
                    F::Rgba8UnormSrgb | F::Rgba8Unorm => [0, 1, 2, 3].map(unorm8),
                    F::Bgra8UnormSrgb | F::Bgra8Unorm => [2, 1, 0, 3].map(unorm8),
                    F::Rgb10a2Unorm => {
                        let v = u32::from_le_bytes(p.try_into().unwrap());
                        let c = |shift: u32| ((v >> shift) & 1023) as f32 / 1023.0;
                        [c(0), c(10), c(20), (v >> 30) as f32 / 3.0]
                    }
                    F::Rgba16Float => [0, 2, 4, 6].map(|i| f16_to_f32(u16::from_le_bytes([p[i], p[i + 1]]))),
                    _ => unreachable!(),
                }
            };
            let stored = color.map(|c| if format.is_srgb() { linear_to_srgb(c) } else { c });
            let expected = [stored[0], stored[1], stored[2], color[3]];
            let step = match format {
                F::Rgb10a2Unorm => 1.0 / 1023.0,
                F::Rgba16Float => 2e-3,
                _ => 1.0 / 255.0,
            };
            for y in 0..H as usize {
                for x in 0..W as usize {
                    let inside = (2..5).contains(&x) && (1..3).contains(&y);
                    let want = if inside { expected } else { [0.0, 0.0, 0.0, 1.0] };
                    let got = pixel(x, y);
                    let tolerance = |w: f32| step * w.abs().max(1.0) * 1.01;
                    assert!(
                        got.iter().zip(want).all(|(g, w)| (g - w).abs() <= tolerance(w)),
                        "{format:?} ×{samples} at ({x}, {y}): {got:?}, expected {want:?}"
                    );
                }
            }
            drop(data);
            tested += 1;
        }
    }
    assert!(tested >= 6, "only {tested} format/sample combinations tested");
    assert_eq!(r.gpu_error_count(), 0);
}

/// Headless app on a device created with these limits, so wgpu's own validation fails past
/// them (not only the renderer's copy of the limits). `None` without an adapter.
fn limited_app(w: u32, h: u32, max_buffer: u64, max_binding: u64) -> Option<space3d_core::App> {
    crate::renderer::TEST_LIMITS.set(Some((max_buffer, max_binding)));
    let app = headless_app(w, h);
    crate::renderer::TEST_LIMITS.set(None);
    let app = app?;
    let caps = app.resources.expect::<Renderer>().capabilities();
    assert_eq!(caps.max_buffer_size, max_buffer);
    // GL (WebGL2 level) has no storage buffers: a binding limit of 0.
    assert!(caps.max_storage_binding <= max_binding);
    Some(app)
}

fn paths_up_to(best: DrawPath) -> impl Iterator<Item = DrawPath> {
    [
        DrawPath::CpuDirect,
        DrawPath::GpuIndirect,
        DrawPath::GpuMultiDraw,
        DrawPath::GpuMultiDrawCount,
    ]
    .into_iter()
    .filter(move |p| *p <= best)
}

fn count_px(img: &space3d_core::ImageData, rgb: [u8; 3]) -> usize {
    img.pixels.chunks(4).filter(|p| p[..3] == rgb).count()
}

/// `n` unlit cubes in a grid `side` wide, from `(x0, -3)` spanning 4 × 6 units.
fn spawn_grid(app: &mut space3d_core::App, mesh: MeshHandle, n: usize, side: usize, x0: f32, rgb: [f32; 3]) {
    spawn_grid_part(app, mesh, n, n, side, x0, rgb);
}

/// The first `n` cubes of the [`spawn_grid`] of `total` cubes (at the same positions).
fn spawn_grid_part(
    app: &mut space3d_core::App,
    mesh: MeshHandle,
    n: usize,
    total: usize,
    side: usize,
    x0: f32,
    rgb: [f32; 3],
) {
    use crate::Mesh3d;
    use space3d_core::Transform;
    let material = Material::unlit(glam::Vec4::new(rgb[0], rgb[1], rgb[2], 1.0));
    for i in 0..n {
        let x = x0 + (i % side) as f32 * 4.0 / side as f32;
        let y = (i / side) as f32 * 6.0 / (total.div_ceil(side)) as f32 - 3.0;
        app.world
            .spawn((Transform::from_xyz(x, y, 0.0), Mesh3d(mesh), material.clone()));
    }
}

#[test]
fn fair_share_cuts_the_largest_batches() {
    let mut kept = Vec::new();
    let mut share = |counts: &[u32], cap: u64| {
        fair_share(counts.iter().copied(), cap, &mut kept);
        kept.clone()
    };
    assert_eq!(share(&[60_000, 10_000, 1], 65_536), [55_535, 10_000, 1]);
    assert_eq!(share(&[1, 60_000, 10_000], 65_536), [1, 55_535, 10_000]);
    // Equal batches share the cut; the remainder goes to the first ones.
    assert_eq!(share(&[5, 5, 5], 7), [3, 2, 2]);
    assert_eq!(share(&[9, 4, 9], 20), [8, 4, 8]);
    assert_eq!(share(&[9, 4, 9], 21), [9, 4, 8]);
    // Everything fits: unchanged.
    assert_eq!(share(&[10, 20], 30), [10, 20]);
    assert_eq!(share(&[10, 20], 1000), [10, 20]);
    assert_eq!(share(&[3, 3], 0), [0, 0]);
    assert_eq!(share(&[], 5), [] as [u32; 0]);
    for cap in 0..40 {
        let k = share(&[17, 2, 11, 0, 6], cap);
        assert_eq!(k.iter().map(|&x| x as u64).sum::<u64>(), cap.min(36), "cap {cap}");
    }
}

/// Regression: the `ecs` instance buffer grew to whatever the scene needed. Past the
/// device's `max_buffer_size` creating it failed validation and every frame was lost,
/// unrelated objects included (a panic in debug builds). Then the matrices past the limit
/// were dropped in batch order, so a small batch sorting after a huge one vanished whole.
/// Now the largest batch is cut (logged once): the frame equals the scene without the cut
/// entities, and the small batches are all drawn.
#[test]
fn ecs_instances_past_max_buffer_size_are_cut_not_fatal() {
    use crate::{Camera3d, RenderStats};
    use space3d_core::{MeshData, Transform};
    // 4 MiB (one staging chunk): 65 536 matrices.
    const MAX: u64 = crate::gpu::STAGING_CHUNK;
    const FIT: usize = (MAX / 64) as usize;
    const RED: usize = 60_000;
    const GREEN: usize = 10_000;
    // A red grid on the left, a green one on the right and one cyan cube (its own
    // material, the last batch); `red` of the RED red cubes are spawned.
    let build = |red: usize| {
        let mut app = limited_app(96, 64, MAX, MAX)?;
        let mesh = app
            .resources
            .expect_mut::<RenderAssets>()
            .add_mesh(MeshData::cube(0.04));
        spawn_grid_part(&mut app, mesh, red, RED, 250, -5.0, [1.0, 0.0, 0.0]);
        for i in 0..GREEN {
            let x = 1.0 + (i % 100) as f32 * 0.04;
            let y = (i / 100) as f32 * 0.06 - 3.0;
            app.world.spawn((
                Transform::from_xyz(x, y, 0.0),
                crate::Mesh3d(mesh),
                Material::unlit(glam::Vec4::new(0.0, 1.0, 0.0, 1.0)),
            ));
        }
        let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.6));
        app.world.spawn((
            Transform::from_xyz(-0.5, 2.0, 0.0),
            crate::Mesh3d(cube),
            Material::unlit(glam::Vec4::new(0.0, 1.0, 1.0, 1.0)),
        ));
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 8.0),
            Camera3d::default().with_clear_color(glam::Vec4::new(0.0, 0.0, 0.0, 1.0)),
        ));
        Some(app)
    };
    let Some(probe) = limited_app(8, 8, MAX, MAX) else {
        return;
    };
    let best = probe.resources.expect::<Renderer>().capabilities().best_draw_path;
    drop(probe);
    for path in paths_up_to(best) {
        let mut app = build(RED).expect("adapter");
        set_path(&mut app, path);
        // Two frames: the first allocates, the second diffs against the shadow.
        crate::render_to_image(&mut app).expect("capture");
        let cut = crate::render_to_image(&mut app).expect("capture");
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.gpu_error_count(), 0, "{path:?}");
        assert!(r.frame.ecs_overflow, "{path:?}");
        assert!(r.frame.ecs.gpu.capacity() <= MAX, "{path:?}");
        assert_eq!(
            r.frame.ecs_kept,
            [(FIT - GREEN - 1) as u32, GREEN as u32, 1],
            "{path:?}"
        );
        let stats = app.resources.expect::<RenderStats>();
        assert_eq!((stats.instances, stats.batches), (FIT as u64, 3), "{path:?}");

        // The same scene without the red cubes that were cut.
        let mut app = build(FIT - GREEN - 1).expect("adapter");
        set_path(&mut app, path);
        let reference = crate::render_to_image(&mut app).expect("capture");
        assert!(
            !app.resources.expect::<Renderer>().frame.ecs_overflow,
            "exactly at the limit"
        );
        let d = differing_px(&cut, &reference);
        assert_eq!(
            d, 0,
            "{path:?}: differs from the scene without the cut entities in {d} px"
        );
        assert!(count_px(&cut, [255, 0, 0]) > 500, "{path:?}: red grid");
        assert!(count_px(&cut, [0, 255, 0]) > 300, "{path:?}: green grid");
        assert!(count_px(&cut, [0, 255, 255]) > 20, "{path:?}: the cyan cube");
    }
}

/// Regression: `cpu_visible` collects the partially visible ECS matrices of every view, so
/// with several views it outgrew `max_buffer_size` even though the `ecs` buffer fit, and
/// every frame was lost. A batch that no longer fits is now drawn whole from `ecs`
/// (unculled): the image is unchanged.
#[test]
fn partially_visible_ecs_past_max_buffer_size_is_drawn_unculled() {
    use crate::{Camera3d, RenderTarget};
    use space3d_core::{MeshData, Transform};
    const MAX: u64 = crate::gpu::STAGING_CHUNK;
    // 50 000 matrices (3.2 MB) fit; two render-target views each partially see about two
    // thirds of them (2.1 MB each), the primary view about 90 %. A 1 MiB storage binding
    // keeps the batch on CPU culling on every path.
    let build = |app: &mut space3d_core::App| {
        let mesh = app
            .resources
            .expect_mut::<RenderAssets>()
            .add_mesh(MeshData::cube(0.04));
        spawn_grid(app, mesh, 50_000, 250, -6.0, [1.0, 0.5, 0.0]);
        for (order, x) in [(-2, -1.5), (-1, 1.5)] {
            let rt = app.resources.expect_mut::<RenderAssets>().add_render_target(64, 64);
            app.world.spawn((
                Transform::from_xyz(x, 0.0, 7.0),
                Camera3d {
                    order,
                    target: RenderTarget::Texture(rt),
                    ..Default::default()
                },
            ));
        }
        app.world.spawn((
            Transform::from_xyz(1.0, 0.0, 7.0),
            Camera3d::default().with_clear_color(glam::Vec4::new(0.0, 0.0, 0.0, 1.0)),
        ));
    };
    let Some(mut reference) = headless_app(96, 64) else {
        return;
    };
    build(&mut reference);
    set_path(&mut reference, DrawPath::CpuDirect);
    let expected = crate::render_to_image(&mut reference).expect("capture");
    assert!(!reference.resources.expect::<Renderer>().frame.visible_overflow);
    assert!(count_px(&expected, [255, 188, 0]) > 500, "the grid is visible");
    let best = reference.resources.expect::<Renderer>().capabilities().best_draw_path;
    drop(reference);
    for path in paths_up_to(best) {
        let mut app = limited_app(96, 64, MAX, 1 << 20).expect("adapter");
        build(&mut app);
        set_path(&mut app, path);
        crate::render_to_image(&mut app).expect("capture");
        let img = crate::render_to_image(&mut app).expect("capture");
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.gpu_error_count(), 0, "{path:?}");
        let f = &r.frame;
        assert!(f.visible_overflow, "{path:?}");
        assert!(f.cpu_visible.len() > 0, "{path:?}: the first view still culls");
        assert!(f.cpu_visible.gpu.capacity() <= MAX, "{path:?}");
        let d = differing_px(&expected, &img);
        assert_eq!(d, 0, "{path:?} differs in {d} px");
    }
}

/// GPU culling binds the `ecs` buffer from offset 0 up to the storage-binding limit, so ECS
/// batches that end inside it keep GPU culling when the whole buffer is larger (before,
/// ECS data past the limit moved every ECS batch to CPU culling).
#[test]
fn ecs_batches_inside_the_storage_binding_keep_gpu_culling() {
    use crate::Camera3d;
    use space3d_core::{MeshData, Transform};
    const BINDING: u64 = 32 << 10;
    let build = |app: &mut space3d_core::App| {
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.1));
        // 200 matrices (12.8 KB) first, then 1000 (64 KB, past the binding).
        spawn_grid(app, mesh, 200, 20, -5.0, [1.0, 0.0, 0.0]);
        spawn_grid(app, mesh, 1000, 40, 1.0, [0.0, 0.0, 1.0]);
        app.world.spawn((
            Transform::from_xyz(-1.0, 0.0, 6.0),
            Camera3d::default().with_clear_color(glam::Vec4::new(0.0, 0.0, 0.0, 1.0)),
        ));
    };
    let Some(mut app) = limited_app(96, 64, 256 << 20, BINDING) else {
        return;
    };
    build(&mut app);
    let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
    if best == DrawPath::CpuDirect {
        return;
    }
    set_path(&mut app, DrawPath::CpuDirect);
    let cpu = crate::render_to_image(&mut app).expect("capture");
    for path in paths_up_to(best).skip(1) {
        set_path(&mut app, path);
        let img = crate::render_to_image(&mut app).expect("capture");
        let d = differing_px(&cpu, &img);
        assert_eq!(d, 0, "{path:?} differs from CpuDirect in {d} px");
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.gpu_error_count(), 0, "{path:?}");
        let f = &r.frame;
        let ecs: Vec<_> = f.sources.iter().filter(|s| s.buffer == SourceBuffer::Ecs).collect();
        assert_eq!(ecs.len(), 2);
        for s in ecs {
            let inside = (s.first + s.count) as u64 * 64 <= BINDING;
            assert_eq!(s.cull != CullSource::None, inside, "{path:?}: batch at {}", s.first);
        }
        assert_eq!(
            f.jobs.iter().filter(|(b, _)| *b == SourceBuffer::Ecs).count(),
            1,
            "{path:?}: the first batch is GPU-culled"
        );
    }
    assert!(count_px(&cpu, [255, 0, 0]) > 50 && count_px(&cpu, [0, 0, 255]) > 50);
}

/// `compact.wgsl` (GpuMultiDrawCount) keeps the non-empty draws of each run in order. Run
/// directly so it is covered on adapters without MULTI_DRAW_INDIRECT_COUNT (e.g. Metal).
#[test]
fn draw_compaction_is_order_preserving() {
    use wgpu::util::DeviceExt;
    let Ok(r) = pollster::block_on(Renderer::new_headless(8, 8)) else {
        return;
    };
    let Some(cull) = &r.pipelines.cull else { return };
    let device = &r.device;
    // Two runs: 150 draws (every third empty) and 7 draws (all empty but one).
    let runs: [[u32; 2]; 2] = [[0, 150], [150, 7]];
    let mut args_in = Vec::new();
    for d in 0..157u32 {
        let instances = if d < 150 {
            if d % 3 == 0 { 0 } else { d }
        } else if d == 153 {
            5
        } else {
            0
        };
        args_in.extend_from_slice(&[1000 + d, instances, d, d * 2, d * 3]);
    }
    let params = GpuCompactParams {
        run_start: 0,
        run_count: 2,
        workgroups_x: 2,
        pad: 0,
    };
    use wgpu::BufferUsages as U;
    let init = |label, contents: &[u8], usage| {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents,
            usage,
        })
    };
    let params_buf = init("params", bytemuck::bytes_of(&params), U::UNIFORM);
    let runs_buf = init("runs", bytemuck::cast_slice(&runs), U::STORAGE);
    let args_buf = init("args", bytemuck::cast_slice(&args_in), U::STORAGE);
    let counts = init("counts", &[0xff; 8], U::STORAGE | U::COPY_SRC);
    let out = init("out", &vec![0xff; args_in.len() * 4], U::STORAGE | U::COPY_SRC);
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &cull.compact_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_binding(&params_buf, 16),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: runs_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: args_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: counts.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: out.as_entire_binding(),
            },
        ],
    });
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 8 + out.size(),
        usage: U::MAP_READ | U::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.set_pipeline(&cull.compact);
        pass.set_bind_group(0, &bg, &[0]);
        pass.dispatch_workgroups(2, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&counts, 0, &readback, 0, 8);
    encoder.copy_buffer_to_buffer(&out, 0, &readback, 8, out.size());
    r.queue.submit([encoder.finish()]);
    readback.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let words: Vec<u32> = bytemuck::cast_slice(&readback.slice(..).get_mapped_range().expect("mapped")).to_vec();
    let (counts, out) = words.split_at(2);
    for (r, [first, n]) in runs.iter().enumerate() {
        let expected: Vec<u32> = (*first..first + n)
            .filter(|d| args_in[*d as usize * 5 + 1] != 0)
            .collect();
        assert_eq!(counts[r], expected.len() as u32, "run {r}");
        for (slot, d) in expected.iter().enumerate() {
            let o = (*first as usize + slot) * 5;
            assert_eq!(
                &out[o..o + 5],
                &args_in[*d as usize * 5..*d as usize * 5 + 5],
                "run {r} slot {slot}"
            );
        }
    }
}

/// Instances with an snorm16 translation: not decodable by the GPU culler
/// (`CullSource::None`), so GPU paths draw them with direct draws too.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SnormInstance {
    translation: [i16; 4],
}
impl InstanceData for SnormInstance {
    const LAYOUT: &'static crate::InstanceLayout = &crate::InstanceLayout {
        name: "SnormInstance",
        stride: 8,
        attributes: &[crate::InstanceAttribute::new(
            crate::InstanceSemantic::Translation,
            wgpu::VertexFormat::Snorm16x4,
            0,
        )],
    };
}

fn snorm(v: f32) -> i16 {
    (v * i16::MAX as f32).round() as i16
}

/// Several meshes (so most draws have `base_vertex > 0`) through every source kind: ECS
/// batches, a cullable explicit buffer and an uncullable one (direct even on GPU paths).
fn many_mesh_scene(app: &mut space3d_core::App) {
    use crate::{Camera3d, InstanceBuffer, InstanceTransform, InstancedMesh, Mesh3d};
    use space3d_core::{MeshData, Transform};
    let (cube, sphere, plane, small) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (
            a.add_mesh(MeshData::cube(0.8)),
            a.add_mesh(MeshData::sphere(0.5, 16, 8)),
            a.add_mesh(MeshData::plane(1.0)),
            a.add_mesh(MeshData::cube(0.3)),
        )
    };
    let colors = [
        Vec4::new(1.0, 0.0, 0.0, 1.0),
        Vec4::new(0.0, 1.0, 0.0, 1.0),
        Vec4::new(1.0, 1.0, 0.0, 1.0),
        Vec4::new(1.0, 0.0, 1.0, 1.0),
    ];
    for (i, (mesh, color)) in [cube, sphere, plane, small].into_iter().zip(colors).enumerate() {
        app.world.spawn((
            Transform::from_xyz(i as f32 * 1.2 - 1.8, 1.0, 0.0).with_rotation(glam::Quat::from_rotation_x(0.6)),
            Mesh3d(mesh),
            Material::unlit(color),
        ));
    }
    let data = (0..4)
        .map(|i| {
            InstanceTransform::from_trs(
                Vec3::new(i as f32 - 1.5, -0.2, 0.0),
                glam::Quat::IDENTITY,
                Vec3::splat(0.6),
            )
        })
        .collect();
    let instances = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, crate::BufferUsage::Static));
    app.world.spawn((InstancedMesh {
        mesh: sphere,
        material: Material::unlit(Vec4::new(0.0, 1.0, 1.0, 1.0)),
        instances,
    },));
    let data = (0..4)
        .map(|i| SnormInstance {
            translation: [snorm(i as f32 * 0.5 - 0.75), snorm(-0.9), 0, 0],
        })
        .collect();
    let instances = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, crate::BufferUsage::Static));
    app.world.spawn((InstancedMesh {
        mesh: small,
        material: Material::unlit(Vec4::new(1.0, 1.0, 1.0, 1.0)),
        instances,
    },));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
    ));
}

/// Regression: direct draws passed each mesh's arena offset as `base_vertex`. WebGL2 and
/// GLES < 3.2 lack `DownlevelFlags::BASE_VERTEX`; wgpu does not reject it there and its GL
/// backend calls a missing entry point (a panic on the web; on ANGLE ES 3.0 every mesh but
/// the first vanished). Without the flag, direct draws now bind the arena from the mesh's
/// first vertex and pass 0, and the image is unchanged. Also run on the best GPU path,
/// whose uncullable sources are drawn directly between indirect draws. (On a device
/// without the flag — e.g. `WGPU_BACKEND=gl` on ANGLE — only the emulated path runs.)
#[test]
fn direct_draws_without_base_vertex_bind_the_arena_at_the_mesh() {
    let Some(mut app) = headless_app(96, 64) else { return };
    many_mesh_scene(&mut app);
    let caps = app.resources.expect::<Renderer>().capabilities().clone();
    let mut reference = None;
    for path in [DrawPath::CpuDirect, caps.best_draw_path] {
        set_path(&mut app, path);
        let native = caps.base_vertex.then(|| {
            let image = crate::render_to_image(&mut app).expect("capture");
            let used = app.resources.expect::<Renderer>().frame.max_direct_base_vertex;
            assert!(used > 0, "{path:?}: the scene draws meshes at base_vertex > 0");
            image
        });

        app.resources.expect_mut::<Renderer>().caps.base_vertex = false;
        let emulated = crate::render_to_image(&mut app).expect("capture");
        let r = app.resources.expect_mut::<Renderer>();
        r.caps.base_vertex = caps.base_vertex;
        assert_eq!(r.frame.max_direct_base_vertex, 0, "{path:?}: direct draws use base 0");
        assert_eq!(r.gpu_error_count(), 0);
        if let Some(native) = &native {
            let d = differing_px(native, &emulated);
            assert_eq!(d, 0, "{path:?}: emulated base_vertex differs in {d} px");
        }
        let reference = reference.get_or_insert(emulated.clone());
        let d = differing_px(reference, &emulated);
        assert_eq!(d, 0, "{path:?} differs from CpuDirect in {d} px");
    }
    // Every mesh is visible: red cube, green sphere, yellow plane, magenta small cube,
    // cyan explicit spheres, white snorm-placed cubes.
    let img = reference.expect("rendered");
    for color in [
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ] {
        let n = img.pixels.chunks(4).filter(|p| p[..3] == color).count();
        assert!(n > 10, "color {color:?} covers {n} px");
    }
}

#[test]
fn direct_vertex_base_rebinds_only_without_base_vertex() {
    assert_eq!(direct_vertex_base(24, true, true), (0, 24));
    assert_eq!(direct_vertex_base(24, true, false), (24, 0));
    assert_eq!(direct_vertex_base(0, true, false), (0, 0));
    // Indirect args carry their own base; slot 0 must start at vertex 0 for them.
    assert_eq!(direct_vertex_base(24, false, false), (0, 24));
}

/// Meshes uploaded before an arena growth, meshes whose placement straddles the old
/// capacity (written through the staging ring after the GPU copy), and in-place /
/// resizing `update_mesh` all render exactly like a fresh single-upload app — with and
/// without `BASE_VERTEX`, on CpuDirect and the best GPU path.
#[test]
fn arena_growth_and_mesh_updates_render_like_fresh_uploads() {
    use crate::{Camera3d, Mesh3d, RenderStats};
    use space3d_core::{MeshData, Transform};
    let Some(probe) = headless_app(8, 8) else { return };
    let caps = probe.resources.expect::<Renderer>().capabilities().clone();
    drop(probe);
    let setup = |path: DrawPath, base_vertex: bool| {
        let mut app = headless_app(128, 64).expect("adapter");
        set_path(&mut app, path);
        app.resources.expect_mut::<Renderer>().caps.base_vertex = base_vertex;
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 6.0),
            Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
        ));
        app
    };
    let white = Material::unlit(Vec4::ONE);
    let red = Material::unlit(Vec4::new(1.0, 0.0, 0.0, 1.0));
    let fresh = |path, base_vertex, a: Option<MeshData>, b: Option<MeshData>| {
        let mut app = setup(path, base_vertex);
        for (mesh, x, m) in [(a, -2.0, &white), (b, 2.0, &red)] {
            if let Some(mesh) = mesh {
                let h = app.resources.expect_mut::<RenderAssets>().add_mesh(mesh);
                app.world
                    .spawn((Transform::from_xyz(x, 0.0, 0.0), Mesh3d(h), m.clone()));
            }
        }
        crate::render_to_image(&mut app).expect("capture")
    };
    let native = caps.base_vertex;
    let runs = [
        (DrawPath::CpuDirect, false),
        (DrawPath::CpuDirect, true),
        (caps.best_draw_path, true),
    ];
    // Native base_vertex only where the device has it.
    for (path, base_vertex) in runs.into_iter().filter(|(_, b)| native || !b) {
        let ctx = format!("{path:?}, base_vertex {base_vertex}");
        let mut app = setup(path, base_vertex);
        let a = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
        app.world
            .spawn((Transform::from_xyz(-2.0, 0.0, 0.0), Mesh3d(a), white.clone()));
        let img = crate::render_to_image(&mut app).expect("capture");
        assert_eq!(
            differing_px(&img, &fresh(path, base_vertex, Some(MeshData::cube(1.0)), None)),
            0,
            "{ctx}"
        );

        // 8385 vertices: the arena grows and B is placed at base 24, straddling the old
        // 4096-vertex capacity.
        let big = || MeshData::sphere(1.0, 128, 64);
        let b = app.resources.expect_mut::<RenderAssets>().add_mesh(big());
        app.world
            .spawn((Transform::from_xyz(2.0, 0.0, 0.0), Mesh3d(b), red.clone()));
        let img = crate::render_to_image(&mut app).expect("capture");
        assert!(
            app.resources.expect::<RenderStats>().buffer_reallocations > 0,
            "{ctx}: arena grew"
        );
        let reference = fresh(path, base_vertex, Some(MeshData::cube(1.0)), Some(big()));
        assert_eq!(differing_px(&img, &reference), 0, "{ctx}: after growth");

        // Many more meshes: another growth, placed entirely in the fresh region.
        for i in 0..300 {
            app.resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::sphere(0.3, 8 + i % 20, 6 + i % 10));
        }
        let img = crate::render_to_image(&mut app).expect("capture");
        assert_eq!(differing_px(&img, &reference), 0, "{ctx}: unrelated growth");

        // Same vertex/index counts: updated in place.
        assert!(
            app.resources
                .expect_mut::<RenderAssets>()
                .update_mesh(a, MeshData::cube(1.6))
        );
        let img = crate::render_to_image(&mut app).expect("capture");
        let reference = fresh(path, base_vertex, Some(MeshData::cube(1.6)), Some(big()));
        assert_eq!(differing_px(&img, &reference), 0, "{ctx}: in-place update");

        // Different size: freed and placed again; then large enough to grow the arena.
        for mesh in [MeshData::sphere(0.5, 16, 8), MeshData::sphere(0.5, 256, 128)] {
            assert!(app.resources.expect_mut::<RenderAssets>().update_mesh(a, mesh.clone()));
            let img = crate::render_to_image(&mut app).expect("capture");
            let reference = fresh(path, base_vertex, Some(mesh), Some(big()));
            assert_eq!(differing_px(&img, &reference), 0, "{ctx}: resizing update");
        }
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    }
}

/// Batches sort opaque → blended, then by pipeline (class, layout, shader/texture kind),
/// material and mesh: an interleaved spawn order groups every material's meshes together.
#[test]
fn sources_sort_pipeline_then_material_then_mesh() {
    let source = |blend: bool, layout: u16, pipeline: u64, material: u32, first_index: u32| Source {
        blend,
        class: if blend { CLASS_BLEND } else { 0 },
        layout,
        pipeline,
        material,
        mesh: GpuMesh {
            alloc: ArenaAlloc {
                base_vertex: 0,
                vertex_count: 0,
                first_index,
                index_count: 36,
            },
            sphere: Sphere {
                center: Vec3::ZERO,
                radius: 1.0,
            },
        },
        triangles: 12,
        buffer: SourceBuffer::Ecs,
        first: first_index,
        count: 1,
        stride: 64,
        cull: CullSource::Matrix { offset: 0 },
    };
    let mut sources = [
        source(true, 0, 0, 0, 0),   // blended: last
        source(false, 0, 0, 1, 36), // (mesh B, material 1)
        source(false, 0, 0, 0, 36), // (mesh B, material 0)
        source(false, 0, 2, 0, 0),  // custom shader: after the built-in pipeline
        source(false, 0, 0, 1, 0),  // (mesh A, material 1)
        source(false, 1, 0, 0, 0),  // another instance layout: after layout 0
        source(false, 0, 0, 0, 0),  // (mesh A, material 0)
    ];
    sources.sort_unstable_by_key(source_sort_key);
    let order: Vec<_> = sources
        .iter()
        .map(|s| (s.blend, s.layout, s.pipeline, s.material, s.mesh.alloc.first_index))
        .collect();
    assert_eq!(
        order,
        [
            (false, 0, 0, 0, 0),
            (false, 0, 0, 0, 36),
            (false, 0, 0, 1, 0),
            (false, 0, 0, 1, 36),
            (false, 0, 2, 0, 0),
            (false, 1, 0, 0, 0),
            (true, 0, 0, 0, 0),
        ]
    );
}

/// `order_by_key` is exactly a sort by (key, index).
#[test]
fn counting_sort_matches_a_key_index_sort() {
    let mut seed = 99u64;
    let mut scratch = KeyBuckets::default();
    let (mut order, mut runs) = (Vec::new(), Vec::new());
    for distinct in [1u64, 3, 40, 1000, 5000] {
        let keys: Vec<u64> = (0..5000)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                ((seed >> 20) % distinct) << 32 | (seed >> 60)
            })
            .collect();
        order_by_key(&keys, &mut order, &mut runs, &mut scratch);
        let mut reference: Vec<u32> = (0..keys.len() as u32).collect();
        reference.sort_by_key(|&i| (keys[i as usize], i));
        assert_eq!(order, reference, "{distinct} keys");
        let mut at = 0;
        for &(key, first, count) in &runs {
            assert_eq!(first, at);
            assert!(
                order[at as usize..(at + count) as usize]
                    .iter()
                    .all(|&i| keys[i as usize] == key)
            );
            at += count;
        }
        assert_eq!(at as usize, keys.len());
    }
}

/// Regression: every frame re-sorted all ECS items (96-byte elements, O(n log n)) even when
/// nothing changed — ~55 ms per frame for a static 1M-entity scene with interleaved
/// meshes/materials. The order now depends only on the key sequence and is reused while
/// that is unchanged; moving entities never reorders.
/// Updating a custom shader drops the pipelines of its old version; removing it drops all
/// of its cached state (checked every 256 frames), so an editor that creates and removes
/// shaders does not accumulate pipelines.
#[test]
fn removed_custom_shaders_release_their_pipelines() {
    use crate::{Camera3d, CustomShader, Mesh3d};
    use space3d_core::{MeshData, Transform};
    let Some(mut app) = headless_app(16, 16) else { return };
    let shader = "fn fragment_hook(in: VsOut, c: vec4<f32>) -> vec4<f32> { return c.bgra; }";
    let (mesh, h) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (
            a.add_mesh(MeshData::cube(1.0)),
            a.add_shader(CustomShader::new("a", shader)),
        )
    };
    app.world
        .spawn((Transform::default(), Mesh3d(mesh), Material::default().with_shader(h)));
    app.world
        .spawn((Transform::from_xyz(0.0, 0.0, 3.0), Camera3d::default()));
    let counts = |app: &space3d_core::App| app.resources.expect::<Renderer>().pipelines.custom_counts();
    app.step_ticks(1);
    assert_eq!(counts(&app), (1, 1));
    let v2 = format!("{shader}\nfn my_unused() {{}}");
    app.resources
        .expect_mut::<RenderAssets>()
        .update_shader(h, CustomShader::new("a", v2));
    app.step_ticks(1);
    assert_eq!(counts(&app), (1, 1), "the old version's pipeline is dropped");
    app.resources.expect_mut::<RenderAssets>().remove_shader(h);
    app.step_ticks(256);
    assert_eq!(counts(&app), (0, 0));
}

/// Regression: on browser WebGPU the error scope around a custom-shader pipeline answers
/// asynchronously and the answer was dropped, so an invalid pipeline stayed cached and
/// every frame drawing with it was lost. Pending answers are now polled each frame and an
/// invalid pipeline is replaced by the built-in one (reported through `shader_error`).
/// Native backends answer at once, so the pending answer is injected here.
#[test]
fn custom_pipelines_rejected_asynchronously_fall_back() {
    use crate::{Camera3d, CustomShader, Mesh3d};
    use space3d_core::{MeshData, Transform};
    let Some(mut app) = headless_app(16, 16) else { return };
    let green = "fn fragment_hook(in: VsOut, c: vec4<f32>) -> vec4<f32> { return vec4<f32>(0.0, 1.0, 0.0, 1.0); }";
    let (mesh, h) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (
            a.add_mesh(MeshData::cube(1.0)),
            a.add_shader(CustomShader::new("green", green)),
        )
    };
    app.world.spawn((
        Transform::default(),
        Mesh3d(mesh),
        Material::unlit(Vec4::new(1.0, 0.0, 0.0, 1.0)).with_shader(h),
    ));
    app.world
        .spawn((Transform::from_xyz(0.0, 0.0, 3.0), Camera3d::default()));
    let center = |app: &mut space3d_core::App| crate::render_to_image(app).expect("capture").get(8, 8);
    assert_eq!(center(&mut app), [0, 255, 0, 255], "custom shader");

    // Every frame until the (injected) answer arrives draws with the custom pipeline; the
    // first frame after it draws with the built-in one.
    let answer = |app: &mut space3d_core::App, error: Option<&str>| {
        let p = &mut app.resources.expect_mut::<Renderer>().pipelines;
        for key in p.custom_keys() {
            p.defer_validation(key, error.map(str::to_owned));
        }
        for _ in 0..1000 {
            let img = crate::render_to_image(app).expect("capture");
            if app.resources.expect::<Renderer>().pipelines.pending_count() == 0 {
                return img.get(8, 8);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the injected answer never resolved");
    };
    assert_eq!(answer(&mut app, None), [0, 255, 0, 255], "valid: kept");
    assert_eq!(app.resources.expect::<Renderer>().shader_error(h), None);
    assert_eq!(answer(&mut app, Some("injected: invalid pipeline")), [255, 0, 0, 255]);
    let r = app.resources.expect::<Renderer>();
    assert_eq!(r.shader_error(h), Some("injected: invalid pipeline"));
    assert_eq!(r.pipelines.custom_keys().len(), 1, "the key maps to the fallback");
    assert_eq!(center(&mut app), [255, 0, 0, 255], "stays on the built-in shader");

    // Answers for a version replaced meanwhile are dropped with it.
    let p = &mut app.resources.expect_mut::<Renderer>().pipelines;
    for key in p.custom_keys() {
        p.defer_validation(key, Some("stale".into()));
    }
    app.resources
        .expect_mut::<RenderAssets>()
        .update_shader(h, CustomShader::new("green", format!("{green}\nfn v2() {{}}")));
    assert_eq!(center(&mut app), [0, 255, 0, 255], "new version");
    let r = app.resources.expect::<Renderer>();
    assert_eq!((r.pipelines.pending_count(), r.shader_error(h)), (0, None));
}

#[test]
fn static_ecs_scenes_reuse_the_batch_order() {
    use crate::{Camera3d, Mesh3d};
    use space3d_core::{MeshData, Transform};
    let Some(mut app) = headless_app(64, 64) else { return };
    let meshes: Vec<_> = (0..3)
        .map(|k| {
            app.resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::cube(0.1 + 0.05 * k as f32))
        })
        .collect();
    let materials: Vec<_> = (0..4).map(|k| Material::srgb(0.2 * k as f32, 0.6, 0.3)).collect();
    let mut entities = Vec::new();
    for i in 0..600 {
        let p = Vec3::new((i % 30) as f32 * 0.3 - 4.5, (i / 30) as f32 * 0.3 - 3.0, 0.0);
        entities.push(app.world.spawn((
            Transform::from_xyz(p.x, p.y, p.z),
            Mesh3d(meshes[i % 3]),
            materials[(i / 3) % 4].clone(),
        )));
    }
    app.world
        .spawn((Transform::from_xyz(0.0, 0.0, 10.0), Camera3d::default()));
    let reorders = |app: &space3d_core::App| app.resources.expect::<Renderer>().frame.ecs_reorders;
    app.step_ticks(1);
    assert_eq!(reorders(&app), 1);
    assert_eq!(app.resources.expect::<crate::RenderStats>().batches, 12);
    let misses = app.resources.expect::<Renderer>().frame.materials.misses;
    for frame in 0..4 {
        for (k, &e) in entities.iter().enumerate().step_by(7) {
            app.world.get::<&mut Transform>(e).unwrap().translation.z = (frame + k) as f32 * 0.01;
        }
        app.step_ticks(1);
    }
    assert_eq!(reorders(&app), 1, "moving entities keeps the batch order");
    assert_eq!(
        app.resources.expect::<Renderer>().frame.materials.misses,
        misses,
        "4 materials interleaved: every intern hits the cache"
    );
    // Changing one entity's material reorders once, and the result renders like a
    // freshly built scene.
    *app.world.get::<&mut Material>(entities[5]).unwrap() = materials[0].clone();
    let img = crate::render_to_image(&mut app).expect("capture");
    assert_eq!(reorders(&app), 2);
    crate::render_to_image(&mut app).expect("capture");
    assert_eq!(reorders(&app), 2);

    let Some(mut fresh) = headless_app(64, 64) else { return };
    let fresh_meshes: Vec<_> = (0..3)
        .map(|k| {
            fresh
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(MeshData::cube(0.1 + 0.05 * k as f32))
        })
        .collect();
    for &e in &entities {
        let t = *app.world.get::<&Transform>(e).unwrap();
        let m = app.world.get::<&Mesh3d>(e).unwrap().0;
        let k = meshes.iter().position(|&x| x == m).unwrap();
        let mat = Material::clone(&app.world.get::<&Material>(e).unwrap());
        fresh.world.spawn((t, Mesh3d(fresh_meshes[k]), mat));
    }
    fresh
        .world
        .spawn((Transform::from_xyz(0.0, 0.0, 10.0), Camera3d::default()));
    let expected = crate::render_to_image(&mut fresh).expect("capture");
    assert_eq!(differing_px(&img, &expected), 0);
}

/// Regression: interning hashed every entity's material; the cache must also never map a
/// key to an id that the garbage collector freed and handed to another material.
#[test]
fn material_cache_survives_id_reuse() {
    let mut t = MaterialTable::default();
    let mut buf = TrackedBuffer::new("test", wgpu::BufferUsages::UNIFORM);
    let mut r = 0;
    let a = Material::srgb(1.0, 0.0, 0.0);
    let b = Material::srgb(0.0, 1.0, 0.0);
    let (ia, _) = t.intern(&a, 1, &mut buf, 256, &mut r);
    for frame in 2..10 {
        assert_eq!(t.intern(&a, frame, &mut buf, 256, &mut r).0, ia);
    }
    assert_eq!(t.misses, 1, "only the first intern misses");
    // `a` idle long enough to be collected; `b` takes over its id.
    t.gc(1024);
    let (ib, _) = t.intern(&b, 1024, &mut buf, 256, &mut r);
    assert_eq!(ib, ia, "freed id reused");
    let (ia2, _) = t.intern(&a, 1025, &mut buf, 256, &mut r);
    assert_ne!(ia2, ib, "a stale cache entry must not map `a` to `b`'s id");
    assert!(t.entries[ia2 as usize].key == MaterialKey::new(&a));
    assert!(t.entries[ib as usize].key == MaterialKey::new(&b));
}

/// Regression: explicit-buffer bounds were recomputed every frame after a single
/// `swap_remove` — a full scan through a per-instance layout search — on every path,
/// although only CPU culling reads them. GPU paths now never compute them; the CPU path
/// recomputes only the edited block.
#[test]
fn explicit_bounds_are_computed_only_for_cpu_culling() {
    use crate::{BufferUsage, Camera3d, InstanceBuffer, InstanceCompact, InstancedMesh};
    use space3d_core::{MeshData, Transform};
    let Some(mut app) = headless_app(32, 32) else { return };
    let best = app.resources.expect::<Renderer>().capabilities().best_draw_path;
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.1));
    let n = 50_000;
    let data = (0..n)
        .map(|i| InstanceCompact::new(Vec3::new(i as f32 * 0.01, 0.0, 0.0), glam::Quat::IDENTITY, 1.0))
        .collect();
    let h = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::default(),
        instances: h,
    },));
    app.world
        .spawn((Transform::from_xyz(0.0, 0.0, 20.0), Camera3d::default()));
    let step = |app: &mut space3d_core::App, frame: usize| {
        let buffers = app.resources.expect_mut::<InstanceBuffers>();
        let b = buffers.get_mut::<InstanceCompact>(h).unwrap();
        let v = b.swap_remove(frame * 997 % b.len());
        b.push(v);
        app.step_ticks(1);
        assert_eq!(app.resources.expect::<crate::RenderStats>().views, 1);
        let buffers = app.resources.expect::<InstanceBuffers>();
        buffers.buffers.get(h.raw()).unwrap().bounds_scanned()
    };
    if best != DrawPath::CpuDirect {
        set_path(&mut app, best);
        for frame in 0..5 {
            assert_eq!(step(&mut app, frame), 0, "{best:?} frame {frame}");
        }
    }
    set_path(&mut app, DrawPath::CpuDirect);
    assert_eq!(step(&mut app, 5), n, "first CPU-culled frame computes every block");
    for frame in 6..10 {
        assert!(
            step(&mut app, frame) <= 2 * crate::instance::CULL_BLOCK,
            "frame {frame}"
        );
    }
}

#[test]
fn instance_sphere_kinds() {
    let mesh = Sphere {
        center: Vec3::ZERO,
        radius: 1.0,
    };
    let c = InstanceCompact::new(Vec3::new(1.0, 2.0, 3.0), glam::Quat::IDENTITY, 2.0);
    let s = instance_sphere(&mesh, InstanceCompact::LAYOUT.cull_source(), bytemuck::bytes_of(&c)).unwrap();
    assert_eq!(s.center, Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(s.radius, 2.0);
}

/// The texture pool hands out free textures of an equal description, creates only what it
/// lacks and evicts textures unused for 3 frames.
#[test]
fn texture_pool_reuses_and_evicts() {
    let Some(app) = headless_app(4, 4) else { return };
    let r = app.resources.expect::<Renderer>();
    let mut pool = TexturePool::default();
    let desc = PoolDesc {
        size: (32, 16),
        format: wgpu::TextureFormat::Rgba8Unorm,
        mips: 3,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
    };
    pool.begin_frame(1);
    let a = pool.acquire(&r.device, desc);
    let b = pool.acquire(&r.device, desc);
    assert_ne!(a, b, "in use: a second texture");
    assert_eq!(pool.get(a).mips.len(), 3);
    assert_eq!(pool.take_created(), 2);
    pool.release_all();
    pool.begin_frame(2);
    let again = pool.acquire(&r.device, desc);
    assert_eq!((again, pool.get(again).generation), (a, 1), "request order, reused");
    assert_eq!(pool.take_created(), 0, "steady state creates nothing");
    pool.release_all();
    pool.begin_frame(5);
    pool.evict();
    let fresh = pool.acquire(&r.device, desc);
    assert_eq!(pool.get(fresh).generation, 3, "evicted after 3 unused frames");
    assert_eq!(r.gpu_error_count(), 0);
}

/// The CPU mirrors of the tonemappers (`Tonemapper::apply`) match the shader's `s3d_tonemap`
/// through the forward shader's `Tonemapped` output (graphics spec F3): unlit quads of radiance
/// 0.05…64 (grey and a saturated orange), each operator and two exposures, within 2/255 of the
/// CPU value encoded to sRGB.
#[test]
fn tonemapper_cpu_mirror_matches_gpu() {
    use crate::components::{Camera3d, Mesh3d, Projection, linear_to_srgb};
    use crate::output::{OutputEncoding, Tonemapper};
    use space3d_core::{Transform, prelude::MeshData};
    const VALUES: [f32; 8] = [0.05, 0.18, 0.5, 1.0, 2.0, 4.0, 16.0, 64.0];
    const HUES: [Vec3; 2] = [Vec3::ONE, Vec3::new(1.0, 0.45, 0.12)];
    let Some(mut app) = headless_app(8 * 8, 8 * 2) else {
        return;
    };
    let quad = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(1.0));
    for (row, hue) in HUES.iter().enumerate() {
        for (col, v) in VALUES.iter().enumerate() {
            let c = *hue * *v;
            app.world.spawn((
                Transform::from_xyz(col as f32 - 3.5, 0.5 - row as f32, 0.0)
                    .with_rotation(glam::Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
                Mesh3d(quad),
                Material::unlit(c.extend(1.0)),
            ));
        }
    }
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 100.0,
            }),
    ));
    let mut checked = 0;
    for tonemapper in [
        Tonemapper::None,
        Tonemapper::Reinhard,
        Tonemapper::AcesFitted,
        Tonemapper::AgX,
        Tonemapper::KhronosPbrNeutral,
        Tonemapper::AgXPunchy,
        Tonemapper::KhronosPbrNeutralNoToe,
    ] {
        for exposure in [1.0f32, 0.25] {
            app.resources.expect_mut::<Renderer>().frame.force_output =
                Some((OutputEncoding::Tonemapped, exposure, tonemapper));
            let img = crate::render_to_image(&mut app).expect("capture");
            for (row, hue) in HUES.iter().enumerate() {
                for (col, v) in VALUES.iter().enumerate() {
                    let cpu = tonemapper.apply(*hue * *v * exposure);
                    let want = cpu.to_array().map(|c| (linear_to_srgb(c) * 255.0).round());
                    let got = img.get(col as u32 * 8 + 4, row as u32 * 8 + 4);
                    for ch in 0..3 {
                        assert!(
                            (got[ch] as f32 - want[ch]).abs() <= 2.0,
                            "{tonemapper:?} ×{exposure} of {:?}: GPU {got:?}, CPU {want:?}",
                            *hue * *v
                        );
                    }
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 7 * 2 * 16);
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}

/// Point lights per view (graphics spec D19): up to `MAX_POINT_LIGHTS` every light in query
/// order (the original renderer's lights, bit for bit); past it the nearest to the camera, still
/// in query order, ties broken by query order.
#[test]
fn point_light_selection_keeps_query_order() {
    use crate::pipeline::{GpuPointLight, MAX_POINT_LIGHTS};
    let light = |x: f32, id: f32| GpuPointLight {
        position_range: [x, 0.0, 0.0, 1.0],
        color: [id, 0.0, 0.0, 1.0],
    };
    let mut scratch = Vec::new();
    let mut out = [GpuPointLight::default(); MAX_POINT_LIGHTS];
    let few: Vec<_> = (0..MAX_POINT_LIGHTS)
        .map(|i| light(100.0 - i as f32, i as f32))
        .collect();
    assert_eq!(
        select_point_lights(&few, Vec3::ZERO, &mut scratch, &mut out),
        MAX_POINT_LIGHTS
    );
    assert!(
        out.iter()
            .zip(&few)
            .all(|(a, b)| bytemuck::bytes_of(a) == bytemuck::bytes_of(b)),
        "≤ 16 lights: unchanged, in query order"
    );
    // 24 lights at x = 0..24 (ids = query order), shuffled positions; camera at x = 30: the
    // nearest 16 are x = 8..24.
    let many: Vec<_> = (0..24).map(|i| light(((i * 7) % 24) as f32, i as f32)).collect();
    assert_eq!(select_point_lights(&many, Vec3::X * 30.0, &mut scratch, &mut out), 16);
    let ids: Vec<u32> = out.iter().map(|l| l.color[0] as u32).collect();
    let mut expected: Vec<u32> = (0..24).filter(|i| (i * 7) % 24 >= 8).collect();
    expected.sort_unstable();
    assert_eq!(ids, expected, "nearest 16, in query order");
    // Equal distances: the first in query order win.
    let ties: Vec<_> = (0..20).map(|i| light(5.0, i as f32)).collect();
    select_point_lights(&ties, Vec3::ZERO, &mut scratch, &mut out);
    assert_eq!(out.map(|l| l.color[0] as u32), std::array::from_fn(|i| i as u32));
}

/// With more than 16 point lights, each view shades with the 16 nearest to its camera
/// (graphics spec D19): a light far from one view but next to the other lights only the second
/// view, although it comes last in query order (the original renderer dropped it everywhere).
#[test]
fn point_lights_over_16_pick_nearest_per_view() {
    use crate::components::{Camera3d, Mesh3d, PointLight, Projection};
    use space3d_core::{Transform, prelude::MeshData};
    let Some(mut app) = headless_app(64, 32) else { return };
    let plane = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(4.0));
    // Two floor patches 1000 units apart, each seen by its own half-screen camera from above.
    for x in [0.0, 1000.0] {
        app.world.spawn((
            Transform::from_xyz(x, 0.0, 0.0),
            Mesh3d(plane),
            Material::color(Vec4::ONE).with_metallic_roughness(0.0, 1.0),
        ));
    }
    for (i, x) in [0.0f32, 1000.0].into_iter().enumerate() {
        app.world.spawn((
            Transform::from_xyz(x, 5.0, 0.0).looking_at(Vec3::new(x, 0.0, 0.0), Vec3::NEG_Z),
            Camera3d {
                viewport: Some([0.5 * i as f32, 0.0, 0.5, 1.0]),
                clear_color: Some(Vec4::new(0.0, 0.0, 0.0, 1.0)),
                projection: Projection::Orthographic {
                    height: 4.0,
                    near: 0.1,
                    far: 50.0,
                },
                order: i as i32,
                ..Default::default()
            },
        ));
    }
    app.insert_resource(crate::components::AmbientLight {
        color: Vec3::ONE,
        intensity: 0.0,
    });
    // 16 dim red lights near patch A (query order first), then one bright green light above
    // patch B.
    for k in 0..16 {
        app.world.spawn((
            Transform::from_xyz(-1.5 + 0.2 * k as f32, 0.5, 0.0),
            PointLight {
                color: Vec3::new(1.0, 0.0, 0.0),
                intensity: 0.2,
                range: 3.0,
            },
        ));
    }
    app.world.spawn((
        Transform::from_xyz(1000.0, 1.0, 0.0),
        PointLight {
            color: Vec3::new(0.0, 1.0, 0.0),
            intensity: 8.0,
            range: 6.0,
        },
    ));
    let img = crate::render_to_image(&mut app).expect("capture");
    let (a, b) = (img.get(16, 16), img.get(48, 16));
    assert!(a[0] > 20 && a[1] == 0, "view A: the 16 red lights, no green: {a:?}");
    assert!(
        b[1] > 60 && b[0] == 0,
        "view B: the green light (17th in query order): {b:?}"
    );
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}

/// The scene of the capture-replay tests: a bright unlit cube and a quad under an orthographic
/// camera. With `post`: the camera has post, a PiP post camera (order 1) and a gizmo display
/// view (order 10) join it; otherwise `render_scale` makes the group an Ldr passthrough group.
fn replay_scene_app(samples: u32, scale: f32, post: bool) -> Option<space3d_core::App> {
    use crate::components::{Camera3d, Mesh3d, Projection};
    use crate::post::api::PostProcess;
    use space3d_core::{Transform, prelude::MeshData};
    let mut app = headless_app(64, 48)?;
    app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
    app.resources.expect_mut::<crate::FrameSettings>().render_scale = scale;
    let (cube, quad) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(1.0)), a.add_mesh(MeshData::plane(0.5)))
    };
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).with_rotation(glam::Quat::from_rotation_y(0.6)),
        Mesh3d(cube),
        Material::unlit(Vec4::new(3.0, 1.0, 0.2, 1.0)),
    ));
    app.world.spawn((
        Transform::from_xyz(0.8, 0.6, 1.0).with_rotation(glam::Quat::from_rotation_x(1.2)),
        Mesh3d(quad),
        Material::unlit(Vec4::new(0.2, 0.9, 0.3, 1.0)),
    ));
    let ortho = Projection::Orthographic {
        height: 3.0,
        near: 0.1,
        far: 100.0,
    };
    let camera = |order: i32, viewport: Option<[f32; 4]>, clear: Option<Vec4>| {
        (
            Transform::from_xyz(0.0, 0.0, 5.0),
            Camera3d {
                order,
                viewport,
                clear_color: clear,
                projection: ortho,
                ..Default::default()
            },
        )
    };
    let main = app.world.spawn(camera(0, None, Some(Vec4::new(0.05, 0.05, 0.1, 1.0))));
    if post {
        app.world.insert_one(main, PostProcess::default()).unwrap();
        let pip = app.world.spawn(camera(
            1,
            Some([0.6, 0.05, 0.35, 0.35]),
            Some(Vec4::new(0.3, 0.0, 0.0, 1.0)),
        ));
        app.world
            .insert_one(pip, PostProcess::default().with_exposure(1.5))
            .unwrap();
        app.world.spawn(camera(10, Some([0.0, 0.5, 0.5, 0.5]), None));
    }
    Some(app)
}

/// Fill `view`'s texture with magenta: what a capture shows of it afterwards was redrawn.
fn paint_magenta(r: &Renderer, view: &wgpu::TextureView) {
    let mut encoder = r.device.create_command_encoder(&Default::default());
    let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: 1.0,
                    g: 0.0,
                    b: 1.0,
                    a: 1.0,
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
    r.queue.submit([encoder.finish()]);
}

/// The windowed-capture replay of an intermediate primary group (spec §1.10) re-runs only its
/// composites from the persisted scene texture, then its display views and the overlays: the
/// capture equals the frame, for an Hdr group (post camera + PiP post camera + gizmo display
/// view, dither on) and an Ldr passthrough group (render scale), with and without MSAA. The
/// offscreen target is painted magenta first, so everything the capture shows was replayed.
#[test]
fn replay_path_equals_frame_output() {
    for (samples, scale, post) in [(1, 1.0, true), (4, 1.0, true), (1, 0.5, false), (4, 0.75, false)] {
        let Some(mut app) = replay_scene_app(samples, scale, post) else {
            return;
        };
        let kinds = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        app.resources.expect_mut::<crate::RenderOverlays>().insert(
            "rect",
            0,
            RectOverlay {
                rect: [2, 2, 6, 6],
                pipeline: None,
                kinds: kinds.clone(),
                stage: crate::OverlayStage::AfterTonemap,
            },
        );
        let frame = crate::render_to_image(&mut app).expect("capture");
        let r = app.resources.expect_mut::<Renderer>();
        assert_eq!(r.stats.intermediate_groups, 1);
        assert_eq!(r.stats.post_groups, if post { 2 } else { 0 });
        // Paint the offscreen target magenta: the replay must redraw every pixel.
        paint_magenta(r, r.offscreen.color_view.as_ref().unwrap());
        r.frame.force_capture_replay = true;
        let replayed = r.capture_blocking().expect("replayed capture");
        assert_eq!(r.gpu_error_count(), 0);
        assert!(
            frame.pixels == replayed.pixels,
            "×{samples} scale {scale} post {post}: the replay differs from the frame ({} px)",
            differing_px(&frame, &replayed)
        );
        use crate::OverlayTargetKind as K;
        assert_eq!(*kinds.lock().unwrap(), [K::Offscreen, K::Capture]);
    }
}

/// Regression: a window whose frame gets no surface texture (occluded or minimized — every
/// frame of a background-mode window on macOS) skips every primary view, the scene views of an
/// intermediate group included. The capture replay composited the scene texture as it was —
/// an older frame's, or zeros — so windowed captures of post-processed frames came back
/// transparent black, and `capture_scene_hdr` returned zeros as a success. Captures now render
/// the skipped scene views into `S` first (their node draws, Distortion pass and HdrScene
/// overlays as replays), then composite: the capture equals the frame of an unoccluded
/// window, for Hdr (post + PiP post + gizmo display view, a distortion writer) and Ldr
/// passthrough groups, with and without MSAA. `S`, the offscreen target and the distortion
/// buffers are painted magenta before the occluded frame. A capture of a frame without any
/// primary view is an error, not an older frame's scene.
#[test]
fn occluded_window_captures_render_the_skipped_scene_views() {
    use crate::{FrameKind, OverlayTargetKind as K, PhaseSlot, RenderPhase};
    for (samples, scale, post) in [(1, 1.0, true), (4, 1.0, true), (1, 0.5, false), (4, 0.75, false)] {
        let Some(mut app) = replay_scene_app(samples, scale, post) else {
            return;
        };
        let (hdr_kinds, after_kinds) = (
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        );
        let overlays = app.resources.expect_mut::<crate::RenderOverlays>();
        overlays.insert(
            "hdr",
            0,
            RectOverlay {
                rect: [12, 8, 8, 6],
                pipeline: None,
                kinds: hdr_kinds.clone(),
                stage: crate::OverlayStage::HdrScene,
            },
        );
        overlays.insert(
            "after",
            1,
            RectOverlay {
                rect: [2, 2, 6, 6],
                pipeline: None,
                kinds: after_kinds.clone(),
                stage: crate::OverlayStage::AfterTonemap,
            },
        );
        // A distortion writer (0.004 view heights to +x in every post group).
        let node = KindNode::new(
            vec![PhaseSlot::new(RenderPhase::Distortion, 0)],
            [0.004 / 0.0625, 0.0, 0.0, 0.0],
        );
        let draws = node.draws.clone();
        app.resources.expect_mut::<crate::RenderNodes>().insert("writer", node);
        let what = format!("×{samples} scale {scale} post {post}");

        let expected = crate::render_to_image(&mut app).expect("capture");
        let r = app.resources.expect_mut::<Renderer>();
        let expected_scene = r.capture_scene_hdr_blocking().expect("scene capture");
        let writers = draws.lock().unwrap().len();
        assert_eq!(writers, if post { 2 } else { 0 }, "{what}");
        let distortion: Vec<_> = r
            .frame
            .cameras
            .iter()
            .filter_map(|c| r.post.distortion_view(c.id))
            .map(|(view, _, _)| view)
            .collect();
        assert_eq!(distortion.len(), writers, "{what}");
        let paint = |r: &Renderer| {
            let scene = r.frame.group_textures[&GroupTarget::Primary].scene_view.clone();
            paint_magenta(r, &scene);
            paint_magenta(r, r.offscreen.color_view.as_ref().unwrap());
            for view in &distortion {
                paint_magenta(r, view);
            }
        };
        paint(r);
        r.frame.simulate_occluded = true;
        r.frame.force_capture_replay = true;

        // The occluded frame draws nothing; its capture renders the scene views, then composites.
        let replayed = crate::render_to_image(&mut app).expect("replayed capture");
        let r = app.resources.expect_mut::<Renderer>();
        assert_eq!(r.stats.draw_calls, 0, "{what}: the occluded frame drew");
        assert_eq!(r.gpu_error_count(), 0);
        assert!(
            expected.pixels == replayed.pixels,
            "{what}: the capture of the occluded frame differs from the frame ({} px)",
            differing_px(&expected, &replayed)
        );
        assert_eq!(*hdr_kinds.lock().unwrap(), [K::Offscreen, K::Capture], "{what}");
        assert_eq!(*after_kinds.lock().unwrap(), [K::Offscreen, K::Capture], "{what}");
        let replay_draws: Vec<_> = draws.lock().unwrap()[writers..].iter().map(|d| d.1).collect();
        assert_eq!(replay_draws, vec![FrameKind::CaptureReplay; writers], "{what}");
        // `S` now holds the frame: the scene capture copies it without rendering again.
        let scene = r.capture_scene_hdr_blocking().expect("scene capture");
        assert!(scene.pixels == expected_scene.pixels, "{what}: scene texture");
        assert_eq!(hdr_kinds.lock().unwrap().len(), 2, "{what}");

        // A scene capture right after an occluded frame renders the skipped scene views itself.
        paint(r);
        let mut renderer = app.resources.remove::<Renderer>().unwrap();
        renderer.render_frame(&app.world, &mut app.resources);
        let scene = renderer.capture_scene_hdr_blocking().expect("scene capture");
        assert!(
            scene.pixels == expected_scene.pixels,
            "{what}: scene capture of an occluded frame"
        );
        assert_eq!(
            *hdr_kinds.lock().unwrap(),
            [K::Offscreen, K::Capture, K::Capture],
            "{what}"
        );
        assert_eq!(renderer.gpu_error_count(), 0);

        // No primary view: the scene texture of an earlier frame is not returned.
        renderer.frame.simulate_occluded = false;
        renderer.frame.force_capture_replay = false;
        let cameras: Vec<_> = app
            .world
            .query::<(space3d_core::Entity, &crate::Camera3d)>()
            .iter()
            .map(|(e, _)| e)
            .collect();
        for e in cameras {
            app.world.despawn(e).unwrap();
        }
        renderer.render_frame(&app.world, &mut app.resources);
        assert!(renderer.capture_scene_hdr_blocking().is_err(), "{what}");
        assert_eq!(renderer.gpu_error_count(), 0);
    }
}

/// A render node drawing a solid full-screen triangle per slot (no bind groups, depth test
/// Always, no writes), counting `prepare` calls and logging the frame kind of every draw.
struct KindNode {
    slots: Vec<crate::PhaseSlot>,
    color: [f32; 4],
    prepares: std::sync::Arc<std::sync::atomic::AtomicU32>,
    draws: std::sync::Arc<std::sync::Mutex<Vec<(usize, crate::FrameKind, crate::PassTarget)>>>,
    pipelines: Vec<(crate::PassTarget, wgpu::RenderPipeline)>,
}

impl KindNode {
    fn new(slots: Vec<crate::PhaseSlot>, color: [f32; 4]) -> Self {
        Self {
            slots,
            color,
            prepares: Default::default(),
            draws: Default::default(),
            pipelines: Vec::new(),
        }
    }
}

impl crate::RenderNode for KindNode {
    fn slots(&self) -> &[crate::PhaseSlot] {
        &self.slots
    }

    fn prepare(&mut self, _: &mut crate::PrepareContext<'_>) {
        self.prepares.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &crate::DrawContext<'_>) {
        self.draws.lock().unwrap().push((ctx.slot, ctx.frame.kind, ctx.target));
        if !self.pipelines.iter().any(|(t, _)| *t == ctx.target) {
            let [r, g, b, a] = self.color;
            let module = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("kind node"),
                source: wgpu::ShaderSource::Wgsl(
                    format!(
                        "{}\n@fragment fn fs() -> @location(0) vec4<f32> {{ return vec4<f32>({r:?}, {g:?}, {b:?}, {a:?}); }}",
                        crate::FULLSCREEN_WGSL
                    )
                    .into(),
                ),
            });
            let t = ctx.target;
            let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("kind node"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: t.depth_stencil(wgpu::CompareFunction::Always, false),
                multisample: t.multisample(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(t.color_target(None))],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.pipelines.push((t, pipeline));
        }
        let (_, p) = self.pipelines.iter().find(|(t, _)| *t == ctx.target).unwrap();
        pass.set_pipeline(p);
        pass.draw(0..3, 0..1);
    }
}

/// Capture replays never call `prepare` (or `prepare_view`): they call `draw` again with
/// `FrameKind::CaptureReplay`, SceneRead passes included, and give the frame's pixels — with
/// and without MSAA (graphics spec §1.10).
#[test]
fn prepare_once_per_frame_not_in_replay() {
    use crate::{FrameKind, PhaseSlot, RenderPhase, SceneReads};
    for samples in [1, 4] {
        let Some(mut app) = split_screen_app(64, 48, samples) else {
            return;
        };
        // One view: with MSAA, views sharing the target draw depth readers in-pass instead.
        let cameras: Vec<_> = app
            .world
            .query::<(space3d_core::Entity, &crate::Camera3d)>()
            .iter()
            .map(|(e, _)| e)
            .collect();
        for e in cameras.iter().skip(1) {
            app.world.despawn(*e).unwrap();
        }
        app.world.get::<&mut crate::Camera3d>(cameras[0]).unwrap().viewport = None;
        let node = KindNode::new(
            vec![
                PhaseSlot::new(RenderPhase::Opaque, 5),
                PhaseSlot::new(RenderPhase::Transparent, 5).reading(SceneReads::DEPTH),
            ],
            [0.0, 0.0, 0.0, 0.0],
        );
        let (prepares, draws) = (node.prepares.clone(), node.draws.clone());
        app.resources.expect_mut::<crate::RenderNodes>().insert("kind", node);
        let frame = crate::render_to_image(&mut app).expect("capture");
        let r = app.resources.expect_mut::<Renderer>();
        let read_passes = r.stats.scene_read_passes;
        r.frame.force_capture_replay = true;
        let replayed = r.capture_blocking().expect("replayed capture");
        assert_eq!(r.gpu_error_count(), 0);
        assert_eq!(prepares.load(std::sync::atomic::Ordering::Relaxed), 1, "×{samples}");
        let kinds: Vec<_> = draws.lock().unwrap().iter().map(|(s, k, _)| (*s, *k)).collect();
        assert_eq!(
            kinds,
            [
                (0, FrameKind::Frame),
                (1, FrameKind::Frame),
                (0, FrameKind::CaptureReplay),
                (1, FrameKind::CaptureReplay)
            ],
            "×{samples}"
        );
        // With MSAA the depth is resolved for the SceneRead pass; GL draws in-pass instead.
        let resolves = r.frame_graphics.msaa_depth_resolve;
        assert_eq!(
            read_passes,
            u32::from(samples == 1 || resolves),
            "×{samples}: a SceneRead pass"
        );
        assert!(
            frame.pixels == replayed.pixels,
            "×{samples}: the replay differs from the frame"
        );
    }
}

/// The Distortion phase writes the post group's distortion buffer: a writer adding a constant
/// `distortion_encode` output fills the half-resolution buffer with it (split signs), and the
/// composite binds it.
#[test]
fn distortion_buffer_holds_the_writers_output() {
    use crate::{PhaseSlot, RenderPhase};
    let Some(mut app) = headless_app(64, 48) else { return };
    app.world.spawn((
        space3d_core::Transform::from_xyz(0.0, 0.0, 5.0),
        crate::Camera3d::default(),
        crate::PostProcess::default(),
    ));
    // A writer drawing distortion_encode((0.01, -0.02), 1) = (0.16, 0, 0, 0.32) with the
    // distortion blend (One/One); the node draws a raw color: the encoded value.
    let (r, a) = (0.01 / 0.0625, 0.02 / 0.0625);
    let node = KindNode::new(vec![PhaseSlot::new(RenderPhase::Distortion, 0)], [r, 0.0, 0.0, a]);
    let draws = node.draws.clone();
    app.resources.expect_mut::<crate::RenderNodes>().insert("writer", node);
    crate::render_to_image(&mut app).expect("capture");
    let r = app.resources.expect::<Renderer>();
    assert_eq!(r.stats.distortion_passes, 1);
    let (_, kind, target) = draws.lock().unwrap()[0];
    assert_eq!(
        (kind, target.color),
        (crate::FrameKind::Frame, crate::DISTORTION_FORMAT)
    );
    let camera = r.frame.cameras[0].id;
    let (view, size, _) = r.post.distortion_view(camera).expect("distortion buffer");
    assert_eq!(size, (32, 24));
    let texture = view.texture();
    let padded = align_up(size.0 as u64 * 4, 256);
    let buffer = r.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: padded * size.1 as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = r.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: None,
            },
        },
        texture.size(),
    );
    r.queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    r.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = buffer.slice(..).get_mapped_range().unwrap();
    for y in 0..size.1 as usize {
        for x in 0..size.0 as usize {
            let p = &data[y * padded as usize + x * 4..][..4];
            assert_eq!(p, [41, 0, 0, 82], "({x}, {y})");
        }
    }
    drop(data);
    assert_eq!(r.gpu_error_count(), 0);
}

/// The distortion buffer is sized from the post group's rect in the scene texture `S` (half of
/// it by default, all of it with `DistortionResolution::Full`), so it follows the render scale
/// like the scene the writers distort — and like the scene depth they occlude against, at a fixed
/// ratio of 2 (or 1). A quarter-size post camera on a 64×48 target at render scale 0.5 has a
/// 16×12 rect in `S`: an 8×6 buffer (16×12 at full resolution).
#[test]
fn distortion_buffer_is_sized_from_the_scene_rect() {
    use crate::{DistortionResolution, PhaseSlot, RenderPhase};
    for (resolution, size) in [
        (DistortionResolution::Half, (8, 6)),
        (DistortionResolution::Full, (16, 12)),
    ] {
        let Some(mut app) = headless_app(64, 48) else { return };
        app.resources.expect_mut::<crate::FrameSettings>().render_scale = 0.5;
        let mut post = crate::PostProcess::default();
        post.distortion.resolution = resolution;
        app.world.spawn((
            space3d_core::Transform::from_xyz(0.0, 0.0, 5.0),
            crate::Camera3d {
                viewport: Some([0.5, 0.0, 0.5, 0.5]),
                ..Default::default()
            },
            post,
        ));
        let node = KindNode::new(vec![PhaseSlot::new(RenderPhase::Distortion, 0)], [0.0; 4]);
        let draws = node.draws.clone();
        app.resources.expect_mut::<crate::RenderNodes>().insert("writer", node);
        crate::render_to_image(&mut app).expect("capture");
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.stats.distortion_passes, 1);
        assert_eq!(draws.lock().unwrap().len(), 1);
        let (_, actual, _) = r
            .post
            .distortion_view(r.frame.cameras[0].id)
            .expect("distortion buffer");
        assert_eq!(actual, size, "{resolution:?}");
        assert_eq!(r.gpu_error_count(), 0);
    }
}

/// HdrScene overlays in capture replays (graphics spec §1.5, §1.10): a Direct primary group
/// replays them (kind `Capture`) after its re-recorded views; an intermediate group does not
/// call them again — they are already in the persisted scene color, which the replay only
/// composites (risk R12). Either way the capture equals the frame.
#[test]
fn hdr_scene_overlay_not_replayed_in_hdr() {
    use crate::OverlayTargetKind as K;
    for post in [false, true] {
        let Some(mut app) = split_screen_app(64, 48, 1) else {
            return;
        };
        if post {
            let cams: Vec<_> = app
                .world
                .query::<(space3d_core::Entity, &crate::Camera3d)>()
                .iter()
                .map(|(e, _)| e)
                .collect();
            // No bloom: the yellow square's own glow would tint the pixel checked below.
            for e in cams {
                app.world
                    .insert_one(e, crate::PostProcess::default().without_bloom())
                    .unwrap();
            }
        }
        let kinds = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        app.resources.expect_mut::<crate::RenderOverlays>().insert(
            "hdr",
            0,
            RectOverlay {
                rect: [24, 16, 16, 16],
                pipeline: None,
                kinds: kinds.clone(),
                stage: crate::OverlayStage::HdrScene,
            },
        );
        let frame = crate::render_to_image(&mut app).expect("capture");
        // Yellow, tonemapped with the scene (AgX, dithered) when post runs.
        let yellow = if post {
            crate::Tonemapper::AgX.apply(Vec3::new(1.0, 1.0, 0.0))
        } else {
            Vec3::new(1.0, 1.0, 0.0)
        };
        let px = frame.get(30, 20);
        for (c, v) in px.iter().zip(yellow.to_array()) {
            let v = (crate::linear_to_srgb(v) * 255.0).round() as u8;
            assert!(c.abs_diff(v) <= 1, "post {post}: {px:?} vs {yellow}");
        }
        let r = app.resources.expect_mut::<Renderer>();
        r.frame.force_capture_replay = true;
        let replayed = r.capture_blocking().expect("replayed capture");
        assert_eq!(r.gpu_error_count(), 0);
        assert!(frame.pixels == replayed.pixels, "post {post}: the replay differs");
        let expected: &[K] = if post {
            &[K::Offscreen]
        } else {
            &[K::Offscreen, K::Capture]
        };
        assert_eq!(*kinds.lock().unwrap(), expected, "post {post}");
    }
}

/// Depth views (shadow maps) are drawn once per frame, before the camera views, and never by a
/// capture replay (graphics spec §1.12): the replay reuses the frame's maps. Camera views and
/// depth views bind group 0 from one cache keyed by the lighting's resources (with the neutral
/// lighting: one bind group for every view).
#[test]
fn depth_views_are_never_replayed_and_share_view_bind_groups() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let cube = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(space3d_core::MeshData::cube(1.0));
    app.world.spawn((
        space3d_core::Transform::default(),
        crate::Mesh3d(cube),
        crate::Material::default(),
    ));
    app.world.spawn((
        space3d_core::Transform::from_xyz(0.0, 0.0, 5.0),
        crate::Camera3d::default(),
    ));
    let light = glam::camera::rh::proj::directx::orthographic(-2.0, 2.0, -2.0, 2.0, 0.0, 10.0)
        * glam::camera::rh::view::look_at_mat4(glam::Vec3::new(0.0, 5.0, 0.0), glam::Vec3::ZERO, glam::Vec3::NEG_Z);
    let r = app.resources.expect_mut::<Renderer>();
    crate::testing::add_depth_view(r, light, 16, Default::default());
    crate::testing::add_depth_view(r, light, 32, Default::default());
    crate::render_to_image(&mut app).expect("capture");
    let r = app.resources.expect_mut::<Renderer>();
    assert_eq!(r.frame.depth_passes_encoded, 2);
    assert_eq!(
        r.frame.view_bgs.len(),
        1,
        "camera and depth views share the neutral bind group"
    );
    r.frame.force_capture_replay = true;
    r.capture_blocking().expect("replayed capture");
    assert_eq!(r.frame.depth_passes_encoded, 2, "the replay draws no depth view");
    assert_eq!(r.gpu_error_count(), 0);
    crate::render_to_image(&mut app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().frame.depth_passes_encoded, 4);
}

/// The scene of the upload-budget tests: a cube that is drawn already, textured red, and five
/// more cubes with three blue textures between them, queued after it. Returns the first cube's
/// mesh and texture with its pixels, and the queued meshes and textures in their order.
#[allow(clippy::type_complexity)]
fn upload_queue_scene(
    app: &mut space3d_core::App,
) -> (
    (MeshHandle, crate::TextureHandle, usize),
    Vec<MeshHandle>,
    Vec<crate::TextureHandle>,
) {
    use crate::{Camera3d, Mesh3d};
    use space3d_core::{ImageData, MeshData, Transform};
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 8.0),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
    ));
    let assets = app.resources.expect_mut::<RenderAssets>();
    let first = assets.add_mesh(MeshData::cube(0.6));
    let red = assets.add_image(ImageData::filled(16, 16, [255, 0, 0, 255]));
    app.world.spawn((
        Transform::from_xyz(-4.0, 0.0, 0.0),
        Mesh3d(first),
        Material::unlit(Vec4::ONE).with_texture(red),
    ));
    let img = crate::render_to_image(app).expect("capture");
    let drawn = count_px(&img, [255, 0, 0]);
    assert!(drawn > 20, "the first cube is drawn: {drawn} px");
    let assets = app.resources.expect_mut::<RenderAssets>();
    let meshes: Vec<_> = (0..5).map(|_| assets.add_mesh(MeshData::cube(0.6))).collect();
    let textures: Vec<_> = (0..3)
        .map(|_| assets.add_image(ImageData::filled(16, 16, [0, 0, 255, 255])))
        .collect();
    for (i, mesh) in meshes.iter().enumerate() {
        app.world.spawn((
            Transform::from_xyz(-2.0 + i as f32, 0.0, 0.0),
            Mesh3d(*mesh),
            Material::unlit(Vec4::ONE).with_texture(textures[i % 3]),
        ));
    }
    ((first, red, drawn), meshes, textures)
}

/// Under a budget the queued textures and then the queued meshes upload over several frames,
/// the first queued first, and the picture is whole in every one of them: what was drawn stays
/// drawn (a texture being replaced with its previous contents), a cube appears once its mesh is
/// there, with its texture from its first frame (no mesh is uploaded while a texture waits), and
/// no pixel shows anything else.
#[test]
fn a_budgeted_queue_drains_in_order_and_what_is_drawn_stays_drawn() {
    use crate::{RenderStats, UploadBudget};
    use space3d_core::ImageData;
    let Some(mut app) = headless_app(192, 64) else { return };
    let ((_, red, drawn), meshes, textures) = upload_queue_scene(&mut app);
    // The red texture is replaced last of all: it waits behind the new ones.
    let assets = app.resources.expect_mut::<RenderAssets>();
    assert!(assets.update_image(red, ImageData::filled(16, 16, [0, 255, 0, 255])));
    // A cube is 24 vertices of 36 bytes and 36 indices: 1008 bytes; a texture 1024. Two cubes
    // fit a frame; a texture and a cube do not, and neither do two textures.
    let (cube, texture) = (1008u64, 1024u64);
    app.insert_resource(UploadBudget {
        bytes: Some(2 * cube),
        headless: true,
    });
    // Meshes and textures (the replaced one included) uploaded after each frame.
    let frames: [(u32, u32); 7] = [(0, 1), (0, 2), (0, 3), (0, 4), (2, 4), (4, 4), (5, 4)];
    let mut before = (0, 0);
    for (frame, (done_meshes, done_textures)) in frames.into_iter().enumerate() {
        let img = crate::render_to_image(&mut app).expect("capture");
        let stats = app.resources.expect::<RenderStats>().clone();
        let assets = app.resources.expect::<RenderAssets>();
        let left = (5 - done_meshes) as u64 * cube + (4 - done_textures) as u64 * texture;
        assert_eq!(
            (stats.pending_meshes, stats.pending_textures, stats.pending_upload_bytes),
            (5 - done_meshes, 4 - done_textures, left),
            "frame {frame}"
        );
        assert_eq!(assets.pending_upload_bytes(), left, "frame {frame}");
        assert_eq!(
            (stats.meshes_uploaded, stats.textures_uploaded),
            (done_meshes - before.0, done_textures - before.1),
            "frame {frame}"
        );
        before = (done_meshes, done_textures);
        // In the order they were queued: the first ones are there, the rest are not.
        for (i, m) in meshes.iter().enumerate() {
            assert_eq!(
                assets.is_mesh_uploaded(*m),
                i < done_meshes as usize,
                "frame {frame}, mesh {i}"
            );
        }
        for (i, t) in textures.iter().chain([&red]).enumerate() {
            assert_eq!(
                assets.is_texture_uploaded(*t),
                i < done_textures as usize,
                "frame {frame}, texture {i}"
            );
        }
        // The first cube is drawn in every frame: red until its new texture is there, green
        // from then on, and never anything between.
        let (reds, greens) = (count_px(&img, [255, 0, 0]), count_px(&img, [0, 255, 0]));
        let expected = if done_textures == 4 { (0, drawn) } else { (drawn, 0) };
        assert_eq!((reds, greens), expected, "frame {frame}");
        // A cube whose mesh is uploaded is drawn from that frame on.
        assert_eq!(stats.instances, 1 + done_meshes as u64, "frame {frame}");
        // Each of them is blue from its first frame, never white (the colour without the
        // texture): the textures were all there before the first of the meshes, and no texel
        // of a texture that is not wholly uploaded is ever sampled.
        let (whites, blues) = (count_px(&img, [255, 255, 255]), count_px(&img, [0, 0, 255]));
        assert_eq!(
            (whites, blues > 0),
            (0, done_meshes > 0),
            "frame {frame}: {whites} white, {blues} blue"
        );
        let black = count_px(&img, [0, 0, 0]);
        assert_eq!(
            black + reds + greens + whites + blues,
            192 * 64,
            "frame {frame}: a pixel of another colour"
        );
    }
    assert!(!app.resources.expect::<RenderAssets>().has_pending());
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}

/// Without a budget everything queued uploads in the next frame, as it always did; and a
/// budget that does not ask for headless renderers leaves a headless one alone, so a capture
/// shows the whole scene.
#[test]
fn without_a_budget_everything_uploads_in_one_frame() {
    use crate::{RenderStats, UploadBudget};
    for budget in [None, Some(UploadBudget::per_frame(1))] {
        let Some(mut app) = headless_app(192, 64) else { return };
        assert_eq!(*app.resources.expect::<UploadBudget>(), UploadBudget::default());
        if let Some(budget) = budget {
            app.insert_resource(budget);
        }
        let ((_, _, drawn), meshes, textures) = upload_queue_scene(&mut app);
        let img = crate::render_to_image(&mut app).expect("capture");
        let stats = app.resources.expect::<RenderStats>();
        assert_eq!((stats.meshes_uploaded, stats.textures_uploaded), (5, 3), "{budget:?}");
        assert_eq!(
            (stats.pending_meshes, stats.pending_textures, stats.pending_upload_bytes),
            (0, 0, 0),
            "{budget:?}"
        );
        let assets = app.resources.expect::<RenderAssets>();
        assert!(meshes.iter().all(|m| assets.is_mesh_uploaded(*m)));
        assert!(textures.iter().all(|t| assets.is_texture_uploaded(*t)));
        assert_eq!(assets.pending_upload_bytes(), 0);
        assert_eq!(count_px(&img, [255, 0, 0]), drawn);
        assert!(count_px(&img, [0, 0, 255]) > 3 * drawn && count_px(&img, [255, 255, 255]) == 0);
    }
}

/// What must not wait does not: an update of a mesh that is being drawn shows in the next
/// frame, and a render target is created in the next frame, however much is queued before them.
/// An explicit flush then uploads all the rest at once.
#[test]
fn mesh_updates_and_render_targets_do_not_wait_behind_the_queue() {
    use crate::{RenderStats, UploadBudget};
    use space3d_core::{ImageData, MeshData};
    let Some(mut app) = headless_app(192, 64) else { return };
    let ((first, _, drawn), meshes, textures) = upload_queue_scene(&mut app);
    app.insert_resource(UploadBudget {
        bytes: Some(1),
        headless: true,
    });
    let assets = app.resources.expect_mut::<RenderAssets>();
    // Queued last of all: twice the cube, and a render target.
    assert!(assets.update_mesh(first, MeshData::cube(1.2)));
    let target = assets.add_render_target(32, 32);
    assert!(!assets.is_mesh_uploaded(first) && !assets.is_texture_uploaded(target));
    let img = crate::render_to_image(&mut app).expect("capture");
    let stats = app.resources.expect::<RenderStats>().clone();
    let assets = app.resources.expect_mut::<RenderAssets>();
    // One new item a frame at this budget, the textures first: the first texture, and no new
    // mesh yet. The replaced mesh and the render target come on top of it.
    assert_eq!((stats.pending_meshes, stats.pending_textures), (5, 2));
    assert_eq!((stats.meshes_uploaded, stats.textures_uploaded), (1, 2));
    assert!(assets.is_mesh_uploaded(first) && assets.is_texture_uploaded(textures[0]));
    assert!(!assets.is_mesh_uploaded(meshes[0]) && !assets.is_texture_uploaded(textures[1]));
    assert!(assets.is_texture_uploaded(target));
    let grown = count_px(&img, [255, 0, 0]);
    assert!(
        grown > 3 * drawn,
        "the larger cube is drawn at once: {grown} px, {drawn} before"
    );
    // Resized, the target is created again in the next frame; replaced while it waits, a
    // texture keeps its place in the queue.
    assert!(assets.resize_render_target(target, 48, 48) && !assets.is_texture_uploaded(target));
    assert!(assets.update_image(textures[2], ImageData::filled(16, 16, [0, 0, 255, 255])));
    assert_eq!(assets.pending_textures, [textures[1], textures[2], target]);
    crate::render_to_image(&mut app).expect("capture");
    let stats = app.resources.expect::<RenderStats>().clone();
    assert_eq!((stats.pending_meshes, stats.pending_textures), (5, 1));
    let mut assets = std::mem::take(app.resources.expect_mut::<RenderAssets>());
    assert!(assets.is_texture_uploaded(target) && assets.is_texture_uploaded(textures[1]));
    assert_eq!(assets.pending_textures, [textures[2]]);
    // The explicit flush is not budgeted.
    app.resources.expect_mut::<Renderer>().upload_assets(&mut assets);
    assert_eq!(assets.pending_uploads(), (0, 0, 0));
    assert!(meshes.iter().all(|m| assets.is_mesh_uploaded(*m)));
    assert!(textures.iter().all(|t| assets.is_texture_uploaded(*t)));
    *app.resources.expect_mut::<RenderAssets>() = assets;
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}

/// A level of detail that is no mesh draws nothing, on every draw path and in the shadow maps:
/// near, the part is drawn and casts, and its sibling with the merged mesh does neither; far,
/// it is the other way round. Exactly one of the two is ever in a view.
#[test]
fn a_level_without_a_mesh_draws_nothing_in_any_view() {
    use crate::{Camera3d, LodSettings, Mesh3d, MeshLod, RenderStats, ShadowMode};
    use space3d_core::{MeshData, Transform};
    let Some(probe) = headless_app(8, 8) else { return };
    let best = probe.resources.expect::<Renderer>().capabilities().best_draw_path;
    drop(probe);
    // A light 10 units above the origin looking down, covering 8 × 8 units around it.
    let light = glam::camera::rh::proj::directx::orthographic(-4.0, 4.0, -4.0, 4.0, 0.0, 20.0)
        * glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 10.0, 0.0), Vec3::ZERO, Vec3::NEG_Z);
    for path in paths_up_to(best) {
        let mut app = headless_app(64, 64).expect("adapter");
        set_path(&mut app, path);
        crate::testing::add_depth_view(app.resources.expect_mut::<Renderer>(), light, 64, Default::default());
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 6.0),
            Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
        ));
        let assets = app.resources.expect_mut::<RenderAssets>();
        let (full, merged) = (
            assets.add_mesh(MeshData::cube(1.0)),
            assets.add_mesh(MeshData::cube(1.0)),
        );
        let casting =
            |rgb: [f32; 3]| Material::unlit(Vec4::new(rgb[0], rgb[1], rgb[2], 1.0)).with_shadows(ShadowMode::CastOnly);
        app.world.spawn((
            Transform::default(),
            Mesh3d(full),
            casting([1.0, 1.0, 1.0]),
            MeshLod::from_levels(1.0, &[Some(full), None]),
        ));
        app.world.spawn((
            Transform::default(),
            Mesh3d(MeshHandle::NONE),
            casting([1.0, 0.0, 0.0]),
            MeshLod::from_levels(1.0, &[None, Some(merged)]),
        ));
        // An entity whose mesh is no mesh is not drawn without a `MeshLod` either.
        app.world
            .spawn((Transform::default(), Mesh3d(MeshHandle::NONE), casting([0.0, 1.0, 0.0])));
        // The first frame uploads the meshes; the levels are picked from the second on.
        crate::render_to_image(&mut app).expect("capture");
        let frame = |app: &mut space3d_core::App, far: bool| {
            // Every size is under the first threshold, or none is.
            app.insert_resource(LodSettings {
                pixels: [if far { 1e9 } else { 0.0 }, 0.0, 0.0],
                ..LodSettings::default()
            });
            crate::lod::select_lods(&mut app.world, &app.resources);
            let img = crate::render_to_image(app).expect("capture");
            let ctx = format!("{path:?}, far {far}");
            let (white, red) = (count_px(&img, [255, 255, 255]), count_px(&img, [255, 0, 0]));
            assert_eq!((white > 50, red > 50), (!far, far), "{ctx}: {white} white, {red} red");
            assert_eq!((white == 0, red == 0), (far, !far), "{ctx}: {white} white, {red} red");
            assert_eq!(count_px(&img, [0, 255, 0]), 0, "{ctx}");
            let stats = app.resources.expect::<RenderStats>();
            assert_eq!((stats.depth_views, stats.shadow_casters), (1, 1), "{ctx}");
            let renderer = app.resources.expect::<Renderer>();
            assert_eq!(crate::testing::depth_view_instances(renderer, 0), 1, "{ctx}");
            assert_eq!(renderer.gpu_error_count(), 0, "{ctx}");
        };
        for far in [false, true, false, true] {
            frame(&mut app, far);
        }
    }
}
