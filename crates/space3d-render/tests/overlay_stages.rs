//! Overlay stages (graphics spec §1.5, milestone F6): `AfterTonemap` (the default) keeps every
//! existing overlay's placement and pixels; opt-in `HdrScene` overlays draw into the primary
//! scene color before post (tonemapped with the scene) — or, in Direct mode, into the target
//! before every `AfterTonemap` overlay — in the encoding `OverlayScene::output` names.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;
use std::sync::{Arc, Mutex};

const W: u32 = 64;
const H: u32 = 32;

/// What an overlay was told.
#[derive(Clone, Debug, PartialEq)]
enum Event {
    Scene {
        mode: FrameMode,
        output: OutputEncoding,
        exposure: f32,
        tonemapper: Tonemapper,
        depth: Option<(u32, u32)>,
    },
    Prepare {
        kind: OverlayTargetKind,
        format: wgpu::TextureFormat,
        size: (u32, u32),
    },
    Render(OverlayTargetKind),
}

type Log = Arc<Mutex<Vec<(&'static str, Event)>>>;

/// Fills `rect` (target pixels) with the linear color `value`: in `OverlayScene::output`'s
/// encoding for HdrScene overlays, or — `legacy` — the way 249c874-era overlays did (manual
/// sRGB iff `!format.is_srgb()`).
struct StageOverlay {
    name: &'static str,
    stage: OverlayStage,
    reads: SceneReads,
    rect: [u32; 4],
    value: [f32; 4],
    scene: Option<(OutputEncoding, f32, Tonemapper)>,
    log: Log,
    pipelines: Vec<(String, wgpu::RenderPipeline)>,
    current: Option<usize>,
}

impl StageOverlay {
    fn new(name: &'static str, stage: OverlayStage, rect: [u32; 4], value: [f32; 4], log: &Log) -> Self {
        Self {
            name,
            stage,
            reads: SceneReads::NONE,
            rect,
            value,
            scene: None,
            log: log.clone(),
            pipelines: Vec::new(),
            current: None,
        }
    }
}

impl RenderOverlay for StageOverlay {
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        _: &mut wgpu::CommandEncoder,
        target: &OverlayTarget,
    ) {
        self.log.lock().unwrap().push((
            self.name,
            Event::Prepare {
                kind: target.kind,
                format: target.format,
                size: target.size,
            },
        ));
        let (encoding, exposure, op) = match self.scene.take() {
            Some((output, exposure, tonemapper)) => (output.id(), exposure, tonemapper.id()),
            // The legacy rule (wrong for float targets: `shader_encodes_srgb` is the fix).
            None => (u32::from(!target.format.is_srgb()), 1.0, 0),
        };
        let [r, g, b, a] = self.value;
        let src = format!(
            "{OUTPUT_WGSL}\n{FULLSCREEN_WGSL}\n@fragment fn fs() -> @location(0) vec4<f32> {{
                 return s3d_output(vec4<f32>({r:?}, {g:?}, {b:?}, {a:?}), {encoding}u, {exposure:?}, {op}u);
             }}"
        );
        let key = format!("{:?}{src}", target.format);
        self.current = Some(match self.pipelines.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("stage overlay"),
                    source: wgpu::ShaderSource::Wgsl(src.into()),
                });
                let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("stage overlay"),
                    layout: None,
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("vs_fullscreen"),
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
                self.pipelines.push((key, pipeline));
                self.pipelines.len() - 1
            }
        });
    }

    fn render(&self, pass: &mut wgpu::RenderPass<'static>, target: &OverlayTarget) {
        self.log.lock().unwrap().push((self.name, Event::Render(target.kind)));
        let Some(i) = self.current else { return };
        let [x, y, w, h] = self.rect;
        pass.set_scissor_rect(x, y, w, h);
        pass.set_pipeline(&self.pipelines[i].1);
        pass.draw(0..3, 0..1);
    }

    fn stage(&self) -> OverlayStage {
        self.stage
    }

    fn scene_reads(&self) -> SceneReads {
        self.reads
    }

    fn prepare_scene(&mut self, scene: &OverlayScene<'_>) {
        self.log.lock().unwrap().push((
            self.name,
            Event::Scene {
                mode: scene.mode,
                output: scene.output,
                exposure: scene.exposure,
                tonemapper: scene.tonemapper,
                depth: scene.scene_depth.map(|d| d.size),
            },
        ));
        self.scene = Some((scene.output, scene.exposure, scene.tonemapper));
    }
}

