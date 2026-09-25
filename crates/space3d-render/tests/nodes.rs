//! Render nodes (graphics spec §1.4, §1.6–§1.8, milestone F5): phase and slot ordering around
//! the built-in batches, per-view slot masks, the node lifecycle (prepare, prepare_view,
//! prewarm, draw), every scene-depth access mode against an analytic plane, the in-pass
//! fallback and the Distortion phase plumbing.

mod common;
use common::nodes::*;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;
use std::sync::{Arc, Mutex};

const W: u32 = 64;
const H: u32 = 32;

fn render(app: &mut App) -> ImageData {
    let img = render_to_image(app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    img
}

fn nodes(app: &mut App) -> &mut RenderNodes {
    app.resources.expect_mut::<RenderNodes>()
}

/// Pixel of the capture at normalized device coordinates `(x, y)` (y up).
fn at(img: &ImageData, x: f32, y: f32) -> [u8; 3] {
    let px = (((x + 1.0) * 0.5 * img.width as f32) as u32).min(img.width - 1);
    let py = (((1.0 - y) * 0.5 * img.height as f32) as u32).min(img.height - 1);
    let p = img.get(px, py);
    [p[0], p[1], p[2]]
}

/// Orthographic camera at z = 5 looking down -Z (world ±1 high, ±2 wide), clear blue.
fn ortho_camera(app: &mut App) -> Entity {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0))
            .with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 100.0,
            }),
    ))
}

/// An unlit camera-facing quad of `size` at `pos`.
fn quad(app: &mut App, color: Vec4, pos: Vec3, size: f32, blend: bool) {
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(size));
    app.world.spawn((
        Transform::from_xyz(pos.x, pos.y, pos.z).with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
        Mesh3d(mesh),
        Material {
            alpha_blend: blend,
            ..Material::unlit(color)
        },
    ));
}

