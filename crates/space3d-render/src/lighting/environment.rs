//! Image-based lighting (graphics spec §2.2.9, §5.2 PM4; design-materials §5.5, §6.4): the
//! fragment-pass bake shared by every backend (WebGL2 included), the environment cache entries
//! and the embedded BRDF LUT.
//!
//! * **Passes.** Every pass draws one face of one mip of a cube (a single-layer 2D view as the
//!   color attachment; on GL `framebufferTexture2D` with the face target) or one mip of a 2D
//!   texture, from a source cube or 2D texture, with one full-screen triangle
//!   (`ibl_bake.wgsl`). A [`Job`] is a list of such [`Step`]s, planned once (textures, views,
//!   bind groups and one uniform buffer of [`BakeParams`], one per step at the device's uniform
//!   offset alignment) and recorded into the frame encoder — all at once (cubemap uploads) or a
//!   budget per frame (environment bakes).
//! * **Environment** of a cubemap ([`Env`]): the GGX-prefiltered specular cube
//!   (`specular_size`² down to 8², roughness `m / (mips − 1)` at mip `m`; mip 0 is a copy) and
//!   the irradiance cube (`irradiance_size`², the cosine-weighted mean radiance E/π), baked from
//!   the cubemap's radiance mips with filtered importance sampling and fixed Hammersley sets
//!   (deterministic). `Rgba16Float` where it can be rendered, else `Rgba8UnormSrgb` (the LDR
//!   bake: values clamp at 1).
//! * **BRDF LUT**: the split-sum scale and bias of GGX with height-correlated Smith masking,
//!   64² `Rg16Float` indexed by (n·v, perceptual roughness) at texel centers, computed offline
//!   by [`brdf_lut_bytes`] (1024 samples of visible normals per texel) into `shaders/brdf_lut.bin` and uploaded
//!   once per renderer.

use super::api::{EnvironmentState, IblSettings};
use super::cubemap::GpuCubemap;
use crate::handles::{CubemapHandle, FastMap};
use bytemuck::{Pod, Zeroable};

/// Edge of the BRDF LUT in texels.
pub(crate) const BRDF_LUT_SIZE: u32 = 64;
/// Format of the BRDF LUT (filterable everywhere, WebGL2 included).
pub(crate) const BRDF_LUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rg16Float;
#[cfg(test)]
/// Hammersley samples per texel of the offline LUT.
pub(crate) const BRDF_LUT_SAMPLES: u32 = 1024;
/// The LUT: `BRDF_LUT_SIZE`² texels of two little-endian f16 (scale, bias); row `y` is roughness
/// `(y + 0.5) / 64`, column `x` is n·v `(x + 0.5) / 64`.
static BRDF_LUT_BIN: &[u8] = include_bytes!("../shaders/brdf_lut.bin");

#[cfg(test)]
/// Van der Corput radical inverse in base 2 (`s3d_radical_inverse` in `ibl_bake.wgsl`).
pub(crate) fn radical_inverse(i: u32) -> f32 {
    i.reverse_bits() as f32 * 2.328_306_4e-10
}

#[cfg(test)]
/// Split-sum environment BRDF (Karis 2013) of GGX with height-correlated Smith masking-shadowing
/// for the view angle `ndv` and perceptual `roughness` (α = roughness²): `(scale, bias)` such
/// that the directional albedo for normal-incidence reflectance f0 is `f0·scale + bias`.
/// `samples` Hammersley points of the distribution of visible normals (Heitz 2018): every
/// sample is above the horizon of the view, so grazing angles converge as fast as the others;
/// each weighs G2(l, v) / G1(v) (× the Fresnel split `1 − Fc` / `Fc`).
pub(crate) fn brdf_integrate(ndv: f32, roughness: f32, samples: u32) -> [f32; 2] {
    let ndv = (ndv as f64).clamp(1e-4, 1.0);
    let a = (roughness as f64 * roughness as f64).max(1e-6);
    let a2 = a * a;
    let v = glam::DVec3::new((1.0 - ndv * ndv).max(0.0).sqrt(), 0.0, ndv);
    // Smith Λ of a direction with cosine `c` to the normal.
    let lambda = |c: f64| {
        let c2 = (c * c).max(1e-12);
        0.5 * ((1.0 + a2 * (1.0 - c2) / c2).sqrt() - 1.0)
    };
    let g1 = 1.0 / (1.0 + lambda(ndv));
    let vh = glam::DVec3::new(a * v.x, a * v.y, v.z).normalize();
    let len2 = vh.x * vh.x + vh.y * vh.y;
    let t1 = if len2 > 0.0 {
        glam::DVec3::new(-vh.y, vh.x, 0.0) / len2.sqrt()
    } else {
        glam::DVec3::X
    };
    let t2 = vh.cross(t1);
    let (mut scale, mut bias) = (0.0f64, 0.0f64);
    for i in 0..samples {
        let (u1, u2) = (i as f64 / samples as f64, radical_inverse(i) as f64);
        let r = u1.sqrt();
        let phi = 2.0 * std::f64::consts::PI * u2;
        let p1 = r * phi.cos();
        let mut p2 = r * phi.sin();
        let s = 0.5 * (1.0 + vh.z);
        p2 = (1.0 - s) * (1.0 - p1 * p1).max(0.0).sqrt() + s * p2;
        let nh = p1 * t1 + p2 * t2 + (1.0 - p1 * p1 - p2 * p2).max(0.0).sqrt() * vh;
        let h = glam::DVec3::new(a * nh.x, a * nh.y, nh.z.max(0.0)).normalize();
        let vdh = v.dot(h);
        let l = 2.0 * vdh * h - v;
        if l.z > 0.0 {
            let g2 = 1.0 / (1.0 + lambda(ndv) + lambda(l.z));
            let w = g2 / g1;
            let fc = (1.0 - vdh.clamp(0.0, 1.0)).powi(5);
            scale += (1.0 - fc) * w;
            bias += fc * w;
        }
    }
    [(scale / samples as f64) as f32, (bias / samples as f64) as f32]
}

