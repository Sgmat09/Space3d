//! Debug overlay (F3): FPS, frame-time sparkline, ticks, renderer statistics, entity count.
//!
//! A small panel docked at the top-left of the screen (or of the [`crate::ToolDockArea`]) that
//! never takes the pointer and looks the same under any egui style (its own frame, fonts and
//! spacing): a header with FPS, average and worst frame time, a sparkline of the last 240
//! frames, and a monospace grid of label/value pairs with right-aligned numbers.
//! [`DebugOverlay::details`] adds the rarer renderer counters.

use crate::{Console, EguiContext, Profiler};
use space3d_core::{SystemCtx, TickRate, Time};
use space3d_platform::{Input, KeyCode};
use space3d_render::RenderStats;
use std::collections::VecDeque;

const HISTORY: usize = 240;

/// Debug overlay state (resource inserted by [`crate::UiPlugin`]).
#[derive(Clone, Debug)]
pub struct DebugOverlay {
    /// Draw the overlay.
    pub visible: bool,
    /// Also show the rarer renderer counters (draw path, indirect / multi draws, batches,
    /// views, culled instances, uploaded instance bytes). Console: `debug details`.
    pub details: bool,
    frame_ms: VecDeque<f32>,
}

impl Default for DebugOverlay {
    fn default() -> Self {
        Self::new(false)
    }
}

impl DebugOverlay {
    /// Overlay, initially shown or hidden.
    pub fn new(visible: bool) -> Self {
        Self {
            visible,
            details: false,
            frame_ms: VecDeque::with_capacity(HISTORY),
        }
    }

    /// Record one frame time (done every frame by the plugin, visible or not).
    pub fn record_frame(&mut self, ms: f32) {
        if self.frame_ms.len() == HISTORY {
            self.frame_ms.pop_front();
        }
        self.frame_ms.push_back(ms);
    }

    /// Recorded frame times in milliseconds, oldest first (last 240 frames).
    pub fn frame_times(&self) -> impl Iterator<Item = f32> + '_ {
        self.frame_ms.iter().copied()
    }

    /// Average frame time over the recorded frames (ms).
    pub fn average_frame_ms(&self) -> f32 {
        if self.frame_ms.is_empty() {
            0.0
        } else {
            self.frame_ms.iter().sum::<f32>() / self.frame_ms.len() as f32
        }
    }

    /// Longest recorded frame time (ms).
    pub fn max_frame_ms(&self) -> f32 {
        self.frame_ms.iter().copied().fold(0.0, f32::max)
    }

    /// Frames per second from the average frame time.
    pub fn fps(&self) -> f32 {
        let avg = self.average_frame_ms();
        if avg > 0.0 { 1000.0 / avg } else { 0.0 }
    }

    /// Frame-time sparkline: one bar per recorded frame (green within the 60 Hz budget, amber
    /// within 30 Hz, red above), faint guides at 16.7 and 33.3 ms.
    fn sparkline(&self, ui: &mut egui::Ui, size: egui::Vec2) {
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 1.0, egui::Color32::from_white_alpha(10));
        let scale_ms = self.frame_ms.iter().copied().fold(33.4f32, f32::max).min(250.0);
        let bar_w = rect.width() / HISTORY as f32;
        let offset = HISTORY - self.frame_ms.len();
        for (i, ms) in self.frame_ms.iter().enumerate() {
            let h = ((ms / scale_ms).min(1.0) * rect.height()).max(1.0);
            let x = rect.left() + (offset + i) as f32 * bar_w;
            p.rect_filled(
                egui::Rect::from_min_max(egui::pos2(x, rect.bottom() - h), egui::pos2(x + bar_w, rect.bottom())),
                0.0,
                frame_color(*ms),
            );
        }
        for target in [1000.0 / 60.0, 1000.0 / 30.0] {
            let y = rect.bottom() - (target / scale_ms) * rect.height();
            if y > rect.top() {
                p.hline(
                    rect.x_range(),
                    y.round() + 0.5,
                    egui::Stroke::new(1.0, egui::Color32::from_white_alpha(45)),
                );
            }
        }
    }
}

fn frame_color(ms: f32) -> egui::Color32 {
    if ms <= 17.0 {
        egui::Color32::from_rgb(90, 200, 120)
    } else if ms <= 34.0 {
        egui::Color32::from_rgb(230, 190, 70)
    } else {
        egui::Color32::from_rgb(230, 80, 70)
    }
}

/// Whether `key` types nothing into a text field (function keys), so it may act as a hotkey
/// while an egui text field has keyboard focus.
fn is_function_key(key: KeyCode) -> bool {
    use KeyCode as K;
    matches!(
        key,
        K::F1
            | K::F2
            | K::F3
            | K::F4
            | K::F5
            | K::F6
            | K::F7
            | K::F8
            | K::F9
            | K::F10
            | K::F11
            | K::F12
            | K::F13
            | K::F14
            | K::F15
            | K::F16
            | K::F17
            | K::F18
            | K::F19
            | K::F20
            | K::F21
            | K::F22
            | K::F23
            | K::F24
    )
}