/// A lit scene and its camera.
fn scene(app: &mut App) -> Entity {
    let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.5));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).with_rotation(Quat::from_rotation_y(0.6)),
        Mesh3d(cube),
        Material::srgb(0.8, 0.4, 0.2),
    ));
    spawn_sun(app);
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default().with_clear_color(Vec4::new(0.05, 0.05, 0.1, 1.0)),
    ))
}

fn render(app: &mut App) -> ImageData {
    let img = render_to_image(app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    img
}

fn overlays(app: &mut App) -> &mut RenderOverlays {
    app.resources.expect_mut::<RenderOverlays>()
}

fn srgb8(linear: f32) -> u8 {
    (linear_to_srgb(linear.clamp(0.0, 1.0)) * 255.0).round() as u8
}

fn no_dither() -> PostProcess {
    PostProcess {
        dither: false,
        ..PostProcess::default()
    }
}

/// Pixels of `rect` in `img`.
fn rect_pixels(img: &ImageData, [x, y, w, h]: [u32; 4]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    for py in y..y + h {
        for px in x..x + w {
            out.push(img.get(px, py));
        }
    }
    out
}

/// An overlay written for 249c874 (prepare/render only) draws exactly the same pixels over
/// Direct and over post-processed (Hdr) frames: after tonemapping, on the final target.
#[test]
fn legacy_overlay_unchanged_direct_and_hdr() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = scene(&mut app);
    let log = Log::default();
    let rect = [8, 8, 16, 12];
    overlays(&mut app).insert(
        "legacy",
        0,
        StageOverlay::new("legacy", OverlayStage::AfterTonemap, rect, [0.25, 0.5, 0.75, 1.0], &log),
    );
    let direct = render(&mut app);
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    let hdr = render(&mut app);
    assert_eq!(stats(&app).intermediate_groups, 1);
    let expected = [srgb8(0.25), srgb8(0.5), srgb8(0.75), 255];
    assert!(rect_pixels(&direct, rect).iter().all(|p| *p == expected));
    assert_eq!(rect_pixels(&hdr, rect), rect_pixels(&direct, rect));
    assert_ne!(direct, hdr, "the scene itself is tonemapped");
    let events: Vec<_> = log.lock().unwrap().iter().map(|(_, e)| e.clone()).collect();
    let prepare = Event::Prepare {
        kind: OverlayTargetKind::Offscreen,
        format: RENDER_TARGET_FORMAT,
        size: (W, H),
    };
    let render_event = Event::Render(OverlayTargetKind::Offscreen);
    assert_eq!(
        events,
        [prepare.clone(), render_event.clone(), prepare, render_event],
        "no prepare_scene for AfterTonemap overlays"
    );
}

