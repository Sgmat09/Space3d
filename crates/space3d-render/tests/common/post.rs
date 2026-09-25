//! Helpers and shared test bodies of the post-processing tests (graphics spec §5.2 P-POST):
//! `tests/post.rs` runs every case on the default backend, `tests/gl_post.rs` on wgpu's GL backend
//! (WebGL2 class: ANGLE on macOS). Included with `#[path = "common/post.rs"]` next to `mod common`.
//!
//! Scenes are unlit, camera-facing quads under an orthographic camera 2 world units high, so
//! every radiance is exact: at `H` pixels high one world unit is `H / 2` pixels. Post settings
//! default to `Tonemapper::None` and no dither ([`post`]), so the display value of a pixel is
//! its exposed radiance clamped to 1, and a bloom halo is directly measurable in linear light.
#![allow(dead_code)]

use space3d_core::prelude::*;
use space3d_render::*;

/// Builds a test app with a `w × h` headless renderer, or `None` (the test skips).
pub type MakeApp = fn(u32, u32) -> Option<App>;

/// Post without tonemapping (display = exposed radiance clamped to 1) and without dither, with
/// `bloom`.
pub fn post(bloom: Bloom) -> PostProcess {
    PostProcess {
        tonemapper: Tonemapper::None,
        dither: false,
        bloom,
        ..PostProcess::default()
    }
}

/// An unlit camera-facing quad of `size` world units (linear radiance `color`) centered at
/// `center`, in the z = 0 plane.
pub fn quad(app: &mut App, color: Vec3, center: Vec2, size: Vec2) -> Entity {
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(1.0));
    app.world.spawn((
        Transform::from_xyz(center.x, center.y, 0.0)
            .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
            .with_scale(Vec3::new(size.x, 1.0, size.y)),
        Mesh3d(mesh),
        Material::unlit(color.extend(1.0)),
    ))
}

/// Orthographic camera at (`x`, 0, 5) looking down -Z, 2 world units high, with `post` (none:
/// Direct mode).
pub fn camera(app: &mut App, x: f32, clear: Vec4, post: Option<PostProcess>) -> Entity {
    let camera = Camera3d::default()
        .with_clear_color(clear)
        .with_projection(Projection::Orthographic {
            height: 2.0,
            near: 0.1,
            far: 100.0,
        });
    let e = app.world.spawn((Transform::from_xyz(x, 0.0, 5.0), camera));
    if let Some(post) = post {
        app.world.insert_one(e, post).unwrap();
    }
    e
}

/// Render one frame and capture the primary target; no GPU errors.
pub fn render(app: &mut App) -> ImageData {
    let img = render_to_image(app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    img
}

/// Replace the post settings of `camera`.
pub fn set_post(app: &mut App, camera: Entity, post: PostProcess) {
    app.world.insert_one(camera, post).unwrap();
}

pub fn frame_settings(app: &mut App) -> &mut FrameSettings {
    app.resources.expect_mut::<FrameSettings>()
}

/// Linear RGB of pixel (`x`, `y`) of an sRGB capture.
pub fn lin(img: &ImageData, x: u32, y: u32) -> Vec3 {
    let p = img.get(x, y);
    Vec3::new(
        srgb_to_linear(p[0] as f32 / 255.0),
        srgb_to_linear(p[1] as f32 / 255.0),
        srgb_to_linear(p[2] as f32 / 255.0),
    )
}

/// Mean linear RGB over the pixels `[x, y, w, h]` of an sRGB capture.
pub fn mean(img: &ImageData, [x0, y0, w, h]: [u32; 4]) -> Vec3 {
    let mut sum = Vec3::ZERO;
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            sum += lin(img, x, y);
        }
    }
    sum / (w * h) as f32
}

/// Mean linear RGB of the whole capture.
pub fn mean_all(img: &ImageData) -> Vec3 {
    mean(img, [0, 0, img.width, img.height])
}

/// Largest channel difference between two captures.
pub fn max_diff(a: &ImageData, b: &ImageData) -> u8 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    a.pixels
        .iter()
        .zip(&b.pixels)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

