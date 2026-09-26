//! Dense layout helpers shared by the editor tools: panel frames and titles, collapsible
//! sections with header actions, and property rows (a label column + the value on one line).
//!
//! Sizes come from the current egui style, so the tools follow the app's theme (the standalone
//! editor and storybook viewer insert `UiTheme::compact()`).

use space3d_core::glam::Vec3;
use std::hash::Hash;
use std::sync::Arc;

/// Gap between the editor's tool windows (texture / model editor) and the edges of the viewport
/// they open over (the side panels), as for the engine tools.
pub(crate) const WINDOW_GAP: f32 = 6.0;

/// What a [`cached_style`] entry changes in its base style.
#[derive(Clone, Copy, Debug, PartialEq)]
enum StyleChange {
    /// Section header rows: 2 pt between the toggle and the title.
    HeaderSpacing,
    /// [`visible_scroll_bars`].
    ScrollBars,
    /// [`fill_slider`]: rail and value box widths (bits).
    Slider(u32, u32),
    /// [`number_field`] narrower than a control: the drag value's minimum width (bits).
    FieldWidth(u32),
}

/// Derived styles by base style and change (a handful: one per panel width and style).
#[derive(Clone, Default)]
struct StyleCache(Vec<(Arc<egui::Style>, StyleChange, Arc<egui::Style>)>);

/// Most entries kept (the oldest go first).
const STYLE_CACHE_SIZE: usize = 32;

/// Give `ui` its style changed by `derive`, reusing the style derived for the same base style and
/// `change` in earlier frames: `ui.spacing_mut()` would copy the whole `Style` (text styles,
/// visuals...) for every row in every frame.
fn cached_style(ui: &mut egui::Ui, change: StyleChange, derive: impl FnOnce(&mut egui::Style)) {
    let id = egui::Id::new("space3d_editor::style_cache");
    let base = ui.style().clone();
    let hit = ui.ctx().data_mut(|d| {
        d.get_temp_mut_or_default::<StyleCache>(id)
            .0
            .iter()
            .find(|(b, c, _)| *c == change && Arc::ptr_eq(b, &base))
            .map(|(_, _, derived)| derived.clone())
    });
    let derived = hit.unwrap_or_else(|| {
        let mut style = (*base).clone();
        derive(&mut style);
        let derived = Arc::new(style);
        ui.ctx().data_mut(|d| {
            let cache = &mut d.get_temp_mut_or_default::<StyleCache>(id).0;
            // Styles nothing else holds any more (a restyled context) go first.
            cache.retain(|(b, _, _)| Arc::strong_count(b) > 1);
            if cache.len() >= STYLE_CACHE_SIZE {
                cache.remove(0);
            }
            cache.push((base, change, derived.clone()));
        });
        derived
    });
    ui.set_style(derived);
}

/// Frame for the editor's side / top panels: the style's window margin on every side (4 pt with
/// the compact theme) instead of egui's 8 pt sides.
pub(crate) fn panel_frame(style: &egui::Style) -> egui::Frame {
    let m = style.spacing.window_margin;
    egui::Frame::side_top_panel(style).inner_margin(egui::Margin::symmetric(m.left, m.top))
}

/// Panel title row: the title in strong body text and optional `info` (a count) on the right.
pub(crate) fn panel_title(ui: &mut egui::Ui, title: &str, info: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).strong());
        if !info.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(info);
            });
        }
    });
}

/// Frame for the editor's menu bar: the style's window margin at the sides and 2 pt above and
/// below, like egui's own top panel frame (one control tall plus 4 pt and the separator line).
pub(crate) fn menu_bar_frame(style: &egui::Style) -> egui::Frame {
    let m = style.spacing.window_margin;
    egui::Frame::side_top_panel(style).inner_margin(egui::Margin::symmetric(m.left, 2))
}

/// Width of a fixed tool column (texture / model editor) laid out for `base` points at 12.5 pt
/// text: it grows with the body text size, so bigger fonts keep the same proportions.
pub(crate) fn tool_column_width(ui: &egui::Ui, base: f32) -> f32 {
    let body = egui::TextStyle::Body.resolve(ui.style()).size;
    (base * (body / 12.5).max(1.0)).round()
}

