//! Post-processing API hub (graphics spec §2.2.7; defaults per design-post §3): the
//! [`PostProcess`] camera component and its effect settings. Re-exported from the crate root.
//!
//! Config structs are plain `pub`-field `Copy` structs with `Default` (functional record update
//! works); enums are `#[non_exhaustive]`. A camera with an enabled `PostProcess` renders through
//! its target group's scene texture (HDR, or the Ldr fallback) and the composite: exposure
//! (manual), the tonemapper and dither work since the frame milestone F4, bloom since PO1, FXAA,
//! vignette, chromatic aberration and film grain since PO3; auto exposure, grading and
//! distortion arrive with the post milestones PO2, PO4 and PO5 (docs/graphics/post.md).

use crate::assets::{AddressMode, FilterMode, SamplerSettings, TextureSource};
use crate::handles::{ShaderHandle, TextureHandle};
use crate::output::Tonemapper;
use crate::quality::GraphicsQuality;
use glam::{Vec2, Vec3};

/// Post-processing of one camera: insert next to a [`crate::Camera3d`]. Cameras without it (or
/// with `enabled: false`) render exactly as before.
///
/// Colors reaching post are linear HDR radiance: 1.0 is diffuse white at `exposure = 0`;
/// emissive and additive values above the bloom threshold glow. Every field can change at
/// runtime; changes apply to the next frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PostProcess {
    /// Master switch. `false` = identical to not having the component. Default true.
    pub enabled: bool,
    /// Exposure in stops: radiance × 2^exposure before tonemapping. With auto exposure, the
    /// compensation added to the metered exposure. Default 0.
    pub exposure: f32,
    /// Histogram-based automatic exposure. Default off.
    pub auto_exposure: AutoExposure,
    /// HDR → display mapping. Default [`Tonemapper::AgX`].
    pub tonemapper: Tonemapper,
    /// Bloom. Default [`Bloom::glow`] (enabled).
    pub bloom: Bloom,
    /// Post anti-aliasing. Default none.
    pub anti_aliasing: AntiAliasing,
    /// Darkened or tinted corners. Default off.
    pub vignette: Vignette,
    /// Radial RGB split toward the corners. Default off.
    pub chromatic_aberration: ChromaticAberration,
    /// Seeded film grain. Default off.
    pub film_grain: FilmGrain,
    /// Applies the camera's distortion buffer (heat haze, shockwaves). Default on (free while
    /// nothing writes distortion in the view).
    pub distortion: Distortion,
    /// White balance, saturation, contrast, lift/gamma/gain, LUT, WGSL hook. Default neutral.
    pub color_grading: ColorGrading,
    /// ±1 LSB triangular dither before 8-bit quantization. Default true.
    pub dither: bool,
}

impl Default for PostProcess {
    fn default() -> Self {
        Self {
            enabled: true,
            exposure: 0.0,
            auto_exposure: AutoExposure::default(),
            tonemapper: Tonemapper::AgX,
            bloom: Bloom::glow(),
            anti_aliasing: AntiAliasing::None,
            vignette: Vignette::default(),
            chromatic_aberration: ChromaticAberration::default(),
            film_grain: FilmGrain::default(),
            distortion: Distortion::default(),
            color_grading: ColorGrading::default(),
            dither: true,
        }
    }
}

impl PostProcess {
    /// Same as `default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Tonemapper `None`, every effect off, no dither: the legacy look through the HDR path.
    pub fn passthrough() -> Self {
        Self {
            tonemapper: Tonemapper::None,
            bloom: Bloom {
                enabled: false,
                ..Bloom::glow()
            },
            distortion: Distortion {
                enabled: false,
                ..Distortion::default()
            },
            dither: false,
            ..Self::default()
        }
    }

    /// Default + FXAA (Medium) + vignette 0.3 + chromatic aberration 0.15 + film grain 0.05.
    pub fn cinematic() -> Self {
        Self::default()
            .with_fxaa(FxaaQuality::Medium)
            .with_vignette(Vignette {
                enabled: true,
                intensity: 0.3,
                ..Vignette::default()
            })
            .with_chromatic_aberration(0.15)
            .with_film_grain(FilmGrain {
                enabled: true,
                intensity: 0.05,
                ..FilmGrain::default()
            })
    }