/// `Stage::PreUpdate`: F3 / F4 / backquote toggles.
///
/// While an egui text field has keyboard focus (as of the last pass), hotkeys that type text
/// (the default backquote console key, or any non-function key) are ignored so typing them
/// into the inspector, a path field or a knob does not toggle anything — except that the
/// console key still closes the console while the console's own input line has focus.
pub(crate) fn hotkeys(sys: &mut SystemCtx, keys: (Option<KeyCode>, Option<KeyCode>, Option<KeyCode>)) {
    let typing = sys
        .resources
        .get::<EguiContext>()
        .is_some_and(EguiContext::wants_keyboard_input);
    let console_focused = sys
        .resources
        .get::<Console>()
        .is_some_and(|c| c.visible && c.input_has_focus());
    let Some(input) = sys.resources.get::<Input>() else {
        return;
    };
    let hit = |k: Option<KeyCode>, allowed_while_typing: bool| {
        k.is_some_and(|k| input.just_pressed(k) && (!typing || allowed_while_typing || is_function_key(k)))
    };
    let (debug, profiler, console) = (hit(keys.0, false), hit(keys.1, false), hit(keys.2, console_focused));
    if debug && let Some(d) = sys.resources.get_mut::<DebugOverlay>() {
        d.visible = !d.visible;
    }
    if profiler && let Some(p) = sys.resources.get_mut::<Profiler>() {
        p.visible = !p.visible;
    }
    if console && let Some(c) = sys.resources.get_mut::<Console>() {
        c.toggle();
    }
}

/// Egui id of the overlay's area.
pub(crate) const OVERLAY_ID: &str = "space3d_debug_overlay";

/// Overlay text size (points); its own, so a game's text styles never enlarge it.
const FONT_SIZE: f32 = 11.0;
/// Label grey: 6:1 on the overlay's fill even over a white scene.
const LABEL: egui::Color32 = egui::Color32::from_gray(165);
const VALUE: egui::Color32 = egui::Color32::from_gray(235);
/// Characters per label and value column.
const LABEL_CHARS: usize = 6;
const VALUE_CHARS: usize = 8;

/// One grid row: label/value pairs in fixed-width monospace columns, values right-aligned.
fn grid_row(ui: &mut egui::Ui, pairs: &[(&str, &str)]) {
    let font = egui::FontId::monospace(FONT_SIZE);
    let format = |color| egui::TextFormat::simple(font.clone(), color);
    let mut job = egui::text::LayoutJob::default();
    for (i, (label, value)) in pairs.iter().enumerate() {
        let gap = if i == 0 { "" } else { "  " };
        job.append(&format!("{gap}{label:<LABEL_CHARS$}"), 0.0, format(LABEL));
        job.append(&format!("{value:>VALUE_CHARS$}"), 0.0, format(VALUE));
    }
    ui.label(job);
}

/// Width of a two-pair grid row (the overlay's content width).
fn grid_width(ui: &egui::Ui) -> f32 {
    let chars = 2 * (LABEL_CHARS + VALUE_CHARS) + 2;
    let font = egui::FontId::monospace(FONT_SIZE);
    ui.fonts_mut(|f| f.glyph_width(&font, '0')) * chars as f32
}

/// The overlay's last line: the keys of the other tools (`"F4 profiler · ` console"`), empty
/// without keys.
pub(crate) fn tool_keys_hint(profiler: Option<KeyCode>, console: Option<KeyCode>) -> String {
    let name = |k: KeyCode| match k {
        KeyCode::Backquote => "`".to_owned(),
        k => format!("{k:?}"),
    };
    let parts: Vec<String> = [(profiler, "profiler"), (console, "console")]
        .into_iter()
        .filter_map(|(k, what)| k.map(|k| format!("{} {what}", name(k))))
        .collect();
    parts.join(" · ")
}