#[cfg(test)]
/// The LUT texel `(x, y)` computed with `samples` samples.
pub(crate) fn brdf_lut_texel(x: u32, y: u32, samples: u32) -> [f32; 2] {
    let at = |i: u32| (i as f32 + 0.5) / BRDF_LUT_SIZE as f32;
    brdf_integrate(at(x), at(y), samples)
}

#[cfg(test)]
/// The LUT's bytes (`shaders/brdf_lut.bin`): row-major, two f16 per texel.
pub(crate) fn brdf_lut_bytes(samples: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((BRDF_LUT_SIZE * BRDF_LUT_SIZE * 4) as usize);
    for y in 0..BRDF_LUT_SIZE {
        for x in 0..BRDF_LUT_SIZE {
            for c in brdf_lut_texel(x, y, samples) {
                out.extend_from_slice(&crate::instance::f32_to_f16(c).to_le_bytes());
            }
        }
    }
    out
}

/// The embedded LUT as a texture (created and uploaded once per renderer).
pub(crate) fn create_brdf_lut(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::TextureView {
    let size = wgpu::Extent3d {
        width: BRDF_LUT_SIZE,
        height: BRDF_LUT_SIZE,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("space3d brdf lut"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: BRDF_LUT_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        BRDF_LUT_BIN,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(BRDF_LUT_SIZE * 4),
            rows_per_image: Some(BRDF_LUT_SIZE),
        },
        size,
    );
    texture.create_view(&Default::default())
}

/// `BakeParams` of `ibl_bake.wgsl` (32 bytes, one per step at the uniform offset alignment).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub(super) struct BakeParams {
    pub face: u32,
    pub samples: u32,
    /// Bit 0: a compute step encodes sRGB (the output is an sRGB texture's linear view).
    pub flags: u32,
    pub pad: u32,
    pub roughness: f32,
    pub src_size: f32,
    pub src_max_lod: f32,
    pub lod: f32,
}

/// What a step computes (the `fs_*` / `cs_*` entry points of `ibl_bake.wgsl`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Entry {
    /// Cube face from a cube at an explicit LOD (face conversion, radiance mips).
    CubeCopy,
    /// 2D mip from the previous one (panorama mips; fragment only).
    Copy2d,
    /// Cube face from an equirectangular panorama.
    Equirect,
    /// GGX-prefiltered specular mip.
    Prefilter,
    /// Irradiance face.
    Irradiance,
}

impl Entry {
    fn fragment(self) -> &'static str {
        match self {
            Self::CubeCopy => "fs_cube_copy",
            Self::Copy2d => "fs_copy_2d",
            Self::Equirect => "fs_equirect",
            Self::Prefilter => "fs_prefilter",
            Self::Irradiance => "fs_irradiance",
        }
    }

    fn compute(self) -> &'static str {
        match self {
            Self::CubeCopy => "cs_cube_copy",
            Self::Copy2d => unreachable!("2D mips are rendered"),
            Self::Equirect => "cs_equirect",
            Self::Prefilter => "cs_prefilter",
            Self::Irradiance => "cs_irradiance",
        }
    }

    /// Reads a 2D source (else a cube).
    fn flat(self) -> bool {
        matches!(self, Self::Copy2d | Self::Equirect)
    }
}

/// How bake steps may write their targets (from the frame's capabilities).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Writes {
    /// Compute dispatches write storage textures (else every step is a render pass).
    pub compute: bool,
    /// `Rg11b10Ufloat` is a write-only storage format (`GraphicsCaps::compact_hdr_storage`).
    pub rg11b10: bool,
}

impl Writes {
    /// Compute dispatches where they can write storage textures (`rg11b10`: HDR skies too).
    pub fn new(compute: bool, rg11b10: bool) -> Self {
        Self {
            compute,
            rg11b10: compute && rg11b10,
        }
    }
}