/// Fills `rect` (target pixels) with the linear color `value`, at `stage`: HdrScene overlays
/// write in `OverlayScene::output`'s encoding, AfterTonemap ones the 249c874 way (manual sRGB
/// iff the target format is not sRGB).
pub struct FillOverlay {
    pub stage: OverlayStage,
    pub rect: [u32; 4],
    pub value: [f32; 4],
    scene: Option<(OutputEncoding, f32, Tonemapper)>,
    pipelines: Vec<(String, wgpu::RenderPipeline)>,
    current: Option<usize>,
}

impl FillOverlay {
    pub fn new(stage: OverlayStage, rect: [u32; 4], value: [f32; 4]) -> Self {
        Self {
            stage,
            rect,
            value,
            scene: None,
            pipelines: Vec::new(),
            current: None,
        }
    }
}

impl RenderOverlay for FillOverlay {
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        _: &mut wgpu::CommandEncoder,
        target: &OverlayTarget,
    ) {
        let (encoding, exposure, op) = match self.scene.take() {
            Some((output, exposure, tonemapper)) => (output.id(), exposure, tonemapper.id()),
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
                    label: Some("fill overlay"),
                    source: wgpu::ShaderSource::Wgsl(src.into()),
                });
                let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("fill overlay"),
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

    fn render(&self, pass: &mut wgpu::RenderPass<'static>, _: &OverlayTarget) {
        let Some(i) = self.current else { return };
        let [x, y, w, h] = self.rect;
        pass.set_scissor_rect(x, y, w, h);
        pass.set_pipeline(&self.pipelines[i].1);
        pass.draw(0..3, 0..1);
    }

    fn stage(&self) -> OverlayStage {
        self.stage
    }

    fn prepare_scene(&mut self, scene: &OverlayScene<'_>) {
        self.scene = Some((scene.output, scene.exposure, scene.tonemapper));
    }
}

/// Test bodies shared by the default-backend and GL runs.
pub mod cases {
    use super::*;

    const W: u32 = 128;
    const H: u32 = 64;
    const BLACK: Vec4 = Vec4::new(0.0, 0.0, 0.0, 1.0);

    /// Bloom with a hard threshold at 1 (no knee).
    fn hard() -> Bloom {
        Bloom::glow().with_threshold(1.0, 0.0)
    }

    /// The largest linear value outside the pixel rect `[x, y, w, h]` (grown by `margin`).
    fn max_outside(img: &ImageData, [x, y, w, h]: [u32; 4], margin: u32) -> f32 {
        let (x0, y0) = (x.saturating_sub(margin), y.saturating_sub(margin));
        let (x1, y1) = (x + w + margin, y + h + margin);
        let mut m = 0.0f32;
        for py in 0..img.height {
            for px in 0..img.width {
                if (x0..x1).contains(&px) && (y0..y1).contains(&py) {
                    continue;
                }
                m = m.max(lin(img, px, py).max_element());
            }
        }
        m
    }

