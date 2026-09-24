//! Explicit high-count instancing: user-owned [`InstanceBuffer`]s of custom [`InstanceData`].
//!
//! Use this path for 100k–1M+ instances. Unlike ECS batching there is no per-entity cost:
//! the renderer uploads only [dirty ranges](InstanceBuffer::dirty_ranges) of the buffer and
//! (on capable GPUs) frustum-culls every instance in a compute shader.
//!
//! ```
//! use space3d_render::*;
//! # fn demo(assets: &mut RenderAssets, buffers: &mut InstanceBuffers) {
//! let mesh = assets.add_mesh(space3d_core::MeshData::cube(1.0));
//! let mut buf = InstanceBuffer::new(BufferUsage::Dynamic);
//! for i in 0..10_000 {
//!     buf.push(InstanceCompact::new(glam::Vec3::new(i as f32, 0.0, 0.0), glam::Quat::IDENTITY, 0.5));
//! }
//! let instances = buffers.insert(buf);
//! let _component = InstancedMesh { mesh, material: Material::default(), instances };
//! // Later: buffers.get_mut::<InstanceCompact>(instances).unwrap().set(42, ...) uploads 1 instance.
//! # }
//! ```

use crate::components::Material;
use crate::culling::Aabb;
use crate::dirty::DirtyRanges;
use crate::handles::{InstanceBufferHandle, MeshHandle, SlotMap};
use glam::{Mat4, Quat, Vec3};
use std::any::Any;
use std::ops::Range;
use wgpu::VertexFormat;

/// What an instance attribute means to the built-in forward shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstanceSemantic {
    /// Column-major model matrix: four consecutive `Float32x4` columns starting at `offset`.
    ModelMatrix,
    /// World translation (`xyz` of a 3/4-component float format).
    Translation,
    /// Rotation quaternion `xyzw` (`Float32x4`, `Float16x4`, `Snorm16x4`, `Snorm8x4`); normalized in-shader.
    Rotation,
    /// Non-uniform scale (`xyz`).
    Scale,
    /// Uniform scale (`Float32` / first component).
    UniformScale,
    /// Linear RGBA tint multiplied with the material color.
    Color,
    /// Atlas rectangle: `uv * zw + xy`.
    UvRect,
    /// Texture-array layer (atlas index / animation frame) as `Uint32` or `Float32`.
    TextureLayer,
    /// Free slot for custom pipelines; ignored by the built-in shader.
    Custom(u8),
}

/// One attribute of an [`InstanceLayout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InstanceAttribute {
    /// Meaning of the attribute.
    pub semantic: InstanceSemantic,
    /// Vertex format of the attribute (for `ModelMatrix`: `Float32x4`, 4 columns).
    /// `Unorm8x4Bgra` works on every backend: it is fetched as `Unorm8x4` and swizzled in the
    /// shader (wgpu's GL backend cannot express it).
    pub format: VertexFormat,
    /// Byte offset inside the instance struct.
    pub offset: u32,
}

impl InstanceAttribute {
    /// Convenience constructor.
    pub const fn new(semantic: InstanceSemantic, format: VertexFormat, offset: u32) -> Self {
        Self {
            semantic,
            format,
            offset,
        }
    }
}

/// Static description of an [`InstanceData`] type's memory layout.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct InstanceLayout {
    /// Debug name (also used in shader labels).
    pub name: &'static str,
    /// Size of one instance in bytes (must equal `size_of::<T>()`, multiple of 4).
    pub stride: u32,
    /// Attributes.
    pub attributes: &'static [InstanceAttribute],
}

/// How culling code can find an instance's position/scale in raw memory (f32 formats only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CullSource {
    /// Translation = column 3, scale = [`crate::max_axis_scale`] of columns 0..2.
    Matrix {
        /// Byte offset of the first matrix column.
        offset: u32,
    },
    /// Translation (f32 xyz) plus optional scale.
    TranslationScale {
        /// Byte offset of the translation.
        translation: u32,
        /// Byte offset and encoding of the scale, if any.
        scale: Option<(u32, ScaleKind)>,
    },
    /// Position not decodable; the whole buffer is drawn without per-instance culling.
    None,
}

/// Scale encoding for [`CullSource::TranslationScale`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleKind {
    /// Single f32.
    Uniform,
    /// f32 xyz.
    Vector,
}

impl InstanceLayout {
    /// Check that attributes fit the stride and use formats the built-in shader understands.
    pub fn validate(&self) -> Result<(), String> {
        if self.stride == 0 || !self.stride.is_multiple_of(4) {
            return Err(format!(
                "{}: stride {} must be a non-zero multiple of 4",
                self.name, self.stride
            ));
        }
        let mut transform = false;
        for a in self.attributes {
            let size = if a.semantic == InstanceSemantic::ModelMatrix {
                if a.format != VertexFormat::Float32x4 {
                    return Err(format!("{}: ModelMatrix must use Float32x4 columns", self.name));
                }
                64
            } else {
                a.format.size() as u32
            };
            if a.offset + size > self.stride || !a.offset.is_multiple_of(4) {
                return Err(format!(
                    "{}: attribute {:?} out of bounds/unaligned",
                    self.name, a.semantic
                ));
            }
            if wgsl_type(a.format).is_none() {
                return Err(format!("{}: unsupported vertex format {:?}", self.name, a.format));
            }
            transform |= matches!(
                a.semantic,
                InstanceSemantic::ModelMatrix | InstanceSemantic::Translation
            );
        }
        if !transform {
            return Err(format!("{}: needs a ModelMatrix or Translation attribute", self.name));
        }
        Ok(())
    }