    /// Adjust only the performance-relevant fields to a tier: bloom mips and first-level height
    /// (Low 4 / 270, Medium 6 / 540, High 7 / 720), the FXAA quality when FXAA is on, and no
    /// chromatic aberration on Low.
    pub fn with_quality(mut self, q: GraphicsQuality) -> Self {
        let (mips, height, fxaa) = match q {
            GraphicsQuality::Low => (4, 270, FxaaQuality::Low),
            GraphicsQuality::Medium => (6, 540, FxaaQuality::Medium),
            _ => (7, 720, FxaaQuality::High),
        };
        self.bloom.mip_count = mips;
        self.bloom.max_mip_height = height;
        if let AntiAliasing::Fxaa(_) = self.anti_aliasing {
            self.anti_aliasing = AntiAliasing::Fxaa(fxaa);
        }
        if q == GraphicsQuality::Low {
            self.chromatic_aberration.enabled = false;
        }
        self
    }

    /// Set [`Self::exposure`] (stops).
    pub fn with_exposure(mut self, stops: f32) -> Self {
        self.exposure = stops;
        self
    }
    /// Enable auto exposure with these settings.
    pub fn with_auto_exposure(mut self, a: AutoExposure) -> Self {
        self.auto_exposure = AutoExposure { enabled: true, ..a };
        self
    }
    /// Set [`Self::tonemapper`].
    pub fn with_tonemapper(mut self, t: Tonemapper) -> Self {
        self.tonemapper = t;
        self
    }
    /// Set [`Self::bloom`] (every field, `enabled` included: `Bloom::glow()`,
    /// `Bloom::natural()` and `Bloom::default()` are enabled).
    pub fn with_bloom(mut self, b: Bloom) -> Self {
        self.bloom = b;
        self
    }
    /// Disable bloom.
    pub fn without_bloom(mut self) -> Self {
        self.bloom.enabled = false;
        self
    }
    /// FXAA at `q`.
    pub fn with_fxaa(mut self, q: FxaaQuality) -> Self {
        self.anti_aliasing = AntiAliasing::Fxaa(q);
        self
    }
    /// Set [`Self::vignette`].
    pub fn with_vignette(mut self, v: Vignette) -> Self {
        self.vignette = v;
        self
    }
    /// Chromatic aberration of `intensity` (off at 0).
    pub fn with_chromatic_aberration(mut self, intensity: f32) -> Self {
        self.chromatic_aberration = ChromaticAberration {
            enabled: intensity > 0.0,
            intensity,
        };
        self
    }
    /// Set [`Self::film_grain`].
    pub fn with_film_grain(mut self, g: FilmGrain) -> Self {
        self.film_grain = g;
        self
    }
    /// Set [`Self::distortion`].
    pub fn with_distortion(mut self, d: Distortion) -> Self {
        self.distortion = d;
        self
    }
    /// Set [`Self::color_grading`].
    pub fn with_color_grading(mut self, g: ColorGrading) -> Self {
        self.color_grading = g;
        self
    }
    /// Grade through a 3D LUT.
    pub fn with_lut(mut self, lut: ColorLut) -> Self {
        self.color_grading.lut = Some(lut);
        self
    }
}

/// Histogram-based automatic exposure: the metered exposure maps the (percentile-filtered)
/// average scene luminance to middle grey (0.18), is clamped to `min_ev..=max_ev`, then
/// [`PostProcess::exposure`] is added as compensation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoExposure {
    /// Default false.
    pub enabled: bool,
    /// Lowest metered exposure in stops. Default −8.
    pub min_ev: f32,
    /// Highest metered exposure in stops. Default 8.
    pub max_ev: f32,
    /// Adaptation rate (1/s, exponential) when the scene gets brighter. Default 3.
    pub adapt_to_bright: f32,
    /// Adaptation rate (1/s) when the scene gets darker. Default 1.
    pub adapt_to_dark: f32,
    /// Fraction of the darkest samples ignored (compute path). Default 0.10.
    pub low_percentile: f32,
    /// Fraction below which samples count (compute path). Default 0.90.
    pub high_percentile: f32,
    /// Sample weighting. Default `CenterWeighted`.
    pub metering: Metering,
}

