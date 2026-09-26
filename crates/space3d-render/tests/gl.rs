//! The GL backend — wgpu's WebGL2 / GLES path — renders like the default backend.
//!
//! wgpu's GL backend differs in ways wgpu does not validate: no `BASE_VERTEX` below GLES 3.2
//! (WebGL2), texture targets fixed by the layer count (a single-layer texture sampled as an
//! array reads black; so does one layer of an array sampled through a `D2` view, and a square
//! array of 6 layers, which becomes a cube map) and no BGRA vertex formats. Each scene in
//! `common::scenes` hits one of those.
//!
//! Skips when there is no GL adapter. On Linux, Mesa's EGL (`libegl1`, `libegl-mesa0`:
//! llvmpipe, GLES 3.2 — so `BASE_VERTEX` is present there) provides one. On macOS, build wgpu's
//! ANGLE support and put ANGLE's `libEGL.dylib`/`libGLESv2.dylib` (shipped with Chrome and
//! every Electron app) on the dyld path — ANGLE there is GLES 3.0, WebGL2's feature level:
//!
//! ```text
//! DYLD_FALLBACK_LIBRARY_PATH=<dir with libEGL.dylib> \
//!     cargo test -p space3d-render --features wgpu/angle --test gl
//! ```
//!
//! (`WGPU_BACKEND=gl ... -- --test-threads=1` runs every other test on GL the same way; ANGLE on
//! macOS crashes when several GL contexts render from different threads at once. ANGLE samples
//! ETC2 textures as black, so `tests/textures.rs` skips ETC2 there.)

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;

fn gl_app(width: u32, height: u32) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    let renderer = pollster::block_on(Renderer::new_headless_with(RendererConfig {
        width,
        height,
        backends: Some(wgpu::Backends::GL),
        ..Default::default()
    }));
    match renderer {
        Ok(r) => {
            assert_eq!(r.capabilities().adapter.backend, wgpu::Backend::Gl);
            eprintln!(
                "GL adapter: {} ({}), draw path {:?}, base_vertex {}",
                r.capabilities().adapter.name,
                r.capabilities().adapter.driver_info,
                r.capabilities().best_draw_path,
                r.capabilities().base_vertex,
            );
            app.insert_resource(r);
            Some(app)
        }
        Err(e) => {
            eprintln!("skipping GL test: no GL adapter ({e})");
            None
        }
    }
}

/// How a portability scene is rendered: as written (Direct), with every camera a post camera
/// (passthrough post: the HDR scene texture and the composite), or post with float targets
/// forced off (the Ldr fallback).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Direct,
    Hdr,
    Ldr,
}

fn render_in(mut app: App, scene: scenes::Scene, mode: Mode) -> (Vec<[u8; 3]>, ImageData) {
    let colors = scene(&mut app).to_vec();
    if mode != Mode::Direct {
        let cameras: Vec<Entity> = app
            .world
            .query::<(Entity, &Camera3d)>()
            .iter()
            .map(|(e, _)| e)
            .collect();
        for e in cameras {
            app.world.insert_one(e, PostProcess::passthrough()).unwrap();
        }
        app.resources.expect_mut::<FrameSettings>().downlevel.no_float_targets = mode == Mode::Ldr;
    }
    let img = render_to_image(&mut app).expect("capture");
    let r = app.resources.expect::<Renderer>();
    assert_eq!(r.gpu_error_count(), 0);
    let intermediate = app.resources.expect::<RenderStats>().intermediate_groups;
    assert_eq!(intermediate > 0, mode != Mode::Direct, "{mode:?}");
    (colors, img)
}

fn render(app: App, scene: scenes::Scene) -> (Vec<[u8; 3]>, ImageData) {
    render_in(app, scene, Mode::Direct)
}

#[test]
fn gl_backend_renders_like_the_default_backend() {
    if gl_app(8, 8).is_none() {
        return;
    }
    for (name, scene) in scenes::ALL {
        let (colors, gl) = render(gl_app(96, 64).expect("GL adapter"), scene);
        for c in &colors {
            let n = count_rgb(&gl, *c);
            assert!(n >= 16, "{name}: color {c:?} covers {n} px on GL");
        }
        if let Some(app) = headless_app(96, 64) {
            let (_, reference) = render(app, scene);
            let d = diff_fraction(&gl, &reference, 8);
            assert!(
                d < 0.01,
                "{name}: {:.2}% of pixels differ from the default backend",
                d * 100.0
            );
        }
    }
}

