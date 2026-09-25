//! Post-processing.
//!
//! [`api`] is the public hub (re-exported from the crate root). The rest is the crate-internal
//! contract between the frame and the post area (graphics spec §2.6): the frame plans post
//! groups, hands each one's scene color, depth and distortion buffer to
//! [`PostState::encode_group`] and owns the pass slot (targets, formats, capture replay); post
//! owns the composite and its effects. F2 stubs: no post group is planned yet (F4), so nothing
//! here runs in a frame; results are neutral. Distortion buffers are real (the frame's Distortion
//! phase plumbing, F5, clears and writes them).

pub(crate) mod api;
mod chain;
mod distortion_mesh;

use crate::caps::GraphicsCaps;
use crate::handles::FastMap;
use crate::node::{FrameInfo, FrameMode, RenderNodes};
use crate::output::OutputEncoding;
use crate::settings::FrameSettings;
use crate::stats::RenderStats;
use api::{DISTORTION_FORMAT, DistortionResolution, PostProcess};
use space3d_core::{Entity, World};

/// One post group of the frame plan (graphics spec §1.3): the settings camera and its rect.
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PostGroupPlan {
    /// The settings camera (highest-order post camera of the rect).
    pub camera: Entity,
    pub settings: PostProcess,
    pub mode: FrameMode,
    /// Rect in the scene texture and in the output target, `[x, y, w, h]` pixels.
    pub scene_rect: [u32; 4],
    pub output_rect: [u32; 4],
}

/// What a post group needs this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PostGroupNeeds {
    pub bloom: bool,
    pub auto_exposure: bool,
    pub fxaa: bool,
    pub distortion: bool,
    /// Scene depth (distortion occlusion).
    pub depth: bool,
}

/// Encoder and device access of [`PostState::encode_group`].
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PostEncode<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: &'a mut wgpu::CommandEncoder,
    pub stats: &'a mut RenderStats,
}

/// Inputs of one post group's passes.
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PostGroupInput<'a> {
    pub camera: Entity,
    pub settings: PostProcess,
    pub mode: FrameMode,
    pub scene_output: OutputEncoding,
    pub scene: &'a wgpu::TextureView,
    pub scene_format: wgpu::TextureFormat,
    pub scene_size: (u32, u32),
    pub scene_rect: [u32; 4],
    /// The scene texture has alpha (not `Rg11b10Ufloat`).
    pub scene_alpha: bool,
    /// Render-target group whose lead clear alpha is below 1: alpha is preserved.
    pub transparent: bool,
    pub depth: Option<&'a wgpu::TextureView>,
    pub distortion: Option<&'a wgpu::TextureView>,
    pub output: &'a wgpu::TextureView,
    pub output_format: wgpu::TextureFormat,
    pub output_size: (u32, u32),
    pub output_rect: [u32; 4],
    pub output_manual_srgb: bool,
    /// First write into the output this frame: clear it (to black) outside the rect.
    pub output_clear: bool,
    /// The implicit group of an Ldr passthrough target (every effect off).
    pub passthrough: bool,
    /// Windowed-capture replay: no metering, no adaptation.
    pub replay: bool,
    pub frame: FrameInfo,
}

/// A post group's persistent distortion buffer.
struct DistortionTarget {
    size: (u32, u32),
    view: wgpu::TextureView,
    last_used: u64,
}

/// Post state: pool users, per-camera history (exposure, distortion buffers), pipelines,
/// readback.
#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
pub(crate) struct PostState {
    frame: u64,
    distortion: FastMap<Entity, DistortionTarget>,
}

#[allow(
    dead_code,
    reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
)]
impl PostState {
    pub fn new(device: &wgpu::Device, caps: &GraphicsCaps) -> Self {
        let _ = (device, caps); // STUB(P-POST): pipelines, dummies, exposure history
        Self {
            frame: 0,
            distortion: FastMap::default(),
        }
    }

    pub fn begin_frame(&mut self, frame: &FrameInfo, settings: &FrameSettings) {
        let _ = settings; // STUB(P-POST)
        self.frame = frame.frame;
    }