    /// Number of shader locations used.
    pub fn location_count(&self) -> u32 {
        self.attributes
            .iter()
            .map(|a| {
                if a.semantic == InstanceSemantic::ModelMatrix {
                    4
                } else {
                    1
                }
            })
            .sum()
    }

    const fn find(&self, s: InstanceSemantic) -> Option<&InstanceAttribute> {
        let mut i = 0;
        while i < self.attributes.len() {
            if same_semantic(self.attributes[i].semantic, s) {
                return Some(&self.attributes[i]);
            }
            i += 1;
        }
        None
    }

    /// Where position/scale live for culling (GPU and CPU). A `const fn`, so each
    /// [`InstanceData`] type's answer is computed once at compile time.
    pub const fn cull_source(&self) -> CullSource {
        if let Some(m) = self.find(InstanceSemantic::ModelMatrix) {
            return CullSource::Matrix { offset: m.offset };
        }
        let Some(t) = self.find(InstanceSemantic::Translation) else {
            return CullSource::None;
        };
        if !matches!(t.format, VertexFormat::Float32x3 | VertexFormat::Float32x4) {
            return CullSource::None;
        }
        let scale = match (
            self.find(InstanceSemantic::Scale),
            self.find(InstanceSemantic::UniformScale),
        ) {
            (Some(s), _) if matches!(s.format, VertexFormat::Float32x3 | VertexFormat::Float32x4) => {
                Some((s.offset, ScaleKind::Vector))
            }
            (None, Some(s)) if matches!(s.format, VertexFormat::Float32) => Some((s.offset, ScaleKind::Uniform)),
            (None, None) => None,
            // Scale present but not f32: bounds unknown.
            _ => return CullSource::None,
        };
        CullSource::TranslationScale {
            translation: t.offset,
            scale,
        }
    }

    /// Decode (world position, max scale) from one instance's bytes using [`Self::cull_source`].
    pub fn decode_bounds(&self, bytes: &[u8]) -> Option<(Vec3, f32)> {
        decode_bounds_with(self.cull_source(), bytes)
    }
}

/// `a == b` usable in `const fn`.
const fn same_semantic(a: InstanceSemantic, b: InstanceSemantic) -> bool {
    use InstanceSemantic::*;
    match (a, b) {
        (Custom(x), Custom(y)) => x == y,
        (ModelMatrix, ModelMatrix)
        | (Translation, Translation)
        | (Rotation, Rotation)
        | (Scale, Scale)
        | (UniformScale, UniformScale)
        | (Color, Color)
        | (UvRect, UvRect)
        | (TextureLayer, TextureLayer) => true,
        _ => false,
    }
}

/// Decode (position, max scale) with a known [`CullSource`]. With a constant `cull` (see
/// [`InstanceData::bounds`]) this inlines to a few loads at fixed offsets.
#[inline(always)]
fn decode_bounds_with(cull: CullSource, bytes: &[u8]) -> Option<(Vec3, f32)> {
    #[inline(always)]
    fn f(bytes: &[u8], o: u32) -> f32 {
        let o = o as usize;
        f32::from_ne_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
    }
    #[inline(always)]
    fn v3(bytes: &[u8], o: u32) -> Vec3 {
        Vec3::new(f(bytes, o), f(bytes, o + 4), f(bytes, o + 8))
    }
    match cull {
        CullSource::Matrix { offset } => {
            let t = v3(bytes, offset + 48);
            let s = crate::culling::stretch_bound(v3(bytes, offset), v3(bytes, offset + 16), v3(bytes, offset + 32));
            Some((t, s))
        }
        CullSource::TranslationScale { translation, scale } => {
            let s = match scale {
                None => 1.0,
                Some((o, ScaleKind::Uniform)) => f(bytes, o).abs(),
                Some((o, ScaleKind::Vector)) => v3(bytes, o).abs().max_element(),
            };
            Some((v3(bytes, translation), s))
        }
        CullSource::None => None,
    }
}

/// `T::LAYOUT.cull_source()` evaluated at compile time.
struct CullOf<T>(std::marker::PhantomData<T>);

impl<T: InstanceData> CullOf<T> {
    const SOURCE: CullSource = T::LAYOUT.cull_source();
}

/// WGSL type a vertex format is presented as in the shader.
pub(crate) fn wgsl_type(format: VertexFormat) -> Option<&'static str> {
    use VertexFormat::*;
    Some(match format {
        Float32 | Unorm8 | Snorm8 | Unorm16 | Snorm16 | Float16 => "f32",
        Float32x2 | Unorm8x2 | Snorm8x2 | Unorm16x2 | Snorm16x2 | Float16x2 => "vec2<f32>",
        Float32x3 => "vec3<f32>",
        Float32x4 | Unorm8x4 | Snorm8x4 | Unorm16x4 | Snorm16x4 | Float16x4 | Unorm10_10_10_2 | Unorm8x4Bgra => {
            "vec4<f32>"
        }
        Uint8 | Uint16 | Uint32 => "u32",
        Uint8x2 | Uint16x2 | Uint32x2 => "vec2<u32>",
        Uint32x3 => "vec3<u32>",
        Uint8x4 | Uint16x4 | Uint32x4 => "vec4<u32>",
        Sint8 | Sint16 | Sint32 => "i32",
        Sint8x2 | Sint16x2 | Sint32x2 => "vec2<i32>",
        Sint32x3 => "vec3<i32>",
        Sint8x4 | Sint16x4 | Sint32x4 => "vec4<i32>",
        _ => return None,
    })
}

