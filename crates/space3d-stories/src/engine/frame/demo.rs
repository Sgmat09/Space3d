//! Render nodes and overlays of the frame stories, written against the public API only (like a
//! game would): colored bands in every node phase, soft particles reading scene depth, and an
//! additive sprite overlay in both stages (the `client/src/game/additive.rs` pattern, before
//! and after its migration to `OverlayStage::HdrScene`).

use space3d_render::wgpu;
use space3d_render::{
    DrawContext, FULLSCREEN_WGSL, FrameMode, OUTPUT_WGSL, OutputEncoding, OverlayScene, OverlayStage, OverlayTarget,
    PassTarget, PhaseSlot, PrepareContext, RenderNode, RenderOverlay, RenderPhase, RenderTarget, SceneReads, SlotMask,
    Tonemapper, VIEW_WGSL, ViewPrepareContext, scene_wgsl,
};

/// Group 0 (view) and group 1 (scene) layouts of the renderer a node draws for, captured in
/// `prepare` (a new renderer has new ones: rebuild).
#[derive(Default)]
struct Layouts {
    renderer: u64,
    layouts: Option<(wgpu::BindGroupLayout, wgpu::BindGroupLayout)>,
}

impl Layouts {
    /// Capture `ctx`'s layouts; `true` when the renderer changed (drop GPU state).
    fn update(&mut self, ctx: &PrepareContext<'_>) -> bool {
        if ctx.frame.renderer == self.renderer {
            return false;
        }
        self.renderer = ctx.frame.renderer;
        self.layouts = Some((ctx.layouts.view.clone(), ctx.layouts.scene.clone()));
        true
    }
}

