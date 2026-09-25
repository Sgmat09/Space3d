//! Render passes: every view's forward pass into its target (clears, viewports, MSAA resolve),
//! the clear of a window nothing drew to, and the windowed-capture replay.

use super::views::{ViewData, ViewTarget};
use crate::output::OutputEncoding;
use crate::pipeline::OFFSCREEN_FORMAT;
use crate::renderer::Renderer;
use glam::Vec4;

pub(super) fn linear_to_srgb_color(c: Vec4) -> Vec4 {
    use crate::components::linear_to_srgb;
    Vec4::new(linear_to_srgb(c.x), linear_to_srgb(c.y), linear_to_srgb(c.z), c.w)
}

impl Renderer {
    /// Record render passes for the prepared views. With `primary_to_offscreen`, only primary
    /// views are re-recorded into the offscreen target (windowed capture).
    ///
    /// Views sharing a target (split screen, picture-in-picture, overlays) draw in camera
    /// order into the same attachments: the first pass into a target in a frame clears it,
    /// later ones load what the earlier ones drew (see [`Self::encode_pass`]).
    pub(crate) fn encode_views(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        surface_view: Option<&wgpu::TextureView>,
        primary_to_offscreen: bool,
    ) {
        let views = std::mem::take(&mut self.frame.views);
        let mut presented = false;
        for (i, view) in views.iter().enumerate() {
            let is_primary = matches!(view.target, ViewTarget::Primary);
            if primary_to_offscreen && !is_primary {
                continue;
            }
            // Resolve attachments.
            let (color, resolve, depth, format, samples, manual_srgb) = match (&view.target, surface_view) {
                (ViewTarget::Primary, Some(sv)) if !primary_to_offscreen => {
                    let s = self.surface.as_ref().expect("surface");
                    let a = &s.attachments;
                    match &a.msaa_view {
                        Some(m) => (
                            m.clone(),
                            Some(sv.clone()),
                            a.depth_view.clone(),
                            a.format,
                            a.samples,
                            s.manual_srgb,
                        ),
                        None => (sv.clone(), None, a.depth_view.clone(), a.format, 1, s.manual_srgb),
                    }
                }
                (ViewTarget::Primary, _) => {
                    if !primary_to_offscreen && self.surface.is_some() {
                        // Surface frame unavailable (minimized/occluded): skip.
                        continue;
                    }
                    let a = &self.offscreen;
                    let target = a.color_view.clone().expect("offscreen color");
                    match &a.msaa_view {
                        Some(m) => (
                            m.clone(),
                            Some(target),
                            a.depth_view.clone(),
                            a.format,
                            a.samples,
                            false,
                        ),
                        None => (target, None, a.depth_view.clone(), a.format, 1, false),
                    }
                }
                (ViewTarget::Texture { handle, color, depth }, _) => {
                    if view.feedback
                        && let Some(fb) = self.frame.feedback.get(handle)
                    {
                        // Snapshot the target before this pass draws into it; the view's
                        // materials sample the snapshot.
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
                    (color.clone(), None, depth.clone(), OFFSCREEN_FORMAT, 1, false)
                }
            };
            // Texture views are never skipped, except all of them in `primary_to_offscreen`.
            let first = match &view.target {
                ViewTarget::Primary => !presented,
                ViewTarget::Texture { handle, .. } => !views[..i]
                    .iter()
                    .any(|v| matches!(&v.target, ViewTarget::Texture { handle: h, .. } if h == handle)),
            };
            presented |= is_primary;
            // The view's encoding, except that only manual-sRGB attachments get manual sRGB (a
            // capture replay of a manual-sRGB window draws into the sRGB offscreen target).
            let output = match view.output {
                OutputEncoding::ManualSrgb | OutputEncoding::Linear if manual_srgb => OutputEncoding::ManualSrgb,
                OutputEncoding::ManualSrgb => OutputEncoding::Linear,
                other => other,
            };
            self.encode_pass(
                encoder,
                view,
                first,
                &color,
                resolve.as_ref(),
                &depth,
                format,
                samples,
                output,
            );
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