fn solid(slot: PhaseSlot, color: [f32; 4], rect: [f32; 4]) -> SolidSlot {
    SolidSlot { slot, color, rect }
}

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const YELLOW: [f32; 4] = [1.0, 1.0, 0.0, 1.0];
const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const CYAN: [f32; 4] = [0.0, 1.0, 1.0, 1.0];
const ORANGE: [f32; 4] = [1.0, 0.0, 0.5, 1.0];
const BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Slots draw by phase (Opaque → Skybox → Transparent), then order, then registration;
/// built-in batches sit at order 0 of their phase and draw before nodes of the same order
/// (graphics spec §1.4, §1.7).
#[test]
fn slot_order_and_builtins_first() {
    let Some(mut app) = headless_app(W, H) else { return };
    ortho_camera(&mut app);
    // Built-ins: an opaque green quad in the center, a blended magenta one near the top.
    quad(&mut app, Vec4::new(0.0, 1.0, 0.0, 1.0), Vec3::ZERO, 1.0, false);
    quad(
        &mut app,
        Vec4::new(1.0, 0.0, 1.0, 1.0),
        Vec3::new(0.0, 0.8, 1.0),
        0.4,
        true,
    );
    let log = log();
    let (op, sky, tr) = (RenderPhase::Opaque, RenderPhase::Skybox, RenderPhase::Transparent);
    // Registered in an order unrelated to their slots' orders.
    nodes(&mut app).insert(
        "late-opaque",
        Solid::new(
            "late-opaque",
            vec![solid(PhaseSlot::new(op, 10), ORANGE, [-1.0, -1.0, 0.0, -0.6])],
            log.clone(),
        ),
    );
    nodes(&mut app).insert(
        "sky",
        Solid::new(
            "sky",
            vec![solid(PhaseSlot::new(sky, 0), BLACK, [-1.0, -1.0, 1.0, -0.6])],
            log.clone(),
        ),
    );
    nodes(&mut app).insert(
        "transparent",
        Solid::new(
            "transparent",
            vec![
                solid(PhaseSlot::new(tr, 0), CYAN, [0.0, 0.6, 1.0, 1.0]),
                solid(PhaseSlot::new(tr, -1), WHITE, [-1.0, 0.6, 1.0, 1.0]),
            ],
            log.clone(),
        ),
    );
    nodes(&mut app).insert(
        "opaque",
        Solid::new(
            "opaque",
            vec![
                solid(PhaseSlot::new(op, 0), YELLOW, [0.0, -1.0, 1.0, 0.5]),
                solid(PhaseSlot::new(op, -1), RED, [-1.0, -1.0, 1.0, 1.0]),
            ],
            log.clone(),
        ),
    );
    let schedule: Vec<_> = nodes(&mut app)
        .schedule()
        .map(|(n, s, _, _)| (n.to_string(), s))
        .collect();
    let img = render(&mut app);
    let draws: Vec<_> = log.lock().unwrap().draws.iter().map(|d| (d.node, d.slot)).collect();
    let expected = [
        ("opaque", 1),
        ("opaque", 0),
        ("late-opaque", 0),
        ("sky", 0),
        ("transparent", 1),
        ("transparent", 0),
    ];
    assert_eq!(draws, expected);
    // The built-in skybox and distortion nodes come first in the registry.
    let user: Vec<_> = schedule
        .iter()
        .filter(|(n, _)| !n.starts_with("space3d/"))
        .map(|(n, s)| (n.as_str(), *s))
        .collect();
    assert_eq!(user, expected, "RenderNodes::schedule is the draw order");
    assert_eq!(
        at(&img, -0.1, 0.0),
        [0, 255, 0],
        "built-in opaque after the order -1 slot"
    );
    assert_eq!(at(&img, -0.6, 0.0), [255, 0, 0], "order -1 opaque slot");
    assert_eq!(at(&img, 0.1, 0.0), [255, 255, 0], "order 0 slot after the built-in");
    assert_eq!(at(&img, -0.5, -0.8), [0, 0, 0], "skybox after every opaque slot");
    assert_eq!(
        at(&img, -0.03, 0.8),
        [255, 0, 255],
        "built-in blended after the order -1 slot"
    );
    assert_eq!(at(&img, -0.5, 0.8), [255, 255, 255], "order -1 transparent slot");
    assert_eq!(
        at(&img, 0.03, 0.8),
        [0, 255, 255],
        "order 0 transparent slot after the built-in"
    );
    let s = stats(&app);
    assert_eq!((s.node_draws, s.scene_read_passes, s.render_passes), (6, 0, 0));
}

/// Slots without work in a view cost nothing: no draw, no SceneRead pass, no extra pass, the
/// same pixels as without the node (graphics spec §1.4 step 8).
#[test]
fn slot_mask_skips_passes() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = ortho_camera(&mut app);
    app.world.insert_one(cam, PostProcess::passthrough()).unwrap();
    quad(&mut app, Vec4::new(0.0, 1.0, 0.0, 1.0), Vec3::ZERO, 1.0, false);
    let plain = render(&mut app);
    let base = stats(&app);
    let log = log();
    let node = nodes(&mut app).insert(
        "idle",
        Solid::new(
            "idle",
            vec![
                Solid::slot(PhaseSlot::new(RenderPhase::Opaque, -1), RED),
                Solid::slot(
                    PhaseSlot::new(RenderPhase::Transparent, 0).reading(SceneReads::DEPTH),
                    RED,
                ),
                Solid::slot(PhaseSlot::new(RenderPhase::Distortion, 0), RED),
            ],
            log.clone(),
        ),
    );
    node.lock().unwrap().mask = SlotMask::NONE;
    assert_eq!(render(&mut app), plain);
    let s = stats(&app);
    assert_eq!(
        (
            s.node_draws,
            s.scene_read_passes,
            s.distortion_passes,
            s.depth_resolves,
            s.render_passes
        ),
        (0, 0, 0, 0, base.render_passes)
    );
    assert!(log.lock().unwrap().draws.is_empty());
    // Only the reading slot has work: one SceneRead pass, one draw.
    node.lock().unwrap().mask = SlotMask::only(1);
    render(&mut app);
    let s = stats(&app);
    assert_eq!((s.node_draws, s.scene_read_passes), (1, 1));
    assert_eq!(s.render_passes, base.render_passes + 1);
}

