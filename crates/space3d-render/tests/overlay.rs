//! Render overlays: pixels drawn by a `RenderOverlay` appear in headless captures, respect
//! priority order, and UI-only frames (no camera) start from a cleared target.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;
use std::sync::{Arc, Mutex};

/// Fills a pixel rectangle with a solid linear color via a fullscreen triangle + scissor.
struct SolidRect {
    color: [f32; 4],
    rect: (u32, u32, u32, u32),
    pipeline: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    prepared: Arc<Mutex<Vec<OverlayTargetKind>>>,
}

impl SolidRect {
    fn new(color: [f32; 4], rect: (u32, u32, u32, u32)) -> Self {
        Self {
            color,
            rect,
            pipeline: None,
            prepared: Default::default(),
        }
    }
}

impl RenderOverlay for SolidRect {
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        _: &mut wgpu::CommandEncoder,
        target: &OverlayTarget,
    ) {
        self.prepared.lock().unwrap().push(target.kind);
        if self.pipeline.as_ref().is_some_and(|(f, _)| *f == target.format) {
            return;
        }
        let [r, g, b, a] = self.color;
        let src = format!(
            "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {{
                 let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
                 return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
             }}
             @fragment fn fs() -> @location(0) vec4<f32> {{ return vec4<f32>({r:?}, {g:?}, {b:?}, {a:?}); }}"
        );
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("solid overlay"),
            source: wgpu::ShaderSource::Wgsl(src.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("solid overlay"),
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

    fn render(&self, pass: &mut wgpu::RenderPass<'static>, _: &OverlayTarget) {
        let Some((_, pipeline)) = &self.pipeline else { return };
        let (x, y, w, h) = self.rect;
        pass.set_scissor_rect(x, y, w, h);
        pass.set_pipeline(pipeline);
        pass.draw(0..3, 0..1);
    }
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];

#[test]
fn overlay_pixels_appear_in_capture() {
    let Some(mut app) = headless_app(64, 64) else { return };
    spawn_camera(&mut app, 3.0);
    let overlay = SolidRect::new([1.0, 0.0, 0.0, 1.0], (0, 0, 32, 32));
    let prepared = overlay.prepared.clone();
    app.resources.expect_mut::<RenderOverlays>().insert("solid", 0, overlay);
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(8, 8), RED, "overlay drawn over the scene");
    assert_eq!(img.get(31, 31), RED);
    assert_eq!(img.get(32, 32), CLEAR, "outside the overlay the 3D clear color remains");
    assert_eq!(img.get(48, 8), CLEAR);
    assert_eq!(prepared.lock().unwrap().as_slice(), &[OverlayTargetKind::Offscreen]);
}

#[test]
fn overlays_draw_in_priority_order_and_can_be_removed() {
    let Some(mut app) = headless_app(32, 32) else { return };
    spawn_camera(&mut app, 3.0);
    {
        let overlays = app.resources.expect_mut::<RenderOverlays>();
        // Inserted first but higher priority: drawn last (on top).
        overlays.insert("top", 10, SolidRect::new([0.0, 1.0, 0.0, 1.0], (0, 0, 32, 32)));
        overlays.insert("bottom", 0, SolidRect::new([1.0, 0.0, 0.0, 1.0], (0, 0, 32, 32)));
    }
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(16, 16), GREEN);
    app.resources.expect_mut::<RenderOverlays>().remove("top");
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(16, 16), RED);
    app.resources.expect_mut::<RenderOverlays>().remove("bottom");
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(16, 16), CLEAR, "no overlays: plain scene");
}

#[test]
fn ui_only_frames_start_from_black() {
    let Some(mut app) = headless_app(32, 32) else { return };
    // First frame with a camera leaves blue in the target...
    let cam = spawn_camera(&mut app, 3.0);
    render_to_image(&mut app).expect("capture");
    // ...then the camera goes away: overlays must not draw over stale pixels.
    app.world.despawn(cam).unwrap();
    app.resources
        .expect_mut::<RenderOverlays>()
        .insert("solid", 0, SolidRect::new([1.0, 0.0, 0.0, 1.0], (0, 0, 8, 8)));
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(2, 2), RED);
    assert_eq!(img.get(20, 20), [0, 0, 0, 255]);
}

#[test]
fn render_target_view_is_available_after_upload() {
    let Some(mut app) = headless_app(16, 16) else { return };
    let target = app.resources.expect_mut::<RenderAssets>().add_render_target(8, 8);
    assert!(
        app.resources
            .expect::<RenderAssets>()
            .render_target_view(target)
            .is_none()
    );
    app.step_ticks(1);
    assert!(
        app.resources
            .expect::<RenderAssets>()
            .render_target_view(target)
            .is_some()
    );
    let image = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_image(ImageData::new(2, 2));
    app.step_ticks(1);
    assert!(
        app.resources
            .expect::<RenderAssets>()
            .render_target_view(image)
            .is_none(),
        "plain textures are not render targets"
    );
}
