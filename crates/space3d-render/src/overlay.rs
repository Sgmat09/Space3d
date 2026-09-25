//! Render overlays: let other crates draw **after the 3D scene into the final color target**.
//!
//! An overlay (the egui UI of `space3d-ui`, debug gizmos, a custom HUD...) implements
//! [`RenderOverlay`] and is registered in the [`RenderOverlays`] resource with a priority.
//! Every frame the renderer:
//!
//! 1. records the 3D views as usual (MSAA is resolved first),
//! 2. calls [`RenderOverlay::prepare`] on every overlay (ascending priority) with the frame's
//!    command encoder — upload buffers/textures here,
//! 3. opens **one** single-sample render pass on the final target (`LoadOp::Load`, no depth)
//!    and calls [`RenderOverlay::render`] on every overlay in the same order.
//!
//! The final target is the window surface when windowed, and the offscreen capture target when
//! headless — so overlays show up in [`crate::Renderer::capture`] / [`crate::render_to_image`]
//! screenshots. For windowed captures the renderer re-records the primary views into the
//! offscreen target and runs the overlays again with [`OverlayTargetKind::Capture`] (possibly
//! with a different texture format than the surface — overlays must handle both).
//!
//! When no camera draws to the primary target this frame, the headless target is cleared to
//! opaque black before the overlays run (the surface is cleared by the renderer already), so
//! UI-only apps never show stale pixels.
//!
//! That is the default stage, [`OverlayStage::AfterTonemap`]. An overlay returning
//! [`OverlayStage::HdrScene`] from [`RenderOverlay::stage`] draws into the primary target's scene
//! color instead — after its last scene view, before post (exposure, bloom, tonemapping), in the
//! encoding [`OverlayScene::output`] names — or, in Direct mode (no post camera), into the target
//! before every `AfterTonemap` overlay. See graphics spec §1.5 and `docs/graphics/frame.md`
//! ("Overlay stages", with the migration of an additive overlay).
//!
//! Overlays are shared as `Arc<Mutex<dyn RenderOverlay>>` ([`SharedOverlay`]): the owner keeps a
//! typed handle (returned by [`RenderOverlays::insert`]) to feed per-frame data, and the
//! renderer keeps a snapshot of the list so windowed captures can replay it.
//!
//! ```no_run
//! use space3d_render::*;
//! struct Hud;
//! impl RenderOverlay for Hud {
//!     fn prepare(&mut self, _: &wgpu::Device, _: &wgpu::Queue, _: &mut wgpu::CommandEncoder, _: &OverlayTarget) {}
//!     fn render(&self, pass: &mut wgpu::RenderPass<'static>, target: &OverlayTarget) {
//!         // pass.set_pipeline(...); pass.draw(0..3, 0..1);
//!         let _ = (pass, target);
//!     }
//! }
//! let mut app = space3d_core::App::new();
//! app.add_plugin(RenderPlugin::default());
//! let hud = app.resources.expect_mut::<RenderOverlays>().insert("hud", 100, Hud);
//! # let _ = hud;
//! ```

use crate::assets::RenderAssets;
use crate::handles::TextureHandle;
use crate::node::{FrameLayouts, FrameMode, SceneDepthRef, SceneReads, ViewInfo};
use crate::output::{OutputEncoding, Tonemapper};
use crate::pipeline::OFFSCREEN_FORMAT;
use crate::renderer::Renderer;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Color format of the headless/capture target and of textures created with
/// [`RenderAssets::add_render_target`] (sRGB: sampling returns linear values).
pub const RENDER_TARGET_FORMAT: wgpu::TextureFormat = OFFSCREEN_FORMAT;

/// Which target an overlay pass draws into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverlayTargetKind {
    /// The window surface (already MSAA-resolved).
    Surface,
    /// The headless offscreen target (what [`crate::Renderer::capture`] reads).
    Offscreen,
    /// The offscreen target while a windowed frame is re-recorded for a capture.
    Capture,
}

/// Description of the color target handed to [`RenderOverlay`]s.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayTarget {
    /// Target size in physical pixels.
    pub size: (u32, u32),
    /// Color format of the target (the render pass uses exactly this format, 1 sample,
    /// no depth attachment).
    pub format: wgpu::TextureFormat,
    /// Physical pixels per logical pixel, from [`RenderOverlays::scale_factor`].
    pub scale_factor: f32,
    /// Surface, headless offscreen, or capture replay.
    pub kind: OverlayTargetKind,
}

