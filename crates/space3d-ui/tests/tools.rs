//! Layout of the docked engine tools (debug overlay, profiler, console): they stay 6 pt inside
//! the screen (or the `ToolDockArea`), follow it when it is resized until the user drags them
//! away, never grow with their contents, never overlap each other, stay above the game's windows
//! and look the same under a game's roomy style.

use space3d_core::glam::{UVec2, Vec2};
use space3d_core::prelude::*;
use space3d_platform::{Input, MouseButton, WindowState};
use space3d_ui::*;
use std::sync::Arc;

const EDGE: f32 = 6.0;

/// UI-only app (layout needs no renderer) of `size` physical pixels at `ppp`, with the profiler
/// (a few recorded frames) and the console (a very long line) open.
fn tools_app(size: (u32, u32), ppp: f32, roomy: bool) -> App {
    let mut app = App::new();
    app.add_plugin(UiPlugin {
        install_tracing: false,
        fallback_size: size,
        ..UiPlugin::deterministic().with_pixels_per_point(ppp)
    });
    if roomy {
        let ctx = app.resources.expect::<EguiContext>().ctx().clone();
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = egui::vec2(14.0, 10.0);
            s.spacing.button_padding = egui::vec2(12.0, 6.0);
            s.spacing.window_margin = egui::Margin::same(16);
            s.spacing.interact_size = egui::vec2(48.0, 32.0);
            for font in s.text_styles.values_mut() {
                font.size += 4.0;
            }
        });
    }
    let mut profiler = Profiler::default();
    for frame in 0..10u64 {
        let spans = (0..14u32)
            .map(|i| TimelineSpan {
                name: Arc::from(format!("game::system_{i}")),
                start: f64::from(i) * 0.0002,
                duration: 0.0001 * f64::from(i + 1),
                depth: 0,
            })
            .collect();
        profiler.record(FrameTimeline {
            index: frame + 1,
            duration: 0.004,
            spans,
        });
    }
    profiler.visible = true;
    profiler.frozen = true;
    app.insert_resource(profiler);
    let console = app.resources.expect_mut::<Console>();
    console.visible = true;
    console.capture_logs = false;
    console.print(
        LogLevel::Warn,
        "assets: failed to load /Users/player/Library/Application Support/Game/mods/community-pack-v3/\
         textures/hull_plating_damaged_normal.ktx2 (unsupported format) after 3 retries",
    );
    app
}

fn area(app: &App, id: &str) -> egui::Rect {
    let ctx = app.resources.expect::<EguiContext>().ctx().clone();
    ctx.memory(|m| m.area_rect(egui::Id::new(id)))
        .unwrap_or_else(|| panic!("{id} shown"))
}

fn rects(app: &App) -> (egui::Rect, egui::Rect, egui::Rect) {
    let ctx = app.resources.expect::<EguiContext>().ctx().clone();
    let rect = |id: &str| {
        ctx.memory(|m| m.area_rect(egui::Id::new(id)))
            .unwrap_or_else(|| panic!("{id} shown"))
    };
    (ctx.content_rect(), rect("space3d_profiler"), rect("space3d_console"))
}

#[test]
fn docked_tools_stay_inside_and_apart() {
    for (size, ppp) in [
        ((1280, 720), 1.0),
        ((1920, 1080), 1.0),
        ((1280, 720), 1.5),
        ((1280, 800), 1.5),
    ] {
        let mut app = tools_app(size, ppp, false);
        app.step_ticks(4);
        let (screen, profiler, console) = rects(&app);
        let inside = screen.shrink(EDGE - 0.5);
        let label = format!("{size:?} at {ppp}x: screen {screen:?}, profiler {profiler:?}, console {console:?}");
        assert!(
            inside.contains_rect(profiler) && inside.contains_rect(console),
            "{label}"
        );
        assert!(
            (profiler.right() - (screen.right() - EDGE)).abs() < 1.0,
            "profiler docked right: {label}"
        );
        assert!((profiler.top() - EDGE).abs() < 1.0, "profiler docked top: {label}");
        assert!((console.left() - EDGE).abs() < 1.0, "console docked left: {label}");
        assert!(
            (console.bottom() - (screen.bottom() - EDGE)).abs() < 1.0,
            "console docked bottom: {label}"
        );
        assert!(console.width() <= 640.5, "a long log line wraps, never widens: {label}");
        assert!(
            !profiler.intersects(console) && profiler.bottom() <= console.top() - EDGE + 0.5,
            "no overlap: {label}"
        );
        // Frames later nothing has moved or grown.
        app.step_ticks(30);
        let (_, p2, c2) = rects(&app);
        assert_eq!((p2, c2), (profiler, console), "stable: {label}");
    }
}

