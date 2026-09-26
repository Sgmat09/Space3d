//! Helpers and shared test bodies of the post-processing tests (graphics spec §5.2 P-POST):
//! `tests/post.rs` runs every case on the default backend, `tests/gl_post.rs` on wgpu's GL backend
//! (WebGL2 class: ANGLE on macOS). Included with `#[path = "common/post.rs"]` next to `mod common`.
//!
//! Scenes are unlit, camera-facing quads under an orthographic camera 2 world units high, so
//! every radiance is exact: at `H` pixels high one world unit is `H / 2` pixels. Post settings
//! default to `Tonemapper::None` and no dither ([`post`]), so the display value of a pixel is
//! its exposed radiance clamped to 1, and a bloom halo is directly measurable in linear light.
#![allow(dead_code)]

use space3d_core::HdrImageData;
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

/// Post without tonemapping, dither and bloom, with auto exposure `ae` (enabled).
pub fn post_ae(ae: AutoExposure) -> PostProcess {
    post(Bloom::glow()).without_bloom().with_auto_exposure(ae)
}

/// Whether a renderer meters with the compute histogram (else the render-pass chain, whose
/// accuracy against the exact metering is ±0.5 EV instead of ±0.1).
pub fn meters_with_compute(app: &App, settings_no_compute: bool) -> bool {
    let caps = app.resources.expect::<Renderer>().graphics_caps();
    !settings_no_compute && caps.compute_histogram && caps.exposure_format == Some(wgpu::TextureFormat::R32Float)
}

/// The exposure (stops) auto exposure should settle at for an HDR scene capture (the post
/// group's rect is the whole capture), computed exactly: every pixel whose luminance is above
/// 2⁻¹⁶ (black is not metered) weighs 1 (`Average`) or 4 at the center falling linearly to 1 at
/// the corners (`CenterWeighted`); with `trim`, the weight outside the [low, high] percentile
/// window of the sorted log2 luminances is cut; the weighted mean log2 luminance is exposed to
/// middle grey and the result clamped. An independent reimplementation of the metering rule
/// (no histogram bins).
pub fn reference_ev(scene: &HdrImageData, ae: &AutoExposure, trim: bool) -> f32 {
    let (w, h) = (scene.width, scene.height);
    let mut samples: Vec<(f64, f64)> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let p = scene.pixels[(y * w + x) as usize];
            let lum = 0.2126 * p[0].max(0.0) + 0.7152 * p[1].max(0.0) + 0.0722 * p[2].max(0.0);
            if lum.is_nan() || lum <= (-16.0f32).exp2() {
                continue;
            }
            let weight = match ae.metering {
                Metering::Average => 1.0,
                _ => {
                    let d = Vec2::new(
                        ((x as f32 + 0.5) / w as f32 - 0.5) * 2.0,
                        ((y as f32 + 0.5) / h as f32 - 0.5) * 2.0,
                    );
                    let r = (d.length() * std::f32::consts::FRAC_1_SQRT_2).clamp(0.0, 1.0);
                    4.0 + (1.0 - 4.0) * r
                }
            };
            samples.push((lum.log2().clamp(-16.0, 16.0) as f64, weight as f64));
        }
    }
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: f64 = samples.iter().map(|s| s.1).sum();
    let (lo, hi) = if trim {
        (total * ae.low_percentile as f64, total * ae.high_percentile as f64)
    } else {
        (0.0, total)
    };
    let (mut sw, mut swl, mut incl) = (0.0, 0.0, 0.0);
    for (l, weight) in samples {
        let excl = incl;
        incl += weight;
        let portion = (incl.min(hi) - excl.max(lo)).max(0.0);
        sw += portion;
        swl += portion * l;
    }
    let mean = if sw > 0.0 { swl / sw } else { 0.18f64.log2() };
    ((0.18f64.log2() - mean) as f32).clamp(ae.min_ev, ae.max_ev)
}

/// The primary post group's scene color `S` of the last frame.
pub fn scene_hdr(app: &mut App) -> HdrImageData {
    app.resources
        .expect_mut::<Renderer>()
        .capture_scene_hdr_blocking()
        .expect("scene capture")
}