/// Something drawn on top of the 3D scene, into the final color target (the window surface,
/// or the headless target read by [`crate::Renderer::capture`], so overlays appear in
/// screenshots).
///
/// Each frame, after the 3D views (MSAA already resolved), the renderer calls
/// [`RenderOverlay::prepare`] on every registered overlay in [`RenderOverlays`] order, then
/// opens one single-sample pass on the target (`LoadOp::Load`, no depth) and calls
/// [`RenderOverlay::render`] in the same order. For windowed captures the overlays run again
/// with [`OverlayTargetKind::Capture`], possibly with a different texture format than the
/// surface, so implementations must handle any `target.format`.
///
/// Overlays whose [`Self::stage`] is [`OverlayStage::HdrScene`] follow the same contract in an
/// earlier pass: into the primary scene color (`Rgba16Float`, `Rg11b10Ufloat`, or
/// `Rgba8UnormSrgb` in the Ldr fallback) after its last scene view, with
/// [`Self::prepare_scene`] called before [`Self::prepare`]. They are skipped in frames without a
/// primary view, and capture replays call them again only when the primary target rendered in
/// Direct mode (otherwise they are already in the scene color the capture composites again).
pub trait RenderOverlay: Send + Sync + 'static {
    /// Upload per-frame data. Called once per overlay pass, before [`Self::render`], with the
    /// frame's encoder (work recorded here executes before the overlay pass).
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &OverlayTarget,
    );

    /// Record draw commands into the shared overlay pass (single sample, no depth,
    /// `target.format`). Viewport and scissor are reset to the full target for each overlay.
    fn render(&self, pass: &mut wgpu::RenderPass<'static>, target: &OverlayTarget);

    /// Where the overlay draws; read every frame. The default, [`OverlayStage::AfterTonemap`],
    /// is exactly the placement described above.
    fn stage(&self) -> OverlayStage {
        OverlayStage::AfterTonemap
    }

    /// [`OverlayStage::HdrScene`] only: scene inputs sampled ([`SceneReads::DEPTH`] keeps the
    /// last primary scene view's depth stored for [`OverlayScene::scene_depth`]).
    fn scene_reads(&self) -> SceneReads {
        SceneReads::NONE
    }

    /// [`OverlayStage::HdrScene`] only: called before [`Self::prepare`] with the scene inputs of
    /// the pass (the encoding the overlay must apply, the last primary view, …).
    fn prepare_scene(&mut self, scene: &OverlayScene<'_>) {
        let _ = scene;
    }
}

/// Where a [`RenderOverlay`] draws (graphics spec §1.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OverlayStage {
    /// Into the primary scene color before post: distorted, metered, bloomed and tonemapped
    /// with the scene. In Direct mode, into the target after the primary views and before every
    /// `AfterTonemap` overlay. Writes values in [`OverlayScene::output`]'s encoding.
    HdrScene,
    /// Into the final target after tonemapping (the placement of every overlay before stages
    /// existed; egui).
    #[default]
    AfterTonemap,
}

/// Scene inputs of an [`OverlayStage::HdrScene`] pass ([`RenderOverlay::prepare_scene`]).
#[non_exhaustive]
pub struct OverlayScene<'a> {
    /// The last primary scene view (matrices, near/far, rect).
    pub view: &'a ViewInfo,
    /// Frame mode of the primary target group.
    pub mode: FrameMode,
    /// Encoding the overlay must apply (`Tonemapped` in the Ldr fallback; see
    /// [`crate::OUTPUT_WGSL`]'s `s3d_output`).
    pub output: OutputEncoding,
    /// Linear exposure for `Tonemapped`.
    pub exposure: f32,
    /// Operator for `Tonemapped`.
    pub tonemapper: Tonemapper,
    /// The last primary scene view's 1× depth, when `scene_reads()` contains `DEPTH` and the
    /// device can sample it.
    pub scene_depth: Option<SceneDepthRef<'a>>,
    /// Group 0 of the view ([`FrameLayouts::view`], dynamic offset `view.view_offset`).
    pub view_bind_group: &'a wgpu::BindGroup,
    /// Group 1 ([`FrameLayouts::scene`]).
    pub scene_bind_group: &'a wgpu::BindGroup,
    /// The shared layouts.
    pub layouts: &'a FrameLayouts,
}