/// `prepare` runs once per rendered frame and `prepare_view` once per camera view, before any
/// draw (capture replays call neither: `frame::tests::prepare_once_per_frame_not_in_replay`).
#[test]
fn prepare_once_per_frame() {
    let Some(mut app) = headless_app(W, H) else { return };
    ortho_camera(&mut app);
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d {
            order: 1,
            viewport: Some([0.5, 0.0, 0.5, 0.5]),
            ..Default::default()
        },
    ));
    let log = log();
    nodes(&mut app).insert(
        "n",
        Solid::new(
            "n",
            vec![Solid::slot(PhaseSlot::new(RenderPhase::Opaque, 0), RED)],
            log.clone(),
        ),
    );
    for _ in 0..3 {
        render(&mut app);
    }
    let l = log.lock().unwrap();
    assert_eq!(l.prepares.len(), 3);
    assert_eq!(l.views.len(), 6, "two views per frame");
    assert_eq!(l.draws.len(), 6);
    assert!(l.draws.iter().all(|d| d.kind == FrameKind::Frame));
    // Pipelines were built before the first draw, once per pass signature.
    assert_eq!(l.prewarms.len(), 1, "{:?}", l.prewarms);
}

/// `DrawContext` names the slot and phase being drawn and the exact pass signature.
#[test]
fn draw_context_reports_slot() {
    let Some(mut app) = headless_app(W, H) else { return };
    ortho_camera(&mut app);
    let log = log();
    let slots = vec![
        Solid::slot(PhaseSlot::new(RenderPhase::Transparent, 3), RED),
        Solid::slot(PhaseSlot::new(RenderPhase::Opaque, 0), RED),
        Solid::slot(
            PhaseSlot::new(RenderPhase::Transparent, 0).reading(SceneReads::DEPTH),
            RED,
        ),
        Solid::slot(PhaseSlot::new(RenderPhase::Skybox, 0), RED),
    ];
    nodes(&mut app).insert("n", Solid::new("n", slots, log.clone()));
    render(&mut app);
    let caps = app.resources.expect::<Renderer>().graphics_caps().clone();
    let l = log.lock().unwrap();
    let got: Vec<_> = l.draws.iter().map(|d| (d.slot, d.phase)).collect();
    assert_eq!(
        got,
        [
            (1, RenderPhase::Opaque),
            (3, RenderPhase::Skybox),
            (0, RenderPhase::Transparent),
            (2, RenderPhase::Transparent)
        ]
    );
    for d in &l.draws {
        assert_eq!(d.target_size, (W, H));
        assert_eq!(d.target.color, wgpu::TextureFormat::Rgba8UnormSrgb);
        assert_eq!(d.target.output, OutputEncoding::Linear);
        if d.slot == 2 {
            // The SceneRead pass (single sample, scene depth bound).
            assert_eq!(d.target.samples, 1);
            assert!(d.depth);
            let expected = if caps.read_only_depth {
                DepthAccess::ReadOnlyAttached
            } else {
                DepthAccess::SampledOnly
            };
            assert_eq!(d.target.depth_access, expected);
            assert_eq!(d.target.depth_read_only, caps.read_only_depth);
        } else {
            assert_eq!(d.target.depth, Some(wgpu::TextureFormat::Depth32Float));
            assert_eq!(
                (d.target.depth_access, d.target.depth_read_only, d.depth),
                (DepthAccess::None, false, false)
            );
        }
    }
    // Prewarmed before the first draw, with the targets the slots were drawn with.
    for d in &l.draws {
        assert!(l.prewarms.iter().any(|(_, s, t)| *s == d.slot && *t == d.target));
    }
}

/// Fraction of pixels of exactly `rgb`.
fn fraction(img: &ImageData, rgb: [u8; 3]) -> f64 {
    count_rgb(img, rgb) as f64 / (img.width * img.height) as f64
}