/// A render pipeline with groups 0 and 1 of `layouts`, entry points `vs` and `fs`.
fn pipeline(
    device: &wgpu::Device,
    layouts: &(wgpu::BindGroupLayout, wgpu::BindGroupLayout),
    src: String,
    target: &PassTarget,
    blend: Option<wgpu::BlendState>,
    depth: Option<wgpu::DepthStencilState>,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("story node"),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("story node"),
        bind_group_layouts: &[Some(&layouts.0), Some(&layouts.1)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("story node"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: depth,
        multisample: target.multisample(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(target.color_target(blend))],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// How a [`Band`] draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum BandKind {
    /// A flat rectangle over everything drawn before it (alpha-blended when `color.w < 1`).
    Flat,
    /// A flat rectangle at the far end of the depth range (writes depth: everything nearer
    /// covers it, the sky does not).
    Backdrop,
    /// A vertical gradient at the far plane: only where nothing was drawn (a skybox).
    Sky,
    /// Haze growing with the scene's view depth over the rectangle (reads scene depth; no
    /// haze without it).
    Haze,
}

/// One slot of [`PhaseBands`]: a rectangle `[x0, y0, x1, y1]` in normalized device coordinates.
#[derive(Clone, Copy, Debug)]
pub(super) struct Band {
    pub slot: PhaseSlot,
    pub kind: BandKind,
    pub rect: [f32; 4],
    pub color: [f32; 4],
}

/// Colored bands in every phase and order (the "Node Phases" story).
pub(super) struct PhaseBands {
    slots: Vec<PhaseSlot>,
    bands: Vec<Band>,
    layouts: Layouts,
    pipelines: Vec<((usize, PassTarget), wgpu::RenderPipeline)>,
}

impl PhaseBands {
    pub fn new(bands: Vec<Band>) -> Self {
        Self {
            slots: bands.iter().map(|b| b.slot).collect(),
            bands,
            layouts: Layouts::default(),
            pipelines: Vec::new(),
        }
    }

    fn build(&mut self, device: &wgpu::Device, slot: usize, target: PassTarget) {
        let Some(layouts) = &self.layouts.layouts else { return };
        if self.pipelines.iter().any(|(k, _)| *k == (slot, target)) {
            return;
        }
        let band = self.bands[slot];
        let [x0, y0, x1, y1] = band.rect;
        let [r, g, b, a] = band.color;
        let z = match band.kind {
            BandKind::Sky => 1.0f32,
            BandKind::Backdrop => 0.9999,
            _ => 0.0,
        };
        let color = match band.kind {
            BandKind::Flat | BandKind::Backdrop => format!("vec4<f32>({r:?}, {g:?}, {b:?}, {a:?})"),
            // Top color → bottom color over the band.
            BandKind::Sky => {
                format!("vec4<f32>(mix(vec3<f32>({r:?}, {g:?}, {b:?}), vec3<f32>(0.01, 0.01, 0.03), in.t), 1.0)")
            }
            // Haze from 4 to 12 units away (none without scene depth).
            BandKind::Haze => format!(
                "vec4<f32>({r:?}, {g:?}, {b:?}, {a:?} * select(0.0, clamp((s3d_view_depth_at(vec2<i32>(in.pos.xy)) - 4.0) / 8.0, 0.0, 1.0), s3d_depth_valid()))"
            ),
        };
        let src = format!(
            "{VIEW_WGSL}\n{}\n{OUTPUT_WGSL}\n
            struct Out {{ @builtin(position) pos: vec4<f32>, @location(0) t: f32 }}
            @vertex fn vs(@builtin(vertex_index) i: u32) -> Out {{
                var corners = array<vec2<f32>, 6>(vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
                                                  vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0));
                let c = corners[i];
                var out: Out;
                out.pos = vec4<f32>(mix(vec2<f32>({x0:?}, {y0:?}), vec2<f32>({x1:?}, {y1:?}), c), {z:?}, 1.0);
                out.t = 1.0 - c.y;
                return out;
            }}
            @fragment fn fs(in: Out) -> @location(0) vec4<f32> {{
                return s3d_output({color}, {}u, view.exposure.x, view.output.y);
            }}",
            scene_wgsl(&target),
            target.output.id()
        );
        let blend = (band.kind == BandKind::Haze || a < 1.0).then_some(wgpu::BlendState::ALPHA_BLENDING);
        let compare = if band.kind == BandKind::Sky {
            wgpu::CompareFunction::LessEqual
        } else {
            wgpu::CompareFunction::Always
        };
        // Only Opaque work may write depth.
        let write = band.kind == BandKind::Backdrop && band.slot.phase == RenderPhase::Opaque;
        let p = pipeline(
            device,
            layouts,
            src,
            &target,
            blend,
            target.depth_stencil(compare, write),
        );
        self.pipelines.push(((slot, target), p));
    }
}

impl RenderNode for PhaseBands {
    fn slots(&self) -> &[PhaseSlot] {
        &self.slots
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        if self.layouts.update(ctx) {
            self.pipelines.clear();
        }
    }

    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        for &(slot, target) in targets {
            self.build(device, slot, target);
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        self.build(ctx.device, ctx.slot, ctx.target);
        let Some((_, p)) = self.pipelines.iter().find(|(k, _)| *k == (ctx.slot, ctx.target)) else {
            return;
        };
        pass.set_pipeline(p);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
}

/// A soft particle: world position, radius, linear color (premultiplied, additive) and
/// softness (world units; 0 = hard intersection).
#[derive(Clone, Copy, Debug)]
pub(super) struct Particle {
    pub position: [f32; 3],
    pub radius: f32,
    pub color: [f32; 3],
    pub softness: f32,
}

static SOFT_SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Transparent, 0).reading(SceneReads::DEPTH)];

/// Camera-facing additive particles in one depth-reading Transparent slot (the "Soft Depth"
/// story), in the views of one render target: occluded by the scene (hardware depth test where
/// depth is attached, manual test otherwise) and faded where they meet it.
pub(super) struct SoftParticles {
    particles: Vec<Particle>,
    target: RenderTarget,
    layouts: Layouts,
    pipelines: Vec<(PassTarget, wgpu::RenderPipeline)>,
}