/// HdrScene overlays draw into the `Rgba16Float` scene color before post: a 4.0 write comes
/// out as `AgX(4.0)` (±2/255), and they see the last primary view in `OverlayScene`.
#[test]
fn hdr_scene_overlay_format_and_tonemap() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = scene(&mut app);
    // No bloom: the 4.0 fill's own glow would add to the pixels checked below (the test is
    // about the overlay's format and tonemapping; `post.rs` covers overlays feeding bloom).
    app.world
        .insert_one(cam, no_dither().with_tonemapper(Tonemapper::AgX).without_bloom())
        .unwrap();
    if app.resources.expect::<Renderer>().graphics_caps().hdr_format.is_none() {
        return;
    }
    let log = Log::default();
    let rect = [4, 4, 20, 10];
    overlays(&mut app).insert(
        "hdr",
        5,
        StageOverlay::new("hdr", OverlayStage::HdrScene, rect, [4.0, 4.0, 4.0, 1.0], &log),
    );
    let img = render(&mut app);
    let v = srgb8(Tonemapper::AgX.apply(Vec3::splat(4.0)).x);
    for p in rect_pixels(&img, rect) {
        assert!(p[..3].iter().all(|c| c.abs_diff(v) <= 2), "{p:?} vs AgX(4.0) = {v}");
    }
    assert!(v > 200 && v < 255, "4.0 is compressed, not clipped: {v}");
    let events: Vec<_> = log.lock().unwrap().iter().map(|(_, e)| e.clone()).collect();
    assert_eq!(
        events,
        [
            Event::Scene {
                mode: FrameMode::Hdr,
                output: OutputEncoding::Linear,
                exposure: 1.0,
                tonemapper: Tonemapper::None,
                depth: None,
            },
            Event::Prepare {
                kind: OverlayTargetKind::Offscreen,
                format: wgpu::TextureFormat::Rgba16Float,
                size: (W, H),
            },
            Event::Render(OverlayTargetKind::Offscreen),
        ]
    );
    // The overlay is part of the scene color (what post reads).
    let hdr = app
        .resources
        .expect_mut::<Renderer>()
        .capture_scene_hdr_blocking()
        .expect("scene color");
    let p = hdr.pixels[(8 * hdr.width + 10) as usize];
    assert!((p[0] - 4.0).abs() < 0.01, "{p:?}");
}

/// In Direct mode, HdrScene overlays draw into the target after the primary views and before
/// every AfterTonemap overlay, whatever their priorities; values clip at 1.
#[test]
fn hdr_scene_overlay_in_direct_runs_before_after_tonemap() {
    let Some(mut app) = headless_app(W, H) else { return };
    scene(&mut app);
    let log = Log::default();
    let (a, b) = ([8, 8, 24, 16], [20, 12, 24, 16]);
    // The HdrScene overlay has the higher priority but still draws first.
    overlays(&mut app).insert(
        "after",
        0,
        StageOverlay::new("after", OverlayStage::AfterTonemap, b, [0.0, 1.0, 0.0, 1.0], &log),
    );
    overlays(&mut app).insert(
        "hdr",
        100,
        StageOverlay::new("hdr", OverlayStage::HdrScene, a, [4.0, 0.0, 0.0, 1.0], &log),
    );
    let img = render(&mut app);
    assert_eq!(
        img.get(10, 10),
        [255, 0, 0, 255],
        "HdrScene over the scene, clipped at 1"
    );
    assert_eq!(img.get(22, 14), [0, 255, 0, 255], "AfterTonemap on top");
    let order: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .map(|(n, e)| (*n, std::mem::discriminant(e)))
        .collect();
    let scene_d = std::mem::discriminant(&Event::Render(OverlayTargetKind::Offscreen));
    let names: Vec<_> = order.iter().filter(|(_, d)| *d == scene_d).map(|(n, _)| *n).collect();
    assert_eq!(names, ["hdr", "after"]);
    let first = log.lock().unwrap()[0].clone();
    assert_eq!(
        first,
        (
            "hdr",
            Event::Scene {
                mode: FrameMode::Direct,
                output: OutputEncoding::Linear,
                exposure: 1.0,
                tonemapper: Tonemapper::None,
                depth: None,
            }
        )
    );
    // Direct mode: the HdrScene overlay pass is the frame's only counted pass.
    assert_eq!(stats(&app).render_passes, 1);
}

