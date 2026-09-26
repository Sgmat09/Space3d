//! Shared look of the engine's in-game tools (debug overlay, profiler, console).
//!
//! The tools show up inside games that style egui themselves, so they never read their sizes
//! from the global style and never change it: each tool applies [`TOOL_DENSITY`] to its own
//! `Ui` (a scoped copy of the style), keeps the context's colors, and draws its own compact
//! header instead of egui's title bar (whose height follows the global heading size).

use crate::theme::{UiDensity, apply_density};

/// Density of the engine tools: the compact preset with slightly smaller text.
pub(crate) const TOOL_DENSITY: UiDensity = UiDensity {
    body_size: 12.0,
    button_size: 12.0,
    monospace_size: 11.5,
    small_size: 10.0,
    heading_size: 12.5,
    ..UiDensity::compact()
};

/// Apply the tool density to `ui` only (the global style is untouched).
pub(crate) fn scope_style(ui: &mut egui::Ui) {
    apply_density(ui.style_mut(), &TOOL_DENSITY);
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
}

/// A tool window at `rect` (its default position and size): compact frame and header row
/// (title, `summary`, close button) under any global style. `open` is cleared by the close
/// button. The contents run with the tool density.
pub(crate) fn window<R>(
    ctx: &egui::Context,
    (id, title): (&str, &str),
    rect: egui::Rect,
    open: &mut bool,
    summary: impl FnOnce(&mut egui::Ui),
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let style = ctx.global_style();
    let frame = egui::Frame::window(&style)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .corner_radius(4);
    let mut close = false;
    let out = egui::Window::new(title)
        .id(egui::Id::new(id))
        .title_bar(false)
        .frame(frame)
        .default_pos(rect.min)
        .default_size(rect.size())
        .min_size([160.0, 60.0])
        .show(ctx, |ui| {
            scope_style(ui);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = close_button(ui).clicked();
                    summary(ui);
                });
            });
            ui.separator();
            contents(ui)
        });
    if close {
        *open = false;
    }
    out.and_then(|r| r.inner)
}

/// A small "×" button (drawn, so it needs no glyph from the game's fonts).
fn close_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    if response.hovered() {
        ui.painter().rect_filled(rect, 2.0, visuals.weak_bg_fill);
    }
    let r = rect.shrink(4.0);
    let stroke = egui::Stroke::new(1.2, visuals.fg_stroke.color);
    ui.painter().line_segment([r.left_top(), r.right_bottom()], stroke);
    ui.painter().line_segment([r.right_top(), r.left_bottom()], stroke);
    response.on_hover_text("Close")
}

/// `n` with a k / M suffix (`950`, `12.3k`, `4.56M`).
pub(crate) fn si(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.2}G", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.2}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// Byte count in at most 6 characters, binary units (`512B`, `2.0K`, `598K`, `1.5M`, `2.3G`).
pub(crate) fn short_bytes(n: u64) -> String {
    let n = n as f64;
    let unit = |v: f64, suffix: &str| {
        if v < 10.0 {
            format!("{v:.1}{suffix}")
        } else {
            format!("{v:.0}{suffix}")
        }
    };
    match n {
        n if n >= (1u64 << 30) as f64 => unit(n / (1u64 << 30) as f64, "G"),
        n if n >= (1u64 << 20) as f64 => unit(n / (1u64 << 20) as f64, "M"),
        n if n >= 1024.0 => unit(n / 1024.0, "K"),
        n => format!("{n}B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formats() {
        assert_eq!(si(950), "950");
        assert_eq!(si(9_999), "9999");
        assert_eq!(si(12_345), "12.3k");
        assert_eq!(si(4_560_000), "4.56M");
        assert_eq!(short_bytes(512), "512B");
        assert_eq!(short_bytes(2048), "2.0K");
        assert_eq!(short_bytes(612_400), "598K");
        assert_eq!(short_bytes(3 << 20), "3.0M");
        assert_eq!(short_bytes(1000 << 20), "1000M");
    }
}