/// The storage format a compute step writes a texture of `format` through, and whether it
/// encodes sRGB itself: `Rgba16Float`, `Rgba8Unorm` and (with `writes.rg11b10`)
/// `Rg11b10Ufloat` directly, `Rgba8UnormSrgb` through its `Rgba8Unorm` view (sRGB formats
/// cannot be storage textures). `None`: rendered only (no compute, or the format cannot be
/// written by it).
pub(super) fn storage_format(format: wgpu::TextureFormat, writes: Writes) -> Option<(wgpu::TextureFormat, bool)> {
    use wgpu::TextureFormat as F;
    if !writes.compute {
        return None;
    }
    match format {
        F::Rgba16Float | F::Rgba8Unorm => Some((format, false)),
        F::Rg11b10Ufloat if writes.rg11b10 => Some((format, false)),
        F::Rgba8UnormSrgb => Some((F::Rgba8Unorm, true)),
        _ => None,
    }
}

/// A cube (6 square layers: `TEXTURE_CUBE_MAP` on GL) that bake steps write.
pub(super) struct Target {
    pub texture: wgpu::Texture,
    /// The format it is sampled and rendered as.
    pub format: wgpu::TextureFormat,
    /// Face edge of mip 0.
    pub size: u32,
    pub mips: u32,
    /// Compute steps write it: (storage format, encode sRGB).
    pub storage: Option<(wgpu::TextureFormat, bool)>,
}

/// Bytes of a texture of `format`, mip 0 `w` × `h` × `layers`, with `mips` levels.
pub(super) fn texture_bytes(format: wgpu::TextureFormat, (w, h): (u32, u32), layers: u32, mips: u32) -> u64 {
    let texel = format.block_copy_size(None).unwrap_or(4) as u64;
    (0..mips)
        .map(|m| ((w >> m).max(1) as u64) * ((h >> m).max(1) as u64))
        .sum::<u64>()
        * texel
        * layers as u64
}

impl Target {
    /// Bytes of the cube with every mip.
    pub fn bytes(&self) -> u64 {
        texture_bytes(self.format, (self.size, self.size), 6, self.mips)
    }

    /// A cube of `format`; written by compute dispatches where `writes` allows it for the
    /// format ([`storage_format`]) — an `Rgba8UnormSrgb` cube is then an `Rgba8Unorm` texture
    /// viewed as sRGB — else by render passes.
    pub fn cube(
        device: &wgpu::Device,
        label: &'static str,
        size: u32,
        mips: u32,
        format: wgpu::TextureFormat,
        writes: Writes,
    ) -> Self {
        let storage = storage_format(format, writes);
        let texture_format = storage.map_or(format, |(f, _)| f);
        let mut usage = wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC;
        if storage.is_some() {
            usage |= wgpu::TextureUsages::STORAGE_BINDING;
        }
        let view_formats = [format];
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 6,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: texture_format,
            usage,
            view_formats: if texture_format == format { &[] } else { &view_formats },
        });
        Self {
            texture,
            format,
            size,
            mips,
            storage,
        }
    }

    /// A `Cube` view of `mips`, sampled as the target's format.
    pub fn cube_view(&self, mips: std::ops::Range<u32>) -> wgpu::TextureView {
        self.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("space3d cube"),
            format: Some(self.format),
            dimension: Some(wgpu::TextureViewDimension::Cube),
            usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
            base_mip_level: mips.start,
            mip_level_count: Some(mips.end - mips.start),
            ..Default::default()
        })
    }

    /// A single-layer 2D view of face `face`, mip `mip` (a color attachment).
    fn face_view(&self, face: u32, mip: u32) -> wgpu::TextureView {
        self.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("space3d cube face"),
            format: Some(self.format),
            dimension: Some(wgpu::TextureViewDimension::D2),
            usage: Some(wgpu::TextureUsages::RENDER_ATTACHMENT),
            base_mip_level: mip,
            mip_level_count: Some(1),
            base_array_layer: face,
            array_layer_count: Some(1),
            ..Default::default()
        })
    }

    /// The six faces of mip `mip` as a storage view.
    fn storage_view(&self, mip: u32, format: wgpu::TextureFormat) -> wgpu::TextureView {
        self.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("space3d cube storage"),
            format: Some(format),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            usage: Some(wgpu::TextureUsages::STORAGE_BINDING),
            base_mip_level: mip,
            mip_level_count: Some(1),
            base_array_layer: 0,
            array_layer_count: Some(6),
            ..Default::default()
        })
    }
}

/// Where a step writes.
enum Output {
    /// A render pass into this color attachment.
    Render(wgpu::TextureView),
    /// A dispatch of `groups` workgroups writing this storage view (all six faces of a mip).
    Compute(wgpu::TextureView, [u32; 3]),
}

/// One pass (fragment) or dispatch (compute) of a [`Job`].
struct Step {
    entry: Entry,
    /// Color format (render) or storage format (compute).
    format: wgpu::TextureFormat,
    bind_group: wgpu::BindGroup,
    offset: u32,
    output: Output,
}

/// Steps recorded in order, all at once or a few per frame; consecutive compute steps share one
/// compute pass.
#[derive(Default)]
pub(super) struct Job {
    steps: Vec<Step>,
    next: usize,
}