impl OverlayTarget {
    /// The shader must encode sRGB itself: only 8-bit non-sRGB unorm formats (false for sRGB
    /// *and* float formats — `!format.is_srgb()` is wrong for float targets).
    pub fn shader_encodes_srgb(&self) -> bool {
        use wgpu::TextureFormat as F;
        matches!(
            self.format,
            F::Rgba8Unorm | F::Bgra8Unorm | F::Rgba8Snorm | F::Rgb10a2Unorm | F::Rgba16Unorm
        )
    }

    /// `format` is a float (HDR) format.
    pub fn is_hdr(&self) -> bool {
        use wgpu::TextureFormat as F;
        matches!(
            self.format,
            F::Rgba16Float
                | F::Rgba32Float
                | F::Rg11b10Ufloat
                | F::R16Float
                | F::R32Float
                | F::Rg16Float
                | F::Rg32Float
        )
    }
}

/// A registered overlay, shared between its owner and the renderer.
pub type SharedOverlay = Arc<Mutex<dyn RenderOverlay>>;

/// Lock an overlay, recovering from a poisoned mutex (a panicking overlay must not take the
/// renderer down with it).
pub fn lock_overlay(overlay: &SharedOverlay) -> MutexGuard<'_, dyn RenderOverlay> {
    overlay.lock().unwrap_or_else(|e| e.into_inner())
}

struct OverlayEntry {
    name: String,
    priority: i32,
    seq: u64,
    overlay: SharedOverlay,
}

/// Globally unique list generations, so the renderer notices any change (even when the
/// resource is replaced wholesale).
fn next_generation() -> u64 {
    static GENERATION: AtomicU64 = AtomicU64::new(1);
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Registered overlays (resource, inserted by [`crate::RenderPlugin`]). Lower priorities draw
/// first (underneath); equal priorities draw in insertion order.
pub struct RenderOverlays {
    entries: Vec<OverlayEntry>,
    next_seq: u64,
    generation: u64,
    scale_factor: f32,
}

impl Default for RenderOverlays {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_seq: 0,
            generation: next_generation(),
            scale_factor: 1.0,
        }
    }
}

impl std::fmt::Debug for RenderOverlays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderOverlays")
            .field("overlays", &self.names().collect::<Vec<_>>())
            .field("scale_factor", &self.scale_factor)
            .finish()
    }
}

impl RenderOverlays {
    /// Register `overlay` under `name` (replacing any overlay with that name) and return a
    /// typed handle the owner can lock to feed it data.
    pub fn insert<T: RenderOverlay>(&mut self, name: impl Into<String>, priority: i32, overlay: T) -> Arc<Mutex<T>> {
        let shared = Arc::new(Mutex::new(overlay));
        self.insert_shared(name, priority, shared.clone());
        shared
    }

    /// Register an already shared overlay. Returns the overlay previously registered under
    /// `name`, if any.
    pub fn insert_shared(
        &mut self,
        name: impl Into<String>,
        priority: i32,
        overlay: SharedOverlay,
    ) -> Option<SharedOverlay> {
        let name = name.into();
        let old = self.remove(&name);
        let seq = self.next_seq;
        self.next_seq += 1;
        let at = self.entries.partition_point(|e| (e.priority, e.seq) < (priority, seq));
        self.entries.insert(
            at,
            OverlayEntry {
                name,
                priority,
                seq,
                overlay,
            },
        );
        self.generation = next_generation();
        old
    }

    /// Unregister an overlay; returns it if it existed.
    pub fn remove(&mut self, name: &str) -> Option<SharedOverlay> {
        let i = self.entries.iter().position(|e| e.name == name)?;
        self.generation = next_generation();
        Some(self.entries.remove(i).overlay)
    }

    /// Overlay registered under `name`.
    pub fn get(&self, name: &str) -> Option<&SharedOverlay> {
        self.entries.iter().find(|e| e.name == name).map(|e| &e.overlay)
    }

    /// Whether an overlay with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// `(name, priority)` in draw order.
    pub fn names(&self) -> impl Iterator<Item = (&str, i32)> {
        self.entries.iter().map(|e| (e.name.as_str(), e.priority))
    }

    /// Number of overlays.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No overlays registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Scale factor passed to overlays in [`OverlayTarget::scale_factor`] (default 1.0).
    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    /// Set the scale factor passed to overlays (e.g. the window's DPI scale).
    pub fn set_scale_factor(&mut self, scale_factor: f32) {
        self.scale_factor = scale_factor;
    }
}

