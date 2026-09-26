//! Lighting: cubemaps, skybox, image-based lighting and directional shadows.
//!
//! [`api`] is the public hub (re-exported from the crate root). The rest is the crate-internal
//! contract between the frame and the materials area (graphics spec §2.6): the frame calls
//! [`Lighting`] at fixed points of `render_frame` (steps 4, 5 and 8 of §1.4) and draws the depth
//! views it requests; the materials area owns the bodies. These are F2 stubs with neutral
//! results (no environment, no shadows, 1×1 dummies), tagged `STUB(P-MAT)`.

pub(crate) mod api;
pub(crate) mod cubemap;
mod environment;
mod shadow;
mod skybox;

use crate::assets::RenderAssets;
use crate::caps::GraphicsCaps;
use crate::culling::Frustum;
use crate::node::{RenderNodes, ViewInfo};
use crate::pipeline::GpuView;
use crate::stats::RenderStats;
use api::LightingSettings;
use glam::Mat4;
use space3d_core::{Entity, World};

/// Inputs of [`Lighting::prepare`] (step 4: cubemap uploads and conversion, IBL bake passes).
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct LightingPrepare<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: &'a mut wgpu::CommandEncoder,
    pub assets: &'a mut RenderAssets,
    pub settings: &'a LightingSettings,
    /// Headless renderers finish every pending bake in the frame it is requested.
    pub headless: bool,
    pub stats: &'a mut RenderStats,
    /// GPU timings: `timing.render("ibl.bake", None)` as a bake pass's `timestamp_writes`
    /// (`None` unless `FrameSettings::gpu_timing`).
    pub timing: &'a crate::timing::GpuTimer,
}

/// One depth-only view the frame renders before the camera views (a shadow map).
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct DepthViewRequest {
    pub light: Entity,
    pub view_proj: Mat4,
    pub frustum: Frustum,
    /// `Depth32Float`, `size`².
    pub target: wgpu::TextureView,
    pub size: u32,
    pub bias: wgpu::DepthBiasState,
}

/// Bindings 1–6 of group 0 for a camera, and a stable key for bind-group caching. The frame
/// keeps one bind group per key (and views-buffer generation): equal keys must mean identical
/// resources, and a new resource (a re-created shadow map, a baked environment) a new key. The
/// resources for `None` (depth views) must not include a shadow map a depth view renders into.
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct ViewResources<'a> {
    pub env_specular: &'a wgpu::TextureView,
    pub env_irradiance: &'a wgpu::TextureView,
    pub brdf_lut: &'a wgpu::TextureView,
    pub env_sampler: &'a wgpu::Sampler,
    pub shadow_map: &'a wgpu::TextureView,
    pub shadow_sampler: &'a wgpu::Sampler,
    pub key: u64,
}

/// Lighting state: cubemaps, the environment cache, the bake queue and the shadow state.
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct Lighting {
    dummy_cube: wgpu::TextureView,
    dummy_2d: wgpu::TextureView,
    dummy_depth: wgpu::TextureView,
    sampler: wgpu::Sampler,
    comparison: wgpu::Sampler,
    depth_views: Vec<DepthViewRequest>,
}

#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
impl Lighting {
    /// Neutral resources (1×1 black environment, far shadow map).
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, caps: &GraphicsCaps) -> Self {
        let _ = (queue, caps); // STUB(P-MAT): BRDF LUT upload, format choice from caps
        let texture = |label, layers, format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: layers,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let sampled = wgpu::TextureUsages::TEXTURE_BINDING;
        let cube = texture("space3d dummy cube", 6, wgpu::TextureFormat::Rgba8Unorm, sampled);
        let flat = texture("space3d dummy 2d", 1, wgpu::TextureFormat::Rgba8Unorm, sampled);
        let depth = texture(
            "space3d dummy shadow map",
            1,
            crate::pipeline::DEPTH_FORMAT,
            sampled | wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        Self {
            dummy_cube: cube.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::Cube),
                ..Default::default()
            }),
            dummy_2d: flat.create_view(&Default::default()),
            dummy_depth: depth.create_view(&Default::default()),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("space3d environment"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            comparison: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("space3d shadow comparison"),
                compare: Some(wgpu::CompareFunction::LessEqual),
                ..Default::default()
            }),
            depth_views: Vec::new(),
        }
    }

    /// Step 4: cubemap uploads and conversion, IBL bake passes (budgeted; all at once when
    /// headless).
    pub fn prepare(&mut self, cx: &mut LightingPrepare<'_>) {
        let _ = cx; // STUB(P-MAT)
    }

    /// Step 5: this frame's shadow depth views (after camera planning).
    pub fn depth_views(
        &mut self,
        world: &World,
        cameras: &[ViewInfo],
        settings: &LightingSettings,
    ) -> &[DepthViewRequest] {
        let _ = (world, cameras, settings); // STUB(P-MAT)
        self.depth_views.clear();
        &self.depth_views
    }

    /// Step 8: fill the environment, sky and shadow fields of a camera's `GpuView` (zero = off).
    pub fn fill_view(&self, world: &World, camera: Entity, view: &mut GpuView) {
        let _ = (world, camera, view); // STUB(P-MAT)
    }

    /// Bindings 1–6 of group 0 for `camera` (`None`: depth views and cameras without lighting).
    pub fn view_resources(&self, camera: Option<Entity>) -> ViewResources<'_> {
        let _ = camera; // STUB(P-MAT)
        ViewResources {
            env_specular: &self.dummy_cube,
            env_irradiance: &self.dummy_cube,
            brdf_lut: &self.dummy_2d,
            env_sampler: &self.sampler,
            shadow_map: &self.dummy_depth,
            shadow_sampler: &self.comparison,
            key: 0,
        }
    }

    /// Register the built-in lighting nodes (`"space3d/skybox"`).
    pub fn register_nodes(nodes: &mut RenderNodes) {
        skybox::register(nodes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubs_are_neutral() {
        let Ok(r) = pollster::block_on(crate::Renderer::new_headless(4, 4)) else {
            eprintln!("skipping GPU test: no adapter available");
            return;
        };
        let mut lighting = Lighting::new(r.device(), r.queue(), r.graphics_caps());
        let world = World::new();
        assert!(
            lighting
                .depth_views(&world, &[], &LightingSettings::default())
                .is_empty()
        );
        assert_eq!(lighting.view_resources(None).key, 0);
        let mut nodes = RenderNodes::default();
        Lighting::register_nodes(&mut nodes);
        assert!(nodes.contains("space3d/skybox"));
        assert_eq!(r.gpu_error_count(), 0);
    }
}