    /// Step 6: what a post group needs (bloom, auto exposure, FXAA, distortion, depth).
    pub fn plan_group(&mut self, world: &World, group: &PostGroupPlan) -> PostGroupNeeds {
        let _ = world; // STUB(P-POST)
        PostGroupNeeds {
            distortion: group.settings.enabled && group.settings.distortion.enabled,
            ..Default::default()
        }
    }

    /// The persistent distortion buffer of a post group (created on demand; freed 60 frames
    /// after its last use), sized from `rect` and `res`. The caller's pass clears it.
    pub fn distortion_target(
        &mut self,
        device: &wgpu::Device,
        camera: Entity,
        rect: [u32; 4],
        res: DistortionResolution,
    ) -> (&wgpu::TextureView, (u32, u32)) {
        let size = match res {
            DistortionResolution::Half => (rect[2].div_ceil(2).max(1), rect[3].div_ceil(2).max(1)),
            _ => (rect[2].max(1), rect[3].max(1)),
        };
        let frame = self.frame;
        let t = self
            .distortion
            .entry(camera)
            .and_modify(|t| {
                if t.size != size {
                    *t = create_distortion_target(device, size, frame);
                }
            })
            .or_insert_with(|| create_distortion_target(device, size, frame));
        t.last_used = frame;
        (&t.view, t.size)
    }

    /// Record meter → bloom → composite (→ FXAA) for one post group into the frame encoder.
    pub fn encode_group(&mut self, cx: &mut PostEncode<'_>, input: &PostGroupInput<'_>) {
        let _ = (cx, input); // STUB(F4 composite, P-POST effects)
    }

    pub fn end_frame(&mut self, stats: &mut RenderStats) {
        let _ = stats;
        let frame = self.frame;
        self.distortion.retain(|_, t| t.last_used + 60 > frame);
    }

    /// Register the built-in post nodes (`"space3d/distortion-meshes"`).
    pub fn register_nodes(nodes: &mut RenderNodes) {
        distortion_mesh::register(nodes);
    }

    /// Last read-back exposure (stops) of a post camera.
    pub fn camera_exposure(&self, camera: Entity) -> Option<f32> {
        let _ = camera; // STUB(P-POST)
        None
    }

    /// Forget a camera's temporal history (auto exposure snaps next frame).
    pub fn reset_history(&mut self, camera: Entity) {
        self.distortion.remove(&camera);
    }
}

fn create_distortion_target(device: &wgpu::Device, (w, h): (u32, u32), frame: u64) -> DistortionTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d distortion buffer"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DISTORTION_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    DistortionTarget {
        size: (w, h),
        view: texture.create_view(&Default::default()),
        last_used: frame,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distortion_targets_persist_per_camera() {
        let Ok(r) = pollster::block_on(crate::Renderer::new_headless(4, 4)) else {
            eprintln!("skipping GPU test: no adapter available");
            return;
        };
        let mut post = PostState::new(r.device(), r.graphics_caps());
        let mut world = World::new();
        let (a, b) = (world.spawn(()), world.spawn(()));
        let (_, size) = post.distortion_target(r.device(), a, [0, 0, 101, 60], DistortionResolution::Half);
        assert_eq!(size, (51, 30));
        let (_, size) = post.distortion_target(r.device(), b, [0, 0, 101, 60], DistortionResolution::Full);
        assert_eq!(size, (101, 60));
        assert_eq!(post.distortion.len(), 2);
        let plan = PostGroupPlan {
            camera: a,
            settings: PostProcess::default(),
            mode: FrameMode::Hdr,
            scene_rect: [0; 4],
            output_rect: [0; 4],
        };
        assert!(post.plan_group(&world, &plan).distortion);
        post.reset_history(a);
        let mut stats = RenderStats::default();
        post.end_frame(&mut stats);
        assert_eq!(post.distortion.len(), 1);
        assert_eq!(post.camera_exposure(b), None);
        let mut nodes = RenderNodes::default();
        PostState::register_nodes(&mut nodes);
        assert!(nodes.contains("space3d/distortion-meshes"));
        assert_eq!(r.gpu_error_count(), 0);
    }
}