/// A collapsible section: a full-width header band (toggle icon, clickable `title`, `actions`
/// right-aligned) and, while open, the body below it. Returns the actions' and body's results.
pub(crate) fn section<A, B>(
    ui: &mut egui::Ui,
    id_salt: impl Hash + std::fmt::Debug,
    title: &str,
    default_open: bool,
    actions: impl FnOnce(&mut egui::Ui) -> A,
    body: impl FnOnce(&mut egui::Ui) -> B,
) -> (A, Option<B>) {
    let id = ui.make_persistent_id(id_salt);
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
    let background = ui.painter().add(egui::Shape::Noop);
    let header = ui.horizontal(|ui| {
        cached_style(ui, StyleChange::HeaderSpacing, |s| s.spacing.item_spacing.x = 2.0);
        state.show_toggle_button(ui, egui::collapsing_header::paint_default_icon);
        let title = ui.add(
            egui::Label::new(egui::RichText::new(title).strong())
                .selectable(false)
                .sense(egui::Sense::click()),
        );
        if title.clicked() {
            state.toggle(ui);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), actions)
            .inner
    });
    let band = header.response.rect.expand2(egui::vec2(2.0, 0.0));
    ui.painter().set(
        background,
        egui::epaint::RectShape::filled(band, 2, ui.visuals().faint_bg_color.gamma_multiply(3.0)),
    );
    // Unindented (like an inspector's component list): every section's property rows share one
    // label column; the header band marks where a section starts.
    let body = state.show_body_unindented(ui, body).map(|r| r.inner);
    (header.inner, body)
}

/// Width of the label column of [`property`] rows in `ui`.
pub(crate) fn label_width(ui: &egui::Ui) -> f32 {
    (ui.available_width() * 0.34).clamp(56.0, 110.0).floor()
}

/// A property row: `label` (truncated with a tooltip when too long) in a fixed column, then
/// `add` on the rest of the line.
pub(crate) fn property<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    property_row(ui, label, false, add)
}

/// A [`property`] row whose value is a group of fixed-size widgets (checkboxes, buttons, small
/// fields) that wraps within the value column when the line is too short (big fonts, a narrow
/// panel) instead of widening the panel. Not for the fill helpers, which size themselves to the
/// rest of the line.
pub(crate) fn property_group<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    property_row(ui, label, true, add)
}

fn property_row<R>(ui: &mut egui::Ui, label: &str, wrap: bool, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = label_width(ui);
    ui.horizontal(|ui| {
        let height = ui.spacing().interact_size.y;
        ui.allocate_ui_with_layout(
            egui::vec2(width, height),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(width);
                ui.add(egui::Label::new(label).truncate().selectable(false));
            },
        );
        if wrap {
            ui.horizontal_wrapped(add).inner
        } else {
            add(ui)
        }
    })
    .inner
}

/// Scroll bars that stay visible while the content overflows (egui's floating bars only show
/// on hover, so a clipped tool panel gives no cue that it continues), in the style's bar width
/// ([`space3d_ui::visible_scroll_bars`]). Scoped to `ui`.
pub(crate) fn visible_scroll_bars(ui: &mut egui::Ui) {
    cached_style(ui, StyleChange::ScrollBars, |s| {
        s.spacing.scroll = space3d_ui::visible_scroll_bars(s.spacing.scroll.bar_width);
    });
}

/// `value` with the most decimals in `decimals` that fit `room` characters (at least none).
fn fit_number(value: f64, decimals: std::ops::RangeInclusive<usize>, room: usize) -> String {
    let (lo, mut hi) = (*decimals.start(), (*decimals.end()).min(6));
    loop {
        let text = egui::emath::format_with_decimals_in_range(value, lo.min(hi)..=hi);
        if text.chars().count() <= room || hi == 0 {
            return text;
        }
        hi -= 1;
    }
}