/// Render the analytic plane with a depth probe; returns the image and the targets it drew
/// with.
fn probe(app: &mut App) -> (ImageData, Vec<(PassTarget, bool)>) {
    analytic_plane_scene(app);
    let (probe, seen) = DepthProbe::new();
    nodes(app).insert("probe", probe);
    let img = render(app);
    let seen = seen.lock().unwrap().clone();
    (img, seen)
}

/// Single sample with read-only depth attachments (Metal, Vulkan, DX12, WebGPU): a SceneRead
/// pass with the depth attached read-only and sampled; view depth within 1 % of the plane.
#[test]
fn scene_depth_matches_analytic_plane_rods_1x() {
    let Some(mut app) = headless_app(W, H) else { return };
    let (img, seen) = probe(&mut app);
    let rods = app.resources.expect::<Renderer>().graphics_caps().read_only_depth;
    let access = if rods {
        DepthAccess::ReadOnlyAttached
    } else {
        DepthAccess::SampledOnly
    };
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].0.depth_access, seen[0].0.samples, seen[0].1),
        (access, 1, true)
    );
    assert!(
        fraction(&img, [0, 255, 0]) > 0.99,
        "{:.3} green",
        fraction(&img, [0, 255, 0])
    );
    let s = stats(&app);
    assert_eq!((s.scene_read_passes, s.depth_resolves), (1, 0));
}

/// 4× MSAA, one view: the multisampled depth is resolved (minimum of the samples) into a
/// single-sample depth, then the SceneRead pass reads it on the resolved color.
#[test]
fn scene_depth_matches_analytic_plane_rods_msaa_single_view() {
    let Some(mut app) = headless_app(W, H) else { return };
    app.resources.expect_mut::<Renderer>().set_msaa_samples(4);
    let (img, seen) = probe(&mut app);
    let caps = app.resources.expect::<Renderer>().graphics_caps().clone();
    let s = stats(&app);
    if !caps.msaa_depth_resolve {
        // GL: the in-pass fallback (no depth to read: the probe draws blue).
        assert_eq!((seen[0].0.depth_access, seen[0].0.samples), (DepthAccess::None, 4));
        assert!(fraction(&img, [0, 0, 255]) > 0.99);
        assert_eq!((s.scene_read_passes, s.depth_resolves), (0, 0));
        return;
    }
    assert_eq!(
        (seen[0].0.depth_access, seen[0].0.samples, seen[0].1),
        (DepthAccess::ReadOnlyAttached, 1, true)
    );
    assert!(
        fraction(&img, [0, 255, 0]) > 0.99,
        "{:.3} green",
        fraction(&img, [0, 255, 0])
    );
    assert_eq!((s.scene_read_passes, s.depth_resolves), (1, 1));
    assert!(s.textures_created >= 1, "the resolved depth");
    render(&mut app);
    assert_eq!(stats(&app).textures_created, 0, "created once");
}

/// Without read-only depth (GL, forced here): the SceneRead pass has no depth attachment and
/// the probe tests depth manually from the sampled texture.
#[test]
fn scene_depth_matches_analytic_plane_gl_manual() {
    let Some(mut app) = headless_app(W, H) else { return };
    app.resources.expect_mut::<FrameSettings>().downlevel.no_read_only_depth = true;
    let (img, seen) = probe(&mut app);
    assert_eq!(
        (seen[0].0.depth_access, seen[0].0.depth, seen[0].1),
        (DepthAccess::SampledOnly, None, true)
    );
    assert!(
        fraction(&img, [0, 255, 0]) > 0.99,
        "{:.3} green",
        fraction(&img, [0, 255, 0])
    );
    assert_eq!(stats(&app).scene_read_passes, 1);
}