    /// Bloom only takes what is above the threshold: a 0.8 quad adds nothing to the chain
    /// (hard knee: bit-identical to no bloom; default knee: a faint halo), an 8.0 quad glows
    /// well beyond its edges, fading with distance, and in one scene the 0.8 quad changes no
    /// pixel outside itself.
    pub fn bloom_only_above_threshold(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        // 8 × 8 px quads: 0.25 units at 32 px per unit.
        let size = Vec2::splat(0.25);
        let dim = quad(&mut app, Vec3::splat(0.8), Vec2::new(0.0, 0.0), size);
        let cam = camera(&mut app, 0.0, BLACK, Some(post(hard())));
        let bloomed = render(&mut app);
        // A 128 × 64 rect: level 0 is 64 × 32, five levels (the last keeps 2 px).
        assert_eq!(stats(&app).post_passes, 10, "prefilter, 4 down, 4 up, composite");
        set_post(&mut app, cam, post(hard()).without_bloom());
        let plain = render(&mut app);
        assert_eq!(stats(&app).post_passes, 1);
        assert_eq!(bloomed, plain, "0.8 is below a hard threshold of 1: no bloom at all");
        // The default soft knee (0.5) lets a little of 0.8 through, far less than 8.0 gets.
        set_post(&mut app, cam, post(Bloom::glow()));
        let knee = max_outside(&render(&mut app), [60, 28, 8, 8], 0);
        assert!(knee > 0.0 && knee < 0.01, "0.8 in the knee: halo {knee}");

        app.world
            .insert_one(dim, Material::unlit(Vec4::new(8.0, 8.0, 8.0, 1.0)))
            .unwrap();
        set_post(&mut app, cam, post(hard()));
        let hot = render(&mut app);
        let near = lin(&hot, 58, 32).x;
        let far = lin(&hot, 40, 32).x;
        let farther = lin(&hot, 10, 32).x;
        assert!(near > 0.15, "right next to the 8.0 quad: {near}");
        assert!(
            near > far && far > farther && farther > 0.0,
            "{near} > {far} > {farther} > 0"
        );
        // The glow of a white source stays neutral (within the chain format's precision).
        let c = lin(&hot, 58, 32);
        assert!((c.max_element() - c.min_element()) < 0.02 * c.max_element(), "{c}");

        // Both quads: the 0.8 one (right) adds nothing to the chain.
        let dim = quad(&mut app, Vec3::splat(0.8), Vec2::new(1.25, 0.0), size);
        let both = render(&mut app);
        app.world.despawn(dim).unwrap();
        let only_hot = render(&mut app);
        for y in 0..H {
            for x in 0..W {
                if (100..108).contains(&x) && (28..36).contains(&y) {
                    continue;
                }
                assert_eq!(both.get(x, y), only_hot.get(x, y), "({x}, {y})");
            }
        }
    }

    /// The threshold applies to exposed values and the composite adds bloom after exposure
    /// (never exposed twice): radiance 0.8 at +2 stops blooms exactly like 3.2 at 0 stops, and
    /// 3.2 at −2 stops like 0.8 (not at all with a hard threshold).
    pub fn bloom_is_in_exposed_units(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let q = quad(&mut app, Vec3::splat(0.8), Vec2::ZERO, Vec2::splat(0.25));
        let cam = camera(&mut app, 0.0, BLACK, Some(post(hard()).with_exposure(2.0)));
        let exposed = render(&mut app);
        app.world
            .insert_one(q, Material::unlit(Vec4::new(3.2, 3.2, 3.2, 1.0)))
            .unwrap();
        set_post(&mut app, cam, post(hard()));
        let scaled = render(&mut app);
        assert!(max_diff(&exposed, &scaled) <= 1, "{}", max_diff(&exposed, &scaled));
        assert!(max_outside(&scaled, [60, 28, 8, 8], 0) > 0.05, "3.2 blooms");
        set_post(&mut app, cam, post(hard()).with_exposure(-2.0));
        let dark = render(&mut app);
        set_post(&mut app, cam, post(hard()).with_exposure(-2.0).without_bloom());
        assert_eq!(dark, render(&mut app), "3.2 at -2 stops is 0.8: below the threshold");
    }