/// The renderer's snapshot of [`RenderOverlays`] (re-cloned only when the list changes), kept so
/// windowed captures can replay the overlays outside of `render_frame`, and every overlay's
/// stage and scene reads (read at the start of every frame).
#[derive(Default)]
pub(crate) struct OverlayCache {
    generation: u64,
    list: Vec<SharedOverlay>,
    /// Stage and scene reads of each overlay of `list`, this frame.
    stages: Vec<(OverlayStage, SceneReads)>,
    scale_factor: f32,
}

impl OverlayCache {
    pub(crate) fn sync(&mut self, overlays: Option<&RenderOverlays>) {
        match overlays {
            Some(o) => {
                self.scale_factor = o.scale_factor;
                if o.generation != self.generation {
                    self.list.clear();
                    self.list.extend(o.entries.iter().map(|e| e.overlay.clone()));
                    self.generation = o.generation;
                }
            }
            None => {
                self.list.clear();
                self.generation = 0;
            }
        }
        self.stages.clear();
        for overlay in &self.list {
            let o = lock_overlay(overlay);
            let stage = o.stage();
            let reads = if stage == OverlayStage::HdrScene {
                o.scene_reads()
            } else {
                SceneReads::NONE
            };
            self.stages.push((stage, reads));
        }
    }

    /// Some overlay draws in `stage` this frame.
    pub(crate) fn has_stage(&self, stage: OverlayStage) -> bool {
        self.stages.iter().any(|(s, _)| *s == stage)
    }

    /// Some HdrScene overlay reads scene depth this frame.
    pub(crate) fn hdr_scene_depth(&self) -> bool {
        self.stages
            .iter()
            .any(|(s, r)| *s == OverlayStage::HdrScene && r.contains(SceneReads::DEPTH))
    }

    /// Overlays in draw order with their stage and scene reads this frame.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&SharedOverlay, OverlayStage, SceneReads)> {
        self.list
            .iter()
            .zip(&self.stages)
            .map(|(o, &(stage, reads))| (o, stage, reads))
    }

    /// Scale factor of the overlays' targets.
    pub(crate) fn scale_factor(&self) -> f32 {
        self.scale_factor
    }
}

/// Overlay target of a surface frame. Its format is the one frames render into — the surface's
/// sRGB *view* format where the surface texture itself has none (browser WebGPU canvases) —
/// never `config.format`: an overlay pipeline built for the texture's own format would not
/// match the pass, a validation error that loses the frame.
fn surface_target(config: &wgpu::SurfaceConfiguration, view_format: wgpu::TextureFormat, scale: f32) -> OverlayTarget {
    OverlayTarget {
        size: (config.width, config.height),
        format: view_format,
        scale_factor: scale,
        kind: OverlayTargetKind::Surface,
    }
}

impl Renderer {
    /// Overlays of a regular frame: surface when windowed, offscreen target when headless.
    /// Called after every view of the frame is encoded (overlays draw on top of them).
    pub(crate) fn encode_frame_overlays(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        surface_view: Option<&wgpu::TextureView>,
    ) {
        if !self.overlays.has_stage(OverlayStage::AfterTonemap) {
            return;
        }
        match (&self.surface, surface_view) {
            (Some(s), Some(view)) => {
                let target = surface_target(&s.config, s.view_format, self.overlays.scale_factor);
                // The renderer already cleared the surface if no camera drew to it.
                self.encode_overlays(encoder, view, &target, false);
            }
            // Surface frame unavailable (minimized/occluded): nothing to draw into.
            (Some(_), None) => {}
            (None, _) => self.encode_offscreen_overlays(encoder, OverlayTargetKind::Offscreen),
        }
    }

    /// Overlays replayed into the offscreen target for a windowed capture. They run after the
    /// replayed views ([`Renderer::encode_capture_replay`]): drawn before them, the re-recorded
    /// scene would paint over them.
    pub(crate) fn encode_capture_overlays(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if self.overlays.has_stage(OverlayStage::AfterTonemap) {
            self.encode_offscreen_overlays(encoder, OverlayTargetKind::Capture);
        }
    }

    fn encode_offscreen_overlays(&mut self, encoder: &mut wgpu::CommandEncoder, kind: OverlayTargetKind) {
        let Some(view) = self.offscreen.color_view.clone() else {
            return;
        };
        let target = OverlayTarget {
            size: (self.offscreen.width, self.offscreen.height),
            format: self.offscreen.format,
            scale_factor: self.overlays.scale_factor,
            kind,
        };
        let clear = !self.frame.has_primary_view();
        self.encode_overlays(encoder, &view, &target, clear);
    }