/// In the Ldr fallback, HdrScene overlays write into the LDR scene texture with the camera's
/// exposure and tonemapper applied (`OutputEncoding::Tonemapped`): the same look as HDR.
#[test]
fn hdr_scene_overlay_output_tonemapped_in_ldr() {
    let Some(mut app) = headless_app(W, H) else { return };
    let cam = scene(&mut app);
    let post = no_dither().with_tonemapper(Tonemapper::AgX).with_exposure(0.5);
    app.world.insert_one(cam, post).unwrap();
    app.resources.expect_mut::<FrameSettings>().downlevel.no_float_targets = true;
    let log = Log::default();
    let rect = [4, 4, 20, 10];
    overlays(&mut app).insert(
        "hdr",
        0,
        StageOverlay::new("hdr", OverlayStage::HdrScene, rect, [4.0, 4.0, 4.0, 1.0], &log),
    );
    let img = render(&mut app);
    let events: Vec<_> = log.lock().unwrap().iter().map(|(_, e)| e.clone()).collect();
    let exposure = 0.5f32.exp2();
    assert_eq!(
        events[0],
        Event::Scene {
            mode: FrameMode::Ldr,
            output: OutputEncoding::Tonemapped,
            exposure,
            tonemapper: Tonemapper::AgX,
            depth: None,
        }
    );
    assert_eq!(
        events[1],
        Event::Prepare {
            kind: OverlayTargetKind::Offscreen,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            size: (W, H),
        }
    );
    let v = srgb8(Tonemapper::AgX.apply(Vec3::splat(4.0 * exposure)).x);
    for p in rect_pixels(&img, rect) {
        assert!(p[..3].iter().all(|c| c.abs_diff(v) <= 2), "{p:?} vs {v}");
    }
}

/// Frames without a primary view skip HdrScene overlays entirely (no `prepare_scene`,
/// `prepare` or `render`); AfterTonemap overlays still run on the cleared target.
#[test]
fn no_primary_view_skips_hdr_scene() {
    let Some(mut app) = headless_app(W, H) else { return };
    let log = Log::default();
    overlays(&mut app).insert(
        "hdr",
        0,
        StageOverlay::new("hdr", OverlayStage::HdrScene, [0, 0, 8, 8], [1.0, 0.0, 0.0, 1.0], &log),
    );
    overlays(&mut app).insert(
        "after",
        1,
        StageOverlay::new(
            "after",
            OverlayStage::AfterTonemap,
            [8, 8, 8, 8],
            [0.0, 1.0, 0.0, 1.0],
            &log,
        ),
    );
    // A camera rendering only into a render target.
    let rt = app.resources.expect_mut::<RenderAssets>().add_render_target(16, 16);
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d {
            target: RenderTarget::Texture(rt),
            ..Default::default()
        },
        PostProcess::default(),
    ));
    render(&mut app);
    let img = render(&mut app);
    let names: Vec<_> = log.lock().unwrap().iter().map(|(n, _)| *n).collect();
    assert!(names.iter().all(|n| *n == "after"), "{names:?}");
    assert_eq!(names.len(), 4, "prepare + render per frame");
    assert_eq!(img.get(2, 2), [0, 0, 0, 255], "cleared, no HdrScene overlay");
    assert_eq!(img.get(10, 10), [0, 255, 0, 255]);
}

/// HdrScene overlays reading `SceneReads::DEPTH` get the last primary scene view's
/// single-sample depth (in Direct and Hdr mode, with and without MSAA where it can be
/// resolved); others get none.
#[test]
fn hdr_scene_overlay_scene_depth() {
    for samples in [1, 4] {
        let Some(mut app) = headless_app(W, H) else { return };
        app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
        let caps = app.resources.expect::<Renderer>().graphics_caps().clone();
        let cam = scene(&mut app);
        let log = Log::default();
        let mut reader = StageOverlay::new("reader", OverlayStage::HdrScene, [0, 0, 4, 4], [1.0; 4], &log);
        reader.reads = SceneReads::DEPTH;
        overlays(&mut app).insert("reader", 0, reader);
        overlays(&mut app).insert(
            "blind",
            1,
            StageOverlay::new("blind", OverlayStage::HdrScene, [4, 0, 4, 4], [1.0; 4], &log),
        );
        for post in [false, true] {
            if post {
                app.world.insert_one(cam, PostProcess::default()).unwrap();
            }
            log.lock().unwrap().clear();
            render(&mut app);
            let depth = |name: &str| {
                log.lock().unwrap().iter().find_map(|(n, e)| match e {
                    Event::Scene { depth, .. } if *n == name => Some(*depth),
                    _ => None,
                })
            };
            let expected = (samples == 1 || caps.msaa_depth_resolve).then_some((W, H));
            assert_eq!(depth("reader"), Some(expected), "×{samples} post {post}");
            assert_eq!(depth("blind"), Some(None));
            let s = stats(&app);
            let resolves = u32::from(samples > 1 && caps.msaa_depth_resolve);
            assert_eq!(s.depth_resolves, resolves, "×{samples} post {post}");
        }
    }
}