    /// A scene of moderate contrast (every radiance below 1, so nothing clips): the
    /// energy-conserving composite (`mix(scene, bloom, intensity)`) keeps the mean within 2 % at
    /// any intensity, while the additive composite with threshold 0 adds `intensity × mean`.
    pub fn energy_conserving_preserves_mean(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let grey = Vec4::new(0.2, 0.2, 0.2, 1.0);
        quad(
            &mut app,
            Vec3::new(0.45, 0.1, 0.05),
            Vec2::new(-1.0, 0.2),
            Vec2::new(1.2, 1.0),
        );
        quad(
            &mut app,
            Vec3::new(0.05, 0.4, 0.1),
            Vec2::new(0.6, -0.3),
            Vec2::new(1.0, 0.8),
        );
        quad(
            &mut app,
            Vec3::new(0.3, 0.35, 0.45),
            Vec2::new(1.4, 0.5),
            Vec2::new(0.5, 0.5),
        );
        quad(&mut app, Vec3::splat(0.45), Vec2::new(-0.2, -0.6), Vec2::new(0.3, 0.3));
        let cam = camera(&mut app, 0.0, grey, Some(post(Bloom::natural()).without_bloom()));
        let plain = render(&mut app);
        let m0 = mean_all(&plain);
        for intensity in [0.15, 0.5, 1.0] {
            set_post(&mut app, cam, post(Bloom::natural().with_intensity(intensity)));
            let img = render(&mut app);
            let m = mean_all(&img);
            let err = (m - m0).abs() / m0;
            assert!(err.max_element() < 0.02, "intensity {intensity}: mean {m} vs {m0}");
            assert_ne!(img, plain, "intensity {intensity}: bloom ran");
        }
        // Additive, threshold 0, intensity 1: the chain is a normalized average, so the mean
        // doubles — up to the compact chain's precision: `Rg11b10Ufloat` (5–6 mantissa bits)
        // converts by truncation on some GPUs (Apple: about −5 %, −7 % in blue, over the chain's
        // writes); the energy-conserving chain above is `Rgba16Float` for that reason.
        let additive = Bloom {
            composite: BloomComposite::Additive,
            ..Bloom::natural().with_intensity(1.0)
        };
        set_post(&mut app, cam, post(additive));
        let added = (mean_all(&render(&mut app)) - m0) / m0;
        for c in added.to_array() {
            assert!(
                (0.9..=1.02).contains(&c),
                "additive: the mean grows by {added} × itself"
            );
        }
    }

    /// Split screen: a very bright quad at the inner edge of one half never lights the other
    /// half (every bloom tap is clamped to its post group's rect), at render scale 1 and 0.5,
    /// either way round.
    pub fn bloom_does_not_bleed_across_viewports(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let bloom = Bloom::glow().with_intensity(1.0).with_scatter(0.9);
        // The left camera (x = 0) sees 2 world units: its right edge is x = 1. The right one
        // (x = 50) looks at empty space.
        let q = quad(&mut app, Vec3::splat(64.0), Vec2::new(0.9, 0.0), Vec2::new(0.2, 0.4));
        let left = camera(&mut app, 0.0, BLACK, Some(post(bloom)));
        let right = camera(&mut app, 50.0, BLACK, Some(post(bloom)));
        for (e, viewport, order) in [(left, [0.0, 0.0, 0.5, 1.0], 0), (right, [0.5, 0.0, 0.5, 1.0], 1)] {
            let mut c = app.world.get::<&mut Camera3d>(e).unwrap();
            c.viewport = Some(viewport);
            c.order = order;
        }
        for scale in [1.0, 0.5] {
            frame_settings(&mut app).render_scale = scale;
            for bright_left in [true, false] {
                let (x, dark) = if bright_left { (0.9, W / 2..W) } else { (49.1, 0..W / 2) };
                app.world.get::<&mut Transform>(q).unwrap().translation.x = x;
                let img = render(&mut app);
                assert_eq!(stats(&app).post_groups, 2);
                for y in 0..H {
                    for px in dark.clone() {
                        assert_eq!(
                            img.get(px, y),
                            [0, 0, 0, 255],
                            "scale {scale}, bright {}: bled into ({px}, {y})",
                            if bright_left { "left" } else { "right" }
                        );
                    }
                }
                // The bright half glows (well past the quad).
                let glow_x = if bright_left { W / 2 - 16 } else { W / 2 + 16 };
                assert!(lin(&img, glow_x, 8).x > 0.01, "scale {scale}: no glow at x {glow_x}");
            }
        }
    }

    /// HdrScene overlays draw into the scene color before post, so they bloom; the same fill as
    /// an AfterTonemap overlay (after post) does not.
    pub fn hdr_scene_overlays_feed_bloom(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        camera(&mut app, 0.0, BLACK, Some(post(Bloom::glow())));
        let rect = [56, 24, 16, 16];
        let overlays = app.resources.expect_mut::<RenderOverlays>();
        overlays.insert(
            "fill",
            0,
            FillOverlay::new(OverlayStage::HdrScene, rect, [8.0, 8.0, 8.0, 1.0]),
        );
        let hdr = render(&mut app);
        assert_eq!(lin(&hdr, 64, 32), Vec3::ONE, "the fill itself");
        let halo = lin(&hdr, 52, 32).x;
        assert!(halo > 0.05, "HdrScene fill blooms: {halo}");
        let overlays = app.resources.expect_mut::<RenderOverlays>();
        overlays.insert(
            "fill",
            0,
            FillOverlay::new(OverlayStage::AfterTonemap, rect, [8.0, 8.0, 8.0, 1.0]),
        );
        let after = render(&mut app);
        assert_eq!(lin(&after, 64, 32), Vec3::ONE);
        assert_eq!(max_outside(&after, rect, 0), 0.0, "AfterTonemap: drawn after bloom");
    }