impl Default for AutoExposure {
    fn default() -> Self {
        Self {
            enabled: false,
            min_ev: -8.0,
            max_ev: 8.0,
            adapt_to_bright: 3.0,
            adapt_to_dark: 1.0,
            low_percentile: 0.10,
            high_percentile: 0.90,
            metering: Metering::CenterWeighted,
        }
    }
}

impl AutoExposure {
    /// Enabled with the defaults.
    pub fn on() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }
    /// Adapt within a single frame (tests, cuts).
    pub fn instant(mut self) -> Self {
        self.adapt_to_bright = f32::INFINITY;
        self.adapt_to_dark = f32::INFINITY;
        self
    }
}

/// Auto-exposure sample weighting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Metering {
    /// Every sample weighs the same.
    Average,
    /// Weight 4 at the center falling to 1 at the corners.
    #[default]
    CenterWeighted,
}

/// Bloom (Jimenez, "Next-Generation Post Processing in Call of Duty: Advanced Warfare", 2014):
/// the exposed scene above a soft-knee threshold (Karis-averaged on the first level, so single
/// hot pixels do not flicker), a chain of 13-tap downsamples and 3×3 tent upsamples blended by
/// `scatter`, added after exposure (or mixed in, [`BloomComposite::EnergyConserving`]).
///
/// Works in Hdr mode; off in the Ldr fallback (no float render targets or `HdrMode::Ldr`,
/// warned once). Each post group has its own chain, clamped to its rect: split screens never
/// bleed into each other. `HdrScene` overlays bloom; `AfterTonemap` overlays and display views do
/// not. See docs/graphics/post.md.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bloom {
    /// On/off.
    pub enabled: bool,
    /// Additive: multiplier (default 0.35). Energy-conserving: mix factor 0..1.
    pub intensity: f32,
    /// Exposed radiance (brightest channel, after exposure) where bloom starts; 1.0 = display
    /// white, 0 = everything blooms. Default 1.
    pub threshold: f32,
    /// Knee width as a fraction of `threshold` (0 = hard cut). Default 0.5.
    pub soft_knee: f32,
    /// 0 keeps the glow tight, 1 is a wide haze: level k of the chain weighs (1 − s)·s^k, the
    /// last level the rest. Default 0.7.
    pub scatter: f32,
    /// How bloom is added.
    pub composite: BloomComposite,
    /// Mip levels, 1..=8 (the last level stays ≥ 2 px). Default 6.
    pub mip_count: u32,
    /// Height cap of the first (half-resolution) level. Default 540.
    pub max_mip_height: u32,
}

impl Default for Bloom {
    fn default() -> Self {
        Self::glow()
    }
}

impl Bloom {
    /// Emissive glow for games: threshold 1, knee 0.5, additive 0.35, scatter 0.7.
    pub fn glow() -> Self {
        Self {
            enabled: true,
            intensity: 0.35,
            threshold: 1.0,
            soft_knee: 0.5,
            scatter: 0.7,
            composite: BloomComposite::Additive,
            mip_count: 6,
            max_mip_height: 540,
        }
    }
    /// Photographic veil: threshold 0, energy-conserving 0.15, scatter 0.85.
    pub fn natural() -> Self {
        Self {
            intensity: 0.15,
            threshold: 0.0,
            scatter: 0.85,
            composite: BloomComposite::EnergyConserving,
            ..Self::glow()
        }
    }
    /// Set [`Self::intensity`].
    pub fn with_intensity(mut self, i: f32) -> Self {
        self.intensity = i;
        self
    }
    /// Set [`Self::threshold`] and [`Self::soft_knee`].
    pub fn with_threshold(mut self, t: f32, knee: f32) -> Self {
        self.threshold = t;
        self.soft_knee = knee;
        self
    }
    /// Set [`Self::scatter`].
    pub fn with_scatter(mut self, s: f32) -> Self {
        self.scatter = s;
        self
    }
}

/// How bloom is added to the scene.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BloomComposite {
    /// `scene + bloom × intensity` (adds energy: glow around lights).
    #[default]
    Additive,
    /// `mix(scene, bloom, intensity)` (redistributes energy: lens veil).
    EnergyConserving,
}

