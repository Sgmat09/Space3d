//! Render nodes for the node, overlay-stage and GL tests: solid-color slots that log every call,
//! a scene-depth probe checking the depth it reads against an analytic plane, and a distortion
//! writer.

use space3d_core::prelude::*;
use space3d_render::*;
use std::sync::{Arc, Mutex};

/// One `draw` call as a node saw it.
#[derive(Clone, Debug)]
pub struct DrawCall {
    pub node: &'static str,
    pub slot: usize,
    pub phase: RenderPhase,
    pub target: PassTarget,
    pub target_size: (u32, u32),
    pub kind: FrameKind,
    pub renderer: u64,
    pub depth: bool,
}

/// Calls of every logged node, in order.
#[derive(Debug, Default)]
pub struct Log {
    pub prepares: Vec<(&'static str, u64)>,
    pub views: Vec<(&'static str, Entity)>,
    pub prewarms: Vec<(&'static str, usize, PassTarget)>,
    pub draws: Vec<DrawCall>,
}

pub type SharedLog = Arc<Mutex<Log>>;

/// The targets a [`DepthProbe`] drew with, and whether it got scene depth.
pub type ProbeLog = Arc<Mutex<Vec<(PassTarget, bool)>>>;

/// The targets and target sizes a [`DistortionWriter`] drew with.
pub type WriterLog = Arc<Mutex<Vec<(PassTarget, (u32, u32))>>>;

pub fn log() -> SharedLog {
    Arc::new(Mutex::new(Log::default()))
}

/// A slot of [`Solid`]: a solid-color rectangle in normalized device coordinates
/// (`[x0, y0, x1, y1]`, y up).
#[derive(Clone, Copy, Debug)]
pub struct SolidSlot {
    pub slot: PhaseSlot,
    pub color: [f32; 4],
    pub rect: [f32; 4],
}

/// Draws a solid rectangle per slot (no bind groups, depth test Always, no depth writes) and
/// logs its calls. `mask` is what `prepare_view` reports.
pub struct Solid {
    pub name: &'static str,
    pub slots: Vec<PhaseSlot>,
    pub rects: Vec<SolidSlot>,
    pub mask: SlotMask,
    pub log: SharedLog,
    /// Log nothing (allocation tests).
    pub quiet: bool,
    renderer: u64,
    pipelines: Vec<((usize, PassTarget), wgpu::RenderPipeline)>,
}

impl Solid {
    pub fn new(name: &'static str, rects: Vec<SolidSlot>, log: SharedLog) -> Self {
        Self {
            name,
            slots: rects.iter().map(|r| r.slot).collect(),
            rects,
            mask: SlotMask::ALL,
            log,
            quiet: false,
            renderer: 0,
            pipelines: Vec::new(),
        }
    }

    /// A full-screen slot of `color`.
    pub fn slot(slot: PhaseSlot, color: [f32; 4]) -> SolidSlot {
        SolidSlot {
            slot,
            color,
            rect: [-1.0, -1.0, 1.0, 1.0],
        }
    }

    fn build(&mut self, device: &wgpu::Device, slot: usize, target: PassTarget) {
        if self.pipelines.iter().any(|(k, _)| *k == (slot, target)) {
            return;
        }
        let SolidSlot { color, rect, .. } = self.rects[slot];
        let src = format!(
            "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {{
                 var corners = array<vec2<f32>, 6>(vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
                                                   vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0));
                 let c = corners[i];
                 let r = vec4<f32>({:?}, {:?}, {:?}, {:?});
                 return vec4<f32>(mix(r.xy, r.zw, c), 0.5, 1.0);
             }}
             @fragment fn fs() -> @location(0) vec4<f32> {{ return vec4<f32>({:?}, {:?}, {:?}, {:?}); }}",
            rect[0], rect[1], rect[2], rect[3], color[0], color[1], color[2], color[3]
        );
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test solid node"),
            source: wgpu::ShaderSource::Wgsl(src.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test solid node"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: target.depth_stencil(wgpu::CompareFunction::Always, false),
            multisample: target.multisample(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(target.color_target(None))],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipelines.push(((slot, target), pipeline));
    }
}

impl RenderNode for Solid {
    fn slots(&self) -> &[PhaseSlot] {
        &self.slots
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        // A new renderer (device): pipelines of the old one are invalid there.
        if ctx.frame.renderer != self.renderer {
            self.renderer = ctx.frame.renderer;
            self.pipelines.clear();
        }
        if !self.quiet {
            self.log.lock().unwrap().prepares.push((self.name, ctx.frame.renderer));
        }
    }