/// The same scenes through the HDR scene texture and the composite (every camera with
/// passthrough post), and through the Ldr fallback (float targets forced off): GL renders them
/// like the default backend, and like Direct mode.
#[test]
fn gl_backend_renders_frame_modes_like_the_default_backend() {
    if gl_app(8, 8).is_none() {
        return;
    }
    for mode in [Mode::Hdr, Mode::Ldr] {
        for (name, scene) in scenes::ALL {
            let (colors, gl) = render_in(gl_app(96, 64).expect("GL adapter"), scene, mode);
            for c in &colors {
                let n = count_rgb(&gl, *c);
                assert!(n >= 16, "{name} {mode:?}: color {c:?} covers {n} px on GL");
            }
            let (_, direct) = render(gl_app(96, 64).expect("GL adapter"), scene);
            let d = diff_fraction(&gl, &direct, 1);
            assert!(
                d < 0.01,
                "{name} {mode:?}: {:.2}% of pixels differ from Direct on GL",
                d * 100.0
            );
            if let Some(app) = headless_app(96, 64) {
                let (_, reference) = render_in(app, scene, mode);
                let d = diff_fraction(&gl, &reference, 8);
                assert!(
                    d < 0.01,
                    "{name} {mode:?}: {:.2}% of pixels differ from the default backend",
                    d * 100.0
                );
            }
        }
    }
}

/// Scene depth on GL (graphics spec §1.6, GL rows): no read-only depth attachments, so depth
/// readers draw in a single-sample SceneRead pass with the depth texture sampled and tested
/// manually — Direct and post-processed alike; with MSAA they draw inside the scene pass (GLES
/// 3.0 cannot sample multisampled depth). The Distortion phase samples depth at 1 sample.
#[test]
fn gl_backend_reads_scene_depth() {
    use common::nodes::*;
    if gl_app(8, 8).is_none() {
        return;
    }
    let green = |img: &ImageData| count_rgb(img, [0, 255, 0]) as f64 / (img.width * img.height) as f64;
    for (samples, post) in [(1, false), (1, true), (4, false)] {
        let mut app = gl_app(64, 32).expect("GL adapter");
        app.resources.expect_mut::<Renderer>().set_msaa_samples(samples);
        let cam = analytic_plane_scene(&mut app);
        if post {
            app.world.insert_one(cam, PostProcess::passthrough()).unwrap();
        }
        let (probe, seen) = DepthProbe::new();
        let nodes = app.resources.expect_mut::<RenderNodes>();
        nodes.insert("probe", probe);
        let writer = nodes.insert("writer", DistortionWriter::new(true));
        let img = render_to_image(&mut app).expect("capture");
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
        let s = app.resources.expect::<RenderStats>().clone();
        let (target, depth) = seen.lock().unwrap()[0];
        let msaa = app.resources.expect::<Renderer>().msaa_samples() > 1;
        if msaa && !post {
            assert_eq!((target.depth_access, depth), (DepthAccess::None, false));
            assert_eq!(count_rgb(&img, [0, 0, 255]), (img.width * img.height) as usize);
            assert_eq!(s.scene_read_passes, 0);
        } else {
            assert_eq!(
                (target.depth_access, target.depth, depth),
                (DepthAccess::SampledOnly, None, true)
            );
            assert!(green(&img) > 0.99, "×{samples} post {post}: {:.3} green", green(&img));
            assert_eq!(s.scene_read_passes, 1);
        }
        // Passthrough post has distortion off; nothing without post either.
        assert_eq!(s.distortion_passes, 0);
        assert!(writer.lock().unwrap().draws.lock().unwrap().is_empty());
    }
    // Distortion with post: the writer samples depth (1 sample) on GL.
    let mut app = gl_app(64, 32).expect("GL adapter");
    let cam = analytic_plane_scene(&mut app);
    app.world.insert_one(cam, PostProcess::default()).unwrap();
    let writer = app
        .resources
        .expect_mut::<RenderNodes>()
        .insert("writer", DistortionWriter::new(true));
    render_to_image(&mut app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    assert_eq!(app.resources.expect::<RenderStats>().distortion_passes, 1);
    let (target, size) = writer.lock().unwrap().draws.lock().unwrap()[0];
    assert_eq!((target.depth_access, size), (DepthAccess::SampledOnly, (32, 16)));
}