/// A drag value in a `width` cell: never wider than the cell (decimals are dropped from the
/// shown value before the row would widen), with an axis color bar at its left edge when
/// `axis` is set. `affix` counts the characters of the drag value's prefix and suffix.
pub(crate) fn number_field(
    ui: &mut egui::Ui,
    width: f32,
    drag: egui::DragValue<'_>,
    affix: usize,
    axis: Option<egui::Color32>,
) -> egui::Response {
    let height = ui.spacing().interact_size.y;
    let room = chars_in(ui, width).saturating_sub(affix);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let mut cell = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::centered_and_justified(egui::Direction::LeftToRight)),
    );
    cell.set_clip_rect(rect.intersect(ui.clip_rect()));
    if width < cell.spacing().interact_size.x {
        // A drag value is at least a control wide: let it shrink to the cell.
        cached_style(&mut cell, StyleChange::FieldWidth(width.to_bits()), |s| {
            s.spacing.interact_size.x = width;
        });
    }
    let response = cell.add(drag.custom_formatter(move |v, decimals| fit_number(v, decimals, room.max(1))));
    if let Some(color) = axis {
        let bar =
            egui::Rect::from_min_size(rect.left_top(), egui::vec2(2.0, rect.height())).shrink2(egui::vec2(0.0, 3.0));
        ui.painter().rect_filled(bar, 1.0, color);
    }
    response
}

/// Three drag values (x, y, z) sharing the rest of the line in equal cells, marked by axis
/// color bars (red, green, blue).
pub(crate) fn vec3_fields(ui: &mut egui::Ui, v: &mut Vec3, speed: f64) -> egui::Response {
    let gap = ui.spacing().item_spacing.x;
    let width = ((ui.available_width() - 2.0 * gap) / 3.0).floor().max(24.0);
    let axes = [
        egui::Color32::from_rgb(220, 80, 80),
        egui::Color32::from_rgb(110, 190, 90),
        egui::Color32::from_rgb(90, 140, 230),
    ];
    let field = |ui: &mut egui::Ui, value: &mut f32, axis: usize| {
        let drag = egui::DragValue::new(value).speed(speed).max_decimals(3);
        number_field(ui, width, drag, 0, Some(axes[axis])).on_hover_text(["x", "y", "z"][axis])
    };
    let x = field(ui, &mut v.x, 0);
    let y = field(ui, &mut v.y, 1);
    let z = field(ui, &mut v.z, 2);
    x | y | z
}

/// Width of a slider's value box in `ui`: six digits and the button padding.
fn value_box_width(ui: &egui::Ui) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let digit = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
    (6.0 * digit + 2.0 * ui.spacing().button_padding.x)
        .max(ui.spacing().interact_size.x)
        .ceil()
}

/// A slider filling the rest of the line: the rail takes what the fixed-width value box (the
/// same in every row, so the boxes line up) leaves. When that would leave the rail shorter than
/// two control heights (big fonts in a narrow column), the box gives up room first, down to
/// three digits. At most 3 decimals, fewer when the value would not fit the box (integer
/// sliders stay integers).
pub(crate) fn fill_slider(ui: &mut egui::Ui, slider: egui::Slider<'_>) -> egui::Response {
    let gap = ui.spacing().item_spacing.x;
    let available = ui.available_width();
    let min_rail = 2.0 * ui.spacing().interact_size.y;
    let value_width = value_box_width(ui)
        .min(available - gap - min_rail)
        .max(min_value_box_width(ui))
        .floor();
    let rail = (available - value_width - gap).floor().max(24.0);
    // The slider's value box is a drag value at least `interact_size.x` wide.
    cached_style(ui, StyleChange::Slider(rail.to_bits(), value_width.to_bits()), |s| {
        s.spacing.slider_width = rail;
        s.spacing.interact_size.x = value_width;
    });
    let room = chars_in(ui, value_width);
    ui.add(slider.custom_formatter(move |v, decimals| {
        let (lo, hi) = (*decimals.start(), (*decimals.end()).min(3));
        fit_number(v, lo.min(hi)..=hi, room)
    }))
}

