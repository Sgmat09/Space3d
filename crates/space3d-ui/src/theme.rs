//! Opt-in UI density: the [`UiTheme`] resource and the [`UiDensity`] tokens behind it.
//!
//! Nothing here runs unless an app inserts a [`UiTheme`]: without the resource the engine never
//! touches egui's `Style` (so a game that styles egui itself keeps exactly its look). With it,
//! [`crate::UiPlugin`] applies the tokens once when the resource is inserted or changed — right
//! before the next egui pass begins — and leaves the style alone in every other frame.
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_ui::{UiPlugin, UiTheme};
//!
//! let mut app = App::new();
//! app.add_plugin(UiPlugin::deterministic());
//! app.insert_resource(UiTheme::compact()); // the one-line opt-in
//! ```
//!
//! The tokens only cover sizes: spacing, margins, control sizes, text sizes, corner radii and
//! the tooltip delay. Colors, fonts (families), shadows and every other `Style` field keep the
//! value they have in the context, so the preset also works on top of a game's own colors.

use egui::{CornerRadius, Margin, Style, TextStyle, vec2};

/// Size tokens of a UI density (all in egui points).
///
/// [`UiDensity::compact`] is the engine preset; [`UiDensity::egui_default`] holds egui's own
/// defaults (applying it to a default style changes nothing but the hovered widgets' corner
/// radius, which egui rounds one point more than the other states). Start from either and
/// change fields for a density of your own ([`UiTheme::Custom`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiDensity {
    /// Space between widgets (x: in a row, y: between rows) — `Spacing::item_spacing`.
    pub item_spacing: egui::Vec2,
    /// Space between a button's frame and its text — `Spacing::button_padding`.
    pub button_padding: egui::Vec2,
    /// Inner margin of windows — `Spacing::window_margin`. Engine tools also use it for their
    /// panels.
    pub window_margin: f32,
    /// Inner margin of menus, popups and tooltips — `Spacing::menu_margin`.
    pub menu_margin: f32,
    /// Height of buttons, sliders, text fields, drag values, combo boxes and menu bars: the
    /// smallest hit target — `Spacing::interact_size.y`.
    pub control_height: f32,
    /// Smallest width of drag values and color buttons — `Spacing::interact_size.x`.
    pub control_min_width: f32,
    /// Indentation of collapsing sections and tree levels — `Spacing::indent`.
    pub indent: f32,
    /// Size of the checkbox / radio / collapse icons — `Spacing::icon_width` (the inner mark is
    /// 4/7 of it, as in egui's defaults).
    pub icon_size: f32,
    /// Space between an icon and its text — `Spacing::icon_spacing`.
    pub icon_spacing: f32,
    /// Default width of a slider's rail — `Spacing::slider_width`.
    pub slider_width: f32,
    /// Default width of a combo box — `Spacing::combo_width`.
    pub combo_width: f32,
    /// Default width of a single-line text field — `Spacing::text_edit_width`.
    pub text_edit_width: f32,
    /// Width of scroll bars — `ScrollStyle::bar_width`.
    pub scroll_bar_width: f32,
    /// `TextStyle::Heading` size (window titles use it too).
    pub heading_size: f32,
    /// `TextStyle::Body` size.
    pub body_size: f32,
    /// `TextStyle::Button` size (also drag values).
    pub button_size: f32,
    /// `TextStyle::Small` size.
    pub small_size: f32,
    /// `TextStyle::Monospace` size.
    pub monospace_size: f32,
    /// Corner radius of widgets (every interaction state).
    pub widget_radius: f32,
    /// Corner radius of windows.
    pub window_radius: f32,
    /// Corner radius of menus, popups and tooltips.
    pub menu_radius: f32,
    /// Row height for tables and grids (`egui::Grid::min_row_height`, egui_extras row heights).
    /// egui's `Style` has no such field: tables read it from the theme ([`UiDensity::of`]).
    pub table_row_height: f32,
    /// Seconds the pointer rests before a tooltip shows — `Interaction::tooltip_delay`.
    pub tooltip_delay: f32,
}

impl Default for UiDensity {
    fn default() -> Self {
        Self::compact()
    }
}