#[test]
fn a_games_roomy_style_changes_no_tool_geometry() {
    for size in [(1280, 720), (640, 420)] {
        let mut plain = tools_app(size, 1.0, false);
        let mut roomy = tools_app(size, 1.0, true);
        plain.step_ticks(4);
        roomy.step_ticks(4);
        let (_, p1, c1) = rects(&plain);
        let (_, p2, c2) = rects(&roomy);
        assert_eq!((p1, c1), (p2, c2), "{size:?}");
    }
}

#[test]
fn the_profiler_keeps_clear_of_the_debug_overlay_on_small_screens() {
    // 640 × 330 to 640 × 400 pt (1280 × 720 at 2x...): the profiler's default 440 pt would
    // reach under the overlay, and the height left above the console fits only a few rows.
    for (size, details) in [
        ((1280, 720), false),
        ((1280, 720), true),
        ((1280, 800), true),
        ((1280, 660), true),
        ((1280, 680), false),
    ] {
        let mut app = tools_app(size, 2.0, false);
        app.insert_resource(space3d_render::RenderStats::default());
        {
            let overlay = app.resources.expect_mut::<DebugOverlay>();
            overlay.visible = true;
            overlay.details = details;
        }
        app.step_ticks(4);
        let (screen, profiler, console) = rects(&app);
        let overlay = area(&app, "space3d_debug_overlay");
        let label = format!(
            "{size:?} at 2x, details {details}: overlay {overlay:?}, profiler {profiler:?}, console {console:?}"
        );
        assert!(screen.shrink(EDGE - 0.5).contains_rect(profiler), "{label}");
        assert!(
            profiler.left() >= overlay.right() + EDGE - 0.5,
            "the profiler ends right of the overlay: {label}"
        );
        assert!(profiler.width() >= 300.0, "{label}");
        assert!(
            (profiler.right() - (screen.right() - EDGE)).abs() < 1.0,
            "still docked right: {label}"
        );
        assert!(
            profiler.bottom() <= console.top() - EDGE + 0.5,
            "the profiler (whole rows only) stays above the console: {label}"
        );
        app.step_ticks(30);
        assert_eq!(rects(&app), (screen, profiler, console), "stable: {label}");
    }
}

/// A 1-pixel-per-point "window" of `size` physical pixels (resizable by the test).
fn window(size: (u32, u32)) -> WindowState {
    WindowState {
        physical_size: UVec2::new(size.0, size.1),
        scale_factor: 1.0,
        focused: true,
        suspended: false,
        title: String::new(),
        cursor_visible: true,
        cursor_grab: false,
    }
}

fn resize(app: &mut App, size: (u32, u32)) {
    app.resources.expect_mut::<WindowState>().physical_size = UVec2::new(size.0, size.1);
    app.step_ticks(3);
}

/// The overlay, profiler and console in their corners of `dock`, 6 pt in, and apart.
fn assert_docked(app: &App, dock: egui::Rect, label: &str) {
    let (_, profiler, console) = rects(app);
    let overlay = area(app, "space3d_debug_overlay");
    let label = format!("{label}: dock {dock:?}, overlay {overlay:?}, profiler {profiler:?}, console {console:?}");
    let inside = dock.shrink(EDGE - 0.5);
    for r in [overlay, profiler, console] {
        assert!(inside.contains_rect(r), "inside the dock area: {label}");
    }
    assert!(
        overlay.left_top().distance(dock.left_top() + egui::vec2(EDGE, EDGE)) < 1.0,
        "overlay top-left: {label}"
    );
    assert!(
        profiler
            .right_top()
            .distance(dock.right_top() + egui::vec2(-EDGE, EDGE))
            < 1.0,
        "profiler top-right: {label}"
    );
    assert!(
        console
            .left_bottom()
            .distance(dock.left_bottom() + egui::vec2(EDGE, -EDGE))
            < 1.0,
        "console bottom-left: {label}"
    );
    assert!(
        !profiler.intersects(console) && !profiler.intersects(overlay),
        "apart: {label}"
    );
}