/// Narrowest slider value box in `ui`: three digits and the button padding.
fn min_value_box_width(ui: &egui::Ui) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let digit = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
    (3.0 * digit + 2.0 * ui.spacing().button_padding.x).ceil()
}

/// How many digits fit a `width` drag value box in `ui` (inside the button padding).
fn chars_in(ui: &egui::Ui, width: f32) -> usize {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let digit = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(1.0);
    (((width - 2.0 * ui.spacing().button_padding.x) / digit).floor() as usize).max(1)
}

/// Gap between the cells of a [`button_row`].
const CELL_GAP: f32 = 2.0;

/// Buttons in equal cells filling the rest of the line, [`CELL_GAP`] apart (labels truncated to
/// their cell); returns the index of the clicked one. On its own line: a row of actions as wide as
/// the property rows; in a [`property`] row: fills the value column.
pub(crate) fn button_row<const N: usize>(ui: &mut egui::Ui, buttons: [egui::Button<'_>; N]) -> Option<usize> {
    let height = ui.spacing().interact_size.y;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let width = ((rect.width() - CELL_GAP * (N as f32 - 1.0)) / N as f32).floor();
    let mut clicked = None;
    for (i, button) in buttons.into_iter().enumerate() {
        let x = if i + 1 == N {
            // The last cell ends exactly at the row's right edge.
            rect.right() - width
        } else {
            rect.left() + i as f32 * (width + CELL_GAP)
        };
        let cell = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(width, height));
        if ui.place(cell, button.truncate()).clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// Width of a button showing `text` in `ui` (text and the button padding).
pub(crate) fn text_button_width(ui: &egui::Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text = ui.fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font, egui::Color32::WHITE).size().x);
    (text + 2.0 * ui.spacing().button_padding.x).ceil()
}

/// A drag value filling the rest of the line.
pub(crate) fn fill_drag(ui: &mut egui::Ui, drag: egui::DragValue<'_>) -> egui::Response {
    let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.add_sized(size, drag)
}

/// A frameless icon button ("×", "+", ...) with an 18 pt (control height) square hit target
/// and a tooltip.
pub(crate) fn icon_button(ui: &mut egui::Ui, icon: &str, tooltip: &str) -> egui::Response {
    let side = ui.spacing().interact_size.y;
    ui.add(egui::Button::new(icon).frame(false).min_size(egui::vec2(side, side)))
        .on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slider rows, section headers and scroll bars reuse the styles they
    /// derived in earlier frames (no `Style` copy per row per frame); a restyled context gets
    /// fresh ones.
    #[test]
    fn rows_reuse_their_styles_frame_after_frame() {
        let ctx = egui::Context::default();
        let mut frames: Vec<[*const egui::Style; 3]> = Vec::new();
        let run = |ctx: &egui::Context, frames: &mut Vec<[*const egui::Style; 3]>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                let mut value = 0.5f32;
                let slider = ui.horizontal(|ui| {
                    fill_slider(ui, egui::Slider::new(&mut value, 0.0..=1.0));
                    Arc::as_ptr(ui.style())
                });
                let (header, _) = section(ui, "s", "Section", true, |ui| Arc::as_ptr(ui.style()), |_| {});
                let bars = ui.scope(|ui| {
                    visible_scroll_bars(ui);
                    Arc::as_ptr(ui.style())
                });
                frames.push([slider.inner, header, bars.inner]);
            });
            out.textures_delta.clear();
        };
        for _ in 0..3 {
            run(&ctx, &mut frames);
        }
        let last = *frames.last().unwrap();
        assert!(frames.iter().all(|f| *f == last), "the same styles every frame");
        assert!(
            last.iter().all(|&p| p != Arc::as_ptr(&ctx.global_style())),
            "derived styles"
        );
        ctx.global_style_mut(|s| s.spacing.item_spacing.y = 5.0);
        run(&ctx, &mut frames);
        let restyled = *frames.last().unwrap();
        assert!(
            restyled.iter().zip(&last).all(|(a, b)| a != b),
            "a restyled context gets fresh ones"
        );
    }
}