/// Several views drawing into one multisampled target: depth readers draw inside the scene
/// pass (a later view would load multisampled content without a single-sample pass's draws),
/// with the far dummy bound (no fade) and no resolve.
#[test]
fn multi_view_msaa_uses_in_pass_fallback() {
    let Some(mut app) = headless_app(W, H) else { return };
    app.resources.expect_mut::<Renderer>().set_msaa_samples(4);
    let cam = analytic_plane_scene(&mut app);
    app.world.get::<&mut Camera3d>(cam).unwrap().viewport = Some([0.0, 0.0, 0.5, 1.0]);
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d {
            order: 1,
            viewport: Some([0.5, 0.0, 0.5, 1.0]),
            ..Default::default()
        },
    ));
    let (probe, seen) = DepthProbe::new();
    nodes(&mut app).insert("probe", probe);
    let img = render(&mut app);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    for (target, depth) in seen {
        assert_eq!(
            (target.depth_access, target.samples, target.depth_read_only, depth),
            (DepthAccess::None, 4, false, false)
        );
    }
    assert!(fraction(&img, [0, 0, 255]) > 0.99, "the in-pass fallback has no depth");
    let s = stats(&app);
    assert_eq!((s.scene_read_passes, s.depth_resolves, s.node_draws), (0, 0, 2));
}

/// The Distortion phase runs only for a post camera with distortion enabled and a writer
/// with work, only in the settings camera's view, into the half-resolution distortion buffer
/// (graphics spec §1.8). Registering a writer changes nothing by itself.
#[test]
fn distortion_phase_only_with_post_and_work() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = ortho_camera(&mut app);
    quad(&mut app, Vec4::new(0.0, 1.0, 0.0, 1.0), Vec3::ZERO, 1.0, false);
    let writer = nodes(&mut app).insert("writer", DistortionWriter::new(true));
    let draws = writer.lock().unwrap().draws.clone();
    let passes = |app: &mut App| {
        render(app);
        stats(app).distortion_passes
    };
    assert_eq!(passes(&mut app), 0, "no post camera");
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    assert_eq!(passes(&mut app), 1, "post camera + work");
    let (target, size) = draws.lock().unwrap()[0];
    assert_eq!(size, (W / 2, H / 2), "half resolution");
    assert_eq!(
        (target.color, target.samples, target.depth, target.output),
        (DISTORTION_FORMAT, 1, None, OutputEncoding::Linear)
    );
    let caps = app.resources.expect::<Renderer>().graphics_caps().clone();
    assert_eq!(target.depth_access, DepthAccess::SampledOnly, "{caps:?}");
    assert!(stats(&app).textures_created >= 1, "the distortion buffer");
    assert_eq!(passes(&mut app), 1);
    assert_eq!(stats(&app).textures_created, 0, "persistent");
    // A second post camera in another rect has its own group; only settings cameras draw.
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d {
            order: 1,
            viewport: Some([0.5, 0.5, 0.5, 0.5]),
            ..Default::default()
        },
        PostProcess::default(),
    ));
    assert_eq!(passes(&mut app), 2);
    // Distortion disabled on both, or no work: nothing.
    for p in app.world.query::<&mut PostProcess>().iter() {
        p.distortion.enabled = false;
    }
    let before = draws.lock().unwrap().len();
    assert_eq!(passes(&mut app), 0);
    for p in app.world.query::<&mut PostProcess>().iter() {
        p.distortion.enabled = true;
    }
    writer.lock().unwrap().work = false;
    assert_eq!(passes(&mut app), 0);
    assert_eq!(draws.lock().unwrap().len(), before);
    // A camera without post never draws distortion, even over a post camera's rect.
    writer.lock().unwrap().work = true;
    let _ = app.world.remove_one::<PostProcess>(cam);
    let n = draws.lock().unwrap().len();
    assert_eq!(passes(&mut app), 1, "only the post camera");
    assert_eq!(draws.lock().unwrap().len(), n + 1);
}

