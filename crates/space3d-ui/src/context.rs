//! [`EguiContext`] resource and the per-frame begin/end systems.

use crate::UiPlugin;
use crate::input::InputTranslator;
use crate::overlay::EguiOverlay;
use space3d_core::glam::Vec2;
use space3d_core::{SystemCtx, Time};
use space3d_platform::{Input, WindowState};
use space3d_render::{RenderOverlays, Renderer};
use std::sync::{Arc, Mutex};

/// Statistics of the last finished egui pass.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UiFrameStats {
    /// Completed egui passes since startup.
    pub passes: u64,
    /// Tessellated clipped primitives.
    pub primitives: usize,
    /// Vertices uploaded for the frame.
    pub vertices: usize,
    /// Indices uploaded for the frame.
    pub indices: usize,
    /// egui-managed textures alive.
    pub textures: usize,
    /// Physical pixels per point used for tessellation.
    pub pixels_per_point: f32,
}

/// The egui context of the app (resource inserted by [`UiPlugin`]).
///
/// Between `Stage::PreUpdate` and `Stage::PostUpdate` an egui pass is active: UI code in
/// `Stage::Update` (see [`crate::UiAppExt`]) can use [`EguiContext::ctx`] directly.
pub struct EguiContext {
    ctx: egui::Context,
    in_pass: bool,
    enabled: bool,
    available_rect: egui::Rect,
    root_uis: u64,
    pixels_per_point: f32,
    screen_size_px: [u32; 2],
    wants_pointer: bool,
    wants_keyboard: bool,
    pointer_over: bool,
    injected: Vec<egui::Event>,
    clipboard: String,
    cursor_icon: egui::CursorIcon,
    stats: UiFrameStats,
}

impl std::fmt::Debug for EguiContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EguiContext")
            .field("in_pass", &self.in_pass)
            .field("enabled", &self.enabled)
            .field("pixels_per_point", &self.pixels_per_point)
            .field("screen_size_px", &self.screen_size_px)
            .finish()
    }
}