/// Per-instance GPU data. Implement for any `#[repr(C)]` `Pod` struct and describe its layout;
/// built-in semantics are consumed by the forward shader, `Custom` ones are carried along.
pub trait InstanceData: bytemuck::Pod + Send + Sync + 'static {
    /// Memory layout (`const LAYOUT: &'static InstanceLayout = &InstanceLayout { .. };`).
    const LAYOUT: &'static InstanceLayout;

    /// World position and max scale for CPU bounds. The default decodes the attributes named
    /// by [`InstanceLayout::cull_source`] (resolved at compile time, so this is a few loads).
    #[inline]
    fn bounds(&self) -> Option<(Vec3, f32)> {
        decode_bounds_with(CullOf::<Self>::SOURCE, bytemuck::bytes_of(self))
    }
}

/// Bounds of a column-major model matrix: translation and [`crate::max_axis_scale`].
#[inline(always)]
fn matrix_bounds(m: &[[f32; 4]; 4]) -> Option<(Vec3, f32)> {
    let c = |i: usize| Vec3::new(m[i][0], m[i][1], m[i][2]);
    Some((c(3), crate::culling::stretch_bound(c(0), c(1), c(2))))
}

/// Full model matrix (64 B). The format used internally for ECS batching.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceTransform {
    /// Column-major model matrix.
    pub model: [[f32; 4]; 4],
}

impl InstanceTransform {
    /// From a matrix.
    pub fn from_mat4(m: Mat4) -> Self {
        Self {
            model: m.to_cols_array_2d(),
        }
    }
    /// From translation / rotation / scale.
    pub fn from_trs(translation: Vec3, rotation: Quat, scale: Vec3) -> Self {
        Self::from_mat4(Mat4::from_scale_rotation_translation(scale, rotation, translation))
    }
}

impl InstanceData for InstanceTransform {
    const LAYOUT: &'static InstanceLayout = &InstanceLayout {
        name: "InstanceTransform",
        stride: 64,
        attributes: &[InstanceAttribute::new(
            InstanceSemantic::ModelMatrix,
            VertexFormat::Float32x4,
            0,
        )],
    };

    #[inline]
    fn bounds(&self) -> Option<(Vec3, f32)> {
        matrix_bounds(&self.model)
    }
}

/// Model matrix + linear RGBA tint (80 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceTransformColor {
    /// Column-major model matrix.
    pub model: [[f32; 4]; 4],
    /// Linear RGBA tint.
    pub color: [f32; 4],
}

impl InstanceTransformColor {
    /// From a matrix and linear color.
    pub fn new(m: Mat4, color: glam::Vec4) -> Self {
        Self {
            model: m.to_cols_array_2d(),
            color: color.to_array(),
        }
    }
}

impl InstanceData for InstanceTransformColor {
    const LAYOUT: &'static InstanceLayout = &InstanceLayout {
        name: "InstanceTransformColor",
        stride: 80,
        attributes: &[
            InstanceAttribute::new(InstanceSemantic::ModelMatrix, VertexFormat::Float32x4, 0),
            InstanceAttribute::new(InstanceSemantic::Color, VertexFormat::Float32x4, 64),
        ],
    };

    #[inline]
    fn bounds(&self) -> Option<(Vec3, f32)> {
        matrix_bounds(&self.model)
    }
}

/// Compact position + uniform scale + rotation (32 B, half of [`InstanceTransform`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceCompact {
    /// World translation.
    pub translation: [f32; 3],
    /// Uniform scale.
    pub scale: f32,
    /// Rotation quaternion `xyzw`.
    pub rotation: [f32; 4],
}

impl InstanceCompact {
    /// Constructor.
    pub fn new(translation: Vec3, rotation: Quat, scale: f32) -> Self {
        Self {
            translation: translation.to_array(),
            scale,
            rotation: rotation.to_array(),
        }
    }
}

impl InstanceData for InstanceCompact {
    const LAYOUT: &'static InstanceLayout = &InstanceLayout {
        name: "InstanceCompact",
        stride: 32,
        attributes: &[
            InstanceAttribute::new(InstanceSemantic::Translation, VertexFormat::Float32x3, 0),
            InstanceAttribute::new(InstanceSemantic::UniformScale, VertexFormat::Float32, 12),
            InstanceAttribute::new(InstanceSemantic::Rotation, VertexFormat::Float32x4, 16),
        ],
    };

    #[inline]
    fn bounds(&self) -> Option<(Vec3, f32)> {
        Some((Vec3::from_array(self.translation), self.scale.abs()))
    }
}

/// Bandwidth-optimized instance: f32 position/scale, snorm16 quaternion, unorm8 sRGB-free
/// linear tint, u32 texture layer (32 B incl. layer; 2.5x smaller than [`InstanceTransformColor`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstancePacked {
    /// World translation.
    pub translation: [f32; 3],
    /// Uniform scale.
    pub scale: f32,
    /// Rotation quaternion `xyzw` as snorm16.
    pub rotation: [i16; 4],
    /// Linear RGBA tint as unorm8.
    pub color: [u8; 4],
    /// Texture-array layer / atlas frame.
    pub layer: u32,
}

