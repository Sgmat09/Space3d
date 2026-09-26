//! The pointer through egui: plugin pass hooks (label selection, drag-and-drop, debug text).
//! Headless: no GPU, no window.

use space3d_core::glam::UVec2;
use space3d_core::prelude::*;
use space3d_platform::{Input, MouseButton, WindowState};
use space3d_ui::*;
use std::sync::{Arc, Mutex};

/// Rows of selectable text in the root ui, plus what the last frame showed.
const ROWS: usize = 6;

struct Page {
    rows: Vec<egui::Rect>,
    primary_down: bool,
    /// Text shapes painted with the selection background when the pass ended.
    highlighted: usize,
    slider: f32,
    slider_rect: egui::Rect,
    source: egui::Rect,
    zone: egui::Rect,
    dropped: u32,
    button: egui::Rect,
    clicks: u32,
    double_clicks: u32,
    corner: egui::Rect,
    corner_hovered: bool,
}

impl Default for Page {
    fn default() -> Self {
        let none = egui::Rect::NOTHING;
        Self {
            rows: Vec::new(),
            primary_down: false,
            highlighted: 0,
            slider: 50.0,
            slider_rect: none,
            source: none,
            zone: none,
            dropped: 0,
            button: none,
            clicks: 0,
            double_clicks: 0,
            corner: none,
            corner_hovered: false,
        }
    }
}

fn window_state() -> WindowState {
    WindowState {
        physical_size: UVec2::new(1280, 720),
        scale_factor: 1.0,
        focused: true,
        suspended: false,
        title: String::new(),
        cursor_visible: true,
        cursor_grab: false,
    }
}

/// The deterministic UI (1 px per point) over a 1280x720 "window" showing a page: selectable
/// rows of text, a slider, a drag-and-drop source and zone and a button, plus a small window in
/// the top-left corner.
fn page_app() -> App {
    let mut app = App::new();
    app.add_plugin(UiPlugin::deterministic());
    app.insert_resource(window_state());
    app.insert_resource(Page::default());
    app.add_root_ui_system(|ui: &mut egui::Ui, sys: &mut SystemCtx| {
        let page = sys.resources.expect_mut::<Page>();
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 120,
                top: 100,
                ..Default::default()
            })
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                page.rows = (0..ROWS)
                    .map(|row| {
                        let label = egui::Label::new(format!("Row {row}: the quick brown fox jumps over the lazy dog"))
                            .selectable(true);
                        ui.add(label).rect
                    })
                    .collect();
                page.slider_rect = ui.add(egui::Slider::new(&mut page.slider, 0.0..=100.0)).rect;
                let (source, drag) = ui.allocate_exact_size(egui::vec2(60.0, 30.0), egui::Sense::click_and_drag());
                drag.dnd_set_drag_payload(7u32);
                page.source = source;
                let (zone, drop) = ui.allocate_exact_size(egui::vec2(60.0, 30.0), egui::Sense::hover());
                if drop.dnd_release_payload::<u32>().is_some() {
                    page.dropped += 1;
                }
                page.zone = zone;
                let button = ui.button("Button");
                page.button = button.rect;
                if button.clicked() {
                    page.clicks += 1;
                }
                if button.double_clicked() {
                    page.double_clicks += 1;
                }
            });
        page.primary_down = ui.input(|i| i.pointer.primary_down());
    });
    app.add_ui_system(|ctx: &egui::Context, sys: &mut SystemCtx| {
        let page = sys.resources.expect_mut::<Page>();
        egui::Area::new(egui::Id::new("corner"))
            .fixed_pos(egui::pos2(0.0, 0.0))
            .show(ctx, |ui| {
                let r = ui.add(egui::Button::new("Corner").min_size(egui::vec2(80.0, 40.0)));
                page.corner = r.rect;
                page.corner_hovered = r.hovered();
            });
    });
    // Before `ui::end_frame` (order 0): the rows painted with the selection background.
    app.add_system_ordered(Stage::PostUpdate, -1, "test::highlighted", |sys| {
        let Some(ctx) = EguiContext::active_context(sys) else {
            return;
        };
        let fill = ctx.global_style().visuals.selection.bg_fill;
        let lit = ctx.graphics(|layers| {
            layers.get(egui::LayerId::background()).map_or(0, |list| {
                list.all_entries()
                    .filter(|c| match &c.shape {
                        egui::Shape::Text(t) => t
                            .galley
                            .rows
                            .iter()
                            .any(|r| r.row.visuals.mesh.vertices.iter().any(|v| v.color == fill)),
                        _ => false,
                    })
                    .count()
            })
        });
        sys.resources.expect_mut::<Page>().highlighted = lit;
    });
    app.step_ticks(2);
    app
}

fn page(app: &App) -> &Page {
    app.resources.expect::<Page>()
}

fn input(app: &mut App) -> &mut Input {
    app.resources.expect_mut::<Input>()
}

fn ctx(app: &App) -> egui::Context {
    app.resources.expect::<EguiContext>().ctx().clone()
}

fn to(app: &mut App, p: egui::Pos2) {
    input(app).inject_mouse_move(Vec2::new(p.x, p.y));
    app.step_ticks(1);
}

fn down(app: &mut App, b: MouseButton) {
    input(app).inject_mouse_button_down(b);
    app.step_ticks(1);
}

fn up(app: &mut App, b: MouseButton) {
    input(app).inject_mouse_button_up(b);
    app.step_ticks(1);
}

fn click(app: &mut App, p: egui::Pos2) {
    to(app, p);
    down(app, MouseButton::Left);
    up(app, MouseButton::Left);
}

