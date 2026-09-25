//! [`GraphicsCaps`]: the one capability probe of the graphics features (frame, post,
//! materials, particles), after [`crate::DownlevelOverride`] masking (graphics spec §2.2.2).

use crate::renderer::RenderCapabilities;
use crate::settings::DownlevelOverride;

/// Graphics capabilities of the device after [`crate::FrameSettings::downlevel`] masking
/// ([`crate::Renderer::graphics_caps`]).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GraphicsCaps {
    /// `Rgba16Float` when it can be rendered to, sampled, filtered and blended. WebGL2 needs
    /// `EXT_color_buffer_float` (wgpu-hal `gles/adapter.rs`).
    pub hdr_format: Option<wgpu::TextureFormat>,
    /// `hdr_format` also supports 4× MSAA and resolves. Without it, MSAA groups in Hdr mode drop
    /// to 1 sample (logged once), never to LDR.
    pub hdr_msaa: bool,
    /// `Rg11b10Ufloat` with the same abilities (`Features::RG11B10UFLOAT_RENDERABLE`).
    pub compact_hdr_format: Option<wgpu::TextureFormat>,
    /// Bloom chain format: compact, else hdr, else `None` (no bloom).
    pub bloom_format: Option<wgpu::TextureFormat>,
    /// 1×1 exposure texture format: `R32Float` if renderable, else `R16Float`, else `None`.
    pub exposure_format: Option<wgpu::TextureFormat>,
    /// `DownlevelFlags::READ_ONLY_DEPTH_STENCIL` (absent on WebGL2 / GLES).
    pub read_only_depth: bool,
    /// Multisampled depth can be resolved with a fragment pass (`read_only_depth` backends).
    pub msaa_depth_resolve: bool,
    /// [`RenderCapabilities::compute`] and `indirect` (culling-class compute).
    pub compute: bool,
    /// `compute` with ≥ 256 invocations per workgroup and ≥ 1 KiB of workgroup storage (the
    /// post exposure histogram).
    pub compute_histogram: bool,
    /// `Features::TIMESTAMP_QUERY` granted.
    pub timestamps: bool,
    /// `DownlevelFlags::DEPTH_BIAS_CLAMP` (shadow bias clamp).
    pub depth_bias_clamp: bool,
    /// `max_texture_dimension_2d`.
    pub max_texture_2d: u32,
    /// Largest cube map face.
    pub max_texture_cube: u32,
    /// `max_bind_groups`.
    pub max_bind_groups: u32,
}

/// The features every area asks for on top of the renderer's own; optional, intersected with
/// the adapter's.
pub(crate) const WANTED_FEATURES: wgpu::Features =
    wgpu::Features::TIMESTAMP_QUERY.union(wgpu::Features::RG11B10UFLOAT_RENDERABLE);

impl GraphicsCaps {
    /// Probe `adapter`/`device` (the device's granted features and limits decide).
    pub(crate) fn detect(adapter: &wgpu::Adapter, device: &wgpu::Device, caps: &RenderCapabilities) -> Self {
        use wgpu::TextureFormat as F;
        use wgpu::TextureFormatFeatureFlags as Flags;
        use wgpu::TextureUsages as U;
        let features = device.features();
        let limits = device.limits();
        let downlevel = adapter.get_downlevel_capabilities().flags;
        let color_target = |format: F| {
            let f = adapter.get_texture_format_features(format);
            f.allowed_usages.contains(U::RENDER_ATTACHMENT | U::TEXTURE_BINDING)
                && f.flags.contains(Flags::FILTERABLE | Flags::BLENDABLE)
        };
        let msaa = |format: F| {
            adapter
                .get_texture_format_features(format)
                .flags
                .contains(Flags::MULTISAMPLE_X4 | Flags::MULTISAMPLE_RESOLVE)
        };
        let hdr_format = color_target(F::Rgba16Float).then_some(F::Rgba16Float);
        let compact_hdr_format = (features.contains(wgpu::Features::RG11B10UFLOAT_RENDERABLE)
            && color_target(F::Rg11b10Ufloat))
        .then_some(F::Rg11b10Ufloat);
        let renderable = |format: F| {
            adapter
                .get_texture_format_features(format)
                .allowed_usages
                .contains(U::RENDER_ATTACHMENT | U::TEXTURE_BINDING)
        };
        let exposure_format = [F::R32Float, F::R16Float].into_iter().find(|f| renderable(*f));
        let read_only_depth = downlevel.contains(wgpu::DownlevelFlags::READ_ONLY_DEPTH_STENCIL);
        let depth_msaa = adapter
            .get_texture_format_features(crate::pipeline::DEPTH_FORMAT)
            .flags
            .sample_count_supported(4);
        let compute = caps.compute && caps.indirect;
        Self {
            hdr_format,
            hdr_msaa: hdr_format.is_some_and(msaa),
            compact_hdr_format,
            bloom_format: compact_hdr_format.or(hdr_format),
            exposure_format,
            read_only_depth,
            msaa_depth_resolve: read_only_depth && depth_msaa,
            compute,
            compute_histogram: compute
                && limits.max_compute_invocations_per_workgroup >= 256
                && limits.max_compute_workgroup_size_x >= 256
                && limits.max_compute_workgroup_storage_size >= 1024,
            timestamps: features.contains(wgpu::Features::TIMESTAMP_QUERY),
            depth_bias_clamp: downlevel.contains(wgpu::DownlevelFlags::DEPTH_BIAS_CLAMP),
            max_texture_2d: limits.max_texture_dimension_2d,
            max_texture_cube: limits.max_texture_dimension_2d,
            max_bind_groups: limits.max_bind_groups,
        }
    }