impl InstancePacked {
    /// Pack from full-precision values.
    pub fn new(translation: Vec3, rotation: Quat, scale: f32, color: glam::Vec4, layer: u32) -> Self {
        let q = rotation
            .normalize()
            .to_array()
            .map(|c| (c.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16);
        let c = color.to_array().map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
        Self {
            translation: translation.to_array(),
            scale,
            rotation: q,
            color: c,
            layer,
        }
    }
}

impl InstanceData for InstancePacked {
    const LAYOUT: &'static InstanceLayout = &InstanceLayout {
        name: "InstancePacked",
        stride: 32,
        attributes: &[
            InstanceAttribute::new(InstanceSemantic::Translation, VertexFormat::Float32x3, 0),
            InstanceAttribute::new(InstanceSemantic::UniformScale, VertexFormat::Float32, 12),
            InstanceAttribute::new(InstanceSemantic::Rotation, VertexFormat::Snorm16x4, 16),
            InstanceAttribute::new(InstanceSemantic::Color, VertexFormat::Unorm8x4, 24),
            InstanceAttribute::new(InstanceSemantic::TextureLayer, VertexFormat::Uint32, 28),
        ],
    };

    #[inline]
    fn bounds(&self) -> Option<(Vec3, f32)> {
        Some((Vec3::from_array(self.translation), self.scale.abs()))
    }
}

/// Upload strategy hint for an [`InstanceBuffer`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BufferUsage {
    /// Written once; GPU buffer sized exactly. Later edits still work (dirty ranges).
    Static,
    /// Edited sparsely; only dirty ranges are uploaded. GPU buffer grows 1.5x.
    #[default]
    Dynamic,
    /// Fully rewritten every frame (particles): always uploaded whole, no range bookkeeping.
    Stream,
}

/// Instances per CPU culling block: the CPU path keeps one bounding box per block of an
/// [`InstanceBuffer`] and draws only the blocks in view.
pub(crate) const CULL_BLOCK: usize = 4096;

/// Stale blocks at or above which a refresh runs on all rayon workers (native).
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_BLOCKS: usize = 16;

/// Per-block CPU culling bounds: (position AABB, max instance scale) per [`CULL_BLOCK`]
/// instances. Edits only flag their block; bounds are recomputed lazily when CPU culling asks
/// ([`InstanceBuffers`] on GPU culling paths never pay for them), exactly, and only for the
/// flagged blocks — so a sparse edit or a `swap_remove` costs one block, not the buffer.
struct BlockBounds {
    blocks: Vec<(Aabb, f32)>,
    /// One bit per block: edited since its bounds were computed.
    stale: Vec<u64>,
    any_stale: bool,
    /// Union of all blocks as of the last refresh.
    total: (Aabb, f32),
    /// Instances visited by the last refresh.
    scanned: usize,
}

impl Default for BlockBounds {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            stale: Vec::new(),
            any_stale: false,
            total: (Aabb::EMPTY, 0.0),
            scanned: 0,
        }
    }
}

impl BlockBounds {
    #[inline]
    fn mark(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let (first, last) = (range.start / CULL_BLOCK, (range.end - 1) / CULL_BLOCK);
        if self.stale.len() <= last / 64 {
            self.stale.resize(last / 64 + 1, 0);
        }
        for b in first..=last {
            self.stale[b / 64] |= 1 << (b % 64);
        }
        self.any_stale = true;
    }

    /// The buffer shrank to `len`: drop whole blocks past it and re-tighten the new last one.
    fn truncate(&mut self, len: usize) {
        self.blocks.truncate(len.div_ceil(CULL_BLOCK));
        if len > 0 {
            self.mark(len - 1..len);
        }
    }

    fn is_stale(stale: &[u64], b: usize) -> bool {
        stale.get(b / 64).is_some_and(|w| w & (1 << (b % 64)) != 0)
    }