impl UiDensity {
    /// The engine's compact preset: dense editor panels (think Blender or Unity) that stay
    /// readable at 1 pixel per point — 12.5 pt body text, 18 pt controls, 2 pt gaps between
    /// rows, 4 pt window margins, 2–4 pt corners and 16 pt text rows in tables (rows holding
    /// controls stay 18 pt tall).
    pub const fn compact() -> Self {
        Self {
            item_spacing: vec2(6.0, 2.0),
            button_padding: vec2(4.0, 1.0),
            window_margin: 4.0,
            menu_margin: 3.0,
            control_height: 18.0,
            control_min_width: 32.0,
            indent: 14.0,
            icon_size: 12.0,
            icon_spacing: 4.0,
            slider_width: 90.0,
            combo_width: 84.0,
            text_edit_width: 160.0,
            scroll_bar_width: 8.0,
            heading_size: 14.0,
            body_size: 12.5,
            button_size: 12.5,
            small_size: 10.0,
            monospace_size: 12.0,
            widget_radius: 2.0,
            window_radius: 4.0,
            menu_radius: 3.0,
            table_row_height: 16.0,
            tooltip_delay: 0.3,
        }
    }

    /// egui's default sizes as tokens (egui 0.36).
    pub const fn egui_default() -> Self {
        Self {
            item_spacing: vec2(8.0, 3.0),
            button_padding: vec2(4.0, 1.0),
            window_margin: 6.0,
            menu_margin: 6.0,
            control_height: 18.0,
            control_min_width: 40.0,
            indent: 18.0,
            icon_size: 14.0,
            icon_spacing: 4.0,
            slider_width: 100.0,
            combo_width: 100.0,
            text_edit_width: 280.0,
            scroll_bar_width: 10.0,
            heading_size: 18.0,
            body_size: 13.0,
            button_size: 13.0,
            small_size: 9.0,
            monospace_size: 13.0,
            widget_radius: 2.0,
            window_radius: 6.0,
            menu_radius: 6.0,
            table_row_height: 18.0,
            tooltip_delay: 0.5,
        }
    }

    /// The density the last [`apply_theme`] gave `ctx` (`None` while no theme was applied).
    /// For UI code that only has the egui context, e.g. to size table rows.
    pub fn of(ctx: &egui::Context) -> Option<Self> {
        ctx.data(|d| d.get_temp::<Self>(density_id()))
    }

    /// Row height for tables in `ui`: [`Self::table_row_height`] of the applied theme, or the
    /// style's control height without a theme.
    pub fn row_height(ui: &egui::Ui) -> f32 {
        Self::of(ui.ctx()).map_or(ui.spacing().interact_size.y, |d| d.table_row_height)
    }

    /// The tokens' bit patterns: equal for the same tokens, NaNs included (unlike `==`).
    fn to_bits(self) -> [u32; 25] {
        [
            self.item_spacing.x,
            self.item_spacing.y,
            self.button_padding.x,
            self.button_padding.y,
            self.window_margin,
            self.menu_margin,
            self.control_height,
            self.control_min_width,
            self.indent,
            self.icon_size,
            self.icon_spacing,
            self.slider_width,
            self.combo_width,
            self.text_edit_width,
            self.scroll_bar_width,
            self.heading_size,
            self.body_size,
            self.button_size,
            self.small_size,
            self.monospace_size,
            self.widget_radius,
            self.window_radius,
            self.menu_radius,
            self.table_row_height,
            self.tooltip_delay,
        ]
        .map(f32::to_bits)
    }
}

fn density_id() -> egui::Id {
    egui::Id::new("space3d_ui::UiDensity")
}

/// Opt-in UI theme (resource). Insert it to apply its density to egui's style; see the
/// [module docs](self).
///
/// Both variants are applied the same way ([`compact_style`]); the variant records where the
/// tokens came from: [`UiTheme::Compact`] is the engine preset (as is, or slightly tweaked),
/// [`UiTheme::Custom`] a density of your own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UiTheme {
    /// The engine's compact preset ([`UiTheme::compact`]).
    Compact(UiDensity),
    /// A density of your own.
    Custom(UiDensity),
}