impl SoftParticles {
    pub fn new(particles: Vec<Particle>, target: RenderTarget) -> Self {
        Self {
            particles,
            target,
            layouts: Layouts::default(),
            pipelines: Vec::new(),
        }
    }

    fn build(&mut self, device: &wgpu::Device, target: PassTarget) {
        let Some(layouts) = &self.layouts.layouts else { return };
        if self.pipelines.iter().any(|(t, _)| *t == target) {
            return;
        }
        let n = self.particles.len();
        let list = |f: &dyn Fn(&Particle) -> String| self.particles.iter().map(f).collect::<Vec<_>>().join(", ");
        let centers = list(&|p| {
            let [x, y, z] = p.position;
            format!("vec4<f32>({x:?}, {y:?}, {z:?}, {:?})", p.radius)
        });
        let colors = list(&|p| {
            let [r, g, b] = p.color;
            format!("vec4<f32>({r:?}, {g:?}, {b:?}, {:?})", p.softness)
        });
        let src = format!(
            "{VIEW_WGSL}\n{}\n{OUTPUT_WGSL}\n
            var<private> CENTERS: array<vec4<f32>, {n}> = array<vec4<f32>, {n}>({centers});
            var<private> COLORS: array<vec4<f32>, {n}> = array<vec4<f32>, {n}>({colors});
            struct Out {{
                @builtin(position) pos: vec4<f32>,
                @location(0) uv: vec2<f32>,
                @location(1) color: vec4<f32>,
                @location(2) view_depth: f32,
            }}
            @vertex fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> Out {{
                var corners = array<vec2<f32>, 6>(vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
                                                  vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
                let c = corners[v];
                let p = CENTERS[i];
                // Camera right and up: the rows of the view matrix's rotation.
                let right = vec3<f32>(view.view[0].x, view.view[1].x, view.view[2].x);
                let up = vec3<f32>(view.view[0].y, view.view[1].y, view.view[2].y);
                let world = p.xyz + (right * c.x + up * c.y) * p.w;
                var out: Out;
                out.pos = view.view_proj * vec4<f32>(world, 1.0);
                out.uv = c;
                out.color = COLORS[i];
                out.view_depth = -(view.view * vec4<f32>(world, 1.0)).z;
                return out;
            }}
            @fragment fn fs(in: Out) -> @location(0) vec4<f32> {{
                let px = vec2<i32>(in.pos.xy);
                // Manual depth test where depth is sampled but not attached (WebGL2).
                if (s3d_scene_occluded(px, in.pos.z)) {{ discard; }}
                let d = dot(in.uv, in.uv);
                let falloff = max(1.0 - d, 0.0);
                let fade = s3d_soft_fade(px, in.view_depth, in.color.w);
                let c = in.color.rgb * falloff * falloff * fade;
                return s3d_output(vec4<f32>(c, 0.0), {}u, view.exposure.x, view.output.y);
            }}",
            scene_wgsl(&target),
            target.output.id()
        );
        let add = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let keep = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let blend = Some(wgpu::BlendState {
            color: add,
            alpha: keep,
        });
        let depth = target.depth_stencil(wgpu::CompareFunction::LessEqual, false);
        let p = pipeline(device, layouts, src, &target, blend, depth);
        self.pipelines.push((target, p));
    }
}

impl RenderNode for SoftParticles {
    fn slots(&self) -> &[PhaseSlot] {
        &SOFT_SLOTS
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        if self.layouts.update(ctx) {
            self.pipelines.clear();
        }
    }

