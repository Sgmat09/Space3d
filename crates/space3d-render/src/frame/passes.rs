//! Render passes (graphics spec §1.4 step 12): every view's forward pass into its target
//! (clears, viewports, MSAA resolve) — `D` in Direct mode, the group's scene texture `S` for
//! scene views of intermediate groups — the finish of intermediate groups (post groups'
//! composites, then display views into `D`), the clear of a window nothing drew to, and the
//! windowed-capture replay.

use super::views::{ViewData, ViewTarget};
use crate::node::{FrameKind, ViewRole};
use crate::output::OutputEncoding;
use crate::pipeline::OFFSCREEN_FORMAT;
use crate::post::{PostEncode, PostGroupInput};
use crate::renderer::Renderer;
use glam::Vec4;

pub(super) fn linear_to_srgb_color(c: Vec4) -> Vec4 {
    use crate::components::linear_to_srgb;
    Vec4::new(linear_to_srgb(c.x), linear_to_srgb(c.y), linear_to_srgb(c.z), c.w)
}

/// Encode state of one target group while recording a frame's passes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GroupEncode {
    /// Some pass drew into `D` this frame: later passes load it.
    drawn: bool,
    /// Some scene pass drew into `S` this frame: later ones load it.
    scene_started: bool,
    /// The group's post groups were composited into `D`.
    finished: bool,
}

/// The display target `D` of a view's group, as attachments.
struct DisplayTarget {
    /// Color attachment of Direct passes (multisampled when `samples > 1`).
    color: wgpu::TextureView,
    resolve: Option<wgpu::TextureView>,
    /// The single-sample texture of `D`: composites and display views draw into it.
    single: wgpu::TextureView,
    /// Depth of Direct passes (`samples` samples).
    depth: wgpu::TextureView,
    format: wgpu::TextureFormat,
    samples: u32,
    size: (u32, u32),
    manual_srgb: bool,
}

/// The encoding a view's pipelines use in a pass: its own, except that only manual-sRGB
/// attachments get manual sRGB (a capture replay of a manual-sRGB window draws into the sRGB
/// offscreen target).
fn pass_output(view: OutputEncoding, manual_srgb: bool) -> OutputEncoding {
    match view {
        OutputEncoding::ManualSrgb | OutputEncoding::Linear if manual_srgb => OutputEncoding::ManualSrgb,
        OutputEncoding::ManualSrgb => OutputEncoding::Linear,
        other => other,
    }
}