    fn encode_overlays(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        target: &OverlayTarget,
        clear: bool,
    ) {
        for (overlay, stage, _) in self.overlays.iter() {
            if stage == OverlayStage::AfterTonemap {
                lock_overlay(overlay).prepare(&self.device, &self.queue, encoder, target);
            }
        }
        let load = if clear {
            wgpu::LoadOp::Clear(wgpu::Color::BLACK)
        } else {
            wgpu::LoadOp::Load
        };
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("space3d overlays"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            })
            .forget_lifetime();
        let (w, h) = target.size;
        for (overlay, stage, _) in self.overlays.iter() {
            if stage != OverlayStage::AfterTonemap {
                continue;
            }
            pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
            pass.set_scissor_rect(0, 0, w, h);
            lock_overlay(overlay).render(&mut pass, target);
        }
    }
}

impl RenderAssets {
    /// A sampleable 2D view (format [`RENDER_TARGET_FORMAT`]) of a texture created with
    /// [`RenderAssets::add_render_target`], once the renderer has created it (the frame after
    /// registration). Used to show render targets in overlays, e.g. as egui images.
    pub fn render_target_view(&self, handle: TextureHandle) -> Option<wgpu::TextureView> {
        let entry = self.textures.get(handle.raw())?;
        entry.gpu.as_ref()?.target_view.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nop;
    impl RenderOverlay for Nop {
        fn prepare(&mut self, _: &wgpu::Device, _: &wgpu::Queue, _: &mut wgpu::CommandEncoder, _: &OverlayTarget) {}
        fn render(&self, _: &mut wgpu::RenderPass<'static>, _: &OverlayTarget) {}
    }

    #[test]
    fn ordering_replace_and_remove() {
        let mut o = RenderOverlays::default();
        o.insert("ui", 100, Nop);
        o.insert("hud", 0, Nop);
        o.insert("gizmo", 100, Nop);
        assert_eq!(
            o.names().collect::<Vec<_>>(),
            vec![("hud", 0), ("ui", 100), ("gizmo", 100)]
        );
        let g = o.generation;
        assert!(o.insert_shared("ui", -5, Arc::new(Mutex::new(Nop))).is_some());
        assert_ne!(o.generation, g, "changes bump the generation");
        assert_eq!(o.names().next(), Some(("ui", -5)));
        assert!(o.remove("hud").is_some() && o.remove("hud").is_none());
        assert_eq!(o.len(), 2);

        let mut cache = OverlayCache::default();
        cache.sync(Some(&o));
        assert_eq!(cache.list.len(), 2);
        cache.sync(None);
        assert!(cache.list.is_empty());
    }

    #[test]
    fn stages_and_target_encodings() {
        assert_eq!(Nop.stage(), OverlayStage::AfterTonemap);
        assert_eq!(Nop.scene_reads(), SceneReads::NONE);
        assert_eq!(OverlayStage::default(), OverlayStage::AfterTonemap);
        let target = |format| OverlayTarget {
            size: (1, 1),
            format,
            scale_factor: 1.0,
            kind: OverlayTargetKind::Offscreen,
        };
        use wgpu::TextureFormat as F;
        for (format, manual, hdr) in [
            (F::Rgba8UnormSrgb, false, false),
            (F::Bgra8UnormSrgb, false, false),
            (F::Bgra8Unorm, true, false),
            (F::Rgba8Unorm, true, false),
            (F::Rgba16Float, false, true),
            (F::Rg11b10Ufloat, false, true),
        ] {
            let t = target(format);
            assert_eq!((t.shader_encodes_srgb(), t.is_hdr()), (manual, hdr), "{format:?}");
        }
    }

    /// Regression: surface overlays got the surface texture's format while frames render
    /// through its sRGB view format (browser WebGPU canvases): egui's pipeline then did not
    /// match the pass. They now get the view format, like the frame's own passes.
    #[test]
    fn surface_overlays_get_the_view_format() {
        use wgpu::TextureFormat as F;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: F::Bgra8Unorm,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: 640,
            height: 360,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![F::Bgra8UnormSrgb],
        };
        let target = surface_target(&config, F::Bgra8UnormSrgb, 2.0);
        assert_eq!(target.format, F::Bgra8UnormSrgb);
        assert_eq!((target.size, target.scale_factor), ((640, 360), 2.0));
        assert_eq!(target.kind, OverlayTargetKind::Surface);
    }
}