/// Move over every row, one per frame, no button held: whether egui held the primary button
/// after each move.
fn sweep(app: &mut App) -> Vec<bool> {
    (0..ROWS)
        .map(|row| {
            let at = page(app).rows[row].center();
            to(app, at);
            page(app).primary_down
        })
        .collect()
}

fn first_row(app: &App) -> egui::Pos2 {
    page(app).rows[0].left_center() + egui::vec2(2.0, 0.0)
}

fn has_payload(app: &App) -> bool {
    egui::DragAndDrop::has_any_payload(&ctx(app))
}

// ---- 1. plugin pass hooks -------------------------------------------------------------------

/// egui's `LabelSelectionState` ends a drag-to-select in its end-of-pass hook. Without the hook
/// a single press on selectable text left it "dragging": the selection followed the pointer
/// over every row with no button held.
#[test]
fn label_drag_select_ends_on_release() {
    let mut app = page_app();
    let at = first_row(&app);
    click(&mut app, at);
    assert_eq!(sweep(&mut app), [false; ROWS], "no button held");
    assert_eq!(page(&app).highlighted, 0, "nothing selected after a click");

    // A deliberate drag selects, and the selection stays as the release left it.
    to(&mut app, at);
    down(&mut app, MouseButton::Left);
    let third = page(&app).rows[2].center();
    to(&mut app, third);
    app.step_ticks(1);
    assert_eq!(page(&app).highlighted, 3, "rows 0 to 2 selected");
    up(&mut app, MouseButton::Left);
    sweep(&mut app);
    assert_eq!(page(&app).highlighted, 3, "the selection ended with the release");
}

/// egui's `DragAndDrop` drops a payload nobody took when the button comes up (end-of-pass
/// hook); without it the payload outlived the drag.
#[test]
fn dnd_payload_is_cleared_on_release() {
    let mut app = page_app();
    let source = page(&app).source.center();
    to(&mut app, source);
    down(&mut app, MouseButton::Left);
    for k in 1..=4 {
        to(&mut app, source + egui::vec2(0.0, 60.0 * k as f32));
    }
    assert!(has_payload(&app), "dragging");
    up(&mut app, MouseButton::Left);
    app.step_ticks(1);
    assert!(!has_payload(&app), "the payload ends with the release");
    assert_eq!(page(&app).dropped, 0, "dropped on no zone");
}

/// `egui::debug_text::print` lines are painted by `DebugTextPlugin`'s end-of-pass hook.
#[test]
fn debug_text_is_painted() {
    let vertices = |print: bool| {
        let mut app = App::new();
        app.add_plugin(UiPlugin::deterministic());
        app.add_ui_system(move |ctx: &egui::Context, _sys: &mut SystemCtx| {
            if print {
                egui::debug_text::print(ctx, "debug text");
            }
        });
        app.step_ticks(3);
        app.resources.expect::<EguiContext>().stats().vertices
    };
    if cfg!(debug_assertions) {
        assert!(vertices(true) > vertices(false), "debug text painted");
    }
}

/// A plugin that logs its pass hooks.
struct HookLog {
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl egui::Plugin for HookLog {
    fn debug_name(&self) -> &'static str {
        "HookLog"
    }

    fn on_begin_pass(&mut self, _ui: &mut egui::Ui) {
        self.log.lock().unwrap().push("begin");
    }

    fn on_end_pass(&mut self, _ui: &mut egui::Ui) {
        self.log.lock().unwrap().push("end");
    }
}

/// A second plugin type (registered directly with egui, then with the engine).
struct OtherHookLog(HookLog);

impl egui::Plugin for OtherHookLog {
    fn debug_name(&self) -> &'static str {
        "OtherHookLog"
    }

    fn on_begin_pass(&mut self, ui: &mut egui::Ui) {
        self.0.on_begin_pass(ui);
    }

    fn on_end_pass(&mut self, ui: &mut egui::Ui) {
        self.0.on_end_pass(ui);
    }
}

/// A plugin added through the engine gets its begin hook before the UI systems and its end
/// hook after them, once per pass; none while the UI is disabled.
#[test]
fn custom_plugin_hooks_run_once_per_pass() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut app = App::new();
    app.add_plugin(UiPlugin::deterministic());
    let e = app.resources.expect_mut::<EguiContext>();
    e.add_plugin(HookLog { log: log.clone() });
    e.add_plugin(HookLog { log: log.clone() }); // Once per type.
    let ui_log = log.clone();
    app.add_ui_system(move |_ctx: &egui::Context, _sys: &mut SystemCtx| {
        ui_log.lock().unwrap().push("ui");
    });
    app.step_ticks(3);
    assert_eq!(*log.lock().unwrap(), ["begin", "ui", "end"].repeat(3));

    log.lock().unwrap().clear();
    app.resources.expect_mut::<EguiContext>().set_enabled(false);
    app.step_ticks(2);
    assert!(log.lock().unwrap().is_empty(), "no pass, no hooks");
    app.resources.expect_mut::<EguiContext>().set_enabled(true);

    // A plugin a library registered directly with egui runs once the engine knows its type.
    let other = Arc::new(Mutex::new(Vec::new()));
    ctx(&app).add_plugin(OtherHookLog(HookLog { log: other.clone() }));
    app.step_ticks(1);
    assert!(other.lock().unwrap().is_empty(), "unknown to the engine");
    let e = app.resources.expect_mut::<EguiContext>();
    e.register_pass_hooks::<OtherHookLog>();
    e.register_pass_hooks::<OtherHookLog>();
    app.step_ticks(2);
    assert_eq!(*other.lock().unwrap(), ["begin", "end"].repeat(2));
}