impl Job {
    /// Steps not recorded yet.
    pub fn remaining(&self) -> u32 {
        (self.steps.len() - self.next) as u32
    }

    /// Every step is recorded.
    pub fn done(&self) -> bool {
        self.next >= self.steps.len()
    }

    /// The next `max` steps (or fewer): how many passes they take and how many steps they are.
    pub fn passes(&self, max: u32) -> (u32, u32) {
        let end = (self.next + max as usize).min(self.steps.len());
        let mut passes = 0;
        let mut compute = false;
        for step in &self.steps[self.next..end] {
            match step.output {
                Output::Render(_) => {
                    passes += 1;
                    compute = false;
                }
                Output::Compute(..) => {
                    if !compute {
                        passes += 1;
                    }
                    compute = true;
                }
            }
        }
        (passes, (end - self.next) as u32)
    }
}

/// A step before its bind group exists: the source (an index into the plan's sources), the
/// parameters and the output.
struct StepPlan {
    entry: Entry,
    format: wgpu::TextureFormat,
    source: usize,
    params: BakeParams,
    output: Output,
}

/// Steps being planned: the distinct source views and the steps reading them.
#[derive(Default)]
pub(super) struct Plan {
    sources: Vec<wgpu::TextureView>,
    steps: Vec<StepPlan>,
}

impl Plan {
    /// Register a source view (a cube view for cube entries, a 2D view for 2D ones).
    pub fn source(&mut self, view: wgpu::TextureView) -> usize {
        self.sources.push(view);
        self.sources.len() - 1
    }

    /// A render pass of `entry` into `target` (of `format`) reading `source`.
    pub fn render(
        &mut self,
        entry: Entry,
        format: wgpu::TextureFormat,
        source: usize,
        params: BakeParams,
        target: wgpu::TextureView,
    ) {
        self.steps.push(StepPlan {
            entry,
            format,
            source,
            params,
            output: Output::Render(target),
        });
    }

    /// Mip `mip` of every face of `target` by `entry` from `source`: one dispatch when compute
    /// steps write the target, else one render pass per face.
    pub fn cube_mip(&mut self, entry: Entry, target: &Target, mip: u32, source: usize, params: BakeParams) {
        if let Some((format, srgb)) = target.storage {
            let groups = (target.size >> mip).max(1).div_ceil(8);
            self.steps.push(StepPlan {
                entry,
                format,
                source,
                params: BakeParams {
                    flags: srgb as u32,
                    ..params
                },
                output: Output::Compute(target.storage_view(mip, format), [groups, groups, 6]),
            });
        } else {
            for face in 0..6 {
                let params = BakeParams { face, ..params };
                self.render(entry, target.format, source, params, target.face_view(face, mip));
            }
        }
    }
}

/// Timestamps of the passes of one frame's lighting work (one `"ibl.bake"` scope: the start of
/// the first pass to the end of the last), and the passes recorded by kind.
pub(super) struct PassTimer<'t> {
    span: Option<(&'t wgpu::QuerySet, u32, u32)>,
    index: u32,
    total: u32,
    pub render_passes: u32,
    pub compute_passes: u32,
}

impl<'t> PassTimer<'t> {
    /// `total`: the passes this frame will record.
    pub fn new(span: Option<(&'t wgpu::QuerySet, u32, u32)>, total: u32) -> Self {
        Self {
            span,
            index: 0,
            total,
            render_passes: 0,
            compute_passes: 0,
        }
    }

    fn next(&mut self) -> Option<(&'t wgpu::QuerySet, Option<u32>, Option<u32>)> {
        let i = self.index;
        self.index += 1;
        let (query_set, begin, end) = self.span?;
        let begin = (i == 0).then_some(begin);
        let end = (i + 1 == self.total).then_some(end);
        (begin.is_some() || end.is_some()).then_some((query_set, begin, end))
    }

    fn render(&mut self) -> Option<wgpu::RenderPassTimestampWrites<'t>> {
        self.render_passes += 1;
        self.next().map(|(query_set, b, e)| wgpu::RenderPassTimestampWrites {
            query_set,
            beginning_of_pass_write_index: b,
            end_of_pass_write_index: e,
        })
    }

    fn compute(&mut self) -> Option<wgpu::ComputePassTimestampWrites<'t>> {
        self.compute_passes += 1;
        self.next().map(|(query_set, b, e)| wgpu::ComputePassTimestampWrites {
            query_set,
            beginning_of_pass_write_index: b,
            end_of_pass_write_index: e,
        })
    }
}