    /// The Ldr fallback (no float render targets, or `HdrMode::Ldr`) has no bloom: the same image
    /// as without it, the composite as the only post pass and no chain memory.
    pub fn bloom_off_in_ldr_fallback(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        quad(&mut app, Vec3::splat(8.0), Vec2::ZERO, Vec2::splat(0.25));
        let cam = camera(&mut app, 0.0, BLACK, Some(post(Bloom::glow())));
        let hdr = render(&mut app);
        assert_eq!(stats(&app).post_passes, 10);
        assert!(max_outside(&hdr, [60, 28, 8, 8], 0) > 0.05, "Hdr: bloom");
        frame_settings(&mut app).downlevel.no_float_targets = true;
        let ldr = render(&mut app);
        let (passes, bytes) = (stats(&app).post_passes, stats(&app).target_bytes);
        assert_eq!(passes, 1, "composite only");
        set_post(&mut app, cam, post(Bloom::glow()).without_bloom());
        assert_eq!(render(&mut app), ldr, "Ldr: exactly no bloom");
        assert_eq!(stats(&app).target_bytes, bytes, "no chain memory");
        // `HdrMode::Ldr` on a device with float targets: the same.
        frame_settings(&mut app).downlevel.no_float_targets = false;
        set_post(&mut app, cam, post(Bloom::glow()));
        app.world
            .insert_one(cam, CameraFrame::default().with_hdr(HdrMode::Ldr))
            .unwrap();
        assert_eq!(render(&mut app), ldr);
        assert_eq!(stats(&app).post_passes, 1);
    }