/// A new renderer (device) reaches nodes with a new id: they rebuild their GPU state.
#[test]
fn node_sees_new_renderer_id() {
    let Some(mut app) = headless_app(W, H) else { return };
    ortho_camera(&mut app);
    let log = log();
    nodes(&mut app).insert(
        "n",
        Solid::new(
            "n",
            vec![Solid::slot(PhaseSlot::new(RenderPhase::Opaque, 0), RED)],
            log.clone(),
        ),
    );
    let first = render(&mut app);
    let a = app.resources.expect::<Renderer>().renderer_id();
    let renderer = pollster::block_on(Renderer::new_headless(W, H)).expect("second renderer");
    let b = renderer.renderer_id();
    app.insert_resource(renderer);
    assert_eq!(render(&mut app), first, "the same image from the new renderer");
    assert_ne!(a, b);
    let l = log.lock().unwrap();
    let ids: Vec<u64> = l.prepares.iter().map(|(_, r)| *r).collect();
    assert_eq!(ids, [a, b]);
    assert_eq!(l.draws.iter().map(|d| d.renderer).collect::<Vec<_>>(), [a, b]);
    // The new renderer prewarmed the node again.
    assert_eq!(l.prewarms.len(), 2);
}

/// A node whose mutex was poisoned (its owner panicked while holding it) still renders: the
/// renderer recovers the lock.
#[test]
fn poisoned_node_does_not_crash() {
    let Some(mut app) = headless_app(W, H) else { return };
    ortho_camera(&mut app);
    let log = log();
    let node = nodes(&mut app).insert(
        "n",
        Solid::new(
            "n",
            vec![Solid::slot(PhaseSlot::new(RenderPhase::Opaque, 0), RED)],
            log.clone(),
        ),
    );
    let n2 = node.clone();
    let _ = std::thread::spawn(move || {
        let _guard = n2.lock().unwrap();
        panic!("the node's owner panicked");
    })
    .join();
    assert!(node.is_poisoned());
    let img = render(&mut app);
    assert_eq!(at(&img, 0.0, 0.0), [255, 0, 0]);
    assert_eq!(log.lock().unwrap().draws.len(), 1);
}

/// `Renderer::prewarm` calls every node's `prewarm` for the targets its slots draw with in the
/// last frame's views, and for extra formats (the windowed capture format).
#[test]
fn prewarm_reaches_nodes_and_extra_formats() {
    let Some(mut app) = headless_app(W, H) else { return };
    ortho_camera(&mut app);
    let log = log();
    let node = nodes(&mut app).insert(
        "n",
        Solid::new(
            "n",
            vec![
                Solid::slot(PhaseSlot::new(RenderPhase::Opaque, 0), RED),
                Solid::slot(PhaseSlot::new(RenderPhase::Distortion, 0), RED),
            ],
            log.clone(),
        ),
    );
    node.lock().unwrap().mask = SlotMask::NONE;
    render(&mut app);
    assert!(log.lock().unwrap().prewarms.is_empty(), "nothing had work");
    let request = PrewarmRequest {
        materials: vec![Material::srgb(0.5, 0.5, 0.5), Material::unlit(Vec4::ONE)],
        formats: vec![wgpu::TextureFormat::Bgra8UnormSrgb],
        ..Default::default()
    };
    app.resources.expect_mut::<Renderer>().prewarm(&request);
    let l = log.lock().unwrap();
    let formats: Vec<_> = l.prewarms.iter().map(|(_, s, t)| (*s, t.color)).collect();
    // The Distortion slot cannot draw in a camera without post: not prewarmed.
    assert_eq!(
        formats,
        [
            (0, wgpu::TextureFormat::Rgba8UnormSrgb),
            (0, wgpu::TextureFormat::Bgra8UnormSrgb)
        ]
    );
    drop(l);
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    render(&mut app);
}

/// Registered nodes without work (the built-ins, or user nodes) leave Direct mode exactly as
/// it was: same pixels, no pass, no allocation of ours.
#[test]
fn idle_nodes_change_nothing() {
    let Some(mut app) = headless_app(W, H) else { return };
    ortho_camera(&mut app);
    quad(&mut app, Vec4::new(0.0, 1.0, 0.0, 1.0), Vec3::ZERO, 1.0, false);
    let plain = render(&mut app);
    let log = log();
    let node = nodes(&mut app).insert(
        "n",
        Solid::new(
            "n",
            vec![Solid::slot(PhaseSlot::new(RenderPhase::Opaque, 0), RED)],
            log.clone(),
        ),
    );
    node.lock().unwrap().mask = SlotMask::NONE;
    for _ in 0..3 {
        assert_eq!(render(&mut app), plain);
        let s = stats(&app);
        assert_eq!((s.render_passes, s.node_draws, s.cpu_reallocations), (0, 0, 0));
    }
    let shared: SharedNode = Arc::new(Mutex::new(Solid::new("m", vec![], log.clone())));
    assert!(nodes(&mut app).insert_shared("m", shared).is_none());
    assert_eq!(render(&mut app), plain);
}

