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
    (ui.available_width() * 0.34).clamp(56.0, 110.0)
}

/// A property row: `label` (weak, truncated with a tooltip when too long) in a fixed column,
/// then `add` on the rest of the line.
pub(crate) fn property<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = label_width(ui);
    ui.horizontal(|ui| {
        let height = ui.spacing().interact_size.y;
        ui.allocate_ui_with_layout(
            egui::vec2(width, height),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(width);
                let text = egui::RichText::new(label).color(ui.visuals().weak_text_color());
                ui.add(egui::Label::new(text).truncate().selectable(false));
            },
        );
        add(ui)
    })
    .inner
}

/// Three drag values (x, y, z) sharing the rest of the line.
pub(crate) fn vec3_fields(ui: &mut egui::Ui, v: &mut Vec3, speed: f64) -> egui::Response {
    let spacing = ui.spacing().item_spacing.x;
    let width = ((ui.available_width() - 2.0 * spacing) / 3.0).max(ui.spacing().interact_size.x);
    let height = ui.spacing().interact_size.y;
    let field = |ui: &mut egui::Ui, value: &mut f32, axis: &str| {
        ui.add_sized(
            [width, height],
            egui::DragValue::new(value).speed(speed).prefix(axis).max_decimals(3),
        )
    };
    let x = field(ui, &mut v.x, "x ");
    let y = field(ui, &mut v.y, "y ");
    let z = field(ui, &mut v.z, "z ");
    x | y | z
}

/// A slider filling the rest of the line (value box included).
pub(crate) fn fill_slider(ui: &mut egui::Ui, slider: egui::Slider<'_>) -> egui::Response {
    let value_box = ui.spacing().interact_size.x + ui.spacing().item_spacing.x + 8.0;
    ui.spacing_mut().slider_width = (ui.available_width() - value_box).max(40.0);
    ui.add(slider)
}

/// A drag value filling the rest of the line.
pub(crate) fn fill_drag(ui: &mut egui::Ui, drag: egui::DragValue<'_>) -> egui::Response {
    let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.add_sized(size, drag)
}

/// A frameless icon-style button ("×", "+", ...) with a tooltip.
pub(crate) fn icon_button(ui: &mut egui::Ui, icon: &str, tooltip: &str) -> egui::Response {
    ui.add(egui::Button::new(icon).frame(false).small())
        .on_hover_text(tooltip)
}