/// Pipelines, layouts and samplers of the bake steps (created with the first cubemap; compute
/// ones with the first compute step).
pub(super) struct Baker {
    fragment: wgpu::ShaderModule,
    cube_bgl: wgpu::BindGroupLayout,
    flat_bgl: wgpu::BindGroupLayout,
    cube_layout: wgpu::PipelineLayout,
    flat_layout: wgpu::PipelineLayout,
    /// Cube sources: linear, clamped (cube sampling is seamless).
    cube_sampler: wgpu::Sampler,
    /// 2D sources (panoramas): repeated across the longitude seam, clamped at the poles.
    flat_sampler: wgpu::Sampler,
    pipelines: FastMap<(Entry, wgpu::TextureFormat), wgpu::RenderPipeline>,
    /// Compute: per storage format, the module and the (cube, 2D) source layouts.
    compute: FastMap<wgpu::TextureFormat, ComputeLayouts>,
    compute_pipelines: FastMap<(Entry, wgpu::TextureFormat), wgpu::ComputePipeline>,
    /// Uniform offset alignment of the device.
    stride: u64,
}

/// The compute module and layouts of one storage format.
struct ComputeLayouts {
    module: wgpu::ShaderModule,
    cube_bgl: wgpu::BindGroupLayout,
    flat_bgl: wgpu::BindGroupLayout,
    cube_layout: wgpu::PipelineLayout,
    flat_layout: wgpu::PipelineLayout,
}

/// Bind group layout of bake steps: params, the source (cube at 1 or 2D at 3), the sampler, and
/// for compute steps the output (4).
fn bake_bgl(
    device: &wgpu::Device,
    flat: bool,
    visibility: wgpu::ShaderStages,
    storage: Option<wgpu::TextureFormat>,
) -> wgpu::BindGroupLayout {
    let mut entries = vec![
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<BakeParams>() as u64),
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: if flat { 3 } else { 1 },
            visibility,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: if flat {
                    wgpu::TextureViewDimension::D2
                } else {
                    wgpu::TextureViewDimension::Cube
                },
                multisampled: false,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
    ];
    if let Some(format) = storage {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 4,
            visibility,
            ty: wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format,
                view_dimension: wgpu::TextureViewDimension::D2Array,
            },
            count: None,
        });
    }
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("space3d ibl bake"),
        entries: &entries,
    })
}

fn pipeline_layout(device: &wgpu::Device, bgl: &wgpu::BindGroupLayout) -> wgpu::PipelineLayout {
    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("space3d ibl bake"),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    })
}

/// WGSL name of a storage format of [`storage_format`].
fn storage_wgsl(format: wgpu::TextureFormat) -> &'static str {
    match format {
        wgpu::TextureFormat::Rgba16Float => "rgba16float",
        wgpu::TextureFormat::Rg11b10Ufloat => "rg11b10ufloat",
        _ => "rgba8unorm",
    }
}