fn screen(app: &App) -> egui::Rect {
    app.resources.expect::<EguiContext>().ctx().content_rect()
}

/// Docked tools follow the screen when the window grows and when it shrinks (they used to stay
/// where they were docked first, floating mid-screen after the window grew).
#[test]
fn docked_tools_follow_the_screen_when_it_is_resized() {
    let mut app = tools_app((1280, 720), 1.0, false);
    app.insert_resource(window((1280, 720)));
    app.resources.expect_mut::<DebugOverlay>().visible = true;
    app.step_ticks(4);
    assert_docked(&app, screen(&app), "1280 x 720");
    for size in [(1920, 1080), (1280, 720), (1600, 600), (900, 1000), (1920, 1080)] {
        resize(&mut app, size);
        assert_eq!(screen(&app).size(), egui::vec2(size.0 as f32, size.1 as f32));
        assert_docked(&app, screen(&app), &format!("resized to {size:?}"));
    }
}

/// Where the pointer is, in physical pixels (1 pixel per point).
fn pointer_at(app: &mut App, p: egui::Pos2) {
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_move(Vec2::new(p.x, p.y));
    app.step_ticks(1);
}

fn click(app: &mut App, p: egui::Pos2) {
    pointer_at(app, p);
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_down(MouseButton::Left);
    app.step_ticks(1);
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_up(MouseButton::Left);
    app.step_ticks(1);
}

/// A tool the user dragged away stays where it was put when the screen changes; double-clicking
/// its title docks it again. Clicks inside a docked tool do not undock it.
#[test]
fn a_dragged_tool_floats_until_its_title_is_double_clicked() {
    let mut app = tools_app((1280, 720), 1.0, false);
    app.insert_resource(window((1280, 720)));
    app.step_ticks(4);
    let (_, profiler, _) = rects(&app);
    // A click on the Freeze checkbox's row (not a drag) leaves it docked.
    click(&mut app, profiler.left_top() + egui::vec2(40.0, 30.0));
    resize(&mut app, (1400, 800));
    let (screen, docked, _) = rects(&app);
    assert!(
        (docked.right() - (screen.right() - EDGE)).abs() < 1.0,
        "still docked: {docked:?}"
    );

    // Drag it by its title 300 pt left and 200 pt down.
    let grab = docked.left_top() + egui::vec2(30.0, 10.0);
    pointer_at(&mut app, grab);
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_down(MouseButton::Left);
    app.step_ticks(1);
    for i in 1..=10 {
        pointer_at(&mut app, grab + egui::vec2(-30.0, 20.0) * i as f32);
    }
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_up(MouseButton::Left);
    app.step_ticks(2);
    let (_, moved, _) = rects(&app);
    assert!(
        (moved.left_top() - (docked.left_top() + egui::vec2(-300.0, 200.0))).length() < 1.5,
        "dragged: {docked:?} -> {moved:?}"
    );
    resize(&mut app, (1920, 1080));
    let (_, floating, _) = rects(&app);
    assert_eq!(floating.left_top(), moved.left_top(), "a dragged tool stays put");

    // Double-click the title: docked again (top-right of the new screen).
    let title = floating.left_top() + egui::vec2(30.0, 10.0);
    click(&mut app, title);
    click(&mut app, title);
    app.step_ticks(2);
    let (screen, redocked, _) = rects(&app);
    assert!(
        redocked
            .right_top()
            .distance(screen.right_top() + egui::vec2(-EDGE, EDGE))
            < 1.0,
        "docked again: {redocked:?}"
    );
}