impl EguiContext {
    pub(crate) fn new(ctx: egui::Context, settings: &UiPlugin) -> Self {
        let (w, h) = settings.fallback_size;
        Self {
            ctx,
            in_pass: false,
            enabled: true,
            available_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w as f32, h as f32)),
            root_uis: 0,
            pixels_per_point: settings.pixels_per_point.unwrap_or(1.0),
            screen_size_px: [w, h],
            wants_pointer: false,
            wants_keyboard: false,
            pointer_over: false,
            injected: Vec::new(),
            clipboard: String::new(),
            cursor_icon: egui::CursorIcon::Default,
            stats: UiFrameStats::default(),
        }
    }

    /// The egui context (cheap to clone).
    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    /// Whether an egui pass is running (between `PreUpdate` and `PostUpdate`).
    pub fn in_pass(&self) -> bool {
        self.in_pass
    }

    /// Enable/disable the whole UI: while disabled no pass runs and nothing is drawn.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// See [`Self::set_enabled`].
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Physical pixels per egui point of the current/last pass.
    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    /// Size of the UI area in physical pixels.
    pub fn screen_size_px(&self) -> [u32; 2] {
        self.screen_size_px
    }

    /// Convert a physical pixel position (e.g. `Input::mouse_position`) to egui points.
    pub fn to_points(&self, physical: Vec2) -> egui::Pos2 {
        egui::pos2(physical.x / self.pixels_per_point, physical.y / self.pixels_per_point)
    }

    /// Convert egui points to physical pixels (e.g. for `Input::inject_mouse_move`).
    pub fn to_physical(&self, points: egui::Pos2) -> Vec2 {
        Vec2::new(points.x * self.pixels_per_point, points.y * self.pixels_per_point)
    }

    /// egui is using the pointer (hovering a window, dragging a slider...) as of the last
    /// finished pass — games should ignore mouse input then.
    pub fn wants_pointer_input(&self) -> bool {
        self.wants_pointer
    }

    /// A text field has keyboard focus (as of the last finished pass).
    pub fn wants_keyboard_input(&self) -> bool {
        self.wants_keyboard
    }

    /// The pointer is over any egui area (as of the last finished pass).
    pub fn is_pointer_over_ui(&self) -> bool {
        self.pointer_over
    }

    /// Queue a raw egui event for the next pass (tests, custom platforms, IME...).
    pub fn push_event(&mut self, event: egui::Event) {
        self.injected.push(event);
    }

    /// The UI clipboard: text copied in egui lands here; `Ctrl/Cmd+V` pastes it.
    pub fn clipboard(&self) -> &str {
        &self.clipboard
    }

    /// Replace the clipboard contents (pasted with `Ctrl/Cmd+V`).
    pub fn set_clipboard(&mut self, text: impl Into<String>) {
        self.clipboard = text.into();
    }

    /// Cursor icon egui asked for in the last pass.
    pub fn cursor_icon(&self) -> egui::CursorIcon {
        self.cursor_icon
    }

    /// Statistics of the last finished pass.
    pub fn stats(&self) -> UiFrameStats {
        self.stats
    }

    /// The egui context if a pass is active (for systems that draw UI).
    pub fn active_context(sys: &SystemCtx) -> Option<egui::Context> {
        let e = sys.resources.get::<EguiContext>()?;
        (e.in_pass && e.enabled).then(|| e.ctx.clone())
    }

    /// Run `f` with the frame's root [`egui::Ui`], i.e. the screen area not yet claimed by
    /// panels added through earlier root-ui calls this frame. Returns `None` when no pass is
    /// active.
    pub fn with_root_ui<R>(sys: &mut SystemCtx, f: impl FnOnce(&mut egui::Ui, &mut SystemCtx) -> R) -> Option<R> {
        let (ctx, rect, id) = {
            let e = sys.resources.get_mut::<EguiContext>()?;
            if !e.in_pass || !e.enabled {
                return None;
            }
            e.root_uis += 1;
            (
                e.ctx.clone(),
                e.available_rect,
                egui::Id::new(("space3d_root_ui", e.root_uis)),
            )
        };
        let mut ui = egui::Ui::new(
            ctx,
            id,
            egui::UiBuilder::new()
                .layer_id(egui::LayerId::background())
                .max_rect(rect),
        );
        let out = f(&mut ui, sys);
        if let Some(e) = sys.resources.get_mut::<EguiContext>() {
            e.available_rect = ui.available_rect_before_wrap();
        }
        Some(out)
    }

    /// Screen area not yet claimed by root-ui panels this frame (points).
    pub fn available_rect(&self) -> egui::Rect {
        self.available_rect
    }
}

/// Screen size (physical px), native pixels-per-point and focus for this frame.
fn screen_info(sys: &SystemCtx, settings: &UiPlugin) -> ([u32; 2], f32, bool, usize) {
    let window = sys.resources.get::<WindowState>();
    let renderer = sys.resources.get::<Renderer>();
    let size = match (renderer, window) {
        (Some(r), _) => {
            let (w, h) = r.size();
            [w, h]
        }
        (None, Some(w)) => [w.physical_size.x, w.physical_size.y],
        (None, None) => [settings.fallback_size.0, settings.fallback_size.1],
    };
    let native_ppp = settings
        .pixels_per_point
        .unwrap_or_else(|| window.map_or(1.0, |w| w.scale_factor as f32))
        .max(0.1);
    let focused = window.is_none_or(|w| w.focused);
    let max_texture_side = renderer.map_or(2048, |r| r.device().limits().max_texture_dimension_2d as usize);
    (size, native_ppp, focused, max_texture_side)
}