impl Baker {
    pub fn new(device: &wgpu::Device) -> Self {
        let fragment = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("space3d ibl bake"),
            source: wgpu::ShaderSource::Wgsl(
                format!(
                    "{}\n{}\n{}",
                    crate::node::FULLSCREEN_WGSL,
                    include_str!("../shaders/ibl_bake.wgsl"),
                    include_str!("../shaders/ibl_bake_fragment.wgsl")
                )
                .into(),
            ),
        });
        let fs = wgpu::ShaderStages::FRAGMENT;
        let cube_bgl = bake_bgl(device, false, fs, None);
        let flat_bgl = bake_bgl(device, true, fs, None);
        let linear = wgpu::SamplerDescriptor {
            label: Some("space3d ibl bake"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        };
        Self {
            cube_layout: pipeline_layout(device, &cube_bgl),
            flat_layout: pipeline_layout(device, &flat_bgl),
            fragment,
            cube_bgl,
            flat_bgl,
            cube_sampler: device.create_sampler(&linear),
            flat_sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::Repeat,
                ..linear
            }),
            pipelines: FastMap::default(),
            compute: FastMap::default(),
            compute_pipelines: FastMap::default(),
            stride: (device.limits().min_uniform_buffer_offset_alignment as u64).max(64),
        }
    }

    fn compute_layouts(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> &ComputeLayouts {
        self.compute.entry(format).or_insert_with(|| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("space3d ibl bake (compute)"),
                source: wgpu::ShaderSource::Wgsl(
                    format!(
                        "{}\n{}",
                        include_str!("../shaders/ibl_bake.wgsl"),
                        include_str!("../shaders/ibl_bake_compute.wgsl")
                            .replace("S3D_BAKE_FORMAT", storage_wgsl(format))
                    )
                    .into(),
                ),
            });
            let cs = wgpu::ShaderStages::COMPUTE;
            let cube_bgl = bake_bgl(device, false, cs, Some(format));
            let flat_bgl = bake_bgl(device, true, cs, Some(format));
            ComputeLayouts {
                cube_layout: pipeline_layout(device, &cube_bgl),
                flat_layout: pipeline_layout(device, &flat_bgl),
                module,
                cube_bgl,
                flat_bgl,
            }
        })
    }

    /// Turn a plan into a job: one uniform buffer with every step's parameters, one bind group
    /// per source view for render steps and one per compute step (its output view).
    pub fn finish(&mut self, device: &wgpu::Device, plan: Plan) -> Job {
        use wgpu::util::DeviceExt;
        if plan.steps.is_empty() {
            return Job::default();
        }
        let size = std::mem::size_of::<BakeParams>();
        let mut contents = vec![0u8; self.stride as usize * plan.steps.len()];
        for (i, s) in plan.steps.iter().enumerate() {
            let at = i * self.stride as usize;
            contents[at..at + size].copy_from_slice(bytemuck::bytes_of(&s.params));
        }
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("space3d ibl bake params"),
            contents: &contents,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let mut render_groups: Vec<Option<wgpu::BindGroup>> = vec![None; plan.sources.len()];
        let mut steps = Vec::with_capacity(plan.steps.len());
        for (i, s) in plan.steps.into_iter().enumerate() {
            let flat = s.entry.flat();
            if matches!(s.output, Output::Compute(..)) {
                self.compute_layouts(device, s.format);
            }
            let source = &plan.sources[s.source];
            let sampler = if flat { &self.flat_sampler } else { &self.cube_sampler };
            let mut entries = vec![
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: wgpu::BufferSize::new(size as u64),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: if flat { 3 } else { 1 },
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ];
            let bind_group = match &s.output {
                Output::Render(_) => render_groups[s.source]
                    .get_or_insert_with(|| {
                        device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("space3d ibl bake"),
                            layout: if flat { &self.flat_bgl } else { &self.cube_bgl },
                            entries: &entries,
                        })
                    })
                    .clone(),
                Output::Compute(view, _) => {
                    entries.push(wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::TextureView(view),
                    });
                    let layouts = &self.compute[&s.format];
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("space3d ibl bake (compute)"),
                        layout: if flat { &layouts.flat_bgl } else { &layouts.cube_bgl },
                        entries: &entries,
                    })
                }
            };
            steps.push(Step {
                entry: s.entry,
                format: s.format,
                bind_group,
                offset: (i as u64 * self.stride) as u32,
                output: s.output,
            });
        }
        Job { steps, next: 0 }
    }

    fn pipeline(&mut self, device: &wgpu::Device, entry: Entry, format: wgpu::TextureFormat) -> &wgpu::RenderPipeline {
        let (module, cube_layout, flat_layout) = (&self.fragment, &self.cube_layout, &self.flat_layout);
        self.pipelines.entry((entry, format)).or_insert_with(|| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("space3d ibl bake"),
                layout: Some(if entry.flat() { flat_layout } else { cube_layout }),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(entry.fragment()),
                    compilation_options: Default::default(),
                    targets: &[Some(format.into())],
                }),
                multiview_mask: None,
                cache: None,
            })
        })
    }

    fn compute_pipeline(
        &mut self,
        device: &wgpu::Device,
        entry: Entry,
        format: wgpu::TextureFormat,
    ) -> &wgpu::ComputePipeline {
        if !self.compute_pipelines.contains_key(&(entry, format)) {
            let layouts = self.compute_layouts(device, format);
            let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("space3d ibl bake (compute)"),
                layout: Some(if entry.flat() {
                    &layouts.flat_layout
                } else {
                    &layouts.cube_layout
                }),
                module: &layouts.module,
                entry_point: Some(entry.compute()),
                compilation_options: Default::default(),
                cache: None,
            });
            self.compute_pipelines.insert((entry, format), pipeline);
        }
        &self.compute_pipelines[&(entry, format)]
    }

    /// Record up to `max` steps of `job`: a render pass per fragment step, one compute pass
    /// per run of compute steps. Returns the steps recorded.
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        job: &mut Job,
        max: u32,
        timer: &mut PassTimer<'_>,
    ) -> u32 {
        let mut n = 0;
        while n < max && !job.done() {
            if let Output::Render(target) = &job.steps[job.next].output {
                let step = &job.steps[job.next];
                let pipeline = self.pipeline(device, step.entry, step.format);
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("space3d ibl bake"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    timestamp_writes: timer.render(),
                    ..Default::default()
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &step.bind_group, &[step.offset]);
                pass.draw(0..3, 0..1);
                drop(pass);
                job.next += 1;
                n += 1;
                continue;
            }
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("space3d ibl bake"),
                timestamp_writes: timer.compute(),
            });
            while n < max && !job.done() {
                let step = &job.steps[job.next];
                let Output::Compute(_, groups) = step.output else {
                    break;
                };
                pass.set_pipeline(self.compute_pipeline(device, step.entry, step.format));
                pass.set_bind_group(0, &step.bind_group, &[step.offset]);
                pass.dispatch_workgroups(groups[0], groups[1], groups[2]);
                job.next += 1;
                n += 1;
            }
        }
        if job.done() {
            // Release the views, bind groups and parameters of a finished job.
            *job = Job::default();
        }
        n
    }
}

/// Mips of a full chain down to 1×1.
pub(super) fn full_mips(size: u32) -> u32 {
    32 - size.max(1).leading_zeros()
}

/// Mips of a prefiltered specular cube of edge `size`: down to 8² (one below 16).
pub(super) fn specular_mips(size: u32) -> u32 {
    if size >= 16 { full_mips(size) - 3 } else { 1 }
}