/// An HdrScene overlay built on the frame's WGSL modules: `VIEW_WGSL`, then the group-1
/// module `OverlayScene::scene_wgsl` names for its pass, `OUTPUT_WGSL` and `FULLSCREEN_WGSL`,
/// drawn with the pass's view and scene bind groups. It paints `hit` where
/// `s3d_scene_occluded(px, 1.0)` (scene geometry at that pixel) and `miss` elsewhere.
struct DepthOverlay {
    reads: SceneReads,
    access: Arc<Mutex<Vec<DepthAccess>>>,
    /// Shader source, view bind group, its dynamic offset, scene bind group and the layouts.
    scene: Option<(
        String,
        wgpu::BindGroup,
        u32,
        wgpu::BindGroup,
        [wgpu::BindGroupLayout; 2],
    )>,
    pipelines: Vec<(String, wgpu::RenderPipeline)>,
    current: Option<usize>,
}

impl DepthOverlay {
    fn new(reads: SceneReads) -> Self {
        Self {
            reads,
            access: Default::default(),
            scene: None,
            pipelines: Vec::new(),
            current: None,
        }
    }
}

impl RenderOverlay for DepthOverlay {
    fn stage(&self) -> OverlayStage {
        OverlayStage::HdrScene
    }

    fn scene_reads(&self) -> SceneReads {
        self.reads
    }

