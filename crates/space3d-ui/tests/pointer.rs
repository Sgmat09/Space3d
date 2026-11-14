//! The pointer through egui: plugin pass hooks (label selection, drag-and-drop, debug text),
//! button events in order, window focus, releases outside the window and cancelled releases
//! (focus loss, releases only the OS saw). Headless: no GPU, no window; focus changes are
//! driven like the window runner does (`WindowState::focused` + `Input::release_all`).

use space3d_core::glam::UVec2;
use space3d_core::prelude::*;
use space3d_platform::{Input, KeyCode, MouseButton, WindowState};
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
    secondary_clicks: u32,
    corner: egui::Rect,
    corner_hovered: bool,
    corner_clicks: u32,
    /// Releases over the drop zone by egui's own view (`contains_pointer()` and `any_released()`), and
    /// those that were not cancels ([`pointer_cancelled_this_frame`]).
    zone_releases: u32,
    zone_real_releases: u32,
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
            secondary_clicks: 0,
            corner: none,
            corner_hovered: false,
            corner_clicks: 0,
            zone_releases: 0,
            zone_real_releases: 0,
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
                if drop.contains_pointer() && ui.input(|i| i.pointer.any_released()) {
                    page.zone_releases += 1;
                    if !pointer_cancelled_this_frame(ui.ctx()) {
                        page.zone_real_releases += 1;
                    }
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
                if button.secondary_clicked() {
                    page.secondary_clicks += 1;
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
                if r.clicked() {
                    page.corner_clicks += 1;
                }
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

/// The window loses the focus: what the window runner does on `WindowEvent::Focused(false)`.
fn blur(app: &mut App) {
    app.resources.expect_mut::<WindowState>().focused = false;
    input(app).release_all();
    app.step_ticks(1);
}

fn refocus(app: &mut App) {
    app.resources.expect_mut::<WindowState>().focused = true;
    app.step_ticks(1);
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

// ---- 3. button events in order --------------------------------------------------------------

/// A release and a new press within one frame (the second click of a fast double click) leave
/// egui's button down, like `Input`'s: the old translation sent the press before the release.
#[test]
fn release_and_press_in_one_frame_keep_the_button_down() {
    let mut app = page_app();
    let empty = egui::pos2(900.0, 600.0);
    to(&mut app, empty);
    down(&mut app, MouseButton::Left);
    assert!(page(&app).primary_down);
    let input = input(&mut app);
    input.inject_mouse_button_up(MouseButton::Left);
    input.inject_mouse_button_down(MouseButton::Left);
    app.step_ticks(1);
    assert!(input_pressed(&mut app, MouseButton::Left));
    assert!(page(&app).primary_down, "egui holds the button like Input");
}

fn input_pressed(app: &mut App, b: MouseButton) -> bool {
    input(app).mouse_pressed(b)
}

/// Double-click-and-drag: the second press follows the first release in the same frame and the
/// drag that follows moves the slider.
#[test]
fn double_click_and_drag_keeps_dragging() {
    let mut app = page_app();
    let slider = page(&app).slider_rect;
    let start = slider.center();
    to(&mut app, start);
    down(&mut app, MouseButton::Left);
    let input = input(&mut app);
    input.inject_mouse_button_up(MouseButton::Left);
    input.inject_mouse_button_down(MouseButton::Left);
    app.step_ticks(1);
    let before = page(&app).slider;
    to(&mut app, start + egui::vec2(30.0, 0.0));
    to(&mut app, start + egui::vec2(60.0, 0.0));
    assert!(
        page(&app).slider > before,
        "the drag after the double click moved the slider"
    );
    up(&mut app, MouseButton::Left);
}

// ---- 4. window focus ------------------------------------------------------------------------

/// egui forgets keys it holds when the window loses the focus (`Event::WindowFocused(false)`).
#[test]
fn focus_loss_reaches_egui() {
    let mut app = page_app();
    // A key egui holds that `Input` knows nothing of (an IME, a custom platform...).
    app.resources.expect_mut::<EguiContext>().push_event(egui::Event::Key {
        key: egui::Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    input(&mut app).inject_key_down(KeyCode::ShiftLeft);
    app.step_ticks(1);
    assert!(ctx(&app).input(|i| i.key_down(egui::Key::A) && i.modifiers.shift));
    blur(&mut app);
    let (focused, a_down, events) = ctx(&app).input(|i| (i.focused, i.key_down(egui::Key::A), i.raw.events.clone()));
    assert!(!focused);
    assert!(events.contains(&egui::Event::WindowFocused(false)), "{events:?}");
    assert!(!a_down, "egui forgot the key");
    refocus(&mut app);
    let events = ctx(&app).input(|i| i.raw.events.clone());
    assert!(events.contains(&egui::Event::WindowFocused(true)), "{events:?}");
    app.step_ticks(1);
    let events = ctx(&app).input(|i| i.raw.events.clone());
    assert!(
        !events.iter().any(|e| matches!(e, egui::Event::WindowFocused(_))),
        "sent on changes only"
    );
}

// ---- 5. releases outside the window ---------------------------------------------------------

/// A drag that ends outside the window: the release is sent where the pointer was last seen and
/// the pointer is gone again afterwards — not at (0, 0), where the corner window would get
/// the hover and the pointer would be over the UI.
#[test]
fn release_outside_the_window_is_not_sent_to_the_corner() {
    let mut app = page_app();
    let slider = page(&app).slider_rect;
    to(&mut app, slider.center());
    down(&mut app, MouseButton::Left);
    let edge = egui::pos2(1275.0, slider.center().y);
    to(&mut app, edge);
    let dragged_to = page(&app).slider;
    assert_eq!(dragged_to, 100.0, "the slider followed");
    input(&mut app).inject_mouse_left();
    app.step_ticks(1);
    up(&mut app, MouseButton::Left);
    let (hover, interact) = ctx(&app).input(|i| (i.pointer.hover_pos(), i.pointer.interact_pos()));
    assert_eq!(hover, None, "the pointer is still outside");
    assert_eq!(interact, Some(edge), "released where it was last seen");
    assert!(!page(&app).primary_down);
    assert!(!page(&app).corner_hovered);
    assert_eq!(page(&app).slider, dragged_to, "the release did not move the slider");
    app.step_ticks(1);
    let e = app.resources.expect::<EguiContext>();
    assert!(!e.is_pointer_over_ui() && !e.wants_pointer_input());
    assert!(!page(&app).corner_hovered);
    assert_eq!(ctx(&app).input(|i| i.pointer.interact_pos()), None);
}

// ---- cancelled releases ---------------------------------------------------------------------

/// A button pressed, then the window loses the focus: the release that `Input` gets is no
/// click (switching away cancels, like native apps).
#[test]
fn focus_loss_release_is_not_a_click() {
    let mut app = page_app();
    let button = page(&app).button.center();
    to(&mut app, button);
    down(&mut app, MouseButton::Left);
    assert!(page(&app).primary_down);
    blur(&mut app);
    assert!(!input_pressed(&mut app, MouseButton::Left), "Input released it");
    assert!(!page(&app).primary_down, "egui too");
    assert_eq!(page(&app).clicks, 0, "no click");
    refocus(&mut app);
    click(&mut app, button);
    assert_eq!(page(&app).clicks, 1, "a real click after the focus came back");
    assert_eq!(page(&app).double_clicks, 0, "and not a double click");
}

/// A drag-and-drop over a drop zone, then the focus goes: nothing is dropped, the payload ends.
#[test]
fn focus_loss_cancels_drag_and_drop() {
    let mut app = page_app();
    let source = page(&app).source.center();
    let zone = page(&app).zone.center();
    to(&mut app, source);
    down(&mut app, MouseButton::Left);
    for k in 1..=4 {
        to(&mut app, source + (zone - source) * (k as f32 / 4.0));
    }
    assert!(has_payload(&app), "dragging over the zone");
    blur(&mut app);
    assert_eq!(page(&app).dropped, 0, "no drop");
    assert!(!has_payload(&app), "the payload ends");
    refocus(&mut app);
    app.step_ticks(2);
    assert_eq!(page(&app).dropped, 0);
    assert!(!page(&app).primary_down);

    // A real drop still works.
    to(&mut app, source);
    down(&mut app, MouseButton::Left);
    for k in 1..=4 {
        to(&mut app, source + (zone - source) * (k as f32 / 4.0));
    }
    up(&mut app, MouseButton::Left);
    assert_eq!(page(&app).dropped, 1);
}

/// A slider dragged when the focus goes stops where it was, and does not follow the pointer
/// after.
#[test]
fn focus_loss_stops_a_slider_where_it_was() {
    let mut app = page_app();
    let slider = page(&app).slider_rect.center();
    to(&mut app, slider);
    down(&mut app, MouseButton::Left);
    to(&mut app, slider + egui::vec2(20.0, 0.0));
    let value = page(&app).slider;
    assert!(value > 50.0, "{value}");
    blur(&mut app);
    assert_eq!(page(&app).slider, value, "unchanged by the cancel");
    to(&mut app, slider - egui::vec2(40.0, 0.0));
    refocus(&mut app);
    to(&mut app, slider - egui::vec2(60.0, 0.0));
    assert_eq!(page(&app).slider, value, "the drag ended with the focus loss");
    assert_eq!(page(&app).highlighted, 0);
}

/// A release only the OS saw (the release event went elsewhere) is treated like a focus loss:
/// no click, no drop.
#[test]
fn os_reconciled_release_is_neither_click_nor_drop() {
    use space3d_platform::MouseButtonCause;
    let mut app = page_app();
    let button = page(&app).button.center();
    to(&mut app, button);
    down(&mut app, MouseButton::Left);
    input(&mut app).release_mouse_button(MouseButton::Left, MouseButtonCause::OsReconciled);
    app.step_ticks(1);
    assert!(!page(&app).primary_down);
    assert_eq!(page(&app).clicks, 0, "no click");

    let source = page(&app).source.center();
    let zone = page(&app).zone.center();
    to(&mut app, source);
    down(&mut app, MouseButton::Left);
    for k in 1..=4 {
        to(&mut app, source + (zone - source) * (k as f32 / 4.0));
    }
    assert!(has_payload(&app));
    input(&mut app).release_mouse_button(MouseButton::Left, MouseButtonCause::OsReconciled);
    app.step_ticks(1);
    assert_eq!(page(&app).dropped, 0, "no drop");
    assert!(!has_payload(&app));
    click(&mut app, button);
    assert_eq!(page(&app).clicks, 1);
}

/// Games see the focus loss's releases as cancels in `Input::mouse_button_events`.
#[test]
fn games_can_tell_cancelled_releases_apart() {
    let mut app = page_app();
    app.init_resource::<Vec<(bool, bool)>>();
    app.add_system(Stage::Update, |sys| {
        let seen: Vec<(bool, bool)> = sys
            .resources
            .expect::<Input>()
            .mouse_button_events()
            .iter()
            .map(|e| (e.pressed, e.cancelled()))
            .collect();
        sys.resources.expect_mut::<Vec<(bool, bool)>>().extend(seen);
    });
    click(&mut app, egui::pos2(900.0, 600.0));
    down(&mut app, MouseButton::Left);
    blur(&mut app);
    assert_eq!(
        *app.resources.expect::<Vec<(bool, bool)>>(),
        [(true, false), (false, false), (true, false), (false, true)]
    );
}

/// A selectable label pressed when the focus goes: nothing gets selected afterwards.
#[test]
fn focus_loss_during_a_press_on_text_selects_nothing() {
    let mut app = page_app();
    let at = first_row(&app);
    to(&mut app, at);
    down(&mut app, MouseButton::Left);
    blur(&mut app);
    refocus(&mut app);
    assert_eq!(sweep(&mut app), [false; ROWS]);
    assert_eq!(page(&app).highlighted, 0);
}

/// A window dragged by its title bar stops with the focus loss; clicks work once it is back,
/// and a text field keeps the keyboard across the focus change.
#[test]
fn focus_loss_ends_a_window_drag_and_text_fields_keep_the_keyboard() {
    #[derive(Default)]
    struct Win {
        rect: Option<egui::Rect>,
        text: String,
        field: Option<egui::Rect>,
        field_focused: bool,
    }
    let mut app = App::new();
    app.add_plugin(UiPlugin::deterministic());
    app.insert_resource(window_state());
    app.init_resource::<Win>();
    app.add_ui_system(|ctx: &egui::Context, sys: &mut SystemCtx| {
        let win = sys.resources.expect_mut::<Win>();
        let shown = egui::Window::new("Drag me")
            .default_pos([400.0, 200.0])
            .show(ctx, |ui| {
                let r = ui.text_edit_singleline(&mut win.text);
                win.field = Some(r.rect);
                win.field_focused = r.has_focus();
            });
        win.rect = shown.map(|s| s.response.rect);
    });
    app.step_ticks(2);
    let win = |app: &App| app.resources.expect::<Win>().rect.unwrap();
    let bar = win(&app).center_top() + egui::vec2(0.0, 8.0);
    to(&mut app, bar);
    down(&mut app, MouseButton::Left);
    for k in 1..=4 {
        to(&mut app, bar + egui::vec2(20.0, 10.0) * k as f32);
    }
    let moved_to = win(&app);
    assert!(moved_to.min.x > 450.0, "{moved_to:?}");
    blur(&mut app);
    for k in 1..=4 {
        to(&mut app, bar + egui::vec2(-20.0, 30.0) * k as f32);
    }
    refocus(&mut app);
    to(&mut app, bar);
    assert_eq!(win(&app), moved_to, "the drag ended with the focus loss");

    let field = app.resources.expect::<Win>().field.unwrap().center();
    click(&mut app, field);
    assert!(app.resources.expect::<Win>().field_focused);
    input(&mut app).inject_text("ab");
    app.step_ticks(1);
    blur(&mut app);
    refocus(&mut app);
    input(&mut app).inject_text("c");
    app.step_ticks(1);
    assert_eq!(app.resources.expect::<Win>().text, "abc");
}

/// A button released while the UI was disabled does not stay held in egui.
#[test]
fn a_release_while_the_ui_is_disabled_does_not_stick() {
    let mut app = page_app();
    let button = page(&app).button.center();
    to(&mut app, button);
    down(&mut app, MouseButton::Left);
    app.resources.expect_mut::<EguiContext>().set_enabled(false);
    up(&mut app, MouseButton::Left);
    app.resources.expect_mut::<EguiContext>().set_enabled(true);
    app.step_ticks(1);
    assert!(!page(&app).primary_down);
    assert_eq!(page(&app).clicks, 0, "a cancel: the release was not seen");
    sweep(&mut app);
    assert_eq!(page(&app).highlighted, 0);
    click(&mut app, button);
    assert_eq!(page(&app).clicks, 1);
}

// ---- edge cases: one cancel among other buttons, releases outside, no position, touches -------

/// A cancelled release belongs to its own button: another button's click is not lost with it,
/// whether the click's release comes in the frame of the cancel or in the next one. (egui
/// tracks one press at a time and ends it on any release, so the cancel must not land between
/// the other button's press and release.)
#[test]
fn a_cancel_does_not_swallow_another_buttons_click() {
    use space3d_platform::MouseButtonCause;
    for release_in_the_same_frame in [true, false] {
        let mut app = page_app();
        let button = page(&app).button.center();
        // The primary button is held over empty page, then over the button.
        to(&mut app, egui::pos2(900.0, 600.0));
        down(&mut app, MouseButton::Left);
        to(&mut app, button);
        // One frame: the secondary button goes down on the button, the OS shows the primary one up.
        let i = input(&mut app);
        i.inject_mouse_button_down(MouseButton::Right);
        i.release_mouse_button(MouseButton::Left, MouseButtonCause::OsReconciled);
        if release_in_the_same_frame {
            i.inject_mouse_button_up(MouseButton::Right);
        }
        app.step_ticks(1);
        if !release_in_the_same_frame {
            up(&mut app, MouseButton::Right);
        }
        assert_eq!(
            page(&app).secondary_clicks,
            1,
            "the secondary click survived the cancel (release in the same frame: {release_in_the_same_frame})"
        );
        assert_eq!(page(&app).clicks, 0, "the cancelled primary press is no click");
        assert!(!page(&app).primary_down);
    }
}

/// The frame's order still holds where the cancel splits nothing: a click that completes
/// before the cancel (a release of a button held from an earlier frame, or a whole click) is
/// a click. The cancelled button's press is no click after a whole click. After the release of
/// a click begun in an earlier frame, egui's one shared press is already over, and its own
/// judgement of the cancelled release stands (the click stays the other button's: what is
/// kept is that nothing swallows it).
#[test]
fn clicks_that_finish_before_a_cancel_stay_clicks() {
    use space3d_platform::MouseButtonCause;
    let mut app = page_app();
    let button = page(&app).button.center();
    to(&mut app, egui::pos2(900.0, 600.0));
    down(&mut app, MouseButton::Left);
    to(&mut app, button);
    down(&mut app, MouseButton::Right);
    // Released, then the OS check cancels the primary button: both in one frame.
    let i = input(&mut app);
    i.inject_mouse_button_up(MouseButton::Right);
    i.release_mouse_button(MouseButton::Left, MouseButtonCause::OsReconciled);
    app.step_ticks(1);
    assert_eq!(page(&app).secondary_clicks, 1, "release before the cancel");
    assert!(!page(&app).primary_down);
    // A whole click before a cancel.
    let clicks_before = page(&app).clicks;
    down(&mut app, MouseButton::Left);
    let i = input(&mut app);
    i.inject_mouse_button_down(MouseButton::Right);
    i.inject_mouse_button_up(MouseButton::Right);
    i.release_mouse_button(MouseButton::Left, MouseButtonCause::OsReconciled);
    app.step_ticks(1);
    assert_eq!(page(&app).secondary_clicks, 2, "a whole click before the cancel");
    assert_eq!(
        page(&app).clicks,
        clicks_before,
        "the cancelled press is still no click"
    );
    assert!(!page(&app).primary_down);
}

/// A release outside the window is no click, even on a widget at the window's edge: the press
/// on the corner button, then the pointer leaves the window and the button comes up outside
/// (in the frame of the pointer leaving, or a frame later).
#[test]
fn a_release_outside_the_window_is_no_click_on_a_widget_at_the_edge() {
    for same_frame in [true, false] {
        let mut app = page_app();
        to(&mut app, egui::pos2(4.0, 20.0));
        down(&mut app, MouseButton::Left);
        let i = input(&mut app);
        i.inject_mouse_move(Vec2::new(1.0, 20.0));
        i.inject_mouse_left();
        if same_frame {
            i.inject_mouse_button_up(MouseButton::Left);
        }
        app.step_ticks(1);
        if !same_frame {
            up(&mut app, MouseButton::Left);
        }
        assert_eq!(
            page(&app).corner_clicks,
            0,
            "released outside (same frame: {same_frame})"
        );
        assert!(!page(&app).primary_down);
        // Released inside, the same widget is a click.
        let corner = page(&app).corner.center();
        click(&mut app, corner);
        assert_eq!(page(&app).corner_clicks, 1, "a click inside still is one");
    }
}

/// Before any pointer position is known, button events are dropped (with their release): they
/// do not go to (0, 0), where the corner window sits.
#[test]
fn buttons_before_the_pointer_is_known_are_dropped() {
    let mut app = page_app();
    assert_eq!(input(&mut app).mouse_position(), None);
    down(&mut app, MouseButton::Left);
    up(&mut app, MouseButton::Left);
    assert_eq!(page(&app).corner_clicks, 0, "no click at (0, 0)");
    assert!(!page(&app).corner_hovered);
    // A press before the pointer is known, the release after it moved in: dropped as a pair.
    down(&mut app, MouseButton::Left);
    let corner = page(&app).corner.center();
    to(&mut app, corner);
    up(&mut app, MouseButton::Left);
    assert_eq!(page(&app).corner_clicks, 0, "the release of a dropped press is dropped");
    assert!(!page(&app).primary_down);
    // Once known, buttons work.
    click(&mut app, corner);
    assert_eq!(page(&app).corner_clicks, 1);
}

/// A touch the system cancelled is no tap: egui gets no click (the pointer touch is a cancelled
/// release), while a touch that ends is one.
#[test]
fn a_cancelled_touch_is_no_tap() {
    use space3d_platform::TouchPhase;
    for (phase, clicks) in [(TouchPhase::Ended, 1), (TouchPhase::Cancelled, 0)] {
        let mut app = page_app();
        let corner = page(&app).corner.center();
        let at = Vec2::new(corner.x, corner.y);
        input(&mut app).inject_touch(1, TouchPhase::Started, at);
        app.step_ticks(1);
        input(&mut app).inject_touch(1, phase, at);
        app.step_ticks(1);
        assert_eq!(page(&app).corner_clicks, clicks, "{phase:?}");
        assert!(!page(&app).primary_down, "{phase:?}");
        app.step_ticks(1);
        assert!(!page(&app).primary_down);
    }
}

// ---- telling a cancel from a real release ------------------------------------------------------

/// egui ends the press on a cancelled release like on a real one (`hovered()` with
/// `any_released()` is true), so game code needs to ask: `pointer_cancelled_this_frame`,
/// `EguiContext::{pointer,button}_cancelled_this_frame` and `pointer_cancel_cause`.
#[test]
fn game_code_can_tell_a_cancelled_release_from_a_real_one() {
    use space3d_platform::MouseButtonCause;
    let flags = |app: &App| {
        let e = app.resources.expect::<EguiContext>();
        let c = ctx(app);
        (
            e.pointer_cancelled_this_frame(),
            e.button_cancelled_this_frame(egui::PointerButton::Primary),
            e.button_cancelled_this_frame(egui::PointerButton::Secondary),
            pointer_cancelled_this_frame(&c),
            pointer_cancel_cause(&c, egui::PointerButton::Primary),
        )
    };
    let none = (false, false, false, false, None);
    let mut app = page_app();
    let source = page(&app).source.center();
    let zone = page(&app).zone.center();
    let drag_to_zone = |app: &mut App| {
        to(app, source);
        down(app, MouseButton::Left);
        for k in 1..=4 {
            to(app, source + (zone - source) * (k as f32 / 4.0));
        }
    };
    assert_eq!(flags(&app), none);
    // A real drop: a release over the zone, not a cancel.
    drag_to_zone(&mut app);
    up(&mut app, MouseButton::Left);
    assert_eq!((page(&app).zone_releases, page(&app).zone_real_releases), (1, 1));
    assert_eq!(flags(&app), none);
    // A cancel by the OS's state: egui sees a release over the zone (the naive check acts), the
    // helper says it was a cancel; only the primary button, with the cause.
    drag_to_zone(&mut app);
    input(&mut app).release_mouse_button(MouseButton::Left, MouseButtonCause::OsReconciled);
    app.step_ticks(1);
    assert_eq!((page(&app).zone_releases, page(&app).zone_real_releases), (2, 1));
    assert_eq!(page(&app).dropped, 1, "and egui's own drop did not happen");
    assert_eq!(
        flags(&app),
        (true, true, false, true, Some(MouseButtonCause::OsReconciled))
    );
    app.step_ticks(1);
    assert_eq!(flags(&app), none, "this frame's only");
    // A focus loss.
    drag_to_zone(&mut app);
    blur(&mut app);
    assert_eq!((page(&app).zone_releases, page(&app).zone_real_releases), (3, 1));
    assert_eq!(flags(&app).4, Some(MouseButtonCause::FocusLost));
    refocus(&mut app);
    assert_eq!(flags(&app), none);
    // A cancel of another button is not the primary's.
    to(&mut app, zone);
    down(&mut app, MouseButton::Right);
    input(&mut app).release_mouse_button(MouseButton::Right, MouseButtonCause::FocusLost);
    app.step_ticks(1);
    let (any, primary, secondary, ..) = flags(&app);
    assert!(any && secondary && !primary);
    // While the UI is disabled nothing is a cancel of the frame.
    app.step_ticks(1);
    down(&mut app, MouseButton::Left);
    input(&mut app).release_mouse_button(MouseButton::Left, MouseButtonCause::FocusLost);
    app.resources.expect_mut::<EguiContext>().set_enabled(false);
    app.step_ticks(1);
    assert_eq!(flags(&app), none, "no pass, no cancel");
}