    fn prepare_view(&mut self, ctx: &mut ViewPrepareContext<'_>) -> SlotMask {
        if self.particles.is_empty() || ctx.view.target != self.target {
            SlotMask::NONE
        } else {
            SlotMask::ALL
        }
    }

    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        for &(_, target) in targets {
            self.build(device, target);
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        self.build(ctx.device, ctx.target);
        let Some((_, p)) = self.pipelines.iter().find(|(t, _)| *t == ctx.target) else {
            return;
        };
        pass.set_pipeline(p);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        pass.draw(0..6, 0..self.particles.len() as u32);
    }
}

/// An additive sprite: center and radius as fractions of the target height (x of its width),
/// linear color (values above 1 are HDR).
#[derive(Clone, Copy, Debug)]
pub(super) struct Sprite {
    pub center: [f32; 2],
    pub radius: f32,
    pub color: [f32; 3],
}

const MAX_SPRITES: usize = 16;

/// Matches `Params` of the sprite shader.
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    /// Target width, height, 1/width, 1/height.
    target: [f32; 4],
    /// Encoding id, tonemapper id, soft clip (0/1), sprite count.
    mode: [u32; 4],
    /// Linear exposure (Tonemapped encoding).
    exposure: [f32; 4],
    /// Per sprite: center (px), radius (px), 0; then color.
    sprites: [[f32; 4]; 2 * MAX_SPRITES],
}