/// A node uploading a color through `PrepareContext::upload` (the renderer's staging ring, in
/// the frame encoder) and drawing with it in the same frame; it also finds the frame's meshes
/// and textures (`PrepareContext::mesh`/`texture`) and reports dispatches.
struct Uploading {
    color: [f32; 4],
    mesh: MeshHandle,
    texture: TextureHandle,
    /// Index count of the mesh, size and layering of the texture, as `prepare` found them.
    found: Arc<Mutex<Option<Found>>>,
    gpu: Option<(wgpu::Buffer, wgpu::BindGroup, wgpu::RenderPipeline)>,
}

type Found = (u32, (u32, u32), bool);

static UPLOAD_SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Opaque, 0)];

impl RenderNode for Uploading {
    fn slots(&self) -> &[PhaseSlot] {
        &UPLOAD_SLOTS
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        let mesh = ctx.mesh(self.mesh).map(|m| m.index_count);
        let texture = ctx.texture(self.texture).map(|t| (t.size, t.layered));
        if let (Some(indices), Some((size, layered))) = (mesh, texture) {
            *self.found.lock().unwrap() = Some((indices, size, layered));
        }
        ctx.count_dispatches(2);
        let device = ctx.device;
        let (buffer, ..) = self.gpu.get_or_insert_with(|| {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("uploading node"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("uploading node"),
                source: wgpu::ShaderSource::Wgsl(
                    format!(
                        "{FULLSCREEN_WGSL}\n@group(0) @binding(0) var<uniform> color: vec4<f32>;
                         @fragment fn fs() -> @location(0) vec4<f32> {{ return color; }}"
                    )
                    .into(),
                ),
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("uploading node"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::TextureFormat::Rgba8UnormSrgb.into())],
                }),
                multiview_mask: None,
                cache: None,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("uploading node"),
                layout: &pipeline.get_bind_group_layout(0),
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            (buffer, bind_group, pipeline)
        });
        let bytes: Vec<u8> = self.color.iter().flat_map(|c| c.to_le_bytes()).collect();
        let buffer = buffer.clone();
        ctx.upload(&buffer, 0, &bytes);
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, _: &DrawContext<'_>) {
        let Some((_, bind_group, pipeline)) = &self.gpu else {
            return;
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[test]
fn prepare_uploads_reach_the_draws() {
    let Some(mut app) = headless_app(W, H) else { return };
    ortho_camera(&mut app);
    let (mesh, texture) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (
            a.add_mesh(MeshData::cube(1.0)),
            a.add_image(ImageData::filled(8, 4, [255, 255, 255, 255])),
        )
    };
    let found = Arc::new(Mutex::new(None));
    let node = nodes(&mut app).insert(
        "uploading",
        Uploading {
            color: [1.0, 0.0, 0.0, 1.0],
            mesh,
            texture,
            found: found.clone(),
            gpu: None,
        },
    );
    assert_eq!(at(&render(&mut app), 0.0, 0.0), [255, 0, 0]);
    assert_eq!(*found.lock().unwrap(), Some((36, (8, 4), false)));
    let s = stats(&app);
    assert_eq!(s.node_dispatches, 2);
    assert!(s.bytes_uploaded >= 16);
    // A new value is uploaded before the frame's passes read it.
    node.lock().unwrap().color = [0.0, 1.0, 0.0, 1.0];
    assert_eq!(at(&render(&mut app), 0.0, 0.0), [0, 255, 0]);
}
