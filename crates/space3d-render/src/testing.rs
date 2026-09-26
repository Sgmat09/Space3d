//! Hooks for this crate's own integration tests (`tests/depth_views.rs`, `tests/alloc_frame.rs`):
//! depth views injected without the lighting (graphics spec §5.1 F7, "a request injector").
//! Not public API: hidden from the docs, no stability guarantee.

use crate::Renderer;
use crate::culling::Frustum;
use crate::lighting::DepthViewRequest;
use glam::Mat4;

/// Inject a depth-only view with light matrix `view_proj` (and its culling frustum) into every
/// following frame, after the lighting's own depth views. Returns its index for the other hooks
/// and its target: a `size`² `Depth32Float` texture (sampleable), cleared to 1.0 and written by
/// each frame's depth pass.
pub fn add_depth_view(
    renderer: &mut Renderer,
    view_proj: Mat4,
    size: u32,
    bias: wgpu::DepthBiasState,
) -> (usize, wgpu::TextureView) {
    let texture = renderer.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d test depth view"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: crate::pipeline::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let target = texture.create_view(&Default::default());
    renderer.injected_depth_views.push(DepthViewRequest {
        light: space3d_core::Entity::DANGLING,
        view_proj,
        frustum: Frustum::from_view_proj(&view_proj),
        target: target.clone(),
        size,
        bias,
    });
    (renderer.injected_depth_views.len() - 1, target)
}

/// Remove every injected depth view.
pub fn clear_depth_views(renderer: &mut Renderer) {
    renderer.injected_depth_views.clear();
}

/// Draw every caster with the alpha-masked depth pipeline (`true`), or as the materials decide.
pub fn force_depth_mask(renderer: &mut Renderer, on: bool) {
    renderer.frame.force_caster_pass = on.then_some(crate::pipeline::PassKind::DepthMask);
}

/// Instances the last frame drew into injected depth view `index`: CPU-culled draws count
/// their instances; GPU-culled ones are read back from the indirect args (blocking).
#[cfg(not(target_arch = "wasm32"))]
pub fn depth_view_instances(renderer: &Renderer, index: usize) -> u64 {
    renderer.frame.depth_view_instances(
        &renderer.device,
        &renderer.queue,
        renderer.frame_depth_view_index(index),
    )
}

/// Draw commands the last frame recorded for injected depth view `index`.
pub fn depth_view_draws(renderer: &Renderer, index: usize) -> usize {
    renderer.frame.depth_view_draws(renderer.frame_depth_view_index(index))
}

impl Renderer {
    /// Index among the frame's depth views of injected depth view `index` (they follow the
    /// lighting's).
    fn frame_depth_view_index(&self, index: usize) -> usize {
        let frame = self.frame.depth_view_count();
        assert!(
            index < self.injected_depth_views.len(),
            "no injected depth view {index}"
        );
        frame - self.injected_depth_views.len() + index
    }
}