impl Default for UiTheme {
    fn default() -> Self {
        Self::compact()
    }
}

impl UiTheme {
    /// The compact preset: `UiTheme::Compact(UiDensity::compact())`.
    pub const fn compact() -> Self {
        Self::Compact(UiDensity::compact())
    }

    /// The theme's tokens.
    pub fn density(&self) -> &UiDensity {
        match self {
            Self::Compact(d) | Self::Custom(d) => d,
        }
    }

    /// Same variant and bit-identical tokens (a NaN token still equals itself, so a malformed
    /// custom density is applied once, not every frame).
    fn same_as(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
            && self.density().to_bits() == other.density().to_bits()
    }
}

fn margin(v: f32) -> Margin {
    Margin::same(v.round().clamp(0.0, i8::MAX as f32) as i8)
}

fn radius(v: f32) -> CornerRadius {
    CornerRadius::same(v.round().clamp(0.0, u8::MAX as f32) as u8)
}

/// Write the tokens of `d` into `style` (sizes only; see [`compact_style`]).
pub fn apply_density(style: &mut Style, d: &UiDensity) {
    let s = &mut style.spacing;
    s.item_spacing = d.item_spacing;
    s.button_padding = d.button_padding;
    s.window_margin = margin(d.window_margin);
    s.menu_margin = margin(d.menu_margin);
    s.interact_size = vec2(d.control_min_width, d.control_height);
    s.indent = d.indent;
    s.icon_width = d.icon_size;
    s.icon_width_inner = (d.icon_size * 4.0 / 7.0).round();
    s.icon_spacing = d.icon_spacing;
    s.slider_width = d.slider_width;
    s.combo_width = d.combo_width;
    s.text_edit_width = d.text_edit_width;
    s.scroll.bar_width = d.scroll_bar_width;

    for (text_style, size) in [
        (TextStyle::Heading, d.heading_size),
        (TextStyle::Body, d.body_size),
        (TextStyle::Button, d.button_size),
        (TextStyle::Small, d.small_size),
        (TextStyle::Monospace, d.monospace_size),
    ] {
        // Keep the family (a game's own font stays); only the size changes.
        let family = match text_style {
            TextStyle::Monospace => egui::FontFamily::Monospace,
            _ => egui::FontFamily::Proportional,
        };
        style
            .text_styles
            .entry(text_style)
            .or_insert_with(|| egui::FontId::new(size, family))
            .size = size;
    }

    let v = &mut style.visuals;
    let w = &mut v.widgets;
    for state in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        state.corner_radius = radius(d.widget_radius);
    }
    v.window_corner_radius = radius(d.window_radius);
    v.menu_corner_radius = radius(d.menu_radius);

    style.interaction.tooltip_delay = d.tooltip_delay;
}

/// `base` with the tokens of `d`: spacing, margins, control and icon sizes, default widths,
/// scroll bar width, text sizes (families kept), corner radii and tooltip delay. Everything
/// else — colors, fonts, shadows, animation time, text options — is `base`'s, and applying
/// the same tokens twice changes nothing (`compact_style(&compact_style(s, a), b) ==
/// compact_style(s, b)`).
pub fn compact_style(base: &Style, d: &UiDensity) -> Style {
    let mut style = base.clone();
    apply_density(&mut style, d);
    style
}

/// Apply `theme` to both of the context's styles (dark and light) and remember its density
/// for [`UiDensity::of`]. [`crate::UiPlugin`] calls this whenever the [`UiTheme`] resource is
/// inserted or changed; call it yourself to apply a theme again after changing the style.
pub fn apply_theme(ctx: &egui::Context, theme: &UiTheme) {
    let d = *theme.density();
    ctx.all_styles_mut(|style| apply_density(style, &d));
    ctx.data_mut(|data| data.insert_temp(density_id(), d));
}

/// Applies [`UiTheme`] changes to an egui context: what [`crate::UiPlugin`] runs before every
/// egui pass. Only an inserted or changed theme touches the style; the same theme in later
/// frames, or no theme, costs one comparison and never allocates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UiThemeSync {
    applied: Option<UiTheme>,
    applications: u64,
}