/// Post anti-aliasing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AntiAliasing {
    /// None.
    #[default]
    None,
    /// FXAA 3.11 (Lottes) after tonemapping: one extra full-screen pass reading an 8-bit
    /// intermediate the size of the target; grain and dither run after it, in that pass.
    Fxaa(FxaaQuality),
}

/// FXAA preset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FxaaQuality {
    /// Preset 10: 3 search steps, edge threshold 1/4.
    Low,
    /// Preset 12: 5 steps, 1/6.
    #[default]
    Medium,
    /// Preset 29: 12 steps, 1/8.
    High,
    /// Preset 39: 12 steps, 1/8, sub-pixel 1.0.
    Ultra,
}

/// Darkened or tinted corners.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vignette {
    /// Default false.
    pub enabled: bool,
    /// 0..1 darkening at the corners. Default 0.3.
    pub intensity: f32,
    /// 0.01..1 falloff width. Default 0.45.
    pub smoothness: f32,
    /// 1 = circular whatever the aspect, 0 = follows the viewport's aspect. Default 1.
    pub roundness: f32,
    /// Normalized viewport center of the effect. Default (0.5, 0.5).
    pub center: Vec2,
    /// Linear multiplier at full vignette (black darkens, a color tints). Default black.
    pub color: Vec3,
}

impl Default for Vignette {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.3,
            smoothness: 0.45,
            roundness: 1.0,
            center: Vec2::splat(0.5),
            color: Vec3::ZERO,
        }
    }
}

/// Radial RGB split toward the corners.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChromaticAberration {
    /// Default false.
    pub enabled: bool,
    /// 0..1: red/blue offset at the corners, 1 = 2 % of the viewport width. Default 0.2.
    pub intensity: f32,
}

impl Default for ChromaticAberration {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.2,
        }
    }
}

/// Seeded film grain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilmGrain {
    /// Default false.
    pub enabled: bool,
    /// 0..1 amplitude in display space. Default 0.06.
    pub intensity: f32,
    /// Grain cell size in pixels at 1080p. Default 1.6.
    pub size: f32,
    /// 0..1: 1 = no grain in highlights. Default 0.8.
    pub luminance_response: f32,
    /// New pattern every frame (seeded from `Time::frame`); `false` = a static pattern. Default
    /// true.
    pub animated: bool,
    /// Pattern seed. Default 0.
    pub seed: u32,
}

impl Default for FilmGrain {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.06,
            size: 1.6,
            luminance_response: 0.8,
            animated: true,
            seed: 0,
        }
    }
}

/// Applies the camera's distortion buffer in the composite.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distortion {
    /// Default true (free while nothing writes distortion in the view).
    pub enabled: bool,
    /// Multiplier on every written offset. Default 1.
    pub strength: f32,
    /// 0..1 spectral split: R/G/B displaced by (1 − c, 1, 1 + c) × offset. Default 0.
    pub chromatic: f32,
    /// Resolution of the distortion buffer. Default `Half`.
    pub resolution: DistortionResolution,
}

impl Default for Distortion {
    fn default() -> Self {
        Self {
            enabled: true,
            strength: 1.0,
            chromatic: 0.0,
            resolution: DistortionResolution::Half,
        }
    }
}

/// Resolution of a post group's distortion buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DistortionResolution {
    /// Half the post group's rect.
    #[default]
    Half,
    /// The post group's rect.
    Full,
}

/// Component: draws the entity's mesh (`Mesh3d` + `GlobalTransform`, optional `Visibility`; no
/// `Material` needed) into the distortion buffer of every post camera that sees it (node
/// `"space3d/distortion-meshes"`), instead of into color. Occluded by scene depth where the
/// device can sample it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DistortionMaterial {
    /// Offset in view heights at full strength (0.01 = 1 % of the screen height). Default 0.01.
    pub strength: f32,
    /// Offset direction.
    pub mode: DistortionMode,
    /// `Normal` mode: RG offsets (linear texture, 0.5 = none); `None` = built-in value noise.
    pub texture: Option<TextureHandle>,
    /// UV scroll per second (heat haze). Default (0, 0.35).
    pub scroll: Vec2,
    /// Fade over this many world units where the mesh meets scene geometry (0 = hard).
    /// Default 0.
    pub soft_depth: f32,
}

