//! The egui [`RenderOverlay`]: draws the tessellated UI with `egui_wgpu::Renderer`.
//!
//! One `egui_wgpu::Renderer` exists per target format (the window surface and the capture
//! target can differ, e.g. `Bgra8UnormSrgb` vs `Rgba8UnormSrgb`). egui-managed textures are
//! mirrored on the CPU with a version number, so every renderer (including ones created later)
//! can be brought up to date independently. Native textures (render targets) are copied into a
//! non-sRGB twin each frame — egui samples textures as gamma-encoded values — and bound to an
//! egui-managed placeholder id, which keeps texture ids identical across renderers.

use space3d_render::wgpu;
use space3d_render::{OverlayTarget, RenderOverlay};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// Lock a typed overlay handle, recovering from poisoning.
pub(crate) fn lock<T>(m: &Arc<Mutex<T>>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct MirrorTexture {
    image: Arc<egui::ColorImage>,
    options: egui::TextureOptions,
    version: u64,
}

struct NativeTexture {
    source: wgpu::TextureView,
    /// Non-sRGB copy of an sRGB source (`None` when the source can be bound directly).
    copy: Option<wgpu::TextureView>,
    generation: u64,
}

struct FormatRenderer {
    format: wgpu::TextureFormat,
    renderer: egui_wgpu::Renderer,
    synced: HashMap<egui::TextureId, u64>,
    bound: HashMap<egui::TextureId, u64>,
}

pub(crate) struct EguiOverlay {
    primitives: Vec<egui::ClippedPrimitive>,
    pixels_per_point: f32,
    textures: HashMap<egui::TextureId, MirrorTexture>,
    pending_free: Vec<egui::TextureId>,
    natives: HashMap<egui::TextureId, NativeTexture>,
    renderers: Vec<FormatRenderer>,
    dithering: bool,
    next_version: u64,
}

impl EguiOverlay {
    pub(crate) fn new(dithering: bool) -> Self {
        Self {
            primitives: Vec::new(),
            pixels_per_point: 1.0,
            textures: HashMap::new(),
            pending_free: Vec::new(),
            natives: HashMap::new(),
            renderers: Vec::new(),
            dithering,
            next_version: 1,
        }
    }

    /// Nothing to draw (UI disabled).
    pub(crate) fn clear_frame(&mut self) {
        self.primitives.clear();
    }

    /// Install the output of one egui pass.
    pub(crate) fn set_frame(
        &mut self,
        primitives: Vec<egui::ClippedPrimitive>,
        pixels_per_point: f32,
        mut delta: egui::TexturesDelta,
        natives: Vec<(egui::TextureId, wgpu::TextureView)>,
    ) {
        // egui frees textures *after* painting the frame that freed them.
        for id in self.pending_free.drain(..) {
            self.textures.remove(&id);
        }
        for (id, deltas) in std::mem::take(&mut delta.set) {
            for d in deltas {
                self.apply_delta(id, d);
            }
        }
        self.pending_free.extend(std::mem::take(&mut delta.free));
        self.primitives = primitives;
        self.pixels_per_point = pixels_per_point;

        // Native textures: keep entries whose source is unchanged.
        let mut next = HashMap::with_capacity(natives.len());
        for (id, view) in natives {
            match self.natives.remove(&id) {
                Some(n) if n.source == view => {
                    next.insert(id, n);
                }
                _ => {
                    let generation = self.next_version;
                    self.next_version += 1;
                    next.insert(
                        id,
                        NativeTexture {
                            source: view,
                            copy: None,
                            generation,
                        },
                    );
                }
            }
        }
        self.natives = next;
    }

    fn apply_delta(&mut self, id: egui::TextureId, d: egui::epaint::ImageDelta) {
        let version = self.next_version;
        self.next_version += 1;
        let egui::ImageData::Color(image) = d.image;
        match d.pos {
            None => {
                self.textures.insert(
                    id,
                    MirrorTexture {
                        image,
                        options: d.options,
                        version,
                    },
                );
            }
            Some([x, y]) => {
                let Some(t) = self.textures.get_mut(&id) else {
                    tracing::warn!(?id, "partial update of an unknown egui texture");
                    return;
                };
                let dst = Arc::make_mut(&mut t.image);
                let [w, h] = image.size;
                let dst_w = dst.size[0];
                for row in 0..h {
                    let dy = y + row;
                    if dy >= dst.size[1] || x >= dst_w {
                        continue;
                    }
                    let n = w.min(dst_w - x);
                    let s = row * w;
                    let di = dy * dst_w + x;
                    dst.pixels[di..di + n].copy_from_slice(&image.pixels[s..s + n]);
                }
                t.options = d.options;
                t.version = version;
            }
        }
    }

    fn renderer_index(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> usize {
        if let Some(i) = self.renderers.iter().position(|r| r.format == format) {
            return i;
        }
        let renderer = egui_wgpu::Renderer::new(
            device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: self.dithering,
                ..Default::default()
            },
        );
        self.renderers.push(FormatRenderer {
            format,
            renderer,
            synced: HashMap::new(),
            bound: HashMap::new(),
        });
        self.renderers.len() - 1
    }

    /// Copy sRGB native sources into their non-sRGB twins (recreated on size change).
    fn update_natives(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder) {
        for n in self.natives.values_mut() {
            let src = n.source.texture();
            let format = src.format();
            if !format.is_srgb() {
                continue;
            }
            let size = src.size();
            let stale = n.copy.as_ref().is_none_or(|c| c.texture().size() != size);
            if stale {
                let copy = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("space3d_ui native texture copy"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: format.remove_srgb_suffix(),
                    usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                n.copy = Some(copy.create_view(&Default::default()));
                n.generation = self.next_version;
                self.next_version += 1;
            }
            if !src.usage().contains(wgpu::TextureUsages::COPY_SRC) {
                continue;
            }
            let dst = n.copy.as_ref().expect("created above").texture();
            encoder.copy_texture_to_texture(src.as_image_copy(), dst.as_image_copy(), size);
        }
    }
}

impl RenderOverlay for EguiOverlay {
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &OverlayTarget,
    ) {
        self.update_natives(device, encoder);
        let i = self.renderer_index(device, target.format);
        let r = &mut self.renderers[i];

        // Managed textures: upload what changed since this renderer last saw it.
        let stale: Vec<egui::TextureId> = r
            .synced
            .keys()
            .filter(|id| !self.textures.contains_key(id))
            .copied()
            .collect();
        for id in stale {
            r.renderer.free_texture(&id);
            r.synced.remove(&id);
            r.bound.remove(&id);
        }
        for (id, t) in &self.textures {
            if r.synced.get(id) != Some(&t.version) {
                let delta = egui::epaint::ImageDelta::full(egui::ImageData::Color(t.image.clone()), t.options);
                r.renderer.update_texture(device, queue, *id, &delta);
                r.synced.insert(*id, t.version);
                r.bound.remove(id);
            }
        }
        // Native textures replace the bind group of their placeholder id.
        for (id, n) in &self.natives {
            if !r.synced.contains_key(id) || r.bound.get(id) == Some(&n.generation) {
                continue;
            }
            let view = n.copy.as_ref().unwrap_or(&n.source);
            r.renderer
                .update_egui_texture_from_wgpu_texture(device, view, wgpu::FilterMode::Linear, *id);
            r.bound.insert(*id, n.generation);
        }

        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [target.size.0, target.size.1],
            pixels_per_point: self.pixels_per_point,
        };
        // No paint callbacks are used, so the returned command buffers are always empty.
        let _ = r
            .renderer
            .update_buffers(device, queue, encoder, &self.primitives, &screen);
    }

    fn render(&self, pass: &mut wgpu::RenderPass<'static>, target: &OverlayTarget) {
        if self.primitives.is_empty() {
            return;
        }
        let Some(r) = self.renderers.iter().find(|r| r.format == target.format) else {
            return;
        };
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [target.size.0, target.size.1],
            pixels_per_point: self.pixels_per_point,
        };
        r.renderer.render(pass, &self.primitives, &screen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(size: [usize; 2], c: egui::Color32, pos: Option<[usize; 2]>) -> egui::epaint::ImageDelta {
        let image = egui::ColorImage::filled(size, c);
        match pos {
            None => egui::epaint::ImageDelta::full(image, egui::TextureOptions::LINEAR),
            Some(p) => egui::epaint::ImageDelta::partial(p, image, egui::TextureOptions::LINEAR),
        }
    }

    #[test]
    fn mirror_applies_partial_updates_and_deferred_frees() {
        let mut o = EguiOverlay::new(false);
        let id = egui::TextureId::Managed(3);
        let mut d = egui::TexturesDelta::default();
        d.set
            .insert(id, [delta([4, 4], egui::Color32::BLACK, None)].into_iter().collect());
        o.set_frame(Vec::new(), 1.0, d, Vec::new());
        let v1 = o.textures[&id].version;

        let mut d = egui::TexturesDelta::default();
        d.set.insert(
            id,
            [delta([2, 1], egui::Color32::WHITE, Some([1, 2]))]
                .into_iter()
                .collect(),
        );
        d.free.insert(id);
        o.set_frame(Vec::new(), 1.0, d, Vec::new());
        let t = &o.textures[&id];
        assert!(t.version > v1);
        assert_eq!(t.image.pixels[2 * 4 + 1], egui::Color32::WHITE);
        assert_eq!(t.image.pixels[2 * 4 + 3], egui::Color32::BLACK);
        assert_eq!(t.image.pixels[4 + 1], egui::Color32::BLACK);
        // Freed only after the next frame.
        o.set_frame(Vec::new(), 1.0, Default::default(), Vec::new());
        assert!(o.textures.is_empty());
    }
}