/// `Stage::Update`: record the frame time and draw the overlay (`hint`: [`tool_keys_hint`]).
pub(crate) fn debug_overlay_system(sys: &mut SystemCtx, hint: &str) {
    let ctx = EguiContext::active_context(sys);
    let (delta_ms, time) = match sys.resources.get::<Time>() {
        Some(t) => (t.delta.as_secs_f32() * 1000.0, t.clone()),
        None => return,
    };
    let Some(overlay) = sys.resources.get_mut::<DebugOverlay>() else {
        return;
    };
    if time.frame > 1 {
        overlay.record_frame(delta_ms);
    }
    let Some(ctx) = ctx else { return };
    if !overlay.visible {
        return;
    }
    let overlay = overlay.clone();
    let rate = sys.resources.get::<TickRate>().map_or(0.0, |r| r.hz);
    let stats = sys.resources.get::<RenderStats>().cloned();
    let ui_stats = sys.resources.get::<EguiContext>().map(EguiContext::stats);
    let entities = sys.world.len() as u64;
    let edge = crate::tool::EDGE;
    let dock = crate::tool::dock_rect(sys.resources, &ctx);

    egui::Area::new(egui::Id::new(OVERLAY_ID))
        .fixed_pos(dock.left_top() + egui::vec2(edge, edge))
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(&ctx, |ui| {
            // Practically opaque: the renderer blends in linear space, where even 92 % black
            // over a white scene is a mid grey that washes the labels out (2.5:1); at 98 %
            // the labels keep 6:1 over any scene.
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(250))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_white_alpha(22)))
                .corner_radius(3)
                .inner_margin(egui::Margin::symmetric(6, 4))
                .show(ui, |ui| {
                    crate::tool::derived_style(ui, "overlay", |style| {
                        style.wrap_mode = Some(egui::TextWrapMode::Extend);
                        style.spacing.item_spacing = egui::vec2(4.0, 1.0);
                    });
                    let width = grid_width(ui);
                    let fps = overlay.fps();
                    // 30 characters, like a grid row.
                    let header = format!(
                        "{fps:>5.1} fps {:>6.2} ms  max {:>5.1}",
                        overlay.average_frame_ms(),
                        overlay.max_frame_ms()
                    );
                    ui.label(
                        egui::RichText::new(header)
                            .font(egui::FontId::monospace(FONT_SIZE))
                            .strong()
                            .color(egui::Color32::WHITE),
                    );
                    overlay.sparkline(ui, egui::vec2(width, 22.0));
                    let n = |v: u64| crate::tool::si(v);
                    grid_row(ui, &[("frame", &n(time.frame)), ("tick", &n(time.tick))]);
                    grid_row(ui, &[("rate", &format!("{rate} Hz")), ("drop", &n(time.dropped_ticks))]);
                    grid_row(ui, &[("ents", &n(entities)), ("alpha", &format!("{:.2}", time.alpha))]);
                    if let Some(s) = &stats {
                        grid_row(ui, &[("draws", &n(s.draw_calls.into())), ("tris", &n(s.triangles))]);
                        grid_row(
                            ui,
                            &[
                                ("inst", &n(s.instances_drawn)),
                                ("upload", &crate::tool::short_bytes(s.bytes_uploaded)),
                            ],
                        );
                        if overlay.details {
                            let path = format!("{:?}{}", s.draw_path, if s.gpu_culling { " +gpu cull" } else { "" });
                            ui.label(
                                egui::RichText::new(path)
                                    .font(egui::FontId::monospace(FONT_SIZE))
                                    .color(LABEL),
                            );
                            grid_row(
                                ui,
                                &[
                                    ("indir", &n(s.indirect_draw_calls.into())),
                                    ("multi", &n(s.multi_draw_calls.into())),
                                ],
                            );
                            grid_row(ui, &[("batch", &n(s.batches.into())), ("views", &n(s.views.into()))]);
                            grid_row(
                                ui,
                                &[
                                    ("culled", &n(s.culled)),
                                    ("inst B", &crate::tool::short_bytes(s.instance_bytes_uploaded)),
                                ],
                            );
                        }
                    }
                    if let Some(u) = ui_stats {
                        grid_row(
                            ui,
                            &[
                                ("egui", &format!("{} prim", u.primitives)),
                                ("verts", &n(u.vertices as u64)),
                            ],
                        );
                    }
                    if !hint.is_empty() {
                        ui.label(
                            egui::RichText::new(hint)
                                .font(egui::FontId::monospace(FONT_SIZE))
                                .color(LABEL),
                        );
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_hint() {
        assert_eq!(
            tool_keys_hint(Some(KeyCode::F4), Some(KeyCode::Backquote)),
            "F4 profiler · ` console"
        );
        assert_eq!(tool_keys_hint(None, Some(KeyCode::F9)), "F9 console");
        assert_eq!(tool_keys_hint(None, None), "");
    }

    #[test]
    fn frame_history_and_fps() {
        let mut d = DebugOverlay::new(true);
        for _ in 0..10 {
            d.record_frame(20.0);
        }
        assert!((d.fps() - 50.0).abs() < 1e-3);
        d.record_frame(40.0);
        assert_eq!(d.max_frame_ms(), 40.0);
        for _ in 0..HISTORY {
            d.record_frame(10.0);
        }
        assert_eq!(d.frame_times().count(), HISTORY);
        assert!((d.average_frame_ms() - 10.0).abs() < 1e-4);
        assert_eq!(d.max_frame_ms(), 10.0, "old frames leave the history");
    }
}