/// What an environment bake depends on besides its cubemap: the bake settings and the output
/// format (from the frame's capabilities).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct EnvKey {
    pub specular_size: u32,
    pub irradiance_size: u32,
    pub specular_samples: u32,
    pub irradiance_samples: u32,
    pub format: wgpu::TextureFormat,
}

impl EnvKey {
    /// `hdr`: `Rgba16Float` can be rendered (else the LDR bake into `Rgba8UnormSrgb`).
    pub fn new(ibl: &IblSettings, hdr: bool) -> Self {
        Self {
            specular_size: ibl.specular_size.max(1),
            irradiance_size: ibl.irradiance_size.max(1),
            specular_samples: ibl.specular_samples.max(1),
            irradiance_samples: ibl.irradiance_samples.max(1),
            format: if hdr {
                wgpu::TextureFormat::Rgba16Float
            } else {
                wgpu::TextureFormat::Rgba8UnormSrgb
            },
        }
    }

    /// Edge of the specular cube baked from a cubemap of face `size`.
    pub fn specular_size(&self, size: u32) -> u32 {
        self.specular_size.min(size).max(1)
    }

    /// Steps of a bake from a cubemap of face `size`: a render pass per face and mip (fragment)
    /// or a dispatch per mip (compute).
    pub fn passes(&self, size: u32, compute: bool) -> u32 {
        let per_mip = if compute { 1 } else { 6 };
        per_mip * (specular_mips(self.specular_size(size)) + 1)
    }
}

/// A cubemap's environment: the baked cubes, the bake job and the cache bookkeeping.
pub(super) struct Env {
    pub cubemap: CubemapHandle,
    /// `GpuCubemap::id` of the radiance cube it is baked from.
    pub source: u64,
    pub key: EnvKey,
    /// Unique id (the view bind groups' resource key).
    pub id: u64,
    pub specular: wgpu::TextureView,
    pub irradiance: wgpu::TextureView,
    /// The textures behind the views (tests read them back).
    #[cfg_attr(not(test), allow(dead_code))]
    pub textures: [wgpu::Texture; 2],
    pub specular_mips: u32,
    pub job: Job,
    /// Lighting frame it was last used in.
    pub last_used: u64,
}

impl Env {
    /// Bytes of its specular and irradiance cubes (every mip).
    pub fn bytes(&self) -> u64 {
        self.textures
            .iter()
            .map(|t| {
                texture_bytes(
                    t.format(),
                    (t.width(), t.height()),
                    t.depth_or_array_layers(),
                    t.mip_level_count(),
                )
            })
            .sum()
    }

