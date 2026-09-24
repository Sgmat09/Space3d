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
//! macOS crashes when several GL contexts render from different threads at once.)

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

fn render(mut app: App, scene: scenes::Scene) -> (Vec<[u8; 3]>, ImageData) {
    let colors = scene(&mut app).to_vec();
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    (colors, img)
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
