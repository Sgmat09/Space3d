//! FXAA (graphics spec §5.2 PO3; design-post §5.5): FXAA 3.11 quality presets, the per-quality
//! shader modules and pipelines. With FXAA on, a post group's composite writes the LDR
//! intermediate `L` (a pooled `Rgba8UnormSrgb` texture of the output's size) instead of the
//! output, and `fs_fxaa` (`shaders/post/fxaa.wgsl`, appended to the composite source) reads `L`,
//! anti-aliases, applies the last-pass finish (grain, dither, encoding) and writes the output.

use super::api::FxaaQuality;
use crate::handles::FastMap;
use std::borrow::Cow;

/// Source of the FXAA pass (after the composite source and the quality constants).
pub(crate) const FXAA_WGSL: &str = include_str!("../shaders/post/fxaa.wgsl");

/// Format of the LDR intermediate `L`: sampled as linear (the hardware decodes sRGB), 8 bits per
/// channel with sRGB precision like the display target.
pub(crate) const LDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// FXAA 3.11 parameters of a quality preset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FxaaPreset {
    /// Search step sizes (pixels), `steps` of them used.
    pub step: [f32; 12],
    pub steps: u32,
    /// Local contrast needed to filter, relative to the local maximum luma.
    pub edge: f32,
    /// Minimum absolute contrast to filter (skips dark areas).
    pub edge_min: f32,
    /// Sub-pixel aliasing removal (0 off … 1 softest).
    pub subpix: f32,
}

impl FxaaPreset {
    /// Lottes' presets 10, 12, 29 and 39.
    pub fn of(q: FxaaQuality) -> Self {
        let pad = |s: &[f32]| {
            let mut a = [0.0; 12];
            a[..s.len()].copy_from_slice(s);
            (a, s.len() as u32)
        };
        let (step, steps, edge, edge_min, subpix) = match q {
            FxaaQuality::Low => {
                let (a, n) = pad(&[1.5, 3.0, 12.0]);
                (a, n, 1.0 / 4.0, 0.0833, 0.75)
            }
            FxaaQuality::Medium => {
                let (a, n) = pad(&[1.0, 1.5, 2.0, 4.0, 12.0]);
                (a, n, 1.0 / 6.0, 0.0833, 0.75)
            }
            FxaaQuality::High => {
                let (a, n) = pad(&[1.0, 1.5, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 4.0, 8.0]);
                (a, n, 1.0 / 8.0, 0.0625, 0.75)
            }
            _ => {
                let (a, n) = pad(&[1.0, 1.0, 1.0, 1.0, 1.0, 1.5, 2.0, 2.0, 2.0, 2.0, 4.0, 8.0]);
                (a, n, 1.0 / 8.0, 0.0312, 1.0)
            }
        };
        Self {
            step,
            steps,
            edge,
            edge_min,
            subpix,
        }
    }

    /// The WGSL constants `fxaa.wgsl` expects.
    pub fn wgsl(&self) -> String {
        let step: Vec<String> = self.step.iter().map(|v| format!("{v:?}")).collect();
        format!(
            "const S3D_FXAA_STEPS: i32 = {};\nconst S3D_FXAA_STEP = array<f32, 12>({});\n\
             const S3D_FXAA_EDGE: f32 = {:?};\nconst S3D_FXAA_EDGE_MIN: f32 = {:?};\n\
             const S3D_FXAA_SUBPIX: f32 = {:?};\n",
            self.steps,
            step.join(", "),
            self.edge,
            self.edge_min,
            self.subpix
        )
    }
}

/// The FXAA source for a quality: the composite source (params, finish), the preset's constants
/// and the pass.
pub(crate) fn fxaa_source(q: FxaaQuality) -> String {
    format!(
        "{}\n{}\n{FXAA_WGSL}",
        super::composite_source(),
        FxaaPreset::of(q).wgsl()
    )
}

/// FXAA shader modules (one per quality, created on first use).
#[derive(Default)]
pub(crate) struct FxaaModules {
    modules: FastMap<FxaaQuality, wgpu::ShaderModule>,
}

impl FxaaModules {
    pub fn get(&mut self, device: &wgpu::Device, q: FxaaQuality) -> &wgpu::ShaderModule {
        self.modules.entry(q).or_insert_with(|| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d fxaa"),
                source: wgpu::ShaderSource::Wgsl(Cow::Owned(fxaa_source(q))),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets() {
        let low = FxaaPreset::of(FxaaQuality::Low);
        assert_eq!((low.steps, low.edge), (3, 0.25));
        assert_eq!(FxaaPreset::of(FxaaQuality::Medium).steps, 5);
        assert_eq!(FxaaPreset::of(FxaaQuality::High).steps, 12);
        assert_eq!(FxaaPreset::of(FxaaQuality::Ultra).subpix, 1.0);
        for q in [
            FxaaQuality::Low,
            FxaaQuality::Medium,
            FxaaQuality::High,
            FxaaQuality::Ultra,
        ] {
            let p = FxaaPreset::of(q);
            // Every used step is positive, the unused ones zero.
            assert!(p.step[..p.steps as usize].iter().all(|s| *s > 0.0), "{q:?}");
            assert!(p.step[p.steps as usize..].iter().all(|s| *s == 0.0), "{q:?}");
        }
    }

    #[test]
    fn fxaa_wgsl_validates_for_every_quality() {
        for q in [
            FxaaQuality::Low,
            FxaaQuality::Medium,
            FxaaQuality::High,
            FxaaQuality::Ultra,
        ] {
            crate::pipeline::validate_wgsl(&fxaa_source(q)).unwrap_or_else(|e| panic!("{q:?}: {e}"));
        }
    }
}
