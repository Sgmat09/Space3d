//! Dense layout helpers shared by the editor tools: panel frames and titles, collapsible
//! sections with header actions, and property rows (a label column + the value on one line).
//!
//! Sizes come from the current egui style, so the tools follow the app's theme (the standalone
//! editor and storybook viewer insert `UiTheme::compact()`).

use space3d_core::glam::Vec3;
use std::hash::Hash;

/// Frame for the editor's side / top panels: the style's window margin on every side (4 pt with
/// the compact theme) instead of egui's 8 pt sides.
pub(crate) fn panel_frame(style: &egui::Style) -> egui::Frame {
    let m = style.spacing.window_margin;
    egui::Frame::side_top_panel(style).inner_margin(egui::Margin::symmetric(m.left, m.top))
}

/// Panel title row: the title in strong body text and optional weak `info` on the right.
pub(crate) fn panel_title(ui: &mut egui::Ui, title: &str, info: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).strong());
        if !info.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(info).weak());
            });
        }
    });
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
        ui.spacing_mut().item_spacing.x = 2.0;
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
/// on hover, so a clipped tool panel gives no cue that it continues). Scoped to `ui`.
pub(crate) fn visible_scroll_bars(ui: &mut egui::Ui) {
    let width = ui.spacing().scroll.bar_width;
    ui.spacing_mut().scroll = egui::style::ScrollStyle {
        bar_width: width,
        bar_inner_margin: 2.0,
        dormant_background_opacity: 0.4,
        dormant_handle_opacity: 0.6,
        ..egui::style::ScrollStyle::solid()
    };
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
    cell.spacing_mut().interact_size.x = width;
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
/// same in every row, so the boxes line up) leaves. At most 3 decimals, fewer when the value
/// would not fit the box (integer sliders stay integers).
pub(crate) fn fill_slider(ui: &mut egui::Ui, slider: egui::Slider<'_>) -> egui::Response {
    let value_width = value_box_width(ui);
    let gap = ui.spacing().item_spacing.x;
    ui.spacing_mut().slider_width = (ui.available_width() - value_width - gap).floor().max(24.0);
    // The slider's value box is a drag value at least `interact_size.x` wide.
    ui.spacing_mut().interact_size.x = value_width;
    let room = chars_in(ui, value_width);
    ui.add(slider.custom_formatter(move |v, decimals| {
        let (lo, hi) = (*decimals.start(), (*decimals.end()).min(3));
        fit_number(v, lo.min(hi)..=hi, room)
    }))
}

/// How many digits fit a `width` drag value box in `ui` (inside the button padding).
fn chars_in(ui: &egui::Ui, width: f32) -> usize {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let digit = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(1.0);
    (((width - 2.0 * ui.spacing().button_padding.x) / digit).floor() as usize).max(1)
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