impl Default for DistortionMaterial {
    fn default() -> Self {
        Self {
            strength: 0.01,
            mode: DistortionMode::Normal,
            texture: None,
            scroll: Vec2::new(0.0, 0.35),
            soft_depth: 0.0,
        }
    }
}

/// Offset direction of a [`DistortionMaterial`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub enum DistortionMode {
    /// Offset from a texture or noise (heat haze).
    #[default]
    Normal,
    /// Push away from the mesh's UV center with a smooth ring profile between two UV radii
    /// (shockwaves, warp rings on a quad).
    Radial {
        /// Inner UV radius.
        inner: f32,
        /// Outer UV radius.
        outer: f32,
    },
}

/// Format of the distortion buffer (signed offsets split into positive and negative channels).
pub const DISTORTION_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// WGSL for distortion writers (requires `VIEW_WGSL` and `scene_wgsl` first):
/// `const DISTORTION_RANGE = 0.0625;`, `distortion_encode(offset_view_heights, weight)`,
/// `distortion_depth_fade(frag_position, db_size, softness)`. Blend One/One.
pub const DISTORTION_WGSL: &str = include_str!("../shaders/post/distortion.wgsl");

/// Grading in the composite: white balance → saturation → contrast (linear HDR) → tonemap →
/// lift/gamma/gain → LUT → hook (display space).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorGrading {
    /// −1 (cool) ..1 (warm). Default 0.
    pub temperature: f32,
    /// Green (−1) .. magenta (1). Default 0.
    pub tint: f32,
    /// 0 = grey, 1 = unchanged. Default 1.
    pub saturation: f32,
    /// Around middle grey in log2 space. Default 1.
    pub contrast: f32,
    /// ASC-CDL offset (display space). Default 0.
    pub lift: Vec3,
    /// ASC-CDL power. Default 1.
    pub gamma: Vec3,
    /// ASC-CDL slope. Default 1.
    pub gain: Vec3,
    /// Optional 3D LUT (strip texture), applied in sRGB-encoded display space.
    pub lut: Option<ColorLut>,
    /// Optional WGSL hook (a `CustomShader` defining `post_hdr_hook` and/or `post_ldr_hook`,
    /// `fn(c: vec3<f32>, uv: vec2<f32>) -> vec3<f32>`). Invalid hooks fall back to identity.
    pub shader: Option<ShaderHandle>,
}

impl Default for ColorGrading {
    fn default() -> Self {
        Self {
            temperature: 0.0,
            tint: 0.0,
            saturation: 1.0,
            contrast: 1.0,
            lift: Vec3::ZERO,
            gamma: Vec3::ONE,
            gain: Vec3::ONE,
            lut: None,
            shader: None,
        }
    }
}

impl ColorGrading {
    /// No-op grading (the composite skips it).
    pub fn is_neutral(&self) -> bool {
        *self == Self::default()
    }
}

/// A 3D LUT stored as an N²×N strip texture (slice b at x ∈ [b·N, (b+1)·N)), indexed by
/// sRGB-encoded color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorLut {
    /// The strip texture.
    pub texture: TextureHandle,
    /// N (16, 32 or 64).
    pub size: u32,
    /// 0..1 blend between ungraded and graded. Default 1.
    pub contribution: f32,
}

impl ColorLut {
    /// Full contribution.
    pub fn new(texture: TextureHandle, size: u32) -> Self {
        Self {
            texture,
            size,
            contribution: 1.0,
        }
    }

    /// Neutral N²×N strip (`Rgba8Unorm`, no mips, clamped linear sampler; N clamped to 2..=64)
    /// to export, grade in an image editor and load back.
    pub fn identity(size: u32) -> TextureSource {
        let n = size.clamp(2, 64);
        let level = |i: u32| ((i as f32 / (n - 1) as f32) * 255.0).round() as u8;
        let mut data = Vec::with_capacity((n * n * n * 4) as usize);
        for y in 0..n {
            for x in 0..n * n {
                data.extend_from_slice(&[level(x % n), level(y), level(x / n), 255]);
            }
        }
        TextureSource {
            width: n * n,
            height: n,
            layers: 1,
            format: wgpu::TextureFormat::Rgba8Unorm,
            mip_level_count: 1,
            data,
            generate_mipmaps: false,
            sampler: SamplerSettings {
                filter: FilterMode::Linear,
                address: AddressMode::Clamp,
            },
        }
    }