    /// These capabilities with `downlevel`'s pretended absences removed (never adds any).
    #[allow(
        dead_code,
        reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
    )]
    pub(crate) fn masked(&self, downlevel: DownlevelOverride) -> Self {
        let mut c = self.clone();
        if downlevel.no_float_targets {
            c.hdr_format = None;
            c.hdr_msaa = false;
            c.compact_hdr_format = None;
            c.bloom_format = None;
            c.exposure_format = None;
        }
        if downlevel.no_compute {
            c.compute = false;
            c.compute_histogram = false;
        }
        if downlevel.no_read_only_depth {
            c.read_only_depth = false;
            c.msaa_depth_resolve = false;
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> GraphicsCaps {
        GraphicsCaps {
            hdr_format: Some(wgpu::TextureFormat::Rgba16Float),
            hdr_msaa: true,
            compact_hdr_format: Some(wgpu::TextureFormat::Rg11b10Ufloat),
            bloom_format: Some(wgpu::TextureFormat::Rg11b10Ufloat),
            exposure_format: Some(wgpu::TextureFormat::R32Float),
            read_only_depth: true,
            msaa_depth_resolve: true,
            compute: true,
            compute_histogram: true,
            timestamps: true,
            depth_bias_clamp: true,
            max_texture_2d: 8192,
            max_texture_cube: 8192,
            max_bind_groups: 4,
        }
    }

    #[test]
    fn downlevel_masks_and_never_adds() {
        let caps = full();
        assert_eq!(caps.masked(DownlevelOverride::default()), caps);
        let gl = caps.masked(DownlevelOverride::webgl2());
        assert_eq!((gl.hdr_format, gl.bloom_format, gl.exposure_format), (None, None, None));
        assert!(!gl.compute && !gl.compute_histogram && !gl.read_only_depth && !gl.msaa_depth_resolve);
        assert!(gl.timestamps && gl.depth_bias_clamp, "unrelated caps stay");
        let mut none = gl.clone();
        none.timestamps = false;
        assert_eq!(
            none.masked(DownlevelOverride::default()),
            none,
            "masking adds nothing back"
        );
    }

    #[test]
    fn detects_the_device() {
        let Ok(r) = pollster::block_on(crate::Renderer::new_headless(4, 4)) else {
            eprintln!("skipping GPU test: no adapter available");
            return;
        };
        let caps = r.graphics_caps();
        eprintln!("{:?}: {caps:?}", r.capabilities().adapter.backend);
        assert_eq!(caps.compute, r.capabilities().compute && r.capabilities().indirect);
        assert_eq!(caps.bloom_format, caps.compact_hdr_format.or(caps.hdr_format));
        assert!(caps.max_texture_2d >= 2048 && caps.max_bind_groups >= 4);
        if r.capabilities().adapter.backend == wgpu::Backend::Gl {
            assert!(!caps.read_only_depth, "GL has no read-only depth-stencil");
        }
        if r.capabilities().adapter.backend == wgpu::Backend::Metal {
            assert_eq!(caps.hdr_format, Some(wgpu::TextureFormat::Rgba16Float));
            assert!(caps.read_only_depth && caps.msaa_depth_resolve);
        }
    }
}