impl UiThemeSync {
    /// Apply `theme` if it differs from the last theme seen. `None` (the resource is absent)
    /// touches nothing; a theme inserted again after that is applied again. Returns whether
    /// the style changed.
    pub fn sync(&mut self, ctx: &egui::Context, theme: Option<&UiTheme>) -> bool {
        let unchanged = match (theme, &self.applied) {
            (Some(t), Some(applied)) => t.same_as(applied),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return false;
        }
        self.applied = theme.copied();
        match theme {
            Some(t) => {
                apply_theme(ctx, t);
                self.applications += 1;
                true
            }
            None => false,
        }
    }

    /// The theme applied last (`None` while the resource is absent).
    pub fn applied(&self) -> Option<&UiTheme> {
        self.applied.as_ref()
    }

    /// How many times a theme was applied.
    pub fn applications(&self) -> u64 {
        self.applications
    }
}

/// State of the theme preview panel ([`ThemePreview::ui`]): a representative tool panel —
/// menu bar, toolbar, buttons, checkbox, slider, combo box, text field, a collapsing section
/// and a 10-row striped table — for judging a density. The engine's `Engine/UI/Theme/*`
/// stories and density tests show it; show it in your game to tune a [`UiDensity`].
#[derive(Clone, Debug, PartialEq)]
pub struct ThemePreview {
    /// Toolbar tool (0 move, 1 rotate, 2 scale).
    pub tool: usize,
    /// "Snap" checkbox.
    pub snap: bool,
    /// "Grid" slider.
    pub grid: f32,
    /// "Space" combo box (0 local, 1 world).
    pub space: usize,
    /// Text field contents.
    pub name: String,
    /// Transform rows (position, rotation, scale).
    pub transform: [[f32; 3]; 3],
    rects: Vec<(egui::Rect, bool)>,
}

impl Default for ThemePreview {
    fn default() -> Self {
        Self {
            tool: 0,
            snap: true,
            grid: 0.5,
            space: 0,
            name: "Hangar bay".into(),
            transform: [[0.0, 1.5, -2.25], [0.0, 90.0, 0.0], [1.0, 1.0, 1.0]],
            rects: Vec::new(),
        }
    }
}

const TABLE: [(&str, &str, u32, &str); 10] = [
    ("hull_a.glb", "mesh", 2_411, "09:12"),
    ("hull_b.glb", "mesh", 1_964, "09:14"),
    ("thruster.glb", "mesh", 312, "10:02"),
    ("cockpit.png", "texture", 4_096, "10:40"),
    ("panel_n.png", "texture", 1_024, "11:05"),
    ("engine.ogg", "audio", 688, "11:31"),
    ("laser.ogg", "audio", 97, "12:00"),
    ("hangar.ron", "scene", 54, "12:48"),
    ("pilot.ron", "prefab", 12, "13:15"),
    ("glow.ron", "effect", 8, "13:20"),
];

impl ThemePreview {
    /// Rect (points) of every widget drawn by the last [`Self::ui`] call and whether it is
    /// interactive (a hit target), for layout checks.
    pub fn widget_rects(&self) -> &[(egui::Rect, bool)] {
        &self.rects
    }

    /// Width of the panel under `ui`'s style: its "Space [combo] Name [field]" row at the
    /// style's default combo and text field widths. Every other row fills this width.
    pub fn panel_width(ui: &egui::Ui) -> f32 {
        let s = ui.spacing();
        let text = |t: &str| text_width(ui, t, TextStyle::Body);
        (text("Space") + s.combo_width + text("Name") + s.text_edit_width + 3.0 * s.item_spacing.x).ceil()
    }

    /// Draw the panel into `ui`, [`Self::panel_width`] wide (or narrower when `ui` is): rows
    /// fill the width, text never wraps.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> egui::Response {
        self.rects.clear();
        let width = Self::panel_width(ui).min(ui.available_width());
        let rects = &mut self.rects;
        // `hit`: a control (a hit target), not text.
        let mut add = |r: egui::Response, hit: bool| {
            rects.push((r.rect, hit));
            r
        };
        let row_height = UiDensity::row_height(ui);