    /// Recompute the stale blocks of `data`; returns the union of all blocks.
    fn refresh<T: InstanceData>(&mut self, data: &[T]) -> (Aabb, f32) {
        self.scanned = 0;
        let n = data.len().div_ceil(CULL_BLOCK);
        if self.blocks.len() != n {
            // Blocks added since the last refresh were flagged when their instances were.
            self.blocks.resize(n, (Aabb::EMPTY, 0.0));
        }
        if !self.any_stale {
            return self.total;
        }
        let Self { blocks, stale, .. } = self;
        let stale_blocks = (0..n).filter(|&b| Self::is_stale(stale, b)).count();
        let rescan = |b: usize, out: &mut (Aabb, f32), chunk: &[T]| {
            if Self::is_stale(stale, b) {
                *out = scan(chunk);
                chunk.len()
            } else {
                0
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        let scanned = if stale_blocks >= PARALLEL_BLOCKS {
            use rayon::prelude::*;
            blocks
                .par_iter_mut()
                .zip(data.par_chunks(CULL_BLOCK))
                .enumerate()
                .map(|(b, (out, chunk))| rescan(b, out, chunk))
                .sum()
        } else {
            let mut n = 0;
            for (b, (out, chunk)) in blocks.iter_mut().zip(data.chunks(CULL_BLOCK)).enumerate() {
                n += rescan(b, out, chunk);
            }
            n
        };
        #[cfg(target_arch = "wasm32")]
        let scanned = {
            let _ = stale_blocks;
            let mut n = 0;
            for (b, (out, chunk)) in blocks.iter_mut().zip(data.chunks(CULL_BLOCK)).enumerate() {
                n += rescan(b, out, chunk);
            }
            n
        };
        self.scanned = scanned;
        self.stale.fill(0);
        self.any_stale = false;
        let mut total = (Aabb::EMPTY, 0.0f32);
        for (aabb, s) in &self.blocks {
            total.0.union(aabb);
            total.1 = total.1.max(*s);
        }
        self.total = total;
        total
    }
}

/// Exact (position AABB, max scale) of `chunk`; instances without bounds are skipped.
#[inline]
fn scan<T: InstanceData>(chunk: &[T]) -> (Aabb, f32) {
    let mut aabb = Aabb::EMPTY;
    let mut scale = 0.0f32;
    for v in chunk {
        if let Some((p, s)) = v.bounds() {
            aabb.grow(p);
            scale = scale.max(s);
        }
    }
    (aabb, scale)
}

/// User-owned CPU instance array with dirty-range tracking. Register it in
/// [`InstanceBuffers`] and reference it from an [`InstancedMesh`].
pub struct InstanceBuffer<T: InstanceData> {
    data: Vec<T>,
    dirty: DirtyRanges,
    usage: BufferUsage,
    bounds: BlockBounds,
    user_bounds: Option<Aabb>,
}

impl<T: InstanceData> InstanceBuffer<T> {
    /// Empty buffer.
    pub fn new(usage: BufferUsage) -> Self {
        Self::from_vec(Vec::new(), usage)
    }
    /// Empty buffer with capacity.
    pub fn with_capacity(capacity: usize, usage: BufferUsage) -> Self {
        Self::from_vec(Vec::with_capacity(capacity), usage)
    }
    /// Take ownership of existing data (all of it is dirty).
    pub fn from_vec(data: Vec<T>, usage: BufferUsage) -> Self {
        let mut buffer = Self {
            data,
            dirty: DirtyRanges::with_merge_gap(16),
            usage,
            bounds: BlockBounds::default(),
            user_bounds: None,
        };
        buffer.touch(0..buffer.data.len());
        buffer
    }
    /// Record an edit of `range` (upload + bounds).
    #[inline]
    fn touch(&mut self, range: Range<usize>) {
        self.dirty.mark(range.clone());
        self.bounds.mark(range);
    }
    /// Number of instances.
    pub fn len(&self) -> usize {
        self.data.len()
    }
    /// No instances.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    /// Usage hint.
    pub fn usage(&self) -> BufferUsage {
        self.usage
    }
    /// Read-only view.
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }
    /// Instance `i`.
    pub fn get(&self, i: usize) -> Option<&T> {
        self.data.get(i)
    }
    /// Overwrite instance `i` (1 instance worth of upload).
    pub fn set(&mut self, i: usize, value: T) {
        self.data[i] = value;
        self.touch(i..i + 1);
    }
    /// Mutable access to instance `i` (marks it dirty).
    pub fn get_mut(&mut self, i: usize) -> &mut T {
        self.touch(i..i + 1);
        &mut self.data[i]
    }
    /// Mutable access to a range (marks it dirty).
    pub fn range_mut(&mut self, range: Range<usize>) -> &mut [T] {
        self.touch(range.clone());
        &mut self.data[range]
    }
    /// Mutable access to everything (marks all dirty).
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.touch(0..self.data.len());
        &mut self.data
    }
    /// Append one instance.
    pub fn push(&mut self, value: T) {
        let i = self.data.len();
        self.data.push(value);
        self.touch(i..i + 1);
    }
    /// Append many.
    pub fn extend_from_slice(&mut self, values: &[T]) {
        let start = self.data.len();
        self.data.extend_from_slice(values);
        self.touch(start..self.data.len());
    }
    /// Shorten (no upload needed).
    pub fn truncate(&mut self, len: usize) {
        if len < self.data.len() {
            self.data.truncate(len);
            self.dirty.clamp(len);
            self.bounds.truncate(len);
        }
    }
    /// Remove all instances (keeps capacity).
    pub fn clear(&mut self) {
        self.truncate(0);
    }
    /// Swap-remove instance `i` (moves the last instance into `i`: 1 instance of upload, and
    /// on CPU-culled paths one block of bounds recomputation).
    pub fn swap_remove(&mut self, i: usize) -> T {
        let last = self.data.len() - 1;
        let v = self.data.swap_remove(i);
        self.dirty.clamp(last);
        if i < last {
            self.touch(i..i + 1);
        }
        // The last block only lost an instance: its bounds stay a valid (conservative) box
        // until it is edited again or dropped.
        self.bounds.blocks.truncate(last.div_ceil(CULL_BLOCK));
        v
    }
    /// Replace all contents.
    pub fn replace(&mut self, data: Vec<T>) {
        self.data = data;
        self.dirty.clear();
        self.touch(0..self.data.len());
    }
    /// Override the automatic bounds used for CPU culling with one box for the whole buffer.
    /// `bounds` is a world-space box containing every instance's *position*; the renderer
    /// grows it by the mesh's bounding radius at scale 1, so include any extra extent of
    /// instances scaled above 1. `None` returns to automatic (per-block) bounds; edits made
    /// meanwhile are still tracked.
    pub fn set_bounds(&mut self, bounds: Option<Aabb>) {
        self.user_bounds = bounds;
    }
    /// Pending dirty ranges (instance units).
    pub fn dirty_ranges(&self) -> &[Range<usize>] {
        self.dirty.ranges()
    }
}

