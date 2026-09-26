//! GPU pass timings: [`GpuTimings`] (resource and [`crate::Renderer::gpu_timings`]), filled 1–3
//! frames late from one timestamp query set shared by the frame, post, materials and nodes
//! (graphics spec §2.2.6, §4.4 labels). Recording lands with milestone F8; until then
//! `supported` reflects the device and `scopes` stays empty.

/// GPU timestamps per pass, 1–3 frames late.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct GpuTimings {
    /// The device offers `TIMESTAMP_QUERY` (and `FrameSettings::gpu_timing` can record).
    pub supported: bool,
    /// Frame (`RenderStats::frame`) the scopes were measured in.
    pub frame: Option<u64>,
    /// Sum of all scopes, in milliseconds.
    pub total_ms: f32,
    /// One entry per timed pass, in recording order.
    pub scopes: Vec<GpuScope>,
}

impl GpuTimings {
    /// Milliseconds of every scope labelled `label` (0 without one).
    pub fn scope_ms(&self, label: &str) -> f32 {
        self.scopes.iter().filter(|s| s.label == label).map(|s| s.ms).sum()
    }
}

/// One timed pass.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct GpuScope {
    /// Pass label (`cull`, `scene`, `post.bloom`, `particles.sim`, …).
    pub label: &'static str,
    /// Camera the pass belongs to, if any.
    pub camera: Option<space3d_core::Entity>,
    /// GPU time in milliseconds.
    pub ms: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_ms_sums_by_label() {
        let t = GpuTimings {
            supported: true,
            frame: Some(3),
            total_ms: 0.6,
            scopes: vec![
                GpuScope {
                    label: "scene",
                    camera: None,
                    ms: 0.25,
                },
                GpuScope {
                    label: "post.bloom",
                    camera: None,
                    ms: 0.1,
                },
                GpuScope {
                    label: "scene",
                    camera: None,
                    ms: 0.25,
                },
            ],
        };
        assert_eq!(t.scope_ms("scene"), 0.5);
        assert_eq!(t.scope_ms("cull"), 0.0);
    }
}