    fn prepare_scene(&mut self, scene: &OverlayScene<'_>) {
        self.access.lock().unwrap().push(scene.depth_access());
        let (encoding, exposure, op) = (scene.output.id(), scene.exposure, scene.tonemapper.id());
        let src = format!(
            "{VIEW_WGSL}\n{}\n{OUTPUT_WGSL}\n{FULLSCREEN_WGSL}\n
             @fragment fn fs(in: FullscreenOut) -> @location(0) vec4<f32> {{
                 let hit = s3d_scene_occluded(vec2<i32>(in.pos.xy), 1.0);
                 let c = select(vec4<f32>(0.0, 0.0, 1.0, 1.0), vec4<f32>(1.0, 0.0, 0.0, 1.0), hit);
                 return s3d_output(c, {encoding}u, {exposure:?}, {op}u);
             }}",
            scene.scene_wgsl()
        );
        self.scene = Some((
            src,
            scene.view_bind_group.clone(),
            scene.view.view_offset,
            scene.scene_bind_group.clone(),
            [scene.layouts.view.clone(), scene.layouts.scene.clone()],
        ));
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        _: &mut wgpu::CommandEncoder,
        target: &OverlayTarget,
    ) {
        let Some((src, _, _, _, [view, scene])) = &self.scene else {
            return;
        };
        let key = format!("{:?}{src}", target.format);
        self.current = Some(match self.pipelines.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("depth overlay"),
                    source: wgpu::ShaderSource::Wgsl(src.clone().into()),
                });
                let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("depth overlay"),
                    bind_group_layouts: &[Some(view), Some(scene)],
                    immediate_size: 0,
                });
                let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("depth overlay"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("vs_fullscreen"),
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
                self.pipelines.push((key, pipeline));
                self.pipelines.len() - 1
            }
        });
    }

    fn render(&self, pass: &mut wgpu::RenderPass<'static>, _: &OverlayTarget) {
        let (Some(i), Some((_, view, offset, scene, _))) = (self.current, &self.scene) else {
            return;
        };
        pass.set_pipeline(&self.pipelines[i].1);
        pass.set_bind_group(0, view, &[*offset]);
        pass.set_bind_group(1, scene, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// Overlays compose the group-1 module of their pass (`OverlayScene::scene_wgsl`, i.e.
/// `scene_wgsl_for(OverlayScene::depth_access)`), so depth-aware HdrScene overlays can use
/// `s3d_scene_occluded` and friends: with `SceneReads::DEPTH` the overlay sees exactly the
/// scene's geometry (Direct and Hdr, with and without MSAA where depth can be resolved: every
/// pixel it marks is one the scene drew, and no other); without, the far dummy (nothing).
#[test]
fn hdr_scene_overlay_composes_scene_wgsl() {
    for samples in [1, 4] {
        for post in [false, true] {
            let Some(mut app) = headless_app(W, H) else { return };
            app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
            let caps = app.resources.expect::<Renderer>().graphics_caps().clone();
            let cam = scene(&mut app);
            if post {
                app.world.insert_one(cam, no_dither()).unwrap();
            }
            let what = format!("×{samples} post {post}");
            // The scene alone: geometry is wherever the clear color is not.
            let reference = render(&mut app);
            let clear = reference.get(0, 0);
            let geometry: Vec<bool> = (0..H)
                .flat_map(|y| (0..W).map(move |x| (x, y)))
                .map(|(x, y)| reference.get(x, y) != clear)
                .collect();
            assert!(
                geometry.iter().filter(|g| **g).count() > 100,
                "{what}: the cube is drawn"
            );
            for reads in [SceneReads::DEPTH, SceneReads::NONE] {
                let overlay = DepthOverlay::new(reads);
                let access = overlay.access.clone();
                overlays(&mut app).insert("depth", 0, overlay);
                let img = render(&mut app);
                let has_depth = reads == SceneReads::DEPTH && (samples == 1 || caps.msaa_depth_resolve);
                let expected = if has_depth {
                    DepthAccess::SampledOnly
                } else {
                    DepthAccess::None
                };
                assert_eq!(*access.lock().unwrap(), [expected], "{what} {reads:?}");
                // Red (hit) or blue (miss), tonemapped with the scene when post runs.
                let marked: Vec<bool> = (0..H)
                    .flat_map(|y| (0..W).map(move |x| (x, y)))
                    .map(|(x, y)| {
                        let [r, g, b, _] = img.get(x, y);
                        assert!(
                            r.abs_diff(b) > 100 && g < r.max(b) / 2,
                            "{what}: ({x}, {y}) {:?}",
                            [r, g, b]
                        );
                        r > b
                    })
                    .collect();
                if has_depth {
                    let wrong = marked.iter().zip(&geometry).filter(|(m, g)| m != g).count();
                    assert_eq!(wrong, 0, "{what}: pixels marked differently from the scene's geometry");
                } else {
                    assert!(marked.iter().all(|m| !m), "{what} {reads:?}: nothing is occluded");
                }
                overlays(&mut app).remove("depth");
            }
        }
    }
}

/// `OverlayTarget::shader_encodes_srgb` is the right test for manual encoding: false for float
/// scene colors (where the legacy `!format.is_srgb()` rule is wrong).
#[test]
fn overlay_targets_report_their_encoding() {
    let t = |format| OverlayTarget {
        size: (1, 1),
        format,
        scale_factor: 1.0,
        kind: OverlayTargetKind::Offscreen,
    };
    assert!(!t(wgpu::TextureFormat::Rgba16Float).shader_encodes_srgb());
    assert!(t(wgpu::TextureFormat::Rgba16Float).is_hdr());
    assert!(t(wgpu::TextureFormat::Bgra8Unorm).shader_encodes_srgb());
    assert!(!t(RENDER_TARGET_FORMAT).shader_encodes_srgb() && !t(RENDER_TARGET_FORMAT).is_hdr());
}