/// CPU culling bounds of an instance buffer.
pub(crate) enum CullBounds<'a> {
    /// [`InstanceBuffer::set_bounds`] override: one box for the whole buffer, scale 1.
    User(Aabb),
    /// Automatic: the union of all blocks, and one entry per [`CULL_BLOCK`] instances.
    /// Empty boxes mean "unknown" (no decodable positions).
    Blocks {
        total: (Aabb, f32),
        blocks: &'a [(Aabb, f32)],
    },
}

/// Type-erased access used by the renderer.
pub(crate) trait ErasedInstanceBuffer: Send + Sync {
    fn layout(&self) -> &'static InstanceLayout;
    fn bytes(&self) -> &[u8];
    fn len(&self) -> usize;
    fn usage(&self) -> BufferUsage;
    fn dirty(&self) -> &DirtyRanges;
    fn clear_dirty(&mut self);
    /// Bring the CPU culling bounds up to date (recomputing only blocks edited since the
    /// last call) and return them.
    fn cull_bounds(&mut self) -> CullBounds<'_>;
    /// Instances visited by the last bounds refresh.
    #[cfg_attr(not(test), allow(dead_code))]
    fn bounds_scanned(&self) -> usize;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: InstanceData> ErasedInstanceBuffer for InstanceBuffer<T> {
    fn layout(&self) -> &'static InstanceLayout {
        T::LAYOUT
    }
    fn bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.data)
    }
    fn len(&self) -> usize {
        self.data.len()
    }
    fn usage(&self) -> BufferUsage {
        self.usage
    }
    fn dirty(&self) -> &DirtyRanges {
        &self.dirty
    }
    fn clear_dirty(&mut self) {
        self.dirty.clear();
    }
    fn cull_bounds(&mut self) -> CullBounds<'_> {
        if let Some(b) = self.user_bounds {
            return CullBounds::User(b);
        }
        let total = self.bounds.refresh(&self.data);
        CullBounds::Blocks {
            total,
            blocks: &self.bounds.blocks,
        }
    }
    fn bounds_scanned(&self) -> usize {
        self.bounds.scanned
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Registry (resource) of user instance buffers, addressed by [`InstanceBufferHandle`].
#[derive(Default)]
pub struct InstanceBuffers {
    pub(crate) buffers: SlotMap<Box<dyn ErasedInstanceBuffer>>,
    pub(crate) removed: Vec<InstanceBufferHandle>,
}

impl InstanceBuffers {
    /// Register a buffer. Panics if `T::LAYOUT` is invalid (see [`InstanceLayout::validate`]).
    pub fn insert<T: InstanceData>(&mut self, buffer: InstanceBuffer<T>) -> InstanceBufferHandle {
        if let Err(e) = T::LAYOUT.validate() {
            panic!("invalid instance layout: {e}");
        }
        assert_eq!(
            T::LAYOUT.stride as usize,
            std::mem::size_of::<T>(),
            "{}: stride must equal size_of",
            T::LAYOUT.name
        );
        InstanceBufferHandle::from_raw(self.buffers.insert(Box::new(buffer)))
    }
    /// Typed read access (None if the handle is stale or `T` mismatches).
    pub fn get<T: InstanceData>(&self, h: InstanceBufferHandle) -> Option<&InstanceBuffer<T>> {
        self.buffers.get(h.raw())?.as_any().downcast_ref()
    }
    /// Typed write access (None if the handle is stale or `T` mismatches).
    pub fn get_mut<T: InstanceData>(&mut self, h: InstanceBufferHandle) -> Option<&mut InstanceBuffer<T>> {
        self.buffers.get_mut(h.raw())?.as_any_mut().downcast_mut()
    }
    /// Remove a buffer (its GPU memory is freed next frame).
    pub fn remove(&mut self, h: InstanceBufferHandle) -> bool {
        let ok = self.buffers.remove(h.raw()).is_some();
        if ok {
            self.removed.push(h);
        }
        ok
    }
    /// Number of registered buffers.
    pub fn len(&self) -> usize {
        self.buffers.len()
    }
    /// No buffers registered.
    pub fn is_empty(&self) -> bool {
        self.buffers.len() == 0
    }
}

/// Component: draw every instance of `instances` with `mesh` + `material` (bypasses the
/// per-entity ECS path). Instance data is in world space.
#[derive(Clone, Debug, PartialEq)]
pub struct InstancedMesh {
    /// Mesh to instance.
    pub mesh: MeshHandle,
    /// Shared material (per-instance tint via [`InstanceSemantic::Color`]).
    pub material: Material,
    /// Instance buffer in [`InstanceBuffers`].
    pub instances: InstanceBufferHandle,
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn builtin_layouts_are_valid() {
        for l in [
            InstanceTransform::LAYOUT,
            InstanceTransformColor::LAYOUT,
            InstanceCompact::LAYOUT,
            InstancePacked::LAYOUT,
        ] {
            l.validate().unwrap();
            assert_ne!(l.cull_source(), CullSource::None, "{}", l.name);
        }
        assert_eq!(std::mem::size_of::<InstancePacked>(), 32);
        assert_eq!(InstanceTransformColor::LAYOUT.location_count(), 5);
    }

    #[test]
    fn decode_bounds_matches_values() {
        let t = InstanceTransform::from_trs(
            Vec3::new(1.0, 2.0, 3.0),
            Quat::from_rotation_y(1.0),
            Vec3::new(1.0, 4.0, 2.0),
        );
        let (p, s) = t.bounds().unwrap();
        assert!((p - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-5);
        assert!((s - 4.0).abs() < 1e-5);
        let c = InstanceCompact::new(Vec3::new(-1.0, 0.0, 5.0), Quat::IDENTITY, 2.5);
        assert_eq!(c.bounds(), Some((Vec3::new(-1.0, 0.0, 5.0), 2.5)));
    }

    fn totals(b: &mut dyn ErasedInstanceBuffer) -> (Aabb, f32) {
        match b.cull_bounds() {
            CullBounds::User(aabb) => (aabb, 1.0),
            CullBounds::Blocks { total, .. } => total,
        }
    }

    #[test]
    fn dirty_tracking_and_bounds() {
        let mut b = InstanceBuffer::from_vec(
            (0..100)
                .map(|i| InstanceCompact::new(Vec3::splat(i as f32), Quat::IDENTITY, 1.0))
                .collect(),
            BufferUsage::Dynamic,
        );
        assert_eq!(b.dirty_ranges(), &[0..100]);
        let (aabb, s) = totals(&mut b);
        assert_eq!((aabb.min, aabb.max, s), (Vec3::ZERO, Vec3::splat(99.0), 1.0));
        b.clear_dirty();
        b.set(5, InstanceCompact::new(Vec3::splat(500.0), Quat::IDENTITY, 3.0));
        assert_eq!(b.dirty_ranges(), &[5..6]);
        let (aabb, s) = totals(&mut b);
        assert_eq!((aabb.max, s), (Vec3::splat(500.0), 3.0));
        b.clear_dirty();
        b.swap_remove(0);
        assert_eq!(b.dirty_ranges(), &[0..1]);
        assert_eq!(b.len(), 99);
        // Bounds are exact per block: overwriting the far instance shrinks them again.
        b.set(5, InstanceCompact::new(Vec3::splat(5.0), Quat::IDENTITY, 1.0));
        let (aabb, s) = totals(&mut b);
        assert_eq!((aabb.min, aabb.max, s), (Vec3::splat(1.0), Vec3::splat(99.0), 1.0));
    }

    /// Regression: a single `swap_remove` / `truncate` invalidated the bounds, so the next
    /// frame rescanned every instance (through a per-instance layout search): ~7 ms per frame
    /// for one removal from a 1M buffer, even on GPU paths that never read the bounds. Only
    /// edited blocks are recomputed now, and only when CPU culling asks for them.
    #[test]
    fn bounds_refresh_only_edited_blocks() {
        let n = 100_000;
        let mut b = InstanceBuffer::from_vec(
            (0..n)
                .map(|i| InstanceCompact::new(Vec3::new(i as f32, 0.0, 0.0), Quat::IDENTITY, 1.0))
                .collect(),
            BufferUsage::Dynamic,
        );
        let scanned = |b: &mut InstanceBuffer<InstanceCompact>| {
            let _ = b.cull_bounds();
            b.bounds_scanned()
        };
        assert_eq!(scanned(&mut b), n, "first refresh scans everything");
        assert_eq!(scanned(&mut b), 0, "nothing changed");
        b.set(50_000, InstanceCompact::new(Vec3::ZERO, Quat::IDENTITY, 1.0));
        assert_eq!(scanned(&mut b), CULL_BLOCK);
        b.swap_remove(10);
        assert_eq!(scanned(&mut b), CULL_BLOCK, "the block that received the last instance");
        for _ in 0..100 {
            b.swap_remove(0);
        }
        assert_eq!(scanned(&mut b), CULL_BLOCK);
        b.truncate(10_000);
        assert!(scanned(&mut b) <= CULL_BLOCK);
        b.as_mut_slice();
        assert_eq!(scanned(&mut b), 10_000);
        // Edits keep being tracked without anybody asking for bounds (GPU culling paths).
        for i in 0..10 {
            b.set(i * 1000, InstanceCompact::new(Vec3::splat(-7.0), Quat::IDENTITY, 2.0));
        }
        let (aabb, s) = totals(&mut b);
        assert_eq!((aabb.min, s), (Vec3::splat(-7.0), 2.0));
        assert_eq!(b.bounds_scanned(), 10_000, "the three blocks that were edited");
    }

    /// Every instance lies in its block's box, and the boxes are exact after any edit mix.
    #[test]
    fn block_bounds_contain_their_instances() {
        let mut seed = 7u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 24) as f32 * 200.0 - 100.0
        };
        let mut b = InstanceBuffer::<InstanceTransform>::new(BufferUsage::Dynamic);
        for _ in 0..3 * CULL_BLOCK + 17 {
            b.push(InstanceTransform::from_trs(
                Vec3::new(rnd(), rnd(), rnd()),
                Quat::IDENTITY,
                Vec3::ONE,
            ));
        }
        for round in 0..6 {
            let _ = b.cull_bounds();
            for k in 0..50 {
                let i = (rnd().abs() * 100.0) as usize % b.len();
                match (round + k) % 4 {
                    0 => b.set(
                        i,
                        InstanceTransform::from_trs(Vec3::splat(rnd()), Quat::IDENTITY, Vec3::splat(2.0)),
                    ),
                    1 => {
                        b.swap_remove(i);
                    }
                    2 => b.push(InstanceTransform::from_trs(
                        Vec3::splat(rnd()),
                        Quat::IDENTITY,
                        Vec3::ONE,
                    )),
                    _ => b.get_mut(i).model[3][1] = rnd(),
                }
            }
            if round == 3 {
                b.truncate(2 * CULL_BLOCK + 5);
            }
            let data: Vec<_> = b.as_slice().to_vec();
            let CullBounds::Blocks { total, blocks } = b.cull_bounds() else {
                unreachable!()
            };
            assert_eq!(blocks.len(), data.len().div_ceil(CULL_BLOCK));
            for (k, chunk) in data.chunks(CULL_BLOCK).enumerate() {
                let (aabb, s) = scan(chunk);
                // Conservative, and exact except where only removals happened.
                let (got, gs) = blocks[k];
                assert!(
                    got.min.cmple(aabb.min).all() && got.max.cmpge(aabb.max).all(),
                    "block {k}"
                );
                assert!(gs >= s);
            }
            for (aabb, _) in blocks {
                assert!(total.0.min.cmple(aabb.min).all() && total.0.max.cmpge(aabb.max).all());
            }
        }
    }

    #[test]
    fn builtin_bounds_match_the_layout_decoder() {
        const _: () = assert!(matches!(
            InstanceCompact::LAYOUT.cull_source(),
            CullSource::TranslationScale {
                translation: 0,
                scale: Some((12, ScaleKind::Uniform))
            }
        ));
        let m = Mat4::from_cols_array(&std::array::from_fn(|i| i as f32 * 0.37 - 2.0));
        let t = InstanceTransform::from_mat4(m);
        let tc = InstanceTransformColor::new(m, glam::Vec4::ONE);
        let c = InstanceCompact::new(Vec3::new(1.0, -2.0, 3.0), Quat::IDENTITY, -1.5);
        let p = InstancePacked::new(Vec3::new(4.0, 5.0, 6.0), Quat::IDENTITY, 0.5, glam::Vec4::ONE, 3);
        assert_eq!(
            t.bounds(),
            InstanceTransform::LAYOUT.decode_bounds(bytemuck::bytes_of(&t))
        );
        assert_eq!(
            tc.bounds(),
            InstanceTransformColor::LAYOUT.decode_bounds(bytemuck::bytes_of(&tc))
        );
        assert_eq!(
            c.bounds(),
            InstanceCompact::LAYOUT.decode_bounds(bytemuck::bytes_of(&c))
        );
        assert_eq!(p.bounds(), InstancePacked::LAYOUT.decode_bounds(bytemuck::bytes_of(&p)));
        assert_eq!(c.bounds(), Some((Vec3::new(1.0, -2.0, 3.0), 1.5)));

        /// A user type relying on the default `bounds` (vector scale at a non-zero offset).
        #[repr(C)]
        #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
        struct Custom {
            color: [f32; 4],
            scale: [f32; 3],
            pos: [f32; 3],
        }
        impl InstanceData for Custom {
            const LAYOUT: &'static InstanceLayout = &InstanceLayout {
                name: "Custom",
                stride: 40,
                attributes: &[
                    InstanceAttribute::new(InstanceSemantic::Color, VertexFormat::Float32x4, 0),
                    InstanceAttribute::new(InstanceSemantic::Scale, VertexFormat::Float32x3, 16),
                    InstanceAttribute::new(InstanceSemantic::Translation, VertexFormat::Float32x3, 28),
                ],
            };
        }
        let u = Custom {
            color: [0.0; 4],
            scale: [1.0, -4.0, 2.0],
            pos: [7.0, 8.0, 9.0],
        };
        assert_eq!(u.bounds(), Some((Vec3::new(7.0, 8.0, 9.0), 4.0)));
    }

    #[test]
    fn user_bounds_override_and_lifting() {
        let mut b = InstanceBuffer::from_vec(
            (0..100)
                .map(|i| InstanceCompact::new(Vec3::new(i as f32 * 0.01, 0.0, 0.0), Quat::IDENTITY, 1.0))
                .collect(),
            BufferUsage::Dynamic,
        );
        let _ = b.cull_bounds();
        b.clear_dirty();
        let user = Aabb::new(Vec3::splat(-1e5), Vec3::splat(1e5));
        b.set_bounds(Some(user));
        for round in 0..10 {
            for i in round * 10..round * 10 + 10 {
                b.set(
                    i,
                    InstanceCompact::new(Vec3::new(1000.0, 0.0, 0.0), Quat::IDENTITY, 2.0),
                );
            }
            assert_eq!(totals(&mut b), (user, 1.0));
            b.clear_dirty();
        }
        b.set_bounds(None);
        let (aabb, s) = totals(&mut b);
        assert_eq!(
            (aabb.min, aabb.max, s),
            (Vec3::new(1000.0, 0.0, 0.0), Vec3::new(1000.0, 0.0, 0.0), 2.0)
        );
    }

    #[test]
    fn registry_is_typed() {
        let mut r = InstanceBuffers::default();
        let h = r.insert(InstanceBuffer::<InstanceTransform>::new(BufferUsage::Static));
        assert!(r.get::<InstanceTransform>(h).is_some());
        assert!(r.get::<InstanceCompact>(h).is_none());
        assert!(r.remove(h));
        assert!(r.get::<InstanceTransform>(h).is_none());
        assert_eq!(r.removed, vec![h]);
    }
}