impl Renderer {
    /// Record render passes for the prepared views. With `primary_to_offscreen`, only primary
    /// views are re-recorded into the offscreen target (windowed capture).
    ///
    /// Views sharing a target (split screen, picture-in-picture, overlays) draw in camera
    /// order into the same attachments: the first pass into a target in a frame clears it,
    /// later ones load what the earlier ones drew (see [`Self::encode_pass`]). In intermediate
    /// groups, scene views draw into `S` the same way; after the group's last scene view its
    /// post groups are composited into `D` (the first composite clears `D`), then its display
    /// views draw into `D` in camera order.
    pub(crate) fn encode_views(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        surface_view: Option<&wgpu::TextureView>,
        primary_to_offscreen: bool,
    ) {
        let views = std::mem::take(&mut self.frame.views);
        let mut state = std::mem::take(&mut self.frame.group_encode);
        state.clear();
        state.resize(self.frame.groups.len(), GroupEncode::default());
        let mut presented = false;
        for (i, view) in views.iter().enumerate() {
            let is_primary = matches!(view.target, ViewTarget::Primary);
            if primary_to_offscreen && !is_primary {
                continue;
            }
            let Some(d) = self.display_target(view, surface_view, primary_to_offscreen) else {
                // Surface frame unavailable (minimized/occluded): skip.
                continue;
            };
            let g = view.group;
            let (intermediate, last_scene_view) = {
                let group = &self.frame.groups[g];
                (group.intermediate(), group.last_scene_view)
            };
            if !intermediate {
                let first = !state[g].drawn;
                state[g].drawn = true;
                presented |= is_primary;
                self.copy_feedback(encoder, view);
                let output = pass_output(view.output, d.manual_srgb);
                self.encode_pass(
                    encoder,
                    view,
                    first,
                    &d.color,
                    d.resolve.as_ref(),
                    &d.depth,
                    d.format,
                    d.samples,
                    output,
                );
                continue;
            }
            match view.role {
                ViewRole::Scene => {
                    // Replays keep `S` from the frame: only its composites run again.
                    if !primary_to_offscreen {
                        let first = !state[g].scene_started;
                        state[g].scene_started = true;
                        self.encode_scene_view(encoder, view, first);
                    }
                    if last_scene_view == Some(i) {
                        self.finish_group(encoder, g, &views[..i], &d, primary_to_offscreen, &mut state[g]);
                        presented |= is_primary;
                    }
                }
                _ => {
                    // Display views before the group's last scene view wait for its finish.
                    if state[g].finished {
                        self.encode_display_view(encoder, view, &d);
                    }
                }
            }
        }
        // Nothing drew to the window: still clear it so stale content is not shown.
        if !presented
            && !primary_to_offscreen
            && let Some(sv) = surface_view
        {
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("space3d clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: sv,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        self.frame.views = views;
        self.frame.group_encode = state;
    }

    /// The display target of `view`'s group: the surface (windowed frames), the offscreen
    /// target (headless frames, capture replays) or the render target. `None` when the
    /// surface frame is unavailable.
    fn display_target(
        &self,
        view: &ViewData,
        surface_view: Option<&wgpu::TextureView>,
        to_offscreen: bool,
    ) -> Option<DisplayTarget> {
        match (&view.target, surface_view) {
            (ViewTarget::Primary, Some(sv)) if !to_offscreen => {
                let s = self.surface.as_ref().expect("surface");
                let a = &s.attachments;
                Some(DisplayTarget {
                    color: a.msaa_view.clone().unwrap_or_else(|| sv.clone()),
                    resolve: a.msaa_view.as_ref().map(|_| sv.clone()),
                    single: sv.clone(),
                    depth: a.depth_view.clone(),
                    format: a.format,
                    samples: a.samples,
                    size: (s.config.width, s.config.height),
                    manual_srgb: s.manual_srgb,
                })
            }
            (ViewTarget::Primary, _) => {
                if !to_offscreen && self.surface.is_some() {
                    return None;
                }
                let a = &self.offscreen;
                let target = a.color_view.clone().expect("offscreen color");
                Some(DisplayTarget {
                    color: a.msaa_view.clone().unwrap_or_else(|| target.clone()),
                    resolve: a.msaa_view.as_ref().map(|_| target.clone()),
                    single: target,
                    depth: a.depth_view.clone(),
                    format: OFFSCREEN_FORMAT,
                    samples: a.samples,
                    size: (a.width, a.height),
                    manual_srgb: false,
                })
            }
            (
                ViewTarget::Texture {
                    color,
                    depth,
                    msaa,
                    format,
                    samples,
                    ..
                },
                _,
            ) => Some(DisplayTarget {
                color: msaa.clone().unwrap_or_else(|| color.clone()),
                resolve: msaa.as_ref().map(|_| color.clone()),
                single: color.clone(),
                depth: depth.clone(),
                format: *format,
                samples: *samples,
                size: (color.texture().width(), color.texture().height()),
                manual_srgb: false,
            }),
        }
    }

    /// Snapshot a render target before a pass draws into it, for the view's materials that
    /// sample it (see `ViewData::feedback`).
    fn copy_feedback(&mut self, encoder: &mut wgpu::CommandEncoder, view: &ViewData) {
        if let ViewTarget::Texture { handle, color, .. } = &view.target
            && view.feedback
            && let Some(fb) = self.frame.feedback.get(handle)
        {
            encoder.copy_texture_to_texture(
                color.texture().as_image_copy(),
                fb.texture.as_image_copy(),
                wgpu::Extent3d {
                    width: fb.size.0,
                    height: fb.size.1,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// A scene view of an intermediate group: its forward pass into the group's `S` (MSAA
    /// resolved into it), with the viewport-clear rules applied to `S`.
    fn encode_scene_view(&mut self, encoder: &mut wgpu::CommandEncoder, view: &ViewData, first: bool) {
        let group = &self.frame.groups[view.group];
        let (format, samples) = (group.scene_format, group.samples);
        let Some(t) = self.frame.group_textures.get(&group.target) else {
            return;
        };
        let (color, resolve) = match &t.scene_msaa {
            Some(m) => (m.clone(), Some(t.scene_view.clone())),
            None => (t.scene_view.clone(), None),
        };
        let depth = t.depth.clone();
        self.encode_pass(
            encoder,
            view,
            first,
            &color,
            resolve.as_ref(),
            &depth,
            format,
            samples,
            view.output,
        );
    }

    /// A display view of an intermediate group: drawn LDR, Direct-style, at 1 sample into `D`
    /// after the group's composites (with the group's display depth).
    fn encode_display_view(&mut self, encoder: &mut wgpu::CommandEncoder, view: &ViewData, d: &DisplayTarget) {
        let group = &self.frame.groups[view.group];
        let Some(depth) = self
            .frame
            .group_textures
            .get(&group.target)
            .and_then(|t| t.display_depth.clone())
        else {
            return;
        };
        self.copy_feedback(encoder, view);
        let output = pass_output(view.output, d.manual_srgb);
        self.encode_pass(encoder, view, false, &d.single, None, &depth, d.format, 1, output);
    }

    /// Finish an intermediate group (spec §1.4 `finish(G)`): composite each post group, in
    /// settings-camera order, into `D` (the first write clears `D`), then the display views
    /// that came before the group's last scene view (`earlier`), in camera order. A capture
    /// replay composites again from the persisted `S` (no temporal update).
    fn finish_group(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        g: usize,
        earlier: &[ViewData],
        d: &DisplayTarget,
        replay: bool,
        state: &mut GroupEncode,
    ) {
        let group = &self.frame.groups[g];
        let (target, mode, scene_format, scene_size, transparent, post_groups) = (
            group.target,
            group.mode,
            group.scene_format,
            group.scene_size,
            group.transparent,
            group.post_groups.clone(),
        );
        let Some(t) = self.frame.group_textures.get(&target) else {
            return;
        };
        let (scene, generation) = (t.scene_view.clone(), t.generation);
        let mut frame = self.frame.info;
        if replay {
            frame.kind = FrameKind::CaptureReplay;
            frame.delta = 0.0;
        }
        for p in post_groups {
            let pg = self.frame.post_groups[p];
            let (rect, _) = pg.display_rect.fitted(d.size, self.frame.max_texture_2d);
            let input = PostGroupInput {
                camera: self.frame.cameras[pg.camera].id,
                settings: pg.settings,
                mode,
                scene_output: pg.scene_output,
                scene: &scene,
                scene_format,
                scene_size,
                scene_rect: pg.scene_rect.scissor(scene_size),
                scene_alpha: scene_format != wgpu::TextureFormat::Rg11b10Ufloat,
                transparent,
                depth: None,
                distortion: None,
                output: &d.single,
                output_format: d.format,
                output_size: d.size,
                output_rect: rect.scissor(d.size),
                output_manual_srgb: d.manual_srgb,
                output_clear: !state.drawn,
                passthrough: pg.passthrough,
                replay,
                frame,
                index: p,
                scene_generation: generation,
                output_viewport: [rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32],
            };
            let mut cx = PostEncode {
                device: &self.device,
                queue: &self.queue,
                encoder,
                stats: &mut self.stats,
                pool: &mut self.frame.pool,
            };
            self.post.encode_group(&mut cx, &input);
            self.frame.pool.release_all();
            state.drawn = true;
        }
        state.finished = true;
        for view in earlier {
            if view.group == g && view.role == ViewRole::Display {
                self.encode_display_view(encoder, view, d);
            }
        }
    }

    /// Record one view's render pass. `first`: no earlier pass of this frame drew into the
    /// view's target.
    ///
    /// Color: a view covering its whole target clears it with its clear color (a load op). The
    /// first pass into a target otherwise clears it to black — never leaving the previous
    /// frame's content, and never painting a viewport's clear color outside the viewport —
    /// and later passes load it. A view with a viewport and a clear color then clears only its
    /// viewport, with a triangle in that color ([`Pipelines::viewport_clear`]). The viewport,
    /// in whole pixels ([`viewport_rect`]), also sets the pass's scissor rect (its part within
    /// the attachment). Depth is per view:
    /// every pass clears the whole depth attachment (views never test against each other) and
    /// discards it.
    ///
    /// [`Pipelines::viewport_clear`]: crate::pipeline::Pipelines::viewport_clear
    #[allow(clippy::too_many_arguments)]
    fn encode_pass(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &ViewData,
        first: bool,
        color: &wgpu::TextureView,
        resolve: Option<&wgpu::TextureView>,
        depth: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        samples: u32,
        output: OutputEncoding,
    ) {
        let rect = view.rect;
        let full = rect.is_none_or(|r| r.covers(view.size));
        // Scissor rects must lie within the attachment itself. It has the size the view was
        // prepared for on every path (see `surface_lost`); should that ever not hold, a scissor
        // rect outside it would fail validation and lose the whole frame's commands.
        let extent = (color.texture().width(), color.texture().height());
        let clear = view.clear.map(|c| {
            let c = if output == OutputEncoding::ManualSrgb {
                linear_to_srgb_color(c)
            } else {
                c
            };
            wgpu::Color {
                r: c.x as f64,
                g: c.y as f64,
                b: c.z as f64,
                a: c.w as f64,
            }
        });
        let (load, rect_clear) = match clear {
            Some(c) if full => (wgpu::LoadOp::Clear(c), None),
            _ if first => (wgpu::LoadOp::Clear(wgpu::Color::BLACK), clear),
            _ => (wgpu::LoadOp::Load, clear),
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("space3d forward"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color,
                depth_slice: None,
                resolve_target: resolve,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        if let Some(r) = rect {
            pass.set_viewport(r.x as f32, r.y as f32, r.w as f32, r.h as f32, 0.0, 1.0);
            let [x, y, w, h] = r.scissor(extent);
            pass.set_scissor_rect(x, y, w, h);
        }
        if let Some(c) = rect_clear {
            pass.set_pipeline(self.pipelines.viewport_clear(&self.device, format, samples));
            pass.set_blend_constant(c);
            pass.draw(0..3, 0..1);
        }
        self.encode_draws(&mut pass, view, format, samples, output);
        if rect.is_some() && resolve.is_some() {
            // wgpu's GL backend resolves MSAA with a framebuffer blit at the end of the pass,
            // and GL clips blits to the scissor rect: only the viewport would be resolved, and
            // the rest of the target would keep its previous content.
            pass.set_scissor_rect(0, 0, extent.0, extent.1);
        }
    }

    /// Whether a capture re-records the last frame into the offscreen target: windowed
    /// renderers (surface textures are not readable), or a test forcing the windowed path.
    pub(crate) fn capture_replays(&self) -> bool {
        #[cfg(test)]
        if self.frame.force_capture_replay {
            return true;
        }
        self.surface.is_some()
    }

    /// Windowed capture: re-record the last frame's primary views into the offscreen target,
    /// then the overlays ([`crate::OverlayTargetKind::Capture`]) on top of them — in that
    /// order, as in a frame (overlays recorded first would be painted over by the views).
    pub(crate) fn encode_capture_replay(&mut self, encoder: &mut wgpu::CommandEncoder) {
        self.encode_primary_to_offscreen(encoder);
        self.encode_capture_overlays(encoder);
    }

    /// Re-record the last frame's primary views into the offscreen target (for capture). The
    /// target gets the size those views were prepared for (their projection and viewports),
    /// even if the window was resized since.
    pub(crate) fn encode_primary_to_offscreen(&mut self, encoder: &mut wgpu::CommandEncoder) {
        self.size_offscreen(self.frame.last_primary_size);
        // Re-recorded passes are not new draws: keep the frame's pass counters.
        let stats = self.stats.clone();
        self.encode_views(encoder, None, true);
        self.stats = stats;
    }
}
