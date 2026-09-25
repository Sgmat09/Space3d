//! The view uniform (`View` in forward.wgsl) and its lights.

use bytemuck::{Pod, Zeroable};

pub(crate) const MAX_DIR_LIGHTS: usize = 4;

pub(crate) const MAX_POINT_LIGHTS: usize = 16;

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(crate) struct GpuDirLight {
    pub direction: [f32; 4],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(crate) struct GpuPointLight {
    pub position_range: [f32; 4],
    pub color: [f32; 4],
}

/// Matches `View` in forward.wgsl (std140-compatible).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuView {
    pub view_proj: [[f32; 4]; 4],
    pub camera_pos: [f32; 4],
    pub ambient: [f32; 4],
    pub counts: [u32; 4],
    pub dir_lights: [GpuDirLight; MAX_DIR_LIGHTS],
    pub point_lights: [GpuPointLight; MAX_POINT_LIGHTS],
}
