//! [`GraphicsQuality`]: coherent quality tiers for graphics menus and device defaults (graphics
//! spec §2.2.6, preset table §4.4). The presets are applied in Phase I; until then the
//! `frame_settings`/`post`/`lighting`/`shadow` adapters return their input unchanged.

use crate::caps::GraphicsCaps;
use crate::lighting::api::{DirectionalShadow, LightingSettings};
use crate::post::api::PostProcess;
use crate::renderer::RenderCapabilities;
use crate::settings::FrameSettings;

/// Coherent quality tiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum GraphicsQuality {
    /// WebGL2 and devices without float render targets.
    Low,
    /// Integrated, Apple and mobile GPUs.
    #[default]
    Medium,
    /// Discrete GPUs.
    High,
}

impl GraphicsQuality {
    /// The tier for a device: WebGL2 / GL or no float render targets → Low; discrete GPUs →
    /// High; everything else (integrated, Apple, mobile, software) → Medium.
    pub fn recommended(caps: &RenderCapabilities, graphics: &GraphicsCaps) -> Self {
        if caps.adapter.backend == wgpu::Backend::Gl || graphics.hdr_format.is_none() {
            Self::Low
        } else if caps.adapter.device_type == wgpu::DeviceType::DiscreteGpu {
            Self::High
        } else {
            Self::Medium
        }
    }

    /// `base` with this tier's frame options (render scale).
    pub fn frame_settings(self, base: FrameSettings) -> FrameSettings {
        base // STUB(I): §4.4 preset table
    }

    /// MSAA sample count for this tier on a device.
    pub fn msaa_samples(self, caps: &RenderCapabilities) -> u32 {
        let _ = caps; // STUB(I): 4 on TBDR / discrete per §4.4
        1
    }

    /// `base` with this tier's post settings (bloom mips, FXAA, auto exposure, CA, grain).
    pub fn post(self, base: PostProcess) -> PostProcess {
        base // STUB(I)
    }

    /// `base` with this tier's lighting settings (shading model, IBL sizes).
    pub fn lighting(self, base: LightingSettings) -> LightingSettings {
        base // STUB(I)
    }

    /// `base` with this tier's shadow map size and filter.
    pub fn shadow(self, base: DirectionalShadow) -> DirectionalShadow {
        base // STUB(I)
    }
}
