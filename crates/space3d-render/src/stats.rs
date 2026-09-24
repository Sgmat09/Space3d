//! Per-frame profiling counters.

/// How instanced draws are culled and issued. Chosen once at startup from adapter
/// capabilities (see [`crate::RenderCapabilities`]) and optionally downgraded via
/// [`crate::RenderSettings::draw_path`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DrawPath {
    /// CPU frustum culling + `draw_indexed` per batch. Works everywhere (WebGL2, downlevel).
    #[default]
    CpuDirect,
    /// Compute-shader culling writing `DrawIndexedIndirectArgs`; one `draw_indexed_indirect`
    /// per batch (no `INDIRECT_FIRST_INSTANCE` needed: the vertex buffer is rebound per draw).
    GpuIndirect,
    /// Compute culling + `multi_draw_indexed_indirect` over runs of batches that share
    /// pipeline, material and instance buffer (needs `INDIRECT_FIRST_INSTANCE`).
    GpuMultiDraw,
    /// Compute culling + draw compaction + `multi_draw_indexed_indirect_count`
    /// (needs `MULTI_DRAW_INDIRECT_COUNT`; Vulkan/DX12). Empty batches cost nothing.
    GpuMultiDrawCount,
}

impl DrawPath {
    /// Whether this path culls on the GPU.
    pub fn gpu_culling(self) -> bool {
        self != DrawPath::CpuDirect
    }
}

/// Renderer counters for the most recent frame. Inserted as a resource by
/// [`crate::RenderPlugin`] and overwritten every frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderStats {
    /// Frame counter of the renderer.
    pub frame: u64,
    /// Cameras rendered.
    pub views: u32,
    /// Draw commands recorded (a multi-draw counts as one).
    pub draw_calls: u32,
    /// Of `draw_calls`, how many were indirect (single or multi).
    pub indirect_draw_calls: u32,
    /// Of `draw_calls`, how many were `multi_draw_indexed_indirect[_count]`.
    pub multi_draw_calls: u32,
    /// Distinct (pipeline, material, mesh, instance source) batches across all views.
    pub batches: u32,
    /// Instances submitted for culling (sum over views).
    pub instances: u64,
    /// Instances issued to the GPU. On GPU paths this is the upper bound (pre-cull count)
    /// because the visible count only exists on the GPU.
    pub instances_drawn: u64,
    /// Instances rejected by CPU frustum culling (whole buffers count all their instances).
    pub culled: u64,
    /// Triangles issued (`instances_drawn * mesh triangles`).
    pub triangles: u64,
    /// All bytes copied CPU→GPU this frame (instances, uniforms, meshes, textures, jobs).
    pub bytes_uploaded: u64,
    /// Subset of `bytes_uploaded` that is per-instance data.
    pub instance_bytes_uploaded: u64,
    /// GPU buffers (re)allocated this frame (growth or first use).
    pub buffer_reallocations: u32,
    /// CPU-side scratch `Vec`s that had to grow this frame (0 in the steady state).
    pub cpu_reallocations: u32,
    /// Compute dispatches recorded (GPU culling).
    pub compute_dispatches: u32,
    /// Draw path used this frame.
    pub draw_path: DrawPath,
    /// GPU culling active this frame.
    pub gpu_culling: bool,
    /// Meshes uploaded to the mesh arena this frame.
    pub meshes_uploaded: u32,
    /// Textures uploaded this frame.
    pub textures_uploaded: u32,
}

impl RenderStats {
    pub(crate) fn reset(&mut self, frame: u64, path: DrawPath) {
        *self = RenderStats {
            frame,
            draw_path: path,
            gpu_culling: path.gpu_culling(),
            ..Default::default()
        };
    }
}
