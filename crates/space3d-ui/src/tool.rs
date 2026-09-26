//! Shared look of the engine's in-game tools (debug overlay, profiler, console).
//!
//! The tools show up inside games that style egui themselves, so they never read their sizes
//! from the global style and never change it: each tool applies [`TOOL_DENSITY`] to its own
//! `Ui` (a scoped copy of the style), keeps the context's colors, and draws its own compact
//! header instead of egui's title bar (whose height follows the global heading size).

use crate::theme::{UiDensity, apply_density};
use std::sync::Arc;

/// Density of the engine tools: the compact preset with slightly smaller text.
pub(crate) const TOOL_DENSITY: UiDensity = UiDensity {
    body_size: 12.0,
    button_size: 12.0,
    monospace_size: 11.5,
    small_size: 10.0,
    heading_size: 12.5,
    ..UiDensity::compact()
};

/// Gap between a docked tool window and the screen edges (and between two tools).
pub(crate) const EDGE: f32 = 6.0;

/// Inner margin of a tool window (a little more at the bottom than at the top, so the last row
/// never touches the border or the resize grip).
const MARGIN: egui::Margin = egui::Margin {
    left: 6,
    right: 6,
    top: 4,
    bottom: 6,
};

/// Scroll bars that stay visible while the content overflows (egui's default floating bars
/// only show on hover, so a clipped list gives no cue that it continues).
pub(crate) fn visible_scroll_bars() -> egui::style::ScrollStyle {
    egui::style::ScrollStyle {
        bar_inner_margin: 2.0,
        dormant_background_opacity: 0.4,
        dormant_handle_opacity: 0.6,
        ..egui::style::ScrollStyle::solid()
    }
}

/// The tool style derived from `base` (and the `base` it was derived from), cached so a tool
/// costs no style clone per frame while the context's style is unchanged.
#[derive(Clone)]
struct ScopedStyle {
    base: Arc<egui::Style>,
    scoped: Arc<egui::Style>,
}

/// Apply the tool density and visible scroll bars to `ui` only (the global style is
/// untouched). Text keeps egui's per-layout wrapping; rows that must stay on one line say so.
pub(crate) fn scope_style(ui: &mut egui::Ui) {
    let id = egui::Id::new("space3d_ui::tool_style");
    let base = ui.style().clone();
    let cached = ui
        .ctx()
        .data(|d| d.get_temp::<ScopedStyle>(id))
        .filter(|c| Arc::ptr_eq(&c.base, &base));
    let scoped = match cached {
        Some(c) => c.scoped,
        None => {
            let mut style = (*base).clone();
            style.spacing.scroll = visible_scroll_bars();
            apply_density(&mut style, &TOOL_DENSITY);
            let scoped = Arc::new(style);
            let entry = ScopedStyle {
                base,
                scoped: scoped.clone(),
            };
            ui.ctx().data_mut(|d| d.insert_temp(id, entry));
            scoped
        }
    };
    ui.set_style(scoped);
}

/// How a tool window is placed and sized.
pub(crate) struct ToolWindow<'a> {
    /// Stable id (egui remembers where the user moved / resized the window).
    pub id: &'a str,
    /// Header title.
    pub title: &'a str,
    /// Screen corner the window docks to, [`EDGE`] in from both edges.
    pub anchor: egui::Align2,
    /// Default outer size (frame included). With [`Self::fit_height`], the height the contents
    /// should stay within.
    pub size: egui::Vec2,
    /// The window is exactly as tall as its contents (up to `size.y`) instead of user-resizable
    /// in height. The contents get the bottom they must stay above.
    pub fit_height: bool,
}