const SPRITE_WGSL: &str = "
struct Params { size: vec4<f32>, mode: vec4<u32>, exposure: vec4<f32>, sprites: array<vec4<f32>, 32> }
@group(0) @binding(0) var<uniform> params: Params;
struct Out { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) color: vec3<f32> }
@vertex fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> Out {
    var corners = array<vec2<f32>, 6>(vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
                                      vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    let c = corners[v];
    let s = params.sprites[i * 2u];
    let px = s.xy + c * s.z;
    var out: Out;
    out.pos = vec4<f32>(px.x * params.size.z * 2.0 - 1.0, 1.0 - px.y * params.size.w * 2.0, 0.0, 1.0);
    out.uv = c;
    out.color = params.sprites[i * 2u + 1u].rgb;
    return out;
}
@fragment fn fs(in: Out) -> @location(0) vec4<f32> {
    let falloff = max(1.0 - dot(in.uv, in.uv), 0.0);
    var c = in.color * falloff * falloff;
    // LDR-era overlays squeeze values above 1 into the 8-bit target themselves.
    if (params.mode.z == 1u) { c = 1.0 - exp(-c); }
    // Premultiplied additive: alpha 0 leaves the target's alpha alone.
    return s3d_output(vec4<f32>(c, 0.0), params.mode.x, params.exposure.x, params.mode.y);
}
";

struct SpriteGpu {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
}

/// Additive, premultiplied sprites drawn as an overlay (the game's `additive.rs` pattern).
///
/// With `migrated = false` it is 249c874-era code: `AfterTonemap`, encodes sRGB itself when the
/// target format is not sRGB, and soft-clips values above 1 (`1 − e^−x`). With `migrated = true`
/// it applies the four migration steps of `docs/graphics/frame.md`: (1) `stage()` returns
/// `HdrScene`; (2) the encoding comes from `OverlayScene::output`; (3) the soft clip only runs in
/// Direct mode; (4) its pipeline cache (per target format) takes `Rgba16Float` too.
pub(super) struct AdditiveSprites {
    migrated: bool,
    sprites: Vec<Sprite>,
    scene: Option<(OutputEncoding, f32, Tonemapper, FrameMode)>,
    gpu: Option<SpriteGpu>,
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    current: Option<usize>,
}

impl AdditiveSprites {
    pub fn new(migrated: bool, sprites: Vec<Sprite>) -> Self {
        Self {
            migrated,
            sprites,
            scene: None,
            gpu: None,
            pipelines: Vec::new(),
            current: None,
        }
    }
}

impl RenderOverlay for AdditiveSprites {
    // Step 1.
    fn stage(&self) -> OverlayStage {
        if self.migrated {
            OverlayStage::HdrScene
        } else {
            OverlayStage::AfterTonemap
        }
    }

    // Step 2: the encoding (and, in the Ldr fallback, exposure and tonemapper) of this pass.
    fn prepare_scene(&mut self, scene: &OverlayScene<'_>) {
        self.scene = Some((scene.output, scene.exposure, scene.tonemapper, scene.mode));
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _: &mut wgpu::CommandEncoder,
        target: &OverlayTarget,
    ) {
        let gpu = self.gpu.get_or_insert_with(|| {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("additive sprites"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("additive sprites"),
                size: std::mem::size_of::<Params>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("additive sprites"),
                layout: &bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("additive sprites"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("additive sprites"),
                source: wgpu::ShaderSource::Wgsl(format!("{OUTPUT_WGSL}\n{FULLSCREEN_WGSL}\n{SPRITE_WGSL}").into()),
            });
            SpriteGpu {
                buffer,
                bind_group,
                layout,
                module,
            }
        });
        let (encoding, exposure, op, soft_clip) = match self.scene.take().filter(|_| self.migrated) {
            // Step 3: values above 1 feed bloom and the tonemapper outside Direct mode.
            Some((output, exposure, tonemapper, mode)) => (output, exposure, tonemapper, mode == FrameMode::Direct),
            // The 249c874 rule (wrong for float targets: `shader_encodes_srgb` is right).
            None if !target.format.is_srgb() => (OutputEncoding::ManualSrgb, 1.0, Tonemapper::None, true),
            None => (OutputEncoding::Linear, 1.0, Tonemapper::None, true),
        };
        let (w, h) = (target.size.0 as f32, target.size.1 as f32);
        let mut params = Params {
            target: [w, h, 1.0 / w.max(1.0), 1.0 / h.max(1.0)],
            mode: [
                encoding.id(),
                op.id(),
                soft_clip as u32,
                self.sprites.len().min(MAX_SPRITES) as u32,
            ],
            exposure: [exposure, 0.0, 0.0, 0.0],
            sprites: [[0.0; 4]; 2 * MAX_SPRITES],
        };
        for (i, s) in self.sprites.iter().take(MAX_SPRITES).enumerate() {
            params.sprites[2 * i] = [s.center[0] * w, s.center[1] * h, s.radius * h, 0.0];
            params.sprites[2 * i + 1] = [s.color[0], s.color[1], s.color[2], 0.0];
        }
        queue.write_buffer(&gpu.buffer, 0, &params_bytes(&params));
        // Step 4: one pipeline per target format (`Rgba16Float` for HdrScene scene colors).
        self.current = Some(match self.pipelines.iter().position(|(f, _)| *f == target.format) {
            Some(i) => i,
            None => {
                let additive = wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                };
                let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("additive sprites"),
                    layout: Some(&gpu.layout),
                    vertex: wgpu::VertexState {
                        module: &gpu.module,
                        entry_point: Some("vs"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &gpu.module,
                        entry_point: Some("fs"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: target.format,
                            blend: Some(wgpu::BlendState {
                                color: additive,
                                alpha: additive,
                            }),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview_mask: None,
                    cache: None,
                });
                self.pipelines.push((target.format, pipeline));
                self.pipelines.len() - 1
            }
        });
    }

    fn render(&self, pass: &mut wgpu::RenderPass<'static>, _: &OverlayTarget) {
        let (Some(gpu), Some(i)) = (&self.gpu, self.current) else {
            return;
        };
        pass.set_pipeline(&self.pipelines[i].1);
        pass.set_bind_group(0, &gpu.bind_group, &[]);
        pass.draw(0..6, 0..self.sprites.len().min(MAX_SPRITES) as u32);
    }
}

/// The bytes of `params` (native-endian floats and integers, as WGSL expects them).
fn params_bytes(p: &Params) -> Vec<u8> {
    let mut out = Vec::with_capacity(std::mem::size_of::<Params>());
    for v in p.target {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    for v in p.mode {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    for v in p.exposure {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    for s in &p.sprites {
        for v in s {
            out.extend_from_slice(&v.to_ne_bytes());
        }
    }
    out
}