/// `Stage::PreUpdate`: translate input and begin the egui pass.
pub(crate) fn begin_frame(sys: &mut SystemCtx, translator: &mut InputTranslator, settings: &UiPlugin) {
    let (size, native_ppp, focused, max_texture_side) = screen_info(sys, settings);
    let (time, dt) = sys
        .resources
        .get::<Time>()
        .map_or((0.0, 0.0), |t| (t.elapsed_secs(), t.delta_secs()));
    let Some(e) = sys.resources.get_mut::<EguiContext>() else {
        return;
    };
    if e.in_pass {
        // The previous pass was never ended (e.g. PostUpdate was skipped): drop it.
        let _ = e.ctx.end_pass();
        e.in_pass = false;
    }
    if !e.enabled {
        return;
    }
    let ppp = native_ppp * e.ctx.zoom_factor();
    let clipboard = e.clipboard.clone();
    let injected = std::mem::take(&mut e.injected);

    let mut events = Vec::new();
    if let Some(input) = sys.resources.get::<Input>() {
        translator.translate(input, ppp, &clipboard, &mut events);
    }
    events.extend(injected);
    if let Some(o) = sys.resources.get_mut::<RenderOverlays>() {
        o.set_scale_factor(ppp);
    }

    let e = sys.resources.expect_mut::<EguiContext>();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(size[0] as f32 / ppp, size[1] as f32 / ppp));
    let mut raw = egui::RawInput {
        screen_rect: Some(screen),
        max_texture_side: Some(max_texture_side),
        time: Some(time),
        predicted_dt: if dt > 0.0 {
            dt.clamp(1.0 / 1000.0, 0.25)
        } else {
            1.0 / 60.0
        },
        events,
        focused,
        ..Default::default()
    };
    raw.viewports.insert(
        egui::ViewportId::ROOT,
        egui::ViewportInfo {
            native_pixels_per_point: Some(native_ppp),
            inner_rect: Some(screen),
            focused: Some(focused),
            ..Default::default()
        },
    );
    e.pixels_per_point = ppp;
    e.screen_size_px = size;
    e.available_rect = screen;
    e.root_uis = 0;
    e.ctx.begin_pass(raw);
    e.in_pass = true;
}

/// `Stage::PostUpdate`: end the pass, tessellate and hand the frame to the render overlay.
pub(crate) fn end_frame(sys: &mut SystemCtx, overlay: &Arc<Mutex<EguiOverlay>>) {
    let natives = crate::textures::resolve_native_views(sys.resources);
    let Some(e) = sys.resources.get_mut::<EguiContext>() else {
        return;
    };
    if !e.in_pass {
        if !e.enabled {
            // Nothing to draw while disabled.
            crate::overlay::lock(overlay).clear_frame();
        }
        return;
    }
    e.in_pass = false;
    // Query interaction state while the pass state is still the current one.
    // (egui's own `is_pointer_over_egui` relies on `run_ui` bookkeeping, so the background
    // layer is tested against our root-ui available rect instead.)
    let pointer = e.ctx.input(|i| i.pointer.interact_pos());
    e.pointer_over = pointer.is_some_and(|p| {
        e.ctx
            .layer_id_at(p)
            .is_some_and(|l| l.order != egui::Order::Background || !e.available_rect.contains(p))
    });
    let any_down = e.ctx.input(|i| i.pointer.any_down());
    e.wants_pointer = e.ctx.egui_is_using_pointer() || (e.pointer_over && !any_down);
    e.wants_keyboard = e.ctx.egui_wants_keyboard_input();
    let output = e.ctx.end_pass();
    for cmd in &output.platform_output.commands {
        if let egui::OutputCommand::CopyText(text) = cmd {
            e.clipboard.clone_from(text);
        }
    }
    e.cursor_icon = output.platform_output.cursor_icon;
    let ppp = output.pixels_per_point;
    let primitives = e.ctx.tessellate(output.shapes, ppp);
    let (mut vertices, mut indices) = (0, 0);
    for p in &primitives {
        if let egui::epaint::Primitive::Mesh(m) = &p.primitive {
            vertices += m.vertices.len();
            indices += m.indices.len();
        }
    }
    e.stats = UiFrameStats {
        passes: e.stats.passes + 1,
        primitives: primitives.len(),
        vertices,
        indices,
        textures: e.ctx.tex_manager().read().num_allocated(),
        pixels_per_point: ppp,
    };
    crate::overlay::lock(overlay).set_frame(primitives, ppp, output.textures_delta, natives);
}