        ui.scope(|ui| {
            ui.set_width(width);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            // A menu bar row (egui's `MenuBar` look without its fill-the-width sizing).
            ui.horizontal(|ui| {
                egui::containers::menu::menu_style(ui.style_mut());
                ui.set_min_height(ui.spacing().interact_size.y);
                for menu in ["File", "Edit", "View", "Help"] {
                    add(ui.menu_button(menu, |ui| ui.label("…")).response, true);
                }
            });
            ui.horizontal(|ui| {
                for b in ["New", "Open", "Save"] {
                    add(ui.button(b), true);
                }
                ui.separator();
                for (i, t) in ["Move", "Rotate", "Scale"].into_iter().enumerate() {
                    add(ui.selectable_value(&mut self.tool, i, t), true);
                }
            });
            ui.horizontal(|ui| {
                add(ui.checkbox(&mut self.snap, "Snap"), true);
                // Label and value box at the right end, the rail fills what is left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    add(ui.label("Grid"), false);
                    let size = egui::vec2(ui.spacing().interact_size.x, ui.spacing().interact_size.y);
                    add(
                        ui.add_sized(size, egui::DragValue::new(&mut self.grid).speed(0.05).range(0.0..=10.0)),
                        true,
                    );
                    ui.spacing_mut().slider_width = ui.available_width();
                    add(
                        ui.add(egui::Slider::new(&mut self.grid, 0.0..=10.0).show_value(false)),
                        true,
                    );
                });
            });
            ui.horizontal(|ui| {
                add(ui.label("Space"), false);
                add(
                    egui::ComboBox::from_id_salt("space3d_theme_preview_space")
                        .selected_text(["Local", "World"][self.space.min(1)])
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.space, 0, "Local");
                            ui.selectable_value(&mut self.space, 1, "World");
                        })
                        .response,
                    true,
                );
                add(ui.label("Name"), false);
                add(
                    ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(ui.available_width())),
                    true,
                );
            });
            let header = egui::CollapsingHeader::new("Transform")
                .id_salt("space3d_theme_preview_transform")
                .default_open(true)
                .show(ui, |ui| {
                    let labels = ["Position", "Rotation", "Scale"];
                    let label_w = labels
                        .iter()
                        .map(|l| text_width(ui, l, TextStyle::Body))
                        .fold(0.0, f32::max)
                        .ceil();
                    for (label, v) in labels.into_iter().zip(&mut self.transform) {
                        ui.horizontal(|ui| {
                            let h = ui.spacing().interact_size.y;
                            add(cell(ui, egui::vec2(label_w, h), egui::Align::Min, label), false);
                            let gap = ui.spacing().item_spacing.x;
                            let w = ((ui.available_width() - 2.0 * gap) / 3.0).floor();
                            for c in v.iter_mut() {
                                add(
                                    ui.add_sized([w, h], egui::DragValue::new(c).speed(0.1).max_decimals(2)),
                                    true,
                                );
                            }
                        });
                    }
                });
            add(header.header_response, true);
            ui.separator();
            self::table(ui, width, row_height, &mut add);
        })
        .response
    }
}

/// Width of `text` in `style` (one line).
fn text_width(ui: &egui::Ui, text: &str, style: TextStyle) -> f32 {
    let font = style.resolve(ui.style());
    ui.fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font, egui::Color32::WHITE).size().x)
}

/// A label in a `size` cell, left- (`Align::Min`) or right-aligned (`Align::Max`), truncated
/// to the cell. The cell always takes its full width.
fn cell(ui: &mut egui::Ui, size: egui::Vec2, align: egui::Align, text: impl Into<egui::WidgetText>) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let layout = match align {
        egui::Align::Max => egui::Layout::right_to_left(egui::Align::Center),
        _ => egui::Layout::left_to_right(egui::Align::Center),
    };
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    child.add(egui::Label::new(text).truncate().selectable(false))
}