    /// Plan the bake of `cube` with `key` (textures and every step; nothing recorded yet), by
    /// compute dispatches where `writes` allows, else by fragment passes.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        baker: &mut Baker,
        device: &wgpu::Device,
        cubemap: CubemapHandle,
        cube: &GpuCubemap,
        key: EnvKey,
        writes: Writes,
        id: u64,
        frame: u64,
    ) -> Self {
        let spec_size = key.specular_size(cube.size);
        let spec_mips = specular_mips(spec_size);
        let specular = Target::cube(
            device,
            "space3d environment specular",
            spec_size,
            spec_mips,
            key.format,
            writes,
        );
        let irradiance = Target::cube(
            device,
            "space3d environment irradiance",
            key.irradiance_size,
            1,
            key.format,
            writes,
        );
        let mut plan = Plan::default();
        let src = plan.source(cube.view.clone());
        let base = BakeParams {
            src_size: cube.size as f32,
            src_max_lod: (cube.mips - 1) as f32,
            ..Default::default()
        };
        for m in 0..spec_mips {
            let roughness = if spec_mips > 1 {
                m as f32 / (spec_mips - 1) as f32
            } else {
                0.0
            };
            let params = BakeParams {
                samples: if m == 0 { 1 } else { key.specular_samples },
                roughness,
                // The roughness-0 copy reads the source mip matching the destination.
                lod: (cube.size as f32 / spec_size as f32).log2().max(0.0),
                ..base
            };
            plan.cube_mip(Entry::Prefilter, &specular, m, src, params);
        }
        let params = BakeParams {
            samples: key.irradiance_samples,
            ..base
        };
        plan.cube_mip(Entry::Irradiance, &irradiance, 0, src, params);
        Self {
            cubemap,
            source: cube.id,
            key,
            id,
            specular: specular.cube_view(0..spec_mips),
            irradiance: irradiance.cube_view(0..1),
            textures: [specular.texture, irradiance.texture],
            specular_mips: spec_mips,
            job: baker.finish(device, plan),
            last_used: frame,
        }
    }

    /// Every bake step is recorded (later passes of the same submission may sample it).
    pub fn ready(&self) -> bool {
        self.job.done()
    }

    pub fn state(&self) -> EnvironmentState {
        if self.ready() {
            EnvironmentState::Ready
        } else {
            EnvironmentState::Baking {
                passes_left: self.job.remaining(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regenerate `shaders/brdf_lut.bin` (run on purpose:
    /// `cargo test -p space3d-render --lib regenerate_brdf_lut -- --ignored`).
    #[test]
    #[ignore = "writes src/shaders/brdf_lut.bin"]
    fn regenerate_brdf_lut() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/shaders/brdf_lut.bin");
        std::fs::write(path, brdf_lut_bytes(BRDF_LUT_SAMPLES)).unwrap();
    }

    fn stored(x: usize, y: usize) -> [f32; 2] {
        let at = (y * BRDF_LUT_SIZE as usize + x) * 4;
        let h = |i: usize| {
            crate::instance::f16_to_f32(u16::from_le_bytes([BRDF_LUT_BIN[at + i], BRDF_LUT_BIN[at + i + 1]]))
        };
        [h(0), h(2)]
    }

    /// The embedded LUT is what the generator computes (the file is current) and matches an
    /// independent 4× denser integration within 2e-3 on 32 texels. Analytic limits: a
    /// mirror-smooth surface reflects `1 − (1 − n·v)⁵` of f0 and `(1 − n·v)⁵` of white; no
    /// texel gains energy; rough ones lose some (scale + bias < 1; the corner texel matches a
    /// brute-force quadrature of the BRDF over the hemisphere).
    #[test]
    fn brdf_lut_matches_reference() {
        let size = BRDF_LUT_SIZE as usize;
        assert_eq!(BRDF_LUT_BIN.len(), size * size * 4);
        for i in 0..32u32 {
            // A spread of texels (a fixed walk).
            let x = (i * 37 + 11) % BRDF_LUT_SIZE;
            let y = (i * 53 + 5) % BRDF_LUT_SIZE;
            let lut = stored(x as usize, y as usize);
            let reference = brdf_lut_texel(x, y, 4 * BRDF_LUT_SAMPLES);
            let current = brdf_lut_texel(x, y, BRDF_LUT_SAMPLES);
            for c in 0..2 {
                assert!(
                    (lut[c] - reference[c]).abs() < 2e-3,
                    "texel ({x}, {y}) channel {c}: {} vs {}",
                    lut[c],
                    reference[c]
                );
                assert_eq!(
                    crate::instance::f32_to_f16(current[c]),
                    crate::instance::f32_to_f16(lut[c]),
                    "texel ({x}, {y}) channel {c}: regenerate brdf_lut.bin"
                );
            }
        }
        for x in 0..size {
            let ndv = (x as f32 + 0.5) / size as f32;
            let [scale, bias] = stored(x, 0);
            let fc = (1.0 - ndv).powi(5);
            assert!((scale - (1.0 - fc)).abs() < 4e-3, "smooth scale at n·v {ndv}: {scale}");
            assert!((bias - fc).abs() < 4e-3, "smooth bias at n·v {ndv}: {bias}");
            for y in 0..size {
                let [s, b] = stored(x, y);
                assert!(s >= 0.0 && b >= 0.0 && s + b <= 1.002, "texel ({x}, {y}): {s} + {b}");
            }
            // A rough lobe loses energy to multiple scattering (GGX with α ≈ 1 keeps about a
            // third at normal incidence: 0.3165 by brute-force quadrature).
            let [s, b] = stored(x, size - 1);
            if ndv > 0.5 {
                assert!(s + b < 0.5, "rough surfaces lose single-scattering energy: {}", s + b);
            }
        }
        let [s, b] = stored(size - 1, size - 1);
        assert!((s + b - 0.3165).abs() < 2e-3, "α ≈ 1 at normal incidence: {}", s + b);
    }

    #[test]
    fn mip_counts() {
        assert_eq!((full_mips(1), full_mips(256), full_mips(1000)), (1, 9, 10));
        assert_eq!((specular_mips(256), specular_mips(16), specular_mips(8)), (6, 2, 1));
        let key = EnvKey::new(&IblSettings::default(), true);
        assert_eq!(key.specular_size(1024), 256);
        assert_eq!(key.specular_size(64), 64);
        assert_eq!((key.passes(1024, false), key.passes(1024, true)), (42, 7));
        assert_eq!(key.passes(64, false), 6 * 4 + 6);
    }

    #[test]
    fn radical_inverse_matches_the_shader_bit_trick() {
        let shader = |i: u32| {
            let mut b = i.rotate_left(16);
            b = ((b & 0x5555_5555) << 1) | ((b & 0xAAAA_AAAA) >> 1);
            b = ((b & 0x3333_3333) << 2) | ((b & 0xCCCC_CCCC) >> 2);
            b = ((b & 0x0F0F_0F0F) << 4) | ((b & 0xF0F0_F0F0) >> 4);
            b = ((b & 0x00FF_00FF) << 8) | ((b & 0xFF00_FF00) >> 8);
            b as f32 * 2.328_306_4e-10
        };
        for i in [0, 1, 2, 3, 7, 100, 1023, 65_537, u32::MAX] {
            assert_eq!(radical_inverse(i), shader(i));
        }
        assert_eq!(radical_inverse(1), 0.5);
        assert_eq!(radical_inverse(2), 0.25);
    }
}
