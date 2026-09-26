//! Frame options: the global [`FrameSettings`] resource, the per-camera [`CameraFrame`]
//! component, [`HdrMode`], downlevel overrides and render-target descriptions (graphics spec
//! §2.2.1).
//!
//! Config structs are plain `pub`-field structs with `Default` (functional record update:
//! `FrameSettings { post: false, ..Default::default() }`); enums are `#[non_exhaustive]`.

/// Global frame options (resource, inserted by [`crate::RenderPlugin`]). Changes apply to the
/// next frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSettings {
    /// Scene resolution scale of Primary-target cameras without
    /// [`CameraFrame::render_scale`] (clamped to 0.25..=1). Below 1 the primary target renders
    /// through an intermediate scene texture (Ldr passthrough without post). Default 1.
    pub render_scale: f32,
    /// Global post-processing switch (graphics menu, A/B comparisons). `false`: every camera
    /// renders as if it had no `PostProcess`. Default true.
    pub post: bool,
    /// Record GPU pass timestamps into [`crate::GpuTimings`] (the resource and
    /// `Renderer::gpu_timings`; needs `TIMESTAMP_QUERY`, see `GraphicsCaps::timestamps`): the
    /// start and end of every pass (labels in `docs/graphics/frame.md`, "GPU timings"). A frame's
    /// timings arrive 2–5 frames later: its timestamps are resolved once its GPU work completed.
    /// Scopes of consecutive passes may overlap on tile-based GPUs (Apple, mobile), so
    /// `total_ms` is a sum, not the frame's GPU time. Default false (no cost).
    pub gpu_timing: bool,
    /// Read back each post camera's exposure ([`crate::Renderer::camera_exposure`]), 1–3 frames
    /// late. Default false.
    pub read_exposure: bool,
    /// Force WebGL2-class code paths on any device (tests, parity, debugging). Default: nothing
    /// forced.
    pub downlevel: DownlevelOverride,
}

impl Default for FrameSettings {
    fn default() -> Self {
        Self {
            render_scale: 1.0,
            post: true,
            gpu_timing: false,
            read_exposure: false,
            downlevel: DownlevelOverride::default(),
        }
    }
}

impl FrameSettings {
    /// Set [`Self::render_scale`].
    pub fn with_render_scale(mut self, scale: f32) -> Self {
        self.render_scale = scale;
        self
    }
    /// Set [`Self::post`].
    pub fn with_post(mut self, on: bool) -> Self {
        self.post = on;
        self
    }
    /// Set [`Self::gpu_timing`].
    pub fn with_gpu_timing(mut self, on: bool) -> Self {
        self.gpu_timing = on;
        self
    }
    /// Set [`Self::downlevel`].
    pub fn with_downlevel(mut self, d: DownlevelOverride) -> Self {
        self.downlevel = d;
        self
    }
}

/// Capabilities to pretend are missing: masks [`crate::GraphicsCaps`], never adds any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DownlevelOverride {
    /// No float render targets: post cameras use the Ldr fallback; no bloom; LDR IBL bake.
    pub no_float_targets: bool,
    /// No compute: downlevel auto exposure; the particles' `Auto` backend picks the CPU
    /// backend. (The draw path is still chosen by [`crate::RenderSettings::draw_path`].)
    pub no_compute: bool,
    /// No read-only depth-stencil: scene depth is sampled without an attachment (the GL path).
    pub no_read_only_depth: bool,
}

impl DownlevelOverride {
    /// Everything WebGL2 lacks: all overrides on.
    pub fn webgl2() -> Self {
        Self {
            no_float_targets: true,
            no_compute: true,
            no_read_only_depth: true,
        }
    }
}

/// Per-camera frame options: an optional component next to a [`crate::Camera3d`] (which is
/// unchanged). Read from the lead camera of the camera's target group (graphics spec §1.3);
/// differing values of other cameras of the group are ignored with a warning.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraFrame {
    /// Scene storage of this camera's target group.
    pub hdr: HdrMode,
    /// Scene resolution scale (0.25..=1) of this camera's target group; `None` uses
    /// [`FrameSettings::render_scale`] (Primary targets) or 1.
    pub render_scale: Option<f32>,
}

impl CameraFrame {
    /// Set [`Self::hdr`].
    pub fn with_hdr(mut self, hdr: HdrMode) -> Self {
        self.hdr = hdr;
        self
    }
    /// Set [`Self::render_scale`].
    pub fn with_render_scale(mut self, scale: f32) -> Self {
        self.render_scale = Some(scale);
        self
    }
}

/// Scene storage of a target group with a post camera.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HdrMode {
    /// `Rgba16Float` scene where float targets exist; the Ldr fallback elsewhere.
    #[default]
    Auto,
    /// LDR scene with per-fragment tonemapping even where HDR exists (cheapest post path).
    /// Opaque single-sample scenes look like `Auto`; MSAA silhouettes (tonemapped per sample
    /// before the resolve) and translucent overlaps (blended after tonemapping) differ.
    Ldr,
    /// `Rg11b10Ufloat` (half the bandwidth, no alpha) on opaque-cleared primary targets where
    /// renderable; `Auto` elsewhere.
    Compact,
}

/// Render-target options for [`crate::RenderAssets::add_render_target_with`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderTargetDesc {
    /// Color format class.
    pub format: RenderTargetFormat,
    /// MSAA samples of the cameras rendering into it (1 or 4; clamped to support). Default 1.
    pub samples: u32,
}

impl Default for RenderTargetDesc {
    fn default() -> Self {
        Self {
            format: RenderTargetFormat::Ldr,
            samples: 1,
        }
    }
}

impl RenderTargetDesc {
    /// A linear HDR target ([`RenderTargetFormat::Hdr`], 1 sample).
    pub fn hdr() -> Self {
        Self {
            format: RenderTargetFormat::Hdr,
            samples: 1,
        }
    }
    /// Set [`Self::samples`].
    pub fn with_samples(mut self, samples: u32) -> Self {
        self.samples = samples;
        self
    }
}

/// Color format class of a render target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RenderTargetFormat {
    /// `Rgba8UnormSrgb` (what [`crate::RenderAssets::add_render_target`] creates):
    /// display-referred.
    #[default]
    Ldr,
    /// [`crate::GraphicsCaps::hdr_format`] (LDR without it): linear radiance, rendered in
    /// Direct mode; `render_target_view` returns the float view (egui shows it unclamped).
    Hdr,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_builders() {
        let s = FrameSettings::default();
        assert_eq!(
            (s.render_scale, s.post, s.gpu_timing, s.read_exposure),
            (1.0, true, false, false)
        );
        assert_eq!(s.downlevel, DownlevelOverride::default());
        let s = FrameSettings::default()
            .with_render_scale(0.5)
            .with_post(false)
            .with_gpu_timing(true)
            .with_downlevel(DownlevelOverride::webgl2());
        assert_eq!((s.render_scale, s.post, s.gpu_timing), (0.5, false, true));
        assert!(s.downlevel.no_float_targets && s.downlevel.no_compute && s.downlevel.no_read_only_depth);
        let c = CameraFrame::default()
            .with_hdr(HdrMode::Compact)
            .with_render_scale(0.75);
        assert_eq!((c.hdr, c.render_scale), (HdrMode::Compact, Some(0.75)));
        assert_eq!(CameraFrame::default().render_scale, None);
        assert_eq!(RenderTargetDesc::default().format, RenderTargetFormat::Ldr);
        assert_eq!(RenderTargetDesc::hdr().with_samples(4).samples, 4);
    }
}