    /// Parse an Adobe/Resolve `.cube` 3D LUT (`LUT_3D_SIZE` ≤ 64) into an `Rgba16Float` strip.
    pub fn from_cube(text: &str) -> Result<TextureSource, LutError> {
        let _ = text; // STUB(P-POST): the parser lands with PO4
        Err(LutError::MissingSize)
    }
}

/// Why a `.cube` LUT was rejected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LutError {
    /// No `LUT_3D_SIZE` line.
    #[error("missing LUT_3D_SIZE")]
    MissingSize,
    /// `LUT_3D_SIZE` outside 2..=64.
    #[error("unsupported LUT size {0} (2..=64)")]
    Size(u32),
    /// Wrong number of table entries.
    #[error("expected {expected} entries, found {found}")]
    Count {
        /// N³.
        expected: usize,
        /// Entries read.
        found: usize,
    },
    /// A malformed line.
    #[error("line {line}: {message}")]
    Parse {
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_presets() {
        let p = PostProcess::default();
        assert!(p.enabled && p.dither && p.bloom.enabled && p.distortion.enabled);
        assert!(!p.auto_exposure.enabled && !p.vignette.enabled && !p.film_grain.enabled);
        assert_eq!(
            (p.exposure, p.tonemapper, p.anti_aliasing),
            (0.0, Tonemapper::AgX, AntiAliasing::None)
        );
        assert!(p.color_grading.is_neutral());
        assert_eq!(PostProcess::new(), p);
        let pass = PostProcess::passthrough();
        assert!(!pass.bloom.enabled && !pass.dither && !pass.distortion.enabled);
        assert_eq!(pass.tonemapper, Tonemapper::None);
        let c = PostProcess::cinematic();
        assert_eq!(c.anti_aliasing, AntiAliasing::Fxaa(FxaaQuality::Medium));
        assert!(c.vignette.enabled && c.chromatic_aberration.enabled && c.film_grain.enabled);
        assert_eq!((c.chromatic_aberration.intensity, c.film_grain.intensity), (0.15, 0.05));
        let low = c.with_quality(GraphicsQuality::Low);
        assert_eq!((low.bloom.mip_count, low.bloom.max_mip_height), (4, 270));
        assert_eq!(low.anti_aliasing, AntiAliasing::Fxaa(FxaaQuality::Low));
        assert!(!low.chromatic_aberration.enabled);
        let n = Bloom::natural();
        assert_eq!((n.threshold, n.composite), (0.0, BloomComposite::EnergyConserving));
        assert!(AutoExposure::on().instant().adapt_to_dark.is_infinite());
        let q = PostProcess::default()
            .with_auto_exposure(AutoExposure::default())
            .without_bloom()
            .with_exposure(1.5);
        assert!(q.auto_exposure.enabled && !q.bloom.enabled && q.exposure == 1.5);
        // `with_bloom` sets every field: presets are enabled, an explicit `enabled: false` stays
        // off (the `Engine/Post/Bloom` story's `enabled` knob).
        assert!(q.with_bloom(Bloom::glow()).bloom.enabled && q.with_bloom(Bloom::default()).bloom.enabled);
        let off = Bloom {
            enabled: false,
            ..Bloom::glow()
        };
        assert!(!PostProcess::default().with_bloom(off).bloom.enabled);
        let graded = ColorGrading {
            saturation: 0.5,
            ..Default::default()
        };
        assert!(!graded.is_neutral());
    }

    #[test]
    fn identity_lut_is_a_strip() {
        let t = ColorLut::identity(4);
        assert_eq!((t.width, t.height, t.format), (16, 4, wgpu::TextureFormat::Rgba8Unorm));
        let px = |x: u32, y: u32| {
            let i = ((y * t.width + x) * 4) as usize;
            [t.data[i], t.data[i + 1], t.data[i + 2]]
        };
        assert_eq!(px(0, 0), [0, 0, 0]);
        assert_eq!(px(3, 0), [255, 0, 0]);
        assert_eq!(px(12, 3), [0, 255, 255]);
        assert_eq!(px(15, 3), [255, 255, 255]);
    }
}