/// The preview's striped table: columns fill `width`, numbers (and their headers) right-aligned.
fn table(ui: &mut egui::Ui, width: f32, row_height: f32, add: &mut impl FnMut(egui::Response, bool) -> egui::Response) {
    let gap = ui.spacing().item_spacing.x * 2.0;
    let kind = TABLE
        .iter()
        .map(|r| text_width(ui, r.1, TextStyle::Body))
        .fold(text_width(ui, "Kind", TextStyle::Body), f32::max)
        .ceil();
    let kib = text_width(ui, "00000", TextStyle::Monospace)
        .max(text_width(ui, "KiB", TextStyle::Body))
        .ceil();
    let modified = text_width(ui, "00:00", TextStyle::Monospace)
        .max(text_width(ui, "Modified", TextStyle::Body))
        .ceil();
    let asset = (width - kind - kib - modified - 3.0 * gap).max(0.0);
    // (space before, width, alignment)
    let columns = [
        (0.0, asset, egui::Align::Min),
        (gap, kind, egui::Align::Min),
        (gap, kib, egui::Align::Max),
        (gap, modified, egui::Align::Max),
    ];
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
        ui.spacing_mut().interact_size.y = row_height;
        let stripe = ui.visuals().faint_bg_color;
        let row = |ui: &mut egui::Ui,
                   cells: [egui::WidgetText; 4],
                   add: &mut dyn FnMut(egui::Response, bool) -> egui::Response,
                   striped: bool| {
            let background = ui.painter().add(egui::Shape::Noop);
            let r = ui.horizontal(|ui| {
                for (text, (space, w, align)) in cells.into_iter().zip(columns) {
                    ui.add_space(space);
                    add(cell(ui, egui::vec2(w, row_height), align, text), false);
                }
            });
            if striped {
                ui.painter()
                    .set(background, egui::Shape::rect_filled(r.response.rect, 0.0, stripe));
            }
        };
        let header = ["Asset", "Kind", "KiB", "Modified"].map(|h| egui::RichText::new(h).strong().into());
        row(ui, header, add, false);
        for (i, (name, kind, kib, time)) in TABLE.into_iter().enumerate() {
            let cells: [egui::WidgetText; 4] = [
                name.into(),
                egui::RichText::new(kind).weak().into(),
                egui::RichText::new(kib.to_string()).monospace().into(),
                egui::RichText::new(time).monospace().into(),
            ];
            row(ui, cells, add, i % 2 == 0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_style_sets_only_size_tokens() {
        let base = Style::default();
        let d = UiDensity::compact();
        let s = compact_style(&base, &d);
        assert_eq!(s.spacing.item_spacing, d.item_spacing);
        assert_eq!(s.spacing.interact_size, vec2(32.0, 18.0));
        assert_eq!(s.spacing.window_margin, Margin::same(4));
        assert_eq!(s.text_styles[&TextStyle::Body].size, 12.5);
        assert_eq!(
            s.text_styles[&TextStyle::Heading].family,
            egui::FontFamily::Proportional
        );
        assert_eq!(s.text_styles[&TextStyle::Monospace].family, egui::FontFamily::Monospace);
        assert_eq!(s.visuals.widgets.hovered.corner_radius, CornerRadius::same(2));
        assert_eq!(s.visuals.window_corner_radius, CornerRadius::same(4));
        assert_eq!(s.interaction.tooltip_delay, 0.3);
        // Colors and everything else stay the base's.
        assert_eq!(s.visuals.window_fill, base.visuals.window_fill);
        assert_eq!(
            s.visuals.widgets.inactive.bg_fill,
            base.visuals.widgets.inactive.bg_fill
        );
        assert_eq!(s.visuals.window_shadow, base.visuals.window_shadow);
        assert_eq!(s.animation_time, base.animation_time);
        assert_eq!(s.spacing.scroll.floating, base.spacing.scroll.floating);
    }

    #[test]
    fn compact_style_is_idempotent_and_keeps_a_games_look() {
        let mut game = Style::default();
        game.visuals.window_fill = egui::Color32::from_rgb(20, 19, 18);
        game.visuals.override_text_color = Some(egui::Color32::from_rgb(245, 244, 239));
        game.text_styles.insert(
            TextStyle::Body,
            egui::FontId::new(16.0, egui::FontFamily::Name("game".into())),
        );
        let custom = UiDensity {
            item_spacing: vec2(10.0, 5.0),
            ..UiDensity::compact()
        };
        let once = compact_style(&game, &UiDensity::compact());
        assert_eq!(compact_style(&once, &UiDensity::compact()), once);
        assert_eq!(compact_style(&once, &custom), compact_style(&game, &custom));
        assert_eq!(once.visuals.window_fill, game.visuals.window_fill);
        assert_eq!(once.visuals.override_text_color, game.visuals.override_text_color);
        let body = &once.text_styles[&TextStyle::Body];
        assert_eq!(
            (body.size, &body.family),
            (12.5, &egui::FontFamily::Name("game".into()))
        );
    }

    #[test]
    fn egui_default_tokens_match_egui() {
        let base = Style::default();
        let mut expected = base.clone();
        // egui rounds hovered widgets one point more; the tokens have one radius for all.
        expected.visuals.widgets.hovered.corner_radius = CornerRadius::same(2);
        assert_eq!(compact_style(&base, &UiDensity::egui_default()), expected);
    }

    #[test]
    fn sync_applies_on_insert_and_change_only() {
        let ctx = egui::Context::default();
        let mut sync = UiThemeSync::default();
        let original = ctx.global_style();
        assert!(!sync.sync(&ctx, None), "no theme: style untouched");
        assert!(std::sync::Arc::ptr_eq(&original, &ctx.global_style()));
        assert_eq!(UiDensity::of(&ctx), None);

        let theme = UiTheme::compact();
        assert!(sync.sync(&ctx, Some(&theme)));
        let applied = ctx.global_style();
        assert_eq!(applied.spacing.item_spacing, vec2(6.0, 2.0));
        assert_eq!(ctx.style_of(egui::Theme::Light).spacing.item_spacing, vec2(6.0, 2.0));
        assert_eq!(UiDensity::of(&ctx), Some(UiDensity::compact()));
        for _ in 0..10 {
            assert!(!sync.sync(&ctx, Some(&theme)), "same theme: nothing to do");
        }
        assert!(std::sync::Arc::ptr_eq(&applied, &ctx.global_style()));
        assert_eq!(sync.applications(), 1);

        // A game restyling afterwards is never overwritten while the theme is unchanged.
        ctx.global_style_mut(|s| s.spacing.item_spacing = vec2(20.0, 20.0));
        assert!(!sync.sync(&ctx, Some(&theme)));
        assert_eq!(ctx.global_style().spacing.item_spacing, vec2(20.0, 20.0));

        let custom = UiTheme::Custom(UiDensity {
            body_size: 14.0,
            ..UiDensity::compact()
        });
        assert!(sync.sync(&ctx, Some(&custom)));
        assert_eq!(ctx.global_style().text_styles[&TextStyle::Body].size, 14.0);
        assert!(!sync.sync(&ctx, None), "removing the theme leaves the style as it is");
        assert_eq!(ctx.global_style().text_styles[&TextStyle::Body].size, 14.0);
        assert!(sync.sync(&ctx, Some(&custom)), "inserted again: applied again");
        assert_eq!(sync.applications(), 3);
        assert_eq!(sync.applied(), Some(&custom));
    }

    #[test]
    fn a_nan_token_is_applied_once() {
        let ctx = egui::Context::default();
        let mut sync = UiThemeSync::default();
        let broken = UiTheme::Custom(UiDensity {
            slider_width: f32::NAN,
            ..UiDensity::compact()
        });
        assert!(sync.sync(&ctx, Some(&broken)));
        let applied = ctx.global_style();
        for _ in 0..5 {
            assert!(!sync.sync(&ctx, Some(&broken)), "unchanged theme with a NaN token");
        }
        assert!(std::sync::Arc::ptr_eq(&applied, &ctx.global_style()));
        assert_eq!(sync.applications(), 1);
        let compact = UiTheme::Compact(*broken.density());
        assert!(sync.sync(&ctx, Some(&compact)), "another variant is a change");
    }
}