    fn prepare_view(&mut self, ctx: &mut ViewPrepareContext<'_>) -> SlotMask {
        if !self.quiet {
            self.log.lock().unwrap().views.push((self.name, ctx.view.camera));
        }
        self.mask
    }

    fn prewarm(&mut self, device: &wgpu::Device, targets: &[(usize, PassTarget)]) {
        for &(slot, target) in targets {
            if !self.quiet {
                self.log.lock().unwrap().prewarms.push((self.name, slot, target));
            }
            self.build(device, slot, target);
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        if !self.quiet {
            self.log.lock().unwrap().draws.push(DrawCall {
                node: self.name,
                slot: ctx.slot,
                phase: ctx.phase,
                target: ctx.target,
                target_size: ctx.target_size,
                kind: ctx.frame.kind,
                renderer: ctx.frame.renderer,
                depth: ctx.scene_depth.is_some(),
            });
        }
        self.build(ctx.device, ctx.slot, ctx.target);
        let (_, p) = self
            .pipelines
            .iter()
            .find(|(k, _)| *k == (ctx.slot, ctx.target))
            .unwrap();
        pass.set_pipeline(p);
        pass.draw(0..6, 0..1);
    }
}

/// Tilt (radians) of the analytic plane of [`DepthProbe`] scenes.
pub const TILT: f32 = 0.5;

/// A large plane through the origin, tilted by [`TILT`] towards the camera (normal
/// `(0, sin TILT, cos TILT)`), and a perspective camera at z = 5 looking at the origin.
pub fn analytic_plane_scene(app: &mut App) -> Entity {
    let plane = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(200.0));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2 - TILT)),
        Mesh3d(plane),
        Material::unlit(Vec4::new(0.3, 0.3, 0.3, 1.0)),
    ));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
    ))
}

/// A depth-reading Transparent slot drawing a full-screen triangle that compares the scene
/// depth it reads (`s3d_view_depth_at`) with the analytic plane of [`analytic_plane_scene`]:
/// green where it lies within 1 % of the plane's depth range over the pixel (its center and
/// the 4× sample positions: a resolved depth is the nearest sample's), red elsewhere, blue
/// without scene depth (the in-pass fallback). Records the targets it drew with.
pub struct DepthProbe {
    renderer: u64,
    layouts: Option<(wgpu::BindGroupLayout, wgpu::BindGroupLayout)>,
    pipelines: Vec<(PassTarget, wgpu::RenderPipeline)>,
    pub seen: ProbeLog,
}

static PROBE_SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Transparent, 10).reading(SceneReads::DEPTH)];

impl DepthProbe {
    pub fn new() -> (Self, ProbeLog) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                renderer: 0,
                layouts: None,
                pipelines: Vec::new(),
                seen: seen.clone(),
            },
            seen,
        )
    }
}

/// WGSL of the probe for `target`.
pub fn probe_source(target: &PassTarget) -> String {
    format!(
        "{VIEW_WGSL}\n{}\n{FULLSCREEN_WGSL}\n
        const TILT: f32 = {TILT:?};
        // View depth of the analytic plane at pixel position `xy` (pixels of the pass).
        fn plane_depth(xy: vec2<f32>) -> f32 {{
            let uv = s3d_scene_uv(xy);
            let far = view.inv_proj * vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 1.0, 1.0);
            let dir = far.xyz / far.w;
            let n = (view.view * vec4<f32>(0.0, sin(TILT), cos(TILT), 0.0)).xyz;
            let p = (view.view * vec4<f32>(0.0, 0.0, 0.0, 1.0)).xyz;
            return -(dir * (dot(p, n) / dot(dir, n))).z;
        }}
        @fragment fn fs(in: FullscreenOut) -> @location(0) vec4<f32> {{
            let px = vec2<i32>(in.pos.xy);
            if (!s3d_depth_valid()) {{ return vec4<f32>(0.0, 0.0, 1.0, 1.0); }}
            // The plane at the pixel center and at the standard 4× sample positions (a resolved
            // depth is the nearest sample's).
            let c = vec2<f32>(px) + 0.5;
            var lo = plane_depth(c);
            var hi = lo;
            let offsets = array<vec2<f32>, 4>(vec2<f32>(-0.125, -0.375), vec2<f32>(0.375, -0.125),
                                              vec2<f32>(-0.375, 0.125), vec2<f32>(0.125, 0.375));
            for (var i = 0; i < 4; i++) {{
                let d = plane_depth(c + offsets[i]);
                lo = min(lo, d);
                hi = max(hi, d);
            }}
            let got = s3d_view_depth_at(px);
            let ok = got >= lo * 0.99 && got <= hi * 1.01;
            return select(vec4<f32>(1.0, 0.0, 0.0, 1.0), vec4<f32>(0.0, 1.0, 0.0, 1.0), ok);
        }}",
        scene_wgsl(target)
    )
}