/// Log2 of a region's displayed linear value over its radiance: the exposure applied (with
/// `Tonemapper::None`, below clipping).
pub fn displayed_ev(img: &ImageData, rect: [u32; 4], radiance: f32) -> f32 {
    (mean(img, rect).x / radiance).log2()
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
    /// beyond its edges, fading with distance, and in one scene the 0.8 quad changes no pixel
    /// outside itself. The glow spans the same fraction of the frame as at 1080p (the reference
    /// chain, `chain_size`): in this 64-px-high frame it fades out about 10 px from the quad.
    pub fn bloom_only_above_threshold(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        // 8 × 8 px quads: 0.25 units at 32 px per unit.
        let size = Vec2::splat(0.25);
        let dim = quad(&mut app, Vec3::splat(0.8), Vec2::new(0.0, 0.0), size);
        let cam = camera(&mut app, 0.0, BLACK, Some(post(hard())));
        let bloomed = render(&mut app);
        // A 128 × 64 rect: the reference chain's 34-px level (standing for the four finer ones,
        // resampled from the prefilter's 32-px half of the rect) and its 17-px level.
        assert_eq!(
            stats(&app).post_passes,
            5,
            "prefilter, resample, 1 down, 1 up, composite"
        );
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
        let far = lin(&hot, 55, 32).x;
        let farther = lin(&hot, 52, 32).x;
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
                // The bright half glows around the quad (its pixels below the quad's 1.0).
                let half = if bright_left { 0..W / 2 } else { W / 2..W };
                let glow = (0..H)
                    .flat_map(|y| half.clone().map(move |x| (x, y)))
                    .map(|(x, y)| lin(&img, x, y).x)
                    .filter(|v| *v < 0.9)
                    .fold(0.0f32, f32::max);
                assert!(glow > 0.05, "scale {scale}: no glow around the quad ({glow})");
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
        assert_eq!(stats(&app).post_passes, 5);
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
        // 64 × 64 rects: the reference chain's 540 / 2^4 = 34-px level stands for the four
        // finer ones (resampled from the prefilter's 32 × 32), then its 17-px level: two levels.
        assert_eq!(s.post_passes, 2 * (2 * 2 - 1 + 1 + 1), "{s:?}");
        let with_bloom = s.target_bytes;
        set_post(&mut app, left, post(Bloom::glow()).without_bloom());
        set_post(&mut app, right, PostProcess::default().without_bloom());
        render(&mut app);
        let no_bloom = stats(&app).target_bytes;
        set_post(&mut app, left, post(Bloom::glow()));
        set_post(&mut app, right, PostProcess::default());
        // One shared chain and half-resolution texture: 34² + 17² + 32² = 2469 pixels of 4
        // (Rg11b10Ufloat) or 8 (Rgba16Float) bytes.
        let chain = with_bloom - no_bloom;
        assert!(chain == 2469 * 4 || chain == 2469 * 8, "{chain} bytes of bloom chain");
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

    /// A 1 px source of radiance 30 gets the same halo wherever it sits (the reviewed failure:
    /// at 720p, 1440p and 4K the prefilter's taps left gaps, and the halo of a moving spark
    /// blinked between none, full and double). Moved over 12 pixel positions (every phase of
    /// the chain's texels), the energy bloom adds stays within 5 % of its mean for each ratio of
    /// rect to level 0 (`chain_size`, emulated on a 320 × 180 rect with a lower
    /// `max_mip_height`): 2× (1080p), 2.65× capped (1440p), 4× (4K), 2.65× below the reference
    /// height (720p, 360p) and 1.67× (900p).
    pub fn bloom_halo_is_independent_of_position(make: MakeApp) {
        let (w, h) = (320u32, 180u32);
        let Some(mut app) = make(w, h) else { return };
        let px = 2.0 / h as f32;
        // Pixel (x, y)'s center in world units (ortho camera 2 units high at x = 0).
        let at = |x: u32, y: u32| {
            Vec2::new(
                (x as f32 + 0.5 - w as f32 / 2.0) * px,
                (h as f32 / 2.0 - y as f32 - 0.5) * px,
            )
        };
        let dot = quad(&mut app, Vec3::new(30.0, 15.0, 6.0), at(150, 80), Vec2::splat(px));
        let cam = camera(&mut app, 0.0, BLACK, None);
        let total = |img: &ImageData| mean_all(img).dot(Vec3::ONE) as f64 * (w * h) as f64;
        for (label, max_mip_height, passes) in [
            ("2× (1080p)", 90, 11),
            ("2.65×, capped (1440p)", 68, 12),
            ("4×, capped (4K)", 45, 10),
            ("2.65×, merged levels (720p)", 540, 6),
            ("1.67× (900p)", 108, 12),
        ] {
            let bloom = Bloom {
                max_mip_height,
                ..Bloom::glow()
            };
            let mut added = Vec::new();
            for k in 0..12u32 {
                let p = at(150 + k, 80 + (5 * k) % 7);
                app.world.get::<&mut Transform>(dot).unwrap().translation = p.extend(0.0);
                set_post(&mut app, cam, post(bloom));
                let bloomed = render(&mut app);
                assert_eq!(stats(&app).post_passes, passes + 1, "{label}");
                set_post(&mut app, cam, post(bloom).without_bloom());
                let plain = render(&mut app);
                added.push(total(&bloomed) - total(&plain));
            }
            let mean = added.iter().sum::<f64>() / added.len() as f64;
            let (lo, hi) = added
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), &a| (lo.min(a), hi.max(a)));
            let spread = (hi - lo) / mean;
            eprintln!(
                "{label}: bloom adds {lo:.2} … {hi:.2} (mean {mean:.2}, spread {:.1} %)",
                100.0 * spread
            );
            assert!(mean > 1.0, "{label}: a halo ({mean:.2})");
            assert!(spread < 0.05, "{label}: halo energy {lo:.2} … {hi:.2} over positions");
        }
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
        assert_eq!(stats(&app).post_passes, 5);
        frame_settings(&mut app).downlevel = DownlevelOverride::webgl2();
        render(&mut app);
        assert_eq!(stats(&app).post_passes, 1, "Ldr fallback: no bloom");
    }

    /// Post without bloom (and, from `post`, without tonemapping and dither).
    fn plain() -> PostProcess {
        post(Bloom::glow()).without_bloom()
    }

    /// Pixels with an intermediate red value (neither 0 nor 255).
    fn partial(img: &ImageData) -> usize {
        img.pixels.chunks(4).filter(|p| p[0] != 0 && p[0] != 255).count()
    }

    /// Standard deviation of the red channel (8-bit levels).
    fn red_std(img: &ImageData) -> f64 {
        let n = (img.width * img.height) as f64;
        let mean = img.pixels.chunks(4).map(|p| p[0] as f64).sum::<f64>() / n;
        (img.pixels.chunks(4).map(|p| (p[0] as f64 - mean).powi(2)).sum::<f64>() / n).sqrt()
    }

    fn grain(intensity: f32, seed: u32, animated: bool) -> FilmGrain {
        FilmGrain {
            enabled: true,
            intensity,
            size: 1.0,
            luminance_response: 0.0,
            animated,
            seed,
        }
    }

    fn advance_frame(app: &mut App) {
        app.resources.init::<Time>().frame += 1;
    }

    /// FXAA smooths the stair steps of a rotated square (no MSAA: every edge pixel is 0 or 255
    /// without it) at every quality, and leaves flat areas alone. It is one pass after the
    /// composite.
    pub fn fxaa_softens_edges(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let q = quad(&mut app, Vec3::ONE, Vec2::ZERO, Vec2::splat(1.0));
        {
            let mut t = app.world.get::<&mut Transform>(q).unwrap();
            t.rotation = Quat::from_rotation_z(0.35) * t.rotation;
        }
        let cam = camera(&mut app, 0.0, BLACK, Some(plain()));
        let hard = render(&mut app);
        assert_eq!(stats(&app).post_passes, 1);
        assert_eq!(partial(&hard), 0, "no anti-aliasing: only 0 and 255");
        for q in [
            FxaaQuality::Low,
            FxaaQuality::Medium,
            FxaaQuality::High,
            FxaaQuality::Ultra,
        ] {
            set_post(&mut app, cam, plain().with_fxaa(q));
            let img = render(&mut app);
            assert_eq!(stats(&app).post_passes, 2, "{q:?}: composite + FXAA");
            let n = partial(&img);
            assert!(n >= 40, "{q:?}: {n} softened edge pixels");
            for (x, y) in [(64, 32), (2, 2), (125, 61)] {
                assert_eq!(img.get(x, y), hard.get(x, y), "{q:?}: flat area at ({x}, {y})");
            }
        }
    }

    /// Film grain is a pure function of its seed: the same seed gives the same image, another
    /// seed another pattern; `animated` grain changes with `Time::frame`, still grain does not;
    /// `luminance_response` 1 leaves white untouched.
    pub fn film_grain_is_seeded(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let grey = Vec4::new(0.2, 0.2, 0.2, 1.0);
        let cam = camera(&mut app, 0.0, grey, Some(plain().with_film_grain(grain(0.2, 7, false))));
        let a = render(&mut app);
        assert_eq!(render(&mut app), a, "render_to_image twice");
        assert!(red_std(&a) > 5.0, "grain visible: σ {}", red_std(&a));
        set_post(&mut app, cam, plain().without_bloom());
        let flat = render(&mut app);
        assert_eq!(red_std(&flat), 0.0);
        // Zero-mean in the encoded space.
        let mean = |img: &ImageData| img.pixels.chunks(4).map(|p| p[0] as f64).sum::<f64>() / (W * H) as f64;
        assert!((mean(&a) - mean(&flat)).abs() < 2.0, "{} vs {}", mean(&a), mean(&flat));
        set_post(&mut app, cam, plain().with_film_grain(grain(0.2, 8, false)));
        assert_ne!(render(&mut app), a, "another seed");
        // Still grain ignores the frame; animated grain follows it (and repeats with it).
        set_post(&mut app, cam, plain().with_film_grain(grain(0.2, 7, false)));
        advance_frame(&mut app);
        assert_eq!(render(&mut app), a, "still grain, next frame");
        set_post(&mut app, cam, plain().with_film_grain(grain(0.2, 7, true)));
        let f1 = render(&mut app);
        assert_eq!(render(&mut app), f1, "same frame, same grain");
        advance_frame(&mut app);
        let f2 = render(&mut app);
        assert_ne!(f2, f1, "animated grain, next frame");
        // A still scene with animated grain uploads nothing (spec §4.4): the seeds of 64 frames
        // went up at once when the grain changed (it uploaded 16 bytes every frame).
        assert_eq!(stats(&app).bytes_uploaded, 0);
        // Highlights: response 1 keeps white clean.
        let white = FilmGrain {
            luminance_response: 1.0,
            ..grain(0.2, 7, false)
        };
        app.world.get::<&mut Camera3d>(cam).unwrap().clear_color = Some(Vec4::ONE);
        set_post(&mut app, cam, plain().with_film_grain(white));
        let img = render(&mut app);
        assert!(img.pixels.chunks(4).all(|p| p[..3] == [255, 255, 255]));
    }

    /// Grain, dither and encoding run in the last pass: with FXAA that is the FXAA pass, so the
    /// grain is not smoothed by it. A flat scene with strong 1-pixel grain looks the same with
    /// and without FXAA (within 1 level: FXAA reads the 8-bit intermediate).
    pub fn grain_after_fxaa(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let grey = Vec4::new(0.3, 0.3, 0.3, 1.0);
        let lens = plain().with_film_grain(grain(0.25, 3, false));
        let cam = camera(&mut app, 0.0, grey, Some(lens));
        let without = render(&mut app);
        set_post(&mut app, cam, lens.with_fxaa(FxaaQuality::High));
        let with = render(&mut app);
        assert_eq!(stats(&app).post_passes, 2);
        assert!(max_diff(&with, &without) <= 1, "{}", max_diff(&with, &without));
        let (a, b) = (red_std(&without), red_std(&with));
        assert!(
            a > 10.0 && (a - b).abs() < 0.05 * a,
            "grain σ {a} without FXAA, {b} with"
        );
    }

    /// AfterTonemap overlays draw after post: grain, chromatic aberration and vignette never
    /// touch them, even in a corner where all three are strongest. The same fill as an HdrScene
    /// overlay is post-processed.
    pub fn after_tonemap_overlays_untouched_by_grain_ca_vignette(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let grey = Vec4::new(0.3, 0.3, 0.3, 1.0);
        let lens = plain()
            .with_vignette(Vignette {
                enabled: true,
                intensity: 1.0,
                ..Vignette::default()
            })
            .with_chromatic_aberration(1.0)
            .with_film_grain(grain(0.05, 5, true))
            .with_fxaa(FxaaQuality::Medium);
        camera(&mut app, 0.0, grey, Some(PostProcess { dither: true, ..lens }));
        let value = [0.5, 0.25, 0.75, 1.0];
        let overlays = app.resources.expect_mut::<RenderOverlays>();
        overlays.insert(
            "after",
            0,
            FillOverlay::new(OverlayStage::AfterTonemap, [0, 0, 16, 16], value),
        );
        overlays.insert(
            "hdr",
            0,
            FillOverlay::new(OverlayStage::HdrScene, [112, 48, 16, 16], value),
        );
        let img = render(&mut app);
        let expected = img.get(0, 0);
        let srgb = |v: f32| (linear_to_srgb(v) * 255.0).round() as i32;
        for (c, v) in value[..3].iter().enumerate() {
            assert!((expected[c] as i32 - srgb(*v)).abs() <= 1, "{expected:?}");
        }
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(img.get(x, y), expected, "AfterTonemap fill at ({x}, {y})");
            }
        }
        // The HdrScene fill got the vignette (dark corner) and grain.
        let hdr: Vec<[u8; 4]> = (48..64)
            .flat_map(|y| (112..128).map(move |x| (x, y)))
            .map(|(x, y)| img.get(x, y))
            .collect();
        assert!(
            hdr.iter().all(|p| (p[2] as i32) < srgb(0.75) - 10),
            "vignetted: {:?}",
            hdr[0]
        );
        assert!(hdr.iter().any(|p| *p != hdr[0]), "grain on the HdrScene fill");
    }

    /// Vignette: the center is untouched; a centered vignette of intensity `i` multiplies the
    /// corners by `mix(1, color, i)` (black: darker, a color: tinted).
    pub fn vignette_darkens_corners(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let grey = Vec4::new(0.5, 0.5, 0.5, 1.0);
        let cam = camera(&mut app, 0.0, grey, Some(plain()));
        let flat = render(&mut app);
        let vignette = Vignette {
            enabled: true,
            intensity: 0.5,
            ..Vignette::default()
        };
        set_post(&mut app, cam, plain().with_vignette(vignette));
        let img = render(&mut app);
        assert_eq!(img.get(64, 32), flat.get(64, 32), "center");
        let corner = lin(&img, 0, 0).x;
        assert!((corner - 0.25).abs() < 0.01, "corner {corner}");
        // Round (default) vs following the rect's 2:1 shape: the left edge's middle is darker
        // when round (it lies closer to the circle's rim).
        let round_edge = lin(&img, 0, 32).x;
        set_post(
            &mut app,
            cam,
            plain().with_vignette(Vignette {
                roundness: 0.0,
                ..vignette
            }),
        );
        let oval_edge = lin(&render(&mut app), 0, 32).x;
        assert!(round_edge < oval_edge - 0.05, "{round_edge} vs {oval_edge}");
        // Tint: full intensity toward red keeps red and removes green and blue at the corners.
        set_post(
            &mut app,
            cam,
            plain().with_vignette(Vignette {
                intensity: 1.0,
                color: Vec3::X,
                ..vignette
            }),
        );
        let c = lin(&render(&mut app), 127, 63);
        assert!((c.x - 0.5).abs() < 0.01 && c.y < 0.01 && c.z < 0.01, "{c}");
    }

    /// Chromatic aberration splits red and blue radially, growing with r²: fringes around a
    /// quad near the corner, none around one at the center.
    pub fn chromatic_aberration_splits_toward_corners(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        quad(&mut app, Vec3::ONE, Vec2::ZERO, Vec2::splat(0.4));
        quad(&mut app, Vec3::ONE, Vec2::new(1.6, 0.6), Vec2::splat(0.4));
        camera(&mut app, 0.0, BLACK, Some(plain().with_chromatic_aberration(1.0)));
        let img = render(&mut app);
        // Largest red/blue difference in linear light over a window.
        let split = |x0: u32, x1: u32, y0: u32, y1: u32| {
            let mut m = 0.0f32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let c = lin(&img, x, y);
                    m = m.max((c.x - c.z).abs());
                }
            }
            m
        };
        // At the center quad's edges (≤ 9 px from the center) the split is ~0.005 px.
        assert!(split(50, 78, 20, 44) < 0.02, "center: {}", split(50, 78, 20, 44));
        // Near the corner (~55 px out) about 1 px: whole fringes.
        assert!(split(100, 128, 0, 24) > 0.5, "corner: {}", split(100, 128, 0, 24));
    }

    /// Chromatic aberration and FXAA taps stay inside their post group: a white half never
    /// lights the black one, either way round.
    pub fn lens_effects_do_not_bleed_across_viewports(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let lens = plain().with_chromatic_aberration(1.0).with_fxaa(FxaaQuality::High);
        let left = camera(&mut app, 0.0, Vec4::ONE, Some(lens));
        let right = camera(&mut app, 50.0, BLACK, Some(lens));
        for (e, viewport, order) in [(left, [0.0, 0.0, 0.5, 1.0], 0), (right, [0.5, 0.0, 0.5, 1.0], 1)] {
            let mut c = app.world.get::<&mut Camera3d>(e).unwrap();
            c.viewport = Some(viewport);
            c.order = order;
        }
        for white_left in [true, false] {
            let (white, black) = if white_left { (left, right) } else { (right, left) };
            app.world.get::<&mut Camera3d>(white).unwrap().clear_color = Some(Vec4::ONE);
            app.world.get::<&mut Camera3d>(black).unwrap().clear_color = Some(BLACK);
            let img = render(&mut app);
            assert_eq!(stats(&app).post_passes, 4);
            for y in 0..H {
                for x in 0..W {
                    let expected = if (x < W / 2) == white_left {
                        [255; 4]
                    } else {
                        [0, 0, 0, 255]
                    };
                    assert_eq!(img.get(x, y), expected, "white left {white_left}: ({x}, {y})");
                }
            }
        }
    }

    /// FXAA and grain run in the Ldr fallback too (display-space effects); post groups of one
    /// size share one LDR intermediate; the steady state creates nothing and a still scene
    /// uploads nothing.
    pub fn fxaa_ldr_fallback_and_steady_state(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let q = quad(&mut app, Vec3::ONE, Vec2::ZERO, Vec2::splat(0.8));
        {
            let mut t = app.world.get::<&mut Transform>(q).unwrap();
            t.rotation = Quat::from_rotation_z(0.4) * t.rotation;
        }
        let fxaa = plain()
            .with_fxaa(FxaaQuality::Medium)
            .with_film_grain(grain(0.05, 1, false));
        let left = camera(&mut app, 0.0, BLACK, Some(fxaa));
        frame_settings(&mut app).downlevel.no_float_targets = true;
        let img = render(&mut app);
        assert_eq!(stats(&app).post_passes, 2, "Ldr fallback: composite + FXAA");
        assert!(partial(&img) > 20);
        frame_settings(&mut app).downlevel.no_float_targets = false;
        let right = camera(&mut app, 0.0, BLACK, Some(fxaa));
        for (e, viewport, order) in [(left, [0.0, 0.0, 0.5, 1.0], 0), (right, [0.5, 0.0, 0.5, 1.0], 1)] {
            let mut c = app.world.get::<&mut Camera3d>(e).unwrap();
            c.viewport = Some(viewport);
            c.order = order;
        }
        let first = render(&mut app);
        let with_fxaa = stats(&app).target_bytes;
        assert_eq!(stats(&app).post_passes, 4);
        for e in [left, right] {
            set_post(&mut app, e, plain());
        }
        render(&mut app);
        let without = stats(&app).target_bytes;
        assert_eq!(with_fxaa - without, 4 * (W * H) as u64, "one shared LDR intermediate");
        for e in [left, right] {
            set_post(&mut app, e, fxaa);
        }
        for _ in 0..4 {
            render(&mut app);
        }
        let s = stats(&app);
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

    /// A render target cleared transparent with every display-space effect: grain, dither,
    /// vignette, chromatic aberration and FXAA are weighted by alpha (alpha 0 stays exactly 0).
    pub fn transparent_rt_lens_effects_keep_alpha(make: MakeApp) {
        let Some(mut app) = make(16, 16) else { return };
        let rt = app.resources.expect_mut::<RenderAssets>().add_render_target(64, 64);
        let q = quad(&mut app, Vec3::splat(4.0), Vec2::ZERO, Vec2::splat(0.6));
        {
            let mut t = app.world.get::<&mut Transform>(q).unwrap();
            t.rotation = Quat::from_rotation_z(0.3) * t.rotation;
        }
        let lens = PostProcess::cinematic().with_film_grain(grain(0.3, 9, false));
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 5.0),
            Camera3d::default()
                .with_clear_color(Vec4::ZERO)
                .with_target(RenderTarget::Texture(rt))
                .with_projection(Projection::Orthographic {
                    height: 2.0,
                    near: 0.1,
                    far: 100.0,
                }),
            lens,
        ));
        camera(&mut app, 0.0, BLACK, None);
        render(&mut app);
        assert_eq!(
            stats(&app).post_passes,
            4 + 2,
            "bloom (2 levels, resampled), composite, FXAA"
        );
        let texture = {
            let assets = app.resources.expect::<RenderAssets>();
            assets.render_target_view(rt).unwrap().texture().clone()
        };
        let px = crate::common::read_texture(app.resources.expect::<Renderer>(), &texture);
        let mut transparent = 0;
        for p in &px {
            if p[3] == 0.0 {
                transparent += 1;
                assert_eq!(*p, [0.0; 4]);
            }
        }
        assert!(transparent > 1000, "{transparent} transparent pixels");
        assert_eq!(px[32 * 64 + 32][3], 1.0);
    }

    // ---- Auto exposure (PO2) ----

    /// Radiances of the auto-exposure scene at brightness 1: the backdrop (clear color), a grey
    /// slab, the probe (whose displayed value measures the applied exposure), a bright panel and
    /// a small light (cut by the high percentile).
    const AE_BACKDROP: f32 = 0.02;
    const AE_SLAB: f32 = 0.3;
    const AE_PROBE: f32 = 0.05;
    const AE_BRIGHT: f32 = 2.0;
    const AE_LIGHT: f32 = 60.0;
    /// Pixels inside the probe quad (in the 128 × 64 frame).
    const AE_PROBE_RECT: [u32; 4] = [86, 15, 20, 12];

    struct AeScene {
        camera: Entity,
        quads: [(Entity, f32); 4],
    }

    impl AeScene {
        fn spawn(app: &mut App, post: PostProcess) -> Self {
            let quads = [
                (AE_SLAB, Vec2::new(-1.0, 0.0), Vec2::new(1.5, 1.7)),
                (AE_PROBE, Vec2::new(1.0, 0.35), Vec2::new(0.8, 0.5)),
                (AE_BRIGHT, Vec2::new(1.0, -0.55), Vec2::new(0.6, 0.4)),
                (AE_LIGHT, Vec2::new(0.2, 0.7), Vec2::new(0.1, 0.1)),
            ]
            .map(|(v, center, size)| (quad(app, Vec3::splat(v), center, size), v));
            let camera = camera(
                app,
                0.0,
                Vec4::new(AE_BACKDROP, AE_BACKDROP, AE_BACKDROP, 1.0),
                Some(post),
            );
            Self { camera, quads }
        }

        /// Every radiance × `f`.
        fn brightness(&self, app: &mut App, f: f32) {
            for (e, v) in self.quads {
                app.world
                    .insert_one(e, Material::unlit(Vec3::splat(v * f).extend(1.0)))
                    .unwrap();
            }
            let b = AE_BACKDROP * f;
            app.world.get::<&mut Camera3d>(self.camera).unwrap().clear_color = Some(Vec4::new(b, b, b, 1.0));
        }
    }

    /// Auto exposure meters the scene like the exact rule (`reference_ev`: percentile window,
    /// center weighting, black excluded) within ±0.1 EV on the compute path and ±0.5 EV on the
    /// render-pass path (no percentile window: there it matches the untrimmed rule within
    /// 0.15 EV): the first frame snaps, a 4× brighter scene is followed gradually (to-bright rate
    /// 3/s: 1/60 s closes 3.4 % of the gap) and converges, and so does the way back (rate 1/s).
    /// The composite applies 2^E (the displayed probe) and `camera_exposure` reads it back.
    pub fn auto_exposure_converges_and_matches_reference(make: MakeApp, no_compute: bool) {
        let Some(mut app) = make(W, H) else { return };
        frame_settings(&mut app).read_exposure = true;
        frame_settings(&mut app).downlevel.no_compute = no_compute;
        let compute = meters_with_compute(&app, no_compute);
        let tolerance = if compute { 0.1 } else { 0.5 };
        let ae = AutoExposure::on();
        let scene = AeScene::spawn(&mut app, post_ae(ae));
        app.step_ticks(1);
        let img = render(&mut app);
        let hdr = scene_hdr(&mut app);
        let want = reference_ev(&hdr, &ae, true);
        let untrimmed = reference_ev(&hdr, &ae, false);
        let got = displayed_ev(&img, AE_PROBE_RECT, AE_PROBE);
        eprintln!("compute {compute}: metered {got:.3} EV, reference {want:.3} (untrimmed {untrimmed:.3})");
        assert!((got - want).abs() <= tolerance, "first frame: {got} vs {want}");
        if !compute {
            assert!((got - untrimmed).abs() <= 0.15, "render passes: {got} vs {untrimmed}");
        }
        let s = stats(&app);
        if compute {
            assert_eq!((s.compute_passes, s.post_passes), (1, 1), "{s:?}");
        } else {
            // 128 × 64: a 32 × 16 level, reduced once to 4 × 2, then the adapt pass.
            assert_eq!((s.compute_passes, s.post_passes), (0, 4), "{s:?}");
        }
        // The readback (1–3 frames late) reports the same exposure.
        app.step_ticks(4);
        let read = app
            .resources
            .expect::<Renderer>()
            .camera_exposure(scene.camera)
            .expect("read back");
        assert!((read - got).abs() < 0.03, "read back {read} vs displayed {got}");

        // 4× brighter: the target drops by 2 EV; the exposure follows gradually.
        scene.brightness(&mut app, 4.0);
        app.step_ticks(1);
        let one = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE * 4.0);
        assert!(
            one < got - 0.03 && one > got - 0.2,
            "one frame later: {one} (from {got})"
        );
        app.step_ticks(150);
        let img = render(&mut app);
        let want4 = reference_ev(&scene_hdr(&mut app), &ae, true);
        assert!((want4 - (want - 2.0)).abs() < 0.05, "reference {want4} vs {want} − 2");
        let got4 = displayed_ev(&img, AE_PROBE_RECT, AE_PROBE * 4.0);
        assert!(
            (got4 - want4).abs() <= tolerance,
            "converged to bright: {got4} vs {want4}"
        );
        assert!(
            (got4 - (got - 2.0)).abs() < 0.06,
            "the same metering 2 EV lower: {got4} vs {got}"
        );
        // Back to dark (rate 1/s: 6 s leave 1/64 of the gap).
        scene.brightness(&mut app, 1.0);
        app.step_ticks(360);
        let back = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE);
        assert!((back - got).abs() < 0.06, "back: {back} vs {got}");
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    }

    /// `AutoExposure::instant()` converges in the frame the scene changes (even when
    /// `Time::frame` does not advance), and any camera's first frame snaps: a one-shot capture
    /// is exposed correctly.
    pub fn instant_converges_frame_1(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let compute = meters_with_compute(&app, false);
        let tolerance = if compute { 0.1 } else { 0.5 };
        let ae = AutoExposure::on();
        // Default adaptation, first frame (no step: `render_to_image` only).
        let scene = AeScene::spawn(&mut app, post_ae(ae));
        let img = render(&mut app);
        let want = reference_ev(&scene_hdr(&mut app), &ae, true);
        let first = displayed_ev(&img, AE_PROBE_RECT, AE_PROBE);
        assert!((first - want).abs() <= tolerance, "snap: {first} vs {want}");
        // Default adaptation without time: the change is not followed at all.
        scene.brightness(&mut app, 4.0);
        let frozen = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE * 4.0);
        assert!(
            (frozen - first).abs() < 0.02,
            "dt = 0 adapts nothing: {frozen} vs {first}"
        );
        // Instant: converged in the frame.
        set_post(&mut app, scene.camera, post_ae(ae.instant()));
        let now = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE * 4.0);
        assert!((now - (first - 2.0)).abs() < 0.06, "instant: {now} vs {first} − 2");
        scene.brightness(&mut app, 1.0);
        let again = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE);
        assert!((again - first).abs() < 0.02, "{again} vs {first}");
    }

    /// With auto exposure and animated grain and dither on, rendering the same frame twice gives
    /// identical images (no adaptation and the same seeds when `Time::frame` does not advance),
    /// and so do two apps stepped through the same ticks.
    pub fn render_to_image_is_idempotent_with_ae_and_grain(make: MakeApp) {
        let run = |make: MakeApp| {
            let mut app = make(W, H)?;
            let post = PostProcess::default()
                .with_auto_exposure(AutoExposure::on())
                .with_film_grain(FilmGrain {
                    enabled: true,
                    animated: true,
                    ..FilmGrain::default()
                });
            let scene = AeScene::spawn(&mut app, post);
            app.step_ticks(10);
            scene.brightness(&mut app, 3.0);
            app.step_ticks(7);
            let a = render(&mut app);
            let b = render(&mut app);
            assert_eq!(a, b, "render_to_image twice");
            Some(a)
        };
        let Some(first) = run(make) else { return };
        let second = run(make).expect("adapter");
        assert_eq!(first, second, "the same ticks give the same image");
    }

    /// Auto exposure feeds the bloom prefilter: a dim scene is brightened by about 3 EV, so a 0.5
    /// panel (below the threshold at 0 EV) blooms; with the same compensation but no auto
    /// exposure it does not.
    pub fn auto_exposure_feeds_bloom(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        quad(&mut app, Vec3::splat(0.5), Vec2::ZERO, Vec2::splat(0.25));
        let ae = AutoExposure {
            metering: Metering::Average,
            ..AutoExposure::on()
        };
        let bloom = Bloom::glow().with_threshold(1.0, 0.0);
        let cam = camera(
            &mut app,
            0.0,
            Vec4::new(0.02, 0.02, 0.02, 1.0),
            Some(post(bloom).with_auto_exposure(ae)),
        );
        let img = render(&mut app);
        let bg = lin(&img, 4, 4).x;
        // Mostly 0.02: exposed toward 0.18, the panel far above 1.
        assert!(bg > 0.1, "backdrop brightened: {bg}");
        let halo = lin(&img, 56, 32).x - bg;
        assert!(halo > 0.02, "the exposed panel blooms: {halo}");
        set_post(&mut app, cam, post(bloom));
        let manual = render(&mut app);
        assert_eq!(lin(&manual, 56, 32).x, lin(&manual, 4, 4).x, "0.5 at 0 EV: no bloom");
    }

    /// Each post group meters its own rect and keeps its own history: a split screen of a dark
    /// and a bright view exposes them about 2 EV apart; the Ldr fallback has no auto exposure
    /// (the manual exposure applies).
    pub fn auto_exposure_per_post_group(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        frame_settings(&mut app).read_exposure = true;
        let ae = AutoExposure {
            metering: Metering::Average,
            ..AutoExposure::on().instant()
        };
        // The left camera sees a 0.05 wall, the right one (x = 50) a 0.2 wall.
        quad(&mut app, Vec3::splat(0.05), Vec2::ZERO, Vec2::splat(4.0));
        quad(&mut app, Vec3::splat(0.2), Vec2::new(50.0, 0.0), Vec2::splat(4.0));
        let left = camera(&mut app, 0.0, BLACK, Some(post_ae(ae)));
        let right = camera(&mut app, 50.0, BLACK, Some(post_ae(ae)));
        for (e, viewport, order) in [(left, [0.0, 0.0, 0.5, 1.0], 0), (right, [0.5, 0.0, 0.5, 1.0], 1)] {
            let mut c = app.world.get::<&mut Camera3d>(e).unwrap();
            c.viewport = Some(viewport);
            c.order = order;
        }
        let img = render(&mut app);
        let (l, r) = (lin(&img, 32, 32).x, lin(&img, 96, 32).x);
        // Both exposed to middle grey.
        assert!((l - 0.18).abs() < 0.02 && (r - 0.18).abs() < 0.02, "{l} {r}");
        app.step_ticks(4);
        let renderer = app.resources.expect::<Renderer>();
        let (el, er) = (
            renderer.camera_exposure(left).unwrap(),
            renderer.camera_exposure(right).unwrap(),
        );
        assert!((el - er - 2.0).abs() < 0.1, "{el} vs {er}");
        // The Ldr fallback: manual exposure, reported as such.
        frame_settings(&mut app).downlevel.no_float_targets = true;
        set_post(&mut app, left, post_ae(ae).with_exposure(1.0));
        app.step_ticks(4);
        let img = render(&mut app);
        assert!((lin(&img, 32, 32).x - 0.1).abs() < 0.01, "0.05 at +1 EV");
        assert_eq!(app.resources.expect::<Renderer>().camera_exposure(left), Some(1.0));
    }

    /// `Renderer::reset_post_history` and turning auto exposure back on snap to the target;
    /// after warm-up metering creates no textures, reallocates nothing and a still scene
    /// uploads nothing (a steady `dt` leaves the adaptation factors unchanged).
    pub fn auto_exposure_history_and_steady_state(make: MakeApp, no_compute: bool) {
        let Some(mut app) = make(W, H) else { return };
        frame_settings(&mut app).downlevel.no_compute = no_compute;
        frame_settings(&mut app).read_exposure = true;
        let ae = AutoExposure::on();
        let scene = AeScene::spawn(&mut app, post_ae(ae));
        app.step_ticks(3);
        let settled = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE);
        scene.brightness(&mut app, 4.0);
        app.step_ticks(5);
        let partial = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE * 4.0);
        assert!(partial > settled - 1.0, "still adapting: {partial} vs {settled}");
        app.resources.expect_mut::<Renderer>().reset_post_history(scene.camera);
        app.step_ticks(1);
        let snapped = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE * 4.0);
        assert!(
            (snapped - (settled - 2.0)).abs() < 0.06,
            "reset snaps: {snapped} vs {settled} − 2"
        );
        // Off (manual 0 EV), then on again: snaps.
        scene.brightness(&mut app, 1.0);
        set_post(&mut app, scene.camera, post_ae(ae).without_bloom());
        app.world
            .get::<&mut PostProcess>(scene.camera)
            .unwrap()
            .auto_exposure
            .enabled = false;
        app.step_ticks(2);
        let manual = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE);
        assert!(manual.abs() < 0.03, "manual 0 EV: {manual}");
        set_post(&mut app, scene.camera, post_ae(ae));
        app.step_ticks(1);
        let on = displayed_ev(&render(&mut app), AE_PROBE_RECT, AE_PROBE);
        assert!((on - settled).abs() < 0.03, "re-enabled snaps: {on} vs {settled}");
        // Steady state.
        app.step_ticks(8);
        for _ in 0..4 {
            app.step_ticks(1);
            let s = stats(&app);
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
        }
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    }

    // ---- Color grading, LUTs, hooks (PO4) ----

    /// Radiance of the grading probe quad (every channel different, all below 1).
    const GRADE_COLOR: Vec3 = Vec3::new(0.4, 0.2, 0.08);

    fn srgb_encode(v: f32) -> f32 {
        if v <= 0.0031308 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    }

    /// A grading probe: a quad of `GRADE_COLOR` filling the frame's middle; returns the camera.
    fn grading_scene(app: &mut App, post: PostProcess) -> Entity {
        quad(app, GRADE_COLOR, Vec2::ZERO, Vec2::new(2.0, 1.0));
        camera(app, 0.0, BLACK, Some(post))
    }

    /// The probe's displayed linear color with `grading` (no tonemapper, no dither, no bloom).
    fn graded(app: &mut App, cam: Entity, grading: ColorGrading) -> Vec3 {
        set_post(
            app,
            cam,
            post(Bloom::glow()).without_bloom().with_color_grading(grading),
        );
        mean(&render(app), [56, 24, 16, 16])
    }

    fn assert_close(got: Vec3, want: Vec3, tolerance: f32, what: &str) {
        assert!((got - want).abs().max_element() <= tolerance, "{what}: {got} vs {want}");
    }

    /// Saturation, contrast, white balance and lift/gamma/gain do what their formulas say (the
    /// probe's display value against a CPU computation: within 1.5/255 in the encoded space).
    pub fn grading_matches_formulas(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let cam = grading_scene(&mut app, post(Bloom::glow()).without_bloom());
        let c = GRADE_COLOR;
        let plain = graded(&mut app, cam, ColorGrading::default());
        assert_close(plain, c, 0.004, "neutral");
        let luma = c.dot(Vec3::new(0.2126, 0.7152, 0.0722));
        let grey = graded(
            &mut app,
            cam,
            ColorGrading {
                saturation: 0.0,
                ..Default::default()
            },
        );
        assert_close(grey, Vec3::splat(luma), 0.004, "saturation 0");
        let vivid = graded(
            &mut app,
            cam,
            ColorGrading {
                saturation: 1.5,
                ..Default::default()
            },
        );
        assert_close(
            vivid,
            Vec3::splat(luma) + (c - Vec3::splat(luma)) * 1.5,
            0.005,
            "saturation 1.5",
        );
        let contrast = graded(
            &mut app,
            cam,
            ColorGrading {
                contrast: 1.5,
                ..Default::default()
            },
        );
        let k = |v: f32| 0.18 * (v / 0.18).powf(1.5);
        assert_close(contrast, Vec3::new(k(c.x), k(c.y), k(c.z)), 0.005, "contrast 1.5");
        let warm = graded(
            &mut app,
            cam,
            ColorGrading {
                temperature: 0.6,
                ..Default::default()
            },
        );
        assert!(warm.x / warm.z > 1.15 * c.x / c.z, "warmer: {warm} vs {c}");
        let cool = graded(
            &mut app,
            cam,
            ColorGrading {
                temperature: -0.6,
                ..Default::default()
            },
        );
        assert!(cool.z / cool.x > 1.15 * c.z / c.x, "cooler: {cool} vs {c}");
        let magenta = graded(
            &mut app,
            cam,
            ColorGrading {
                tint: 0.6,
                ..Default::default()
            },
        );
        assert!(magenta.y / magenta.x < 0.95 * c.y / c.x, "magenta: {magenta} vs {c}");
        // Lift/gamma/gain on the encoded color: (e · gain + lift)^(1 / gamma).
        let cdl = ColorGrading {
            lift: Vec3::new(0.05, 0.0, 0.1),
            gamma: Vec3::new(1.0, 1.6, 0.8),
            gain: Vec3::new(0.9, 1.1, 1.0),
            ..Default::default()
        };
        let got = graded(&mut app, cam, cdl);
        let want = Vec3::from_array(std::array::from_fn(|i| {
            let e = srgb_encode(c[i]) * cdl.gain[i] + cdl.lift[i];
            srgb_to_linear(e.max(0.0).powf(1.0 / cdl.gamma[i]).min(1.0))
        }));
        assert_close(got, want, 0.006, "lift/gamma/gain");
        // The Ldr fallback grades too (display space, after its per-fragment tonemapping).
        frame_settings(&mut app).downlevel.no_float_targets = true;
        let grey = graded(
            &mut app,
            cam,
            ColorGrading {
                saturation: 0.0,
                ..Default::default()
            },
        );
        assert!(
            (grey.x - grey.z).abs() < 0.004 && grey.x > 0.05,
            "Ldr fallback: grey {grey}"
        );
    }

    /// A few colored quads and a ramp: the scene the LUT tests compare.
    fn lut_scene(app: &mut App, post: PostProcess) -> Entity {
        let colors = [
            Vec3::new(0.8, 0.1, 0.05),
            Vec3::new(0.05, 0.6, 0.1),
            Vec3::new(0.1, 0.2, 0.9),
            Vec3::new(0.9, 0.85, 0.2),
            Vec3::new(0.3, 0.3, 0.3),
            Vec3::new(2.0, 1.2, 0.4),
        ];
        for (i, c) in colors.into_iter().enumerate() {
            let x = -1.6 + i as f32 * 0.64;
            quad(app, c, Vec2::new(x, 0.3), Vec2::new(0.5, 0.8));
            quad(
                app,
                Vec3::splat(i as f32 / 5.0),
                Vec2::new(x, -0.6),
                Vec2::new(0.6, 0.6),
            );
        }
        camera(app, 0.0, Vec4::new(0.01, 0.015, 0.03, 1.0), Some(post))
    }

    /// An identity LUT changes nothing (8-bit strips of 16 and 32 within 2/255, a `.cube`
    /// identity within 1/255, an sRGB-format strip within 2/255); after warm-up nothing is
    /// created or reallocated.
    pub fn lut_identity_is_neutral(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let base = PostProcess {
            dither: false,
            ..PostProcess::default()
        };
        let cam = lut_scene(&mut app, base);
        let plain = render(&mut app);
        let cube = {
            let mut s = String::from("LUT_3D_SIZE 17\n");
            for b in 0..17 {
                for g in 0..17 {
                    for r in 0..17 {
                        s += &format!("{} {} {}\n", r as f32 / 16.0, g as f32 / 16.0, b as f32 / 16.0);
                    }
                }
            }
            s
        };
        let srgb = {
            let mut t = ColorLut::identity(16);
            t.format = wgpu::TextureFormat::Rgba8UnormSrgb;
            // The strip's encoded values, stored as sRGB bytes: sampled (decoded) and
            // re-encoded by the composite.
            t
        };
        let strips = [
            (ColorLut::identity(16), 16, 2),
            (ColorLut::identity(32), 32, 2),
            (ColorLut::from_cube(&cube).unwrap(), 17, 1),
            (srgb, 16, 3),
        ];
        for (source, n, tolerance) in strips {
            let format = source.format;
            let lut = app.resources.expect_mut::<RenderAssets>().add_texture(source);
            set_post(&mut app, cam, base.with_lut(ColorLut::new(lut, n)));
            let graded = render(&mut app);
            let d = max_diff(&plain, &graded);
            assert!(d <= tolerance, "{format:?} {n}: max difference {d}");
        }
        for _ in 0..3 {
            render(&mut app);
        }
        let s = stats(&app);
        assert_eq!(
            (s.textures_created, s.cpu_reallocations, s.bytes_uploaded),
            (0, 0, 0),
            "{s:?}"
        );
    }

    /// A LUT that swaps red and blue turns the red quad blue; contribution 0.5 mixes; a strip of
    /// the wrong size is ignored.
    pub fn lut_changes_colors(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let base = post(Bloom::glow()).without_bloom();
        let cam = grading_scene(&mut app, base);
        let mut cube = String::from("TITLE \"swap\"\nLUT_3D_SIZE 9\n");
        for b in 0..9 {
            for g in 0..9 {
                for r in 0..9 {
                    cube += &format!("{} {} {}\n", b as f32 / 8.0, g as f32 / 8.0, r as f32 / 8.0);
                }
            }
        }
        let lut = app
            .resources
            .expect_mut::<RenderAssets>()
            .add_texture(ColorLut::from_cube(&cube).unwrap());
        let c = GRADE_COLOR;
        let swapped = graded(
            &mut app,
            cam,
            ColorGrading {
                lut: Some(ColorLut::new(lut, 9)),
                ..Default::default()
            },
        );
        assert_close(swapped, Vec3::new(c.z, c.y, c.x), 0.01, "swapped");
        let half = graded(
            &mut app,
            cam,
            ColorGrading {
                lut: Some(ColorLut {
                    contribution: 0.5,
                    ..ColorLut::new(lut, 9)
                }),
                ..Default::default()
            },
        );
        // Mixed in the encoded space.
        let mixed = |a: f32, b: f32| srgb_to_linear((srgb_encode(a) + srgb_encode(b)) * 0.5);
        assert_close(half, Vec3::new(mixed(c.x, c.z), c.y, mixed(c.z, c.x)), 0.01, "half");
        let wrong = graded(
            &mut app,
            cam,
            ColorGrading {
                lut: Some(ColorLut::new(lut, 8)),
                ..Default::default()
            },
        );
        assert_close(wrong, c, 0.004, "a strip of the wrong size is ignored");
    }

    /// Grading hooks: `post_hdr_hook` doubling the HDR color equals +1 EV, `post_ldr_hook`
    /// swapping channels swaps them; a hook that does not compile (or declares a resource) is
    /// replaced by identity and reported by `Renderer::shader_error`, and a hot-reload fixes it.
    pub fn invalid_grading_hook_falls_back(make: MakeApp) {
        let Some(mut app) = make(W, H) else { return };
        let base = post(Bloom::glow()).without_bloom();
        let cam = grading_scene(&mut app, base);
        let plain = render(&mut app);
        let hdr = app.resources.expect_mut::<RenderAssets>().add_shader(CustomShader::new(
            "double",
            "fn post_hdr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> { return c * 2.0; }",
        ));
        set_post(
            &mut app,
            cam,
            base.with_color_grading(ColorGrading {
                shader: Some(hdr),
                ..Default::default()
            }),
        );
        let doubled = render(&mut app);
        set_post(&mut app, cam, base.with_exposure(1.0));
        assert!(max_diff(&doubled, &render(&mut app)) <= 1, "hdr hook × 2 = +1 EV");
        let ldr = app.resources.expect_mut::<RenderAssets>().add_shader(CustomShader::new(
            "swap",
            "fn post_ldr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> { return c.bgr; }",
        ));
        let swap = ColorGrading {
            shader: Some(ldr),
            ..Default::default()
        };
        let swapped = graded(&mut app, cam, swap);
        let c = GRADE_COLOR;
        assert_close(swapped, Vec3::new(c.z, c.y, c.x), 0.004, "ldr hook");
        assert!(app.resources.expect::<Renderer>().shader_error(ldr).is_none());
        // A syntax error: identity, reported.
        app.resources.expect_mut::<RenderAssets>().update_shader(
            ldr,
            CustomShader::new(
                "broken",
                "fn post_ldr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> { return c.bgr + ; }",
            ),
        );
        set_post(&mut app, cam, base.with_color_grading(swap));
        assert_eq!(render(&mut app), plain, "broken hook: identity");
        let error = app.resources.expect::<Renderer>().shader_error(ldr).map(str::to_owned);
        assert!(error.is_some(), "the error is reported");
        // A resource the composite does not bind: rejected the same way.
        app.resources.expect_mut::<RenderAssets>().update_shader(
            ldr,
            CustomShader::new(
                "resource",
                "@group(2) @binding(0) var<uniform> extra: vec4<f32>;\n\
                 fn post_ldr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> { return c * extra.x; }",
            ),
        );
        assert_eq!(render(&mut app), plain, "resource hook: identity");
        let error = app
            .resources
            .expect::<Renderer>()
            .shader_error(ldr)
            .unwrap_or_default()
            .to_owned();
        assert!(error.contains("extra"), "{error}");
        // Fixed by a hot reload.
        app.resources.expect_mut::<RenderAssets>().update_shader(
            ldr,
            CustomShader::new(
                "swap again",
                "fn post_ldr_hook(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32> { return c.bgr; }",
            ),
        );
        let swapped = mean(&render(&mut app), [56, 24, 16, 16]);
        assert_close(swapped, Vec3::new(c.z, c.y, c.x), 0.004, "fixed");
        assert!(app.resources.expect::<Renderer>().shader_error(ldr).is_none());
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    }

    fn stats(app: &App) -> RenderStats {
        app.resources.expect::<RenderStats>().clone()
    }

    fn set_path(app: &mut App, path: DrawPath) {
        app.resources.expect_mut::<RenderSettings>().draw_path = Some(path);
    }
}