/// A tool window docked to a screen corner: compact frame and header row (title, `summary`,
/// close button) under any global style. `open` is cleared by the close button. The contents
/// run with the tool density and get the screen y their last row must end above (the window's
/// largest content bottom).
pub(crate) fn window<R>(
    ctx: &egui::Context,
    spec: ToolWindow<'_>,
    open: &mut bool,
    summary: impl FnOnce(&mut egui::Ui),
    contents: impl FnOnce(&mut egui::Ui, f32) -> R,
) -> Option<R> {
    let style = ctx.global_style();
    let frame = egui::Frame::window(&style).inner_margin(MARGIN).corner_radius(4);
    let chrome = frame.total_margin().sum();
    let screen = ctx.content_rect();
    let bounds = screen.shrink(EDGE);
    let size = spec.size.min(bounds.size()).max(egui::vec2(160.0, 60.0));
    let id = egui::Id::new(spec.id);
    let fit_id = id.with("fit_height");
    let first_show = ctx.memory(|m| m.area_rect(id)).is_none();
    let mut window = egui::Window::new(spec.title)
        .id(id)
        .title_bar(false)
        .frame(frame)
        .pivot(spec.anchor)
        .default_pos(spec.anchor.pos_in_rect(&bounds))
        .default_size(size)
        .constrain_to(bounds)
        .min_size([160.0, 60.0]);
    if first_show {
        // egui caps a new window's size by the *global* window margin; asking for the size as
        // the minimum on the first frame keeps the tool's size independent of the game's style.
        window = window.min_size(size);
    }
    if spec.fit_height {
        // The contents keep below `size.y` whatever fits; when not even their smallest layout
        // does (a tiny screen), the window grows rather than cutting a row.
        let height = ctx.data(|d| d.get_temp::<f32>(fit_id)).unwrap_or(size.y).max(60.0);
        window = window.resizable([true, false]).min_height(height).max_height(height);
    }
    let mut close = false;
    let out = window.show(ctx, |ui| {
        scope_style(ui);
        let top = ui.max_rect().top();
        let max_bottom = top + size.y - chrome.y;
        if spec.fit_height {
            // Lay out as if the window had its largest height, so the measured height below
            // is the contents' own (the window follows next frame).
            ui.set_max_height(max_bottom - top);
        }
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(spec.title).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = close_button(ui).clicked();
                summary(ui);
            });
        });
        ui.separator();
        let r = contents(ui, max_bottom);
        if spec.fit_height {
            let height = (ui.min_rect().bottom() - top + chrome.y).round();
            if ctx.data(|d| d.get_temp::<f32>(fit_id)) != Some(height) {
                ctx.data_mut(|d| d.insert_temp(fit_id, height));
                ctx.request_repaint();
            }
        }
        r
    });
    if close {
        *open = false;
    }
    out.and_then(|r| r.inner)
}

/// An 18 pt "×" button (drawn, so it needs no glyph from the game's fonts).
fn close_button(ui: &mut egui::Ui) -> egui::Response {
    let side = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    if response.hovered() {
        ui.painter().rect_filled(rect.shrink(1.0), 2.0, visuals.weak_bg_fill);
    }
    let r = rect.shrink((side - 8.0) / 2.0);
    let stroke = egui::Stroke::new(1.2, visuals.fg_stroke.color);
    ui.painter().line_segment([r.left_top(), r.right_bottom()], stroke);
    ui.painter().line_segment([r.right_top(), r.left_bottom()], stroke);
    response.on_hover_text("Close")
}

/// Case-insensitive `haystack.contains(needle_lower)` (`needle_lower` already lowercase),
/// without allocating for ASCII needles.
pub(crate) fn contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if needle_lower.is_ascii() {
        let (h, n) = (haystack.as_bytes(), needle_lower.as_bytes());
        h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        haystack.to_lowercase().contains(needle_lower)
    }
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

    #[test]
    fn case_insensitive_filter() {
        assert!(contains_ignore_case("Shader CACHE corrupt", "cache"));
        assert!(contains_ignore_case("anything", ""));
        assert!(!contains_ignore_case("abc", "abcd"));
        assert!(contains_ignore_case("ÄRGER über", "über"));
        assert!(!contains_ignore_case("render", "physics"));
    }
}