/// A pipeline drawing `vs_fullscreen` + `fs` of `src` for `target` with groups 0 and 1 of
/// `layouts` (blend `blend`, depth test Always, no writes).
pub fn fullscreen_pipeline(
    device: &wgpu::Device,
    layouts: &(wgpu::BindGroupLayout, wgpu::BindGroupLayout),
    src: &str,
    target: &PassTarget,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("test node"),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("test node"),
        bind_group_layouts: &[Some(&layouts.0), Some(&layouts.1)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("test node"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_fullscreen"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: target.depth_stencil(wgpu::CompareFunction::Always, false),
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

impl RenderNode for DepthProbe {
    fn slots(&self) -> &[PhaseSlot] {
        &PROBE_SLOTS
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        if ctx.frame.renderer != self.renderer {
            self.renderer = ctx.frame.renderer;
            self.layouts = Some((ctx.layouts.view.clone(), ctx.layouts.scene.clone()));
            self.pipelines.clear();
        }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let mut seen = self.seen.lock().unwrap();
        if seen.len() < 8 {
            seen.push((ctx.target, ctx.scene_depth.is_some()));
        }
        drop(seen);
        let Some(layouts) = &self.layouts else { return };
        if !self.pipelines.iter().any(|(t, _)| *t == ctx.target) {
            let p = fullscreen_pipeline(ctx.device, layouts, &probe_source(&ctx.target), &ctx.target, None);
            self.pipelines.push((ctx.target, p));
        }
        let (_, p) = self.pipelines.iter().find(|(t, _)| *t == ctx.target).unwrap();
        pass.set_pipeline(p);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// A Distortion slot writing a constant offset `(0.01, -0.02)` (view heights) over the whole
/// distortion buffer, when `work` is set.
pub struct DistortionWriter {
    pub work: bool,
    renderer: u64,
    layouts: Option<(wgpu::BindGroupLayout, wgpu::BindGroupLayout)>,
    pipelines: Vec<(PassTarget, wgpu::RenderPipeline)>,
    pub draws: WriterLog,
}

static DISTORTION_SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Distortion, 0)];

impl DistortionWriter {
    pub fn new(work: bool) -> Self {
        Self {
            work,
            renderer: 0,
            layouts: None,
            pipelines: Vec::new(),
            draws: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl RenderNode for DistortionWriter {
    fn slots(&self) -> &[PhaseSlot] {
        &DISTORTION_SLOTS
    }

    fn prepare(&mut self, ctx: &mut PrepareContext<'_>) {
        if ctx.frame.renderer != self.renderer {
            self.renderer = ctx.frame.renderer;
            self.layouts = Some((ctx.layouts.view.clone(), ctx.layouts.scene.clone()));
            self.pipelines.clear();
        }
    }

    fn prepare_view(&mut self, _: &mut ViewPrepareContext<'_>) -> SlotMask {
        if self.work { SlotMask::ALL } else { SlotMask::NONE }
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        self.draws.lock().unwrap().push((ctx.target, ctx.target_size));
        let Some(layouts) = &self.layouts else { return };
        if !self.pipelines.iter().any(|(t, _)| *t == ctx.target) {
            let src = format!(
                "{VIEW_WGSL}\n{}\n{DISTORTION_WGSL}\n{FULLSCREEN_WGSL}\n
                @fragment fn fs(in: FullscreenOut) -> @location(0) vec4<f32> {{
                    return distortion_encode(vec2<f32>(0.01, -0.02), 1.0);
                }}",
                scene_wgsl(&ctx.target)
            );
            let add = wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            };
            let blend = Some(wgpu::BlendState { color: add, alpha: add });
            let p = fullscreen_pipeline(ctx.device, layouts, &src, &ctx.target, blend);
            self.pipelines.push((ctx.target, p));
        }
        let (_, p) = self.pipelines.iter().find(|(t, _)| *t == ctx.target).unwrap();
        pass.set_pipeline(p);
        pass.set_bind_group(0, ctx.view_bind_group, &[ctx.view.view_offset]);
        pass.set_bind_group(1, ctx.scene_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