/// With a `ToolDockArea` (an editor's viewport) the tools dock to its corners, never over what
/// lies outside it, and follow it when it changes; an area too small for a tool is ignored.
#[test]
fn tools_dock_inside_the_tool_dock_area() {
    let mut app = tools_app((1280, 720), 1.0, false);
    app.insert_resource(window((1280, 720)));
    app.resources.expect_mut::<DebugOverlay>().visible = true;
    let viewport = egui::Rect::from_min_max(egui::pos2(210.0, 23.0), egui::pos2(1000.0, 720.0));
    app.insert_resource(ToolDockArea(viewport));
    app.step_ticks(4);
    assert_docked(&app, viewport, "editor viewport at 1280 x 720");
    // The panels are resized, then the window.
    let narrower = egui::Rect::from_min_max(egui::pos2(300.0, 23.0), egui::pos2(900.0, 720.0));
    app.insert_resource(ToolDockArea(narrower));
    app.step_ticks(3);
    assert_docked(&app, narrower, "narrower viewport");
    resize(&mut app, (1920, 1080));
    let grown = egui::Rect::from_min_max(egui::pos2(210.0, 23.0), egui::pos2(1640.0, 1080.0));
    app.insert_resource(ToolDockArea(grown));
    app.step_ticks(3);
    assert_docked(&app, grown, "viewport at 1920 x 1080");
    // An area partly off screen is clipped to the screen; one too small is ignored.
    app.insert_resource(ToolDockArea(egui::Rect::from_min_max(
        egui::pos2(210.0, 23.0),
        egui::pos2(4000.0, 4000.0),
    )));
    app.step_ticks(3);
    assert_docked(
        &app,
        egui::Rect::from_min_max(egui::pos2(210.0, 23.0), egui::pos2(1920.0, 1080.0)),
        "clipped",
    );
    app.insert_resource(ToolDockArea(egui::Rect::from_min_size(
        egui::pos2(500.0, 500.0),
        egui::vec2(100.0, 50.0),
    )));
    app.step_ticks(3);
    assert_docked(&app, screen(&app), "too small: the whole screen");
    app.resources.remove::<ToolDockArea>();
    app.step_ticks(3);
    assert_docked(&app, screen(&app), "no dock area");
}

/// Small screens: the profiler narrows (average and calls columns only) to stay beside the
/// overlay, or docks under it on a tall narrow screen; the three tools never overlap.
#[test]
fn tools_share_small_screens_without_overlapping() {
    for size in [(480, 320), (390, 844), (640, 400)] {
        let mut app = tools_app(size, 1.0, false);
        app.insert_resource(window(size));
        app.resources.expect_mut::<DebugOverlay>().visible = true;
        app.step_ticks(4);
        let (screen, profiler, console) = rects(&app);
        let overlay = area(&app, "space3d_debug_overlay");
        let label = format!("{size:?}: overlay {overlay:?}, profiler {profiler:?}, console {console:?}");
        let inside = screen.shrink(EDGE - 0.5);
        assert!(
            inside.contains_rect(profiler) && inside.contains_rect(console),
            "{label}"
        );
        assert!(
            !profiler.intersects(overlay) && !profiler.intersects(console) && !console.intersects(overlay),
            "no overlap: {label}"
        );
        assert!(
            (profiler.right() - (screen.right() - EDGE)).abs() < 1.0,
            "docked right: {label}"
        );
        if size.0 < size.1 {
            assert!(
                profiler.top() >= overlay.bottom() + EDGE - 0.5,
                "under the overlay: {label}"
            );
        } else {
            assert!(
                profiler.left() >= overlay.right() + EDGE - 0.5,
                "beside the overlay: {label}"
            );
        }
        app.step_ticks(20);
        assert_eq!(rects(&app), (screen, profiler, console), "stable: {label}");
    }
}

/// The tools sit on egui's foreground layer: a game window over them is drawn (and hit) below.
#[test]
fn tools_stay_above_the_games_windows() {
    let mut app = tools_app((1280, 720), 1.0, false);
    app.add_ui_system(|ctx: &egui::Context, _: &mut SystemCtx| {
        egui::Window::new("Cargo hold")
            .id(egui::Id::new("cargo_hold"))
            .fixed_pos([900.0, 40.0])
            .show(ctx, |ui| {
                ui.label("Salvage: 12 t");
                let _ = ui.button("Jettison");
            });
    });
    app.step_ticks(4);
    let (_, profiler, _) = rects(&app);
    let ctx = app.resources.expect::<EguiContext>().ctx().clone();
    let game = ctx
        .memory(|m| m.area_rect(egui::Id::new("cargo_hold")))
        .expect("game window shown");
    let both = game.intersect(profiler);
    assert!(
        both.is_positive(),
        "the game window overlaps the profiler: {game:?} {profiler:?}"
    );
    let top = ctx.layer_id_at(both.center()).expect("a layer");
    assert_eq!(top.id, egui::Id::new("space3d_profiler"), "the profiler is on top");
    assert_eq!(top.order, egui::Order::Foreground);
}
