//! Debug overlay (F3): FPS, frame-time graph, ticks, renderer statistics, entity count.

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

    /// Frames per second from the average frame time.
    pub fn fps(&self) -> f32 {
        let avg = self.average_frame_ms();
        if avg > 0.0 { 1000.0 / avg } else { 0.0 }
    }

    fn graph(&self, ui: &mut egui::Ui) {
        let size = egui::vec2(220.0, 48.0);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 2.0, egui::Color32::from_black_alpha(120));
        let scale_ms = self.frame_ms.iter().copied().fold(33.4f32, f32::max).min(250.0);
        let bar_w = rect.width() / HISTORY as f32;
        let offset = HISTORY - self.frame_ms.len();
        for (i, ms) in self.frame_ms.iter().enumerate() {
            let h = (ms / scale_ms).min(1.0) * rect.height();
            let x = rect.left() + (offset + i) as f32 * bar_w;
            let color = if *ms <= 17.0 {
                egui::Color32::from_rgb(90, 200, 120)
            } else if *ms <= 34.0 {
                egui::Color32::from_rgb(230, 190, 70)
            } else {
                egui::Color32::from_rgb(230, 80, 70)
            };
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x, rect.bottom() - h),
                    egui::pos2(x + bar_w.max(1.0), rect.bottom()),
                ),
                0.0,
                color,
            );
        }
        for target in [1000.0 / 60.0, 1000.0 / 30.0] {
            let y = rect.bottom() - (target / scale_ms) * rect.height();
            if y > rect.top() {
                p.hline(
                    rect.x_range(),
                    y,
                    egui::Stroke::new(1.0, egui::Color32::from_white_alpha(60)),
                );
            }
        }
        if let Some(pos) = response.hover_pos() {
            let i = ((pos.x - rect.left()) / bar_w) as usize;
            if let Some(ms) = i.checked_sub(offset).and_then(|i| self.frame_ms.get(i)) {
                response.on_hover_text(format!("{ms:.2} ms"));
            }
        }
    }
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.2} MiB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KiB", n as f64 / 1024.0),
        n => format!("{n} B"),
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

/// `Stage::Update`: record the frame time and draw the overlay.
pub(crate) fn debug_overlay_system(sys: &mut SystemCtx) {
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
    let entities = sys.world.len();

    egui::Area::new(egui::Id::new("space3d_debug_overlay"))
        .anchor(egui::Align2::LEFT_TOP, [8.0, 8.0])
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(egui::Color32::from_black_alpha(190))
                .show(ui, |ui| {
                    ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
                    let avg = overlay.average_frame_ms();
                    ui.label(
                        egui::RichText::new(format!("{:>6.1} FPS  {avg:>6.2} ms", overlay.fps()))
                            .strong()
                            .color(egui::Color32::WHITE),
                    );
                    overlay.graph(ui);
                    ui.label(format!(
                        "frame {}  tick {} @ {rate} Hz\ndropped {}  alpha {:.2}",
                        time.frame, time.tick, time.dropped_ticks, time.alpha
                    ));
                    ui.label(format!("entities {entities}"));
                    if let Some(s) = stats {
                        ui.label(format!(
                            "render {:?}{}\ndraws {} (indirect {}, multi {})\nbatches {}  views {}\ninstances {} / drawn {}\ntriangles {}\nuploaded {} (instances {})",
                            s.draw_path,
                            if s.gpu_culling { " +gpu cull" } else { "" },
                            s.draw_calls,
                            s.indirect_draw_calls,
                            s.multi_draw_calls,
                            s.batches,
                            s.views,
                            s.instances,
                            s.instances_drawn,
                            s.triangles,
                            bytes(s.bytes_uploaded),
                            bytes(s.instance_bytes_uploaded),
                        ));
                    }
                    if let Some(u) = ui_stats {
                        ui.label(format!("ui {} prims  {} verts", u.primitives, u.vertices));
                    }
                    ui.label(egui::RichText::new("F3 overlay · F4 profiler · ` console").weak());
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_history_and_fps() {
        let mut d = DebugOverlay::new(true);
        for _ in 0..10 {
            d.record_frame(20.0);
        }
        assert!((d.fps() - 50.0).abs() < 1e-3);
        for _ in 0..HISTORY {
            d.record_frame(10.0);
        }
        assert_eq!(d.frame_times().count(), HISTORY);
        assert!((d.average_frame_ms() - 10.0).abs() < 1e-4);
        assert_eq!(bytes(2048), "2.0 KiB");
    }
}
