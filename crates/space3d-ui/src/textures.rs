//! [`UiTextures`]: show GPU textures (render targets) inside egui.

use space3d_core::Resources;
use space3d_render::wgpu;
use space3d_render::{RenderAssets, TextureHandle};
use std::sync::Arc;

enum Source {
    RenderTarget(TextureHandle),
    View(wgpu::TextureView),
}

struct Entry {
    id: egui::TextureId,
    source: Source,
}

/// Registry of native textures shown in egui (resource inserted by [`crate::UiPlugin`]).
///
/// A `Camera3d` rendering into `RenderAssets::add_render_target` can be displayed with
/// `ui.image((id, size))` after [`UiTextures::register_render_target`]. Each frame the target
/// is copied into an egui-compatible (non-sRGB) texture after the 3D views were rendered, so
/// the image always shows the current frame.
///
/// ```no_run
/// # use space3d_core::prelude::*;
/// # use space3d_render::*;
/// # use space3d_ui::*;
/// # let mut app = App::new();
/// let target = app.resources.expect_mut::<RenderAssets>().add_render_target(256, 256);
/// app.world.spawn((Transform::from_xyz(0.0, 0.0, 3.0), Camera3d::default().with_target(RenderTarget::Texture(target))));
/// let ctx = app.resources.expect::<EguiContext>().ctx().clone();
/// let id = app.resources.expect_mut::<UiTextures>().register_render_target(&ctx, target);
/// app.add_ui_system(move |ctx, _| {
///     egui::Window::new("Preview").show(ctx, |ui| ui.image((id, egui::vec2(256.0, 256.0))));
/// });
/// ```
#[derive(Default)]
pub struct UiTextures {
    entries: Vec<Entry>,
}

impl std::fmt::Debug for UiTextures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiTextures").field("len", &self.entries.len()).finish()
    }
}

fn placeholder(ctx: &egui::Context, name: &str) -> egui::TextureId {
    let image = egui::ColorImage::filled([1, 1], egui::Color32::TRANSPARENT);
    ctx.tex_manager().write().alloc(
        name.to_owned(),
        egui::ImageData::Color(Arc::new(image)),
        egui::TextureOptions::LINEAR,
    )
}

impl UiTextures {
    /// Show a render target (created with `RenderAssets::add_render_target`) in egui.
    /// Registering the same target twice returns the same id.
    pub fn register_render_target(&mut self, ctx: &egui::Context, target: TextureHandle) -> egui::TextureId {
        if let Some(id) = self.render_target_id(target) {
            return id;
        }
        let id = placeholder(ctx, "space3d render target");
        self.entries.push(Entry {
            id,
            source: Source::RenderTarget(target),
        });
        id
    }

    /// Show an arbitrary texture view. sRGB textures need `COPY_SRC` usage (they are copied
    /// into a non-sRGB twin each frame); other formats are sampled directly.
    pub fn register_view(&mut self, ctx: &egui::Context, view: wgpu::TextureView) -> egui::TextureId {
        let id = placeholder(ctx, "space3d native texture");
        self.entries.push(Entry {
            id,
            source: Source::View(view),
        });
        id
    }

    /// egui id of a registered render target.
    pub fn render_target_id(&self, target: TextureHandle) -> Option<egui::TextureId> {
        self.entries.iter().find_map(|e| match e.source {
            Source::RenderTarget(t) if t == target => Some(e.id),
            _ => None,
        })
    }

    /// Stop showing a texture and free its egui id.
    pub fn unregister(&mut self, ctx: &egui::Context, id: egui::TextureId) -> bool {
        let Some(i) = self.entries.iter().position(|e| e.id == id) else {
            return false;
        };
        self.entries.remove(i);
        ctx.tex_manager().write().free(id);
        true
    }

    /// Number of registered textures.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Nothing registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Views for this frame (render targets appear once the renderer created them).
pub(crate) fn resolve_native_views(resources: &Resources) -> Vec<(egui::TextureId, wgpu::TextureView)> {
    let Some(t) = resources.get::<UiTextures>() else {
        return Vec::new();
    };
    let assets = resources.get::<RenderAssets>();
    t.entries
        .iter()
        .filter_map(|e| match &e.source {
            Source::RenderTarget(h) => assets?.render_target_view(*h).map(|v| (e.id, v)),
            Source::View(v) => Some((e.id, v.clone())),
        })
        .collect()
}