    /// A render target cleared transparent: the composite weights bloom by the scene alpha, so
    /// no haze appears where nothing was drawn (alpha 0 stays exactly 0), while an opaque clear
    /// shows the same glow around the quad.
    pub fn transparent_rt_bloom_weighted_by_alpha(make: MakeApp) {
        let Some(mut app) = make(16, 16) else { return };
        let rt = app.resources.expect_mut::<RenderAssets>().add_render_target(64, 64);
        // 10 × 10 px in the 64 px target (32 px per unit).
        quad(&mut app, Vec3::splat(8.0), Vec2::ZERO, Vec2::splat(0.3125));
        let cam = app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 5.0),
            Camera3d::default()
                .with_clear_color(Vec4::ZERO)
                .with_target(RenderTarget::Texture(rt))
                .with_projection(Projection::Orthographic {
                    height: 2.0,
                    near: 0.1,
                    far: 100.0,
                }),
            post(Bloom::glow().with_intensity(1.0)),
        ));
        camera(&mut app, 0.0, BLACK, None);
        let read = |app: &mut App| {
            render(app);
            assert_eq!(stats(app).post_groups, 1);
            let texture = {
                let assets = app.resources.expect::<RenderAssets>();
                assets.render_target_view(rt).unwrap().texture().clone()
            };
            crate::common::read_texture(app.resources.expect::<Renderer>(), &texture)
        };
        let at = |px: &[[f32; 4]], x: usize, y: usize| px[y * 64 + x];
        let transparent = read(&mut app);
        for y in 0..64 {
            for x in 0..64 {
                let p = at(&transparent, x, y);
                if p[3] == 0.0 {
                    assert_eq!(p, [0.0; 4], "haze at ({x}, {y})");
                }
            }
        }
        assert_eq!(at(&transparent, 32, 32), [1.0; 4], "the quad");
        assert_eq!(at(&transparent, 24, 32), [0.0; 4], "right next to it");
        // Opaque clear: the glow is there.
        app.world.get::<&mut Camera3d>(cam).unwrap().clear_color = Some(Vec4::new(0.0, 0.0, 0.0, 1.0));
        let opaque = read(&mut app);
        let glow = at(&opaque, 24, 32);
        assert!(glow[0] > 0.05 && glow[3] == 1.0, "{glow:?}");
    }

    /// Two post groups of one size share one pooled chain; after warm-up nothing is created or
    /// reallocated and a still scene uploads nothing, with identical frames.
    pub fn bloom_steady_state(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        quad(&mut app, Vec3::splat(6.0), Vec2::new(0.5, 0.0), Vec2::splat(0.25));
        let left = camera(&mut app, 0.0, BLACK, Some(post(Bloom::glow())));
        let right = camera(&mut app, 0.0, BLACK, Some(PostProcess::default()));
        for (e, viewport, order) in [(left, [0.0, 0.0, 0.5, 1.0], 0), (right, [0.5, 0.0, 0.5, 1.0], 1)] {
            let mut c = app.world.get::<&mut Camera3d>(e).unwrap();
            c.viewport = Some(viewport);
            c.order = order;
        }
        let first = render(&mut app);
        let s = stats(&app);
        // 64 × 64 rects: level 0 is 32 × 32, five levels (the last keeps 2 px).
        assert_eq!(s.post_passes, 2 * (2 * 5 - 1 + 1), "{s:?}");
        let with_bloom = s.target_bytes;
        set_post(&mut app, left, post(Bloom::glow()).without_bloom());
        set_post(&mut app, right, PostProcess::default().without_bloom());
        render(&mut app);
        let no_bloom = stats(&app).target_bytes;
        set_post(&mut app, left, post(Bloom::glow()));
        set_post(&mut app, right, PostProcess::default());
        // One shared chain: 32² + 16² + 8² + 4² + 2² = 1364 pixels of 4 (Rg11b10Ufloat) or 8
        // (Rgba16Float) bytes.
        let chain = with_bloom - no_bloom;
        assert!(chain == 1364 * 4 || chain == 1364 * 8, "{chain} bytes of bloom chain");
        for _ in 0..4 {
            render(&mut app);
        }
        let s = stats(&app);
        assert_eq!(s.target_bytes, with_bloom);
        assert_eq!(
            (
                s.textures_created,
                s.cpu_reallocations,
                s.buffer_reallocations,
                s.bytes_uploaded
            ),
            (0, 0, 0, 0),
            "{s:?}"
        );
        assert_eq!(render(&mut app), first, "deterministic");
    }

    /// The downlevel switches bloom does not depend on (no compute, no read-only depth, the
    /// CPU draw path) change no pixel; `DownlevelOverride::webgl2()` (no float targets) is the
    /// Ldr fallback without bloom.
    pub fn bloom_downlevel_parity(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        quad(
            &mut app,
            Vec3::new(12.0, 4.0, 1.0),
            Vec2::new(-0.5, 0.2),
            Vec2::new(0.6, 0.05),
        );
        quad(
            &mut app,
            Vec3::new(1.0, 3.0, 9.0),
            Vec2::new(0.8, -0.3),
            Vec2::splat(0.15),
        );
        camera(
            &mut app,
            0.0,
            Vec4::new(0.01, 0.01, 0.02, 1.0),
            Some(PostProcess::default()),
        );
        let full = render(&mut app);
        frame_settings(&mut app).downlevel = DownlevelOverride {
            no_compute: true,
            no_read_only_depth: true,
            ..Default::default()
        };
        set_path(&mut app, DrawPath::CpuDirect);
        assert_eq!(render(&mut app), full);
        assert_eq!(stats(&app).post_passes, 10);
        frame_settings(&mut app).downlevel = DownlevelOverride::webgl2();
        render(&mut app);
        assert_eq!(stats(&app).post_passes, 1, "Ldr fallback: no bloom");
    }

    fn stats(app: &App) -> RenderStats {
        app.resources.expect::<RenderStats>().clone()
    }

    fn set_path(app: &mut App, path: DrawPath) {
        app.resources.expect_mut::<RenderSettings>().draw_path = Some(path);
    }
}
