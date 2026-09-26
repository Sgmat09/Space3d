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

/// Whether `a` and `b` are the same rect up to rounding.
fn same_rect(a: egui::Rect, b: egui::Rect) -> bool {
    a.min.distance(b.min) < 1.0 && a.max.distance(b.max) < 1.0
}

/// A tools app of `size` with the overlay, profiler and console open, after a few frames.
fn tools_at(size: (u32, u32)) -> App {
    let mut app = tools_app(size, 1.0, false);
    app.insert_resource(window(size));
    app.resources.expect_mut::<DebugOverlay>().visible = true;
    app.step_ticks(4);
    app
}

/// Docked tools the user did not resize take the size they get when the app starts at the new
/// size: the console its 40 % of the new height, the profiler the room above it (the console
/// kept its first screen's height, squeezing the profiler on a smaller one).
#[test]
fn docked_tools_take_their_default_size_when_the_screen_changes() {
    for (from, to) in [
        ((1920, 1080), (1280, 600)),
        ((1280, 720), (1920, 1080)),
        ((1280, 600), (640, 400)),
        ((640, 400), (1280, 720)),
    ] {
        let mut app = tools_at(from);
        resize(&mut app, to);
        app.step_ticks(3);
        let fresh = tools_at(to);
        let (_, profiler, console) = rects(&app);
        let (_, want_profiler, want_console) = rects(&fresh);
        let label = format!(
            "{from:?} -> {to:?}: profiler {profiler:?} (fresh {want_profiler:?}), console {console:?} (fresh \
             {want_console:?})"
        );
        assert!(same_rect(console, want_console), "console: {label}");
        assert!(same_rect(profiler, want_profiler), "profiler: {label}");
        assert_docked(&app, screen(&app), &label);
    }
    // The same when a host's dock area changes (an editor's panels, or the window under them),
    // and when the host sets it from the first frame on.
    let editor = |size: (u32, u32)| {
        egui::Rect::from_min_max(
            egui::pos2(210.0, 23.0),
            egui::pos2(size.0 as f32 - 280.0, size.1 as f32),
        )
    };
    // `late`: the dock area is set during the first frame, after the tools drew (as the editor
    // does): they start on the whole screen.
    let fresh_at = |size: (u32, u32), dock: egui::Rect, late: bool| {
        let mut app = tools_app(size, 1.0, false);
        app.insert_resource(window(size));
        app.resources.expect_mut::<DebugOverlay>().visible = true;
        if late {
            app.step_ticks(1);
        }
        app.insert_resource(ToolDockArea(dock));
        app.step_ticks(6);
        rects(&app)
    };
    let fresh = |size: (u32, u32), dock: egui::Rect| {
        let (early, late) = (fresh_at(size, dock, false), fresh_at(size, dock, true));
        assert!(
            same_rect(early.1, late.1) && same_rect(early.2, late.2),
            "{size:?}: dock area from the first frame {early:?}, from the second {late:?}"
        );
        early
    };
    for (from, to) in [((1920, 1080), (1280, 600)), ((1280, 720), (1920, 1080))] {
        let mut app = tools_at(from);
        app.insert_resource(ToolDockArea(editor(from)));
        app.step_ticks(4);
        app.resources.expect_mut::<WindowState>().physical_size = UVec2::new(to.0, to.1);
        app.insert_resource(ToolDockArea(editor(to)));
        app.step_ticks(6);
        let ((_, p1, c1), (_, p2, c2)) = (rects(&app), fresh(to, editor(to)));
        let label = format!("{from:?} -> {to:?}: {p1:?} {c1:?} vs fresh {p2:?} {c2:?}");
        assert!(same_rect(c1, c2) && same_rect(p1, p2), "{label}");
        assert_docked(&app, editor(to), &label);
    }
    let narrow = egui::Rect::from_min_max(egui::pos2(300.0, 23.0), egui::pos2(900.0, 400.0));
    let mut app = tools_at((1280, 720));
    app.insert_resource(ToolDockArea(narrow));
    app.step_ticks(6);
    let ((_, p1, c1), (_, p2, c2)) = (rects(&app), fresh((1280, 720), narrow));
    assert!(
        same_rect(c1, c2) && same_rect(p1, p2),
        "{p1:?} {c1:?} vs fresh {p2:?} {c2:?}"
    );
}

/// Press at `from`, move to `to` in steps, release (physical pixels, 1 pixel per point).
fn drag(app: &mut App, from: egui::Pos2, to: egui::Pos2) {
    pointer_at(app, from);
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_down(MouseButton::Left);
    app.step_ticks(1);
    for i in 1..=10 {
        pointer_at(app, from + (to - from) * (i as f32 / 10.0));
    }
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_up(MouseButton::Left);
    app.step_ticks(2);
}

/// A tool the user resized keeps that size (capped to the dock area) when the screen changes,
/// and stays docked when resized by a free edge (egui moves a window's top-left corner a frame
/// before it applies the new size: the docked corner seemed to move, which undocked the tool).
#[test]
fn a_tool_the_user_resized_keeps_its_size() {
    let mut app = tools_at((1280, 720));
    let (_, _, console) = rects(&app);
    // Its top edge 60 pt up.
    let edge = egui::pos2(console.center().x, console.top() + 1.0);
    drag(&mut app, edge, edge - egui::vec2(0.0, 60.0));
    let (_, profiler, resized) = rects(&app);
    let label = format!("{console:?} -> {resized:?}");
    // (The log is a whole number of lines tall.)
    let grown = resized.height() - console.height();
    assert!((45.0..61.0).contains(&grown), "resized: {label}");
    assert!(
        resized.left_bottom().distance(console.left_bottom()) < 1.0,
        "still docked: {label}"
    );
    assert!(
        profiler.bottom() <= resized.top() - EDGE + 0.5,
        "the profiler keeps above it"
    );
    for size in [(1920, 1080), (1280, 600)] {
        resize(&mut app, size);
        app.step_ticks(3);
        let (screen, profiler, console) = rects(&app);
        let label = format!("{size:?}: {resized:?} -> {console:?}");
        assert!((console.size() - resized.size()).length() < 1.0, "same size: {label}");
        assert!(
            console
                .left_bottom()
                .distance(screen.left_bottom() + egui::vec2(EDGE, -EDGE))
                < 1.0,
            "docked: {label}"
        );
        assert!(!profiler.intersects(console), "apart: {label}, profiler {profiler:?}");
    }

    // The profiler by its left edge (free: it docks top-right), 80 pt wider.
    let (_, profiler, _) = rects(&app);
    let edge = egui::pos2(profiler.left() + 1.0, profiler.center().y);
    drag(&mut app, edge, edge - egui::vec2(80.0, 0.0));
    let (screen, wider, _) = rects(&app);
    let label = format!("{profiler:?} -> {wider:?}");
    assert!(
        (wider.width() - (profiler.width() + 80.0)).abs() < 1.5,
        "resized: {label}"
    );
    let dock = screen.right_top() + egui::vec2(-EDGE, EDGE);
    assert!(wider.right_top().distance(dock) < 1.0, "still docked: {label}");
    resize(&mut app, (1600, 900));
    app.step_ticks(3);
    let (screen, after, _) = rects(&app);
    let label = format!("{wider:?} -> {after:?}");
    assert!((after.width() - wider.width()).abs() < 1.0, "same width: {label}");
    let dock = screen.right_top() + egui::vec2(-EDGE, EDGE);
    assert!(after.right_top().distance(dock) < 1.0, "docked: {label}");
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

/// A tools app of `screen` with the overlay (with render stats, as in a game), profiler and
/// console open, docked in `dock`, after a few frames.
fn tools_in(screen: (u32, u32), dock: egui::Rect) -> App {
    let mut app = tools_app(screen, 1.0, false);
    app.insert_resource(window(screen));
    app.insert_resource(space3d_render::RenderStats::default());
    app.resources.expect_mut::<DebugOverlay>().visible = true;
    app.insert_resource(ToolDockArea(dock));
    app.step_ticks(6);
    app
}

/// The overlay, profiler and console inside `dock` (6 pt in), apart (6 pt between them), side
/// by side: the overlay top-left, the console bottom-docked right of it and the profiler
/// top-right, right of the console.
fn assert_side_by_side(app: &App, dock: egui::Rect, label: &str) {
    let (_, profiler, console) = rects(app);
    let overlay = area(app, "space3d_debug_overlay");
    let label = format!("{label}: dock {dock:?}, overlay {overlay:?}, profiler {profiler:?}, console {console:?}");
    let inside = dock.shrink(EDGE - 0.5);
    for r in [overlay, profiler, console] {
        assert!(inside.contains_rect(r), "inside the dock area: {label}");
    }
    let apart = |a: egui::Rect, b: egui::Rect| !a.expand(EDGE - 0.5).intersects(b);
    assert!(
        apart(overlay, profiler) && apart(overlay, console) && apart(profiler, console),
        "no overlaps: {label}"
    );
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
        (console.bottom() - (dock.bottom() - EDGE)).abs() < 1.0,
        "console docked at the bottom: {label}"
    );
    assert!(
        console.left() >= overlay.right() + EDGE - 0.5 && console.right() <= profiler.left() - EDGE + 0.5,
        "console between the overlay and the profiler: {label}"
    );
    assert!(console.width() >= 160.0 - 0.5, "{label}");
}

/// SpaceCorps2027's HUD leaves a 1024 × 233 pt band of a 1024 × 640 screen free: with F3, F4 and
/// the console open the three used to overlap (the profiler's 300 pt minimum height, the overlay
/// over the console's 40 %). In a short area they dock side by side: overlay | console |
/// profiler, each as tall as the area allows.
#[test]
fn tools_dock_side_by_side_in_a_short_dock_area() {
    let band = egui::Rect::from_min_max(egui::pos2(0.0, 300.0), egui::pos2(1024.0, 533.0));
    let mut app = tools_in((1024, 640), band);
    assert_side_by_side(&app, band, "1024 x 233 band");
    let (_, profiler, console) = rects(&app);
    assert_eq!(profiler.width(), 440.0, "the profiler keeps its width");
    assert!(
        console.height() > 0.8 * (band.height() - 2.0 * EDGE),
        "nothing above the console: it takes the area's height, {console:?}"
    );
    app.step_ticks(30);
    assert_eq!(rects(&app), (screen(&app), profiler, console), "stable");
}

/// Short areas of several shapes: side by side, inside, no overlaps.
#[test]
fn short_dock_areas_never_overlap() {
    for (w, h) in [
        (800, 200),
        (1280, 180),
        (1920, 260),
        (640, 240),
        (1024, 233),
        (1100, 280),
    ] {
        let dock = egui::Rect::from_min_size(egui::pos2(0.0, 300.0), egui::vec2(w as f32, h as f32));
        let mut app = tools_in((w, h + 400), dock);
        let label = format!("{w} x {h}");
        assert_side_by_side(&app, dock, &label);
        let before = rects(&app);
        app.step_ticks(20);
        assert_eq!(rects(&app), before, "stable: {label}");
    }
}

/// The overlay, profiler and console in their corners of `dock` and the full 6 pt apart.
fn assert_stacked(app: &App, dock: egui::Rect, label: &str) {
    assert_docked(app, dock, label);
    let (_, profiler, console) = rects(app);
    let overlay = area(app, "space3d_debug_overlay");
    let label = format!("{label}: overlay {overlay:?}, profiler {profiler:?}, console {console:?}");
    let apart = |a: egui::Rect, b: egui::Rect| !a.expand(EDGE - 0.5).intersects(b);
    assert!(
        apart(overlay, profiler) && apart(overlay, console) && apart(profiler, console),
        "apart: {label}"
    );
}

/// A profiler with 14 systems nested 3 deep (a taller flame view), shown.
fn nested_profiler() -> Profiler {
    let mut profiler = Profiler::default();
    for frame in 0..10u64 {
        let spans = (0..14u32)
            .map(|i| TimelineSpan {
                name: Arc::from(format!("game::system_{i}")),
                start: f64::from(i) * 0.0002,
                duration: 0.0001 * f64::from(i + 1),
                depth: i % 3,
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
    profiler
}

/// Short areas where the stacked layout fits as the tools are drawn keep it: the console is a
/// whole number of log lines tall (about 125 pt of its default 140), so the overlay ends above it
/// in a 1100 × 300 pt band (at any scale), and a profiler with nested spans (3 rows in about
/// 180 pt) fits above it on a 640 × 330 pt screen. The tools used to go side by side there,
/// going by the console's default height.
#[test]
fn short_areas_where_the_stacked_layout_fits_stay_stacked() {
    let band = egui::Rect::from_min_size(egui::pos2(0.0, 300.0), egui::vec2(1100.0, 300.0));
    for ppp in [1.0f32, 1.5, 2.0] {
        let size = ((1100.0 * ppp) as u32, (700.0 * ppp) as u32);
        let mut app = tools_app(size, ppp, false);
        app.insert_resource(window(size));
        app.insert_resource(space3d_render::RenderStats::default());
        app.resources.expect_mut::<DebugOverlay>().visible = true;
        app.insert_resource(ToolDockArea(band));
        app.step_ticks(6);
        assert_stacked(&app, band, &format!("1100 x 300 band at {ppp}x"));
    }
    let mut app = tools_app((640, 330), 1.0, false);
    app.insert_resource(window((640, 330)));
    app.insert_resource(space3d_render::RenderStats::default());
    app.insert_resource(nested_profiler());
    app.resources.expect_mut::<DebugOverlay>().visible = true;
    app.step_ticks(6);
    assert_stacked(&app, screen(&app), "640 x 330 screen, nested spans");
    let before = rects(&app);
    app.step_ticks(20);
    assert_eq!(rects(&app), before, "stable");
}

/// On a dock area off the pixel grid (the overlay is drawn half a point right of its corner)
/// the tools side by side keep the full 6 pt apart.
#[test]
fn gaps_stay_whole_on_a_dock_area_off_the_pixel_grid() {
    let dock = egui::Rect::from_min_size(egui::pos2(37.5, 123.25), egui::vec2(1011.5, 233.5));
    let app = tools_in((1100, 500), dock);
    assert_side_by_side(&app, dock, "off the grid");
    let (_, profiler, console) = rects(&app);
    let overlay = area(&app, "space3d_debug_overlay");
    let label = format!("overlay {overlay:?}, console {console:?}, profiler {profiler:?}");
    assert!(console.left() - overlay.right() >= EDGE - 0.01, "{label}");
    assert!(profiler.left() - console.right() >= EDGE - 0.01, "{label}");
}

/// A profiler without frames yet shows only its message: it stays top-right above the console
/// (which moves right of the overlay) instead of taking a column of the band's height.
#[test]
fn a_profiler_without_frames_stays_above_the_console() {
    let band = egui::Rect::from_min_max(egui::pos2(0.0, 300.0), egui::pos2(1024.0, 533.0));
    let mut app = tools_app((1024, 640), 1.0, false);
    app.insert_resource(window((1024, 640)));
    app.insert_resource(space3d_render::RenderStats::default());
    let mut empty = Profiler::default();
    empty.visible = true;
    app.insert_resource(empty);
    app.resources.expect_mut::<DebugOverlay>().visible = true;
    app.insert_resource(ToolDockArea(band));
    app.step_ticks(6);
    let (_, profiler, console) = rects(&app);
    let overlay = area(&app, "space3d_debug_overlay");
    let label = format!("overlay {overlay:?}, profiler {profiler:?}, console {console:?}");
    let inside = band.shrink(EDGE - 0.5);
    let apart = |a: egui::Rect, b: egui::Rect| !a.expand(EDGE - 0.5).intersects(b);
    for r in [overlay, profiler, console] {
        assert!(inside.contains_rect(r), "inside: {label}");
    }
    assert!(
        apart(overlay, profiler) && apart(overlay, console) && apart(profiler, console),
        "apart: {label}"
    );
    assert_eq!(profiler.width(), 440.0, "its own width: {label}");
    assert!(profiler.bottom() < console.top(), "above the console: {label}");
    assert!(
        console.left() >= overlay.right() + EDGE - 0.5,
        "right of the overlay: {label}"
    );
}

/// Making a docked tool smaller by a free edge is a resize too (egui remembers a window being
/// resized where the drag moves it next frame; a narrowing drag moved that off the press, so only
/// growing counted): in a short band the console takes the room a narrowed profiler leaves, and
/// both keep the sizes the user gave them when the area changes.
#[test]
fn a_tool_the_user_made_smaller_keeps_its_size() {
    let band = egui::Rect::from_min_max(egui::pos2(0.0, 300.0), egui::pos2(1024.0, 533.0));
    let tall = egui::Rect::from_min_max(egui::pos2(0.0, 23.0), egui::pos2(1024.0, 640.0));
    let mut app = tools_in((1024, 640), band);
    let (_, profiler, console) = rects(&app);
    // The profiler by its left edge (free: it docks top-right), 150 pt narrower.
    let edge = egui::pos2(profiler.left() + 1.0, profiler.center().y);
    drag(&mut app, edge, edge + egui::vec2(150.0, 0.0));
    app.step_ticks(2);
    let (_, narrowed, wider) = rects(&app);
    let label = format!("{profiler:?} -> {narrowed:?}, console {console:?} -> {wider:?}");
    assert!(
        (narrowed.width() - (profiler.width() - 150.0)).abs() < 1.5,
        "resized: {label}"
    );
    assert_side_by_side(&app, band, "narrowed profiler");
    assert!(
        (narrowed.left() - wider.right() - EDGE).abs() < 1.0,
        "the console takes the room: {label}"
    );
    app.insert_resource(ToolDockArea(tall));
    app.step_ticks(6);
    assert_stacked(&app, tall, "tall");
    let (_, after, console) = rects(&app);
    assert!(
        (after.width() - narrowed.width()).abs() < 1.0,
        "the profiler keeps its width: {narrowed:?} -> {after:?}"
    );

    // The console by its top edge (free: it docks bottom-left), 40 pt lower.
    let edge = egui::pos2(console.center().x, console.top() + 1.0);
    drag(&mut app, edge, edge + egui::vec2(0.0, 40.0));
    app.step_ticks(2);
    let (_, _, shorter) = rects(&app);
    let label = format!("{console:?} -> {shorter:?}");
    // (The log is a whole number of lines tall: up to a line more.)
    assert!(
        (26.0..54.0).contains(&(console.height() - shorter.height())),
        "resized: {label}"
    );
    assert!(
        shorter.left_bottom().distance(console.left_bottom()) < 1.0,
        "still docked: {label}"
    );
    let other = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1024.0, 640.0));
    app.insert_resource(ToolDockArea(other));
    app.step_ticks(6);
    let (_, _, after) = rects(&app);
    assert!(
        (after.height() - shorter.height()).abs() < 1.0,
        "the console keeps its height: {shorter:?} -> {after:?}"
    );
    assert_stacked(&app, other, "another area");
}

/// When the dock area gets short the tools go side by side; when it gets tall again they dock
/// back to their corners, at the sizes a fresh start at each area gives them.
#[test]
fn tools_dock_back_when_the_area_is_tall_again() {
    let tall = egui::Rect::from_min_max(egui::pos2(0.0, 23.0), egui::pos2(1024.0, 640.0));
    let band = egui::Rect::from_min_max(egui::pos2(0.0, 300.0), egui::pos2(1024.0, 533.0));
    let mut app = tools_in((1024, 640), tall);
    let fresh_tall = rects(&app);
    assert_docked(&app, tall, "tall");
    let fresh_band = rects(&tools_in((1024, 640), band));
    for (dock, fresh, label) in [
        (band, fresh_band, "short"),
        (tall, fresh_tall, "tall again"),
        (band, fresh_band, "short again"),
    ] {
        app.insert_resource(ToolDockArea(dock));
        app.step_ticks(6);
        let now = rects(&app);
        assert!(
            same_rect(now.1, fresh.1) && same_rect(now.2, fresh.2),
            "{label}: {now:?} vs fresh {fresh:?}"
        );
        if dock == tall {
            assert_docked(&app, dock, label);
        } else {
            assert_side_by_side(&app, dock, label);
        }
    }
}

/// A tools app of a `screen` (points) at `ppp` physical pixels per point, with the overlay (with
/// render stats), profiler and console open, docked in `dock` (with the overlay closed when
/// `overlay` is false), after a few frames.
fn tools_scaled(screen: (f32, f32), ppp: f32, dock: egui::Rect, overlay: bool) -> App {
    let size = ((screen.0 * ppp).round() as u32, (screen.1 * ppp).round() as u32);
    let mut app = tools_app(size, ppp, false);
    app.insert_resource(window(size));
    app.insert_resource(space3d_render::RenderStats::default());
    app.resources.expect_mut::<DebugOverlay>().visible = overlay;
    app.insert_resource(ToolDockArea(dock));
    app.step_ticks(6);
    app
}

/// Press at `from`, move to `to` in 10 steps, release (points, at `ppp`), calling `each` after
/// every step of the move; then the pointer rests away from the tools.
fn drag_scaled(app: &mut App, ppp: f32, from: egui::Pos2, to: egui::Pos2, mut each: impl FnMut(&App, usize)) {
    let at = |app: &mut App, p: egui::Pos2| {
        app.resources
            .expect_mut::<Input>()
            .inject_mouse_move(Vec2::new(p.x * ppp, p.y * ppp));
        app.step_ticks(1);
    };
    at(app, from);
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_down(MouseButton::Left);
    app.step_ticks(1);
    for i in 1..=10 {
        at(app, from + (to - from) * (i as f32 / 10.0));
        each(app, i);
    }
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_up(MouseButton::Left);
    app.step_ticks(2);
    at(app, egui::pos2(2.0, 2.0));
    app.step_ticks(4);
}

/// The console's right edge, the profiler's left edge and the console's top edge, where a press
/// grabs them.
fn console_right(r: egui::Rect) -> egui::Pos2 {
    egui::pos2(r.right() - 1.0, r.center().y)
}

fn profiler_left(r: egui::Rect) -> egui::Pos2 {
    egui::pos2(r.left() + 1.0, r.center().y)
}

fn console_top(r: egui::Rect) -> egui::Pos2 {
    egui::pos2(r.center().x, r.top() + 1.0)
}

fn same_size(a: egui::Rect, b: egui::Rect) -> bool {
    (a.size() - b.size()).abs().max_elem() < 0.5
}

/// A console and a profiler the user resized in a tall area are squeezed side by side in a
/// short band and get their sizes back when the area is tall again, at any scale: egui keeps
/// only the size a window was last squeezed to (the console came back at the 347 pt the band
/// left it instead of the user's 890 pt, the profiler at 467 pt instead of 640 pt, and the
/// console's height at the band's instead of the user's).
#[test]
fn resized_tools_get_their_sizes_back_after_a_short_area() {
    let tall = egui::Rect::from_min_max(egui::pos2(0.0, 23.0), egui::pos2(1024.0, 640.0));
    let band = egui::Rect::from_min_max(egui::pos2(0.0, 300.0), egui::pos2(1024.0, 533.0));
    for ppp in [1.0f32, 1.5, 2.0] {
        let mut app = tools_scaled((1024.0, 640.0), ppp, tall, true);
        let (_, profiler, console) = rects(&app);
        drag_scaled(
            &mut app,
            ppp,
            console_right(console),
            console_right(console) + egui::vec2(250.0, 0.0),
            |_, _| {},
        );
        let (_, _, console) = rects(&app);
        drag_scaled(
            &mut app,
            ppp,
            console_top(console),
            console_top(console) - egui::vec2(0.0, 120.0),
            |_, _| {},
        );
        drag_scaled(
            &mut app,
            ppp,
            profiler_left(profiler),
            profiler_left(profiler) - egui::vec2(200.0, 0.0),
            |_, _| {},
        );
        let sized = rects(&app);
        let label = format!("{ppp}x, resized {sized:?}");
        assert!((sized.2.width() - 890.0).abs() < 1.0, "console widened: {label}");
        assert!(sized.2.height() > 300.0, "console taller: {label}");
        assert!((sized.1.width() - 640.0).abs() < 1.0, "profiler widened: {label}");
        assert_stacked(&app, tall, &label);

        app.insert_resource(ToolDockArea(band));
        app.step_ticks(6);
        assert_side_by_side(&app, band, &format!("{label}, band"));
        for (dock, what) in [(tall, "tall again"), (band, "band again"), (tall, "tall once more")] {
            app.insert_resource(ToolDockArea(dock));
            app.step_ticks(6);
            let now = rects(&app);
            let label = format!("{label}, {what}: {now:?}");
            if dock == tall {
                assert!(
                    same_size(now.1, sized.1) && same_size(now.2, sized.2),
                    "the user's sizes: {label}"
                );
                assert_stacked(&app, tall, &label);
            } else {
                assert_side_by_side(&app, band, &label);
            }
        }
    }
}

/// Dragging the console's edge toward the profiler beside it stops a gap from it on every
/// frame of the drag, whatever width the console starts from, and neither loses a log line nor
/// becomes resized when it could not move (the console's width limit was lifted every other
/// frame: it was drawn over the profiler); in a wide, short area either tool resized toward the
/// other stops a gap from it, and the tools keep their layout.
#[test]
fn an_edge_dragged_toward_the_other_tool_stops_a_gap_from_it() {
    let apart = |app: &App, label: &str| {
        let (_, profiler, console) = rects(app);
        assert!(
            console.right() + EDGE <= profiler.left() + 0.5,
            "a gap apart: {label}: console {console:?}, profiler {profiler:?}"
        );
    };
    let band = egui::Rect::from_min_max(egui::pos2(0.0, 300.0), egui::pos2(1024.0, 533.0));
    for ppp in [1.0f32, 1.5, 2.0] {
        let mut app = tools_scaled((1024.0, 640.0), ppp, band, true);
        let before = rects(&app);
        let from = console_right(before.2);
        drag_scaled(&mut app, ppp, from, from + egui::vec2(300.0, 0.0), |app, i| {
            let label = format!("{ppp}x, toward the profiler, move {i}");
            apart(app, &label);
            assert!(
                same_size(rects(app).2, before.2),
                "{label}: {:?} vs {before:?}",
                rects(app)
            );
        });
        assert_eq!(rects(&app), before, "{ppp}x: unchanged");
        // Narrowed, then widened toward the profiler again: up to the same limit.
        let from = console_right(before.2);
        drag_scaled(&mut app, ppp, from, from - egui::vec2(150.0, 0.0), |_, _| {});
        let narrowed = rects(&app).2;
        assert!(
            (narrowed.width() - (before.2.width() - 150.0)).abs() < 1.5,
            "{ppp}x: {narrowed:?}"
        );
        let from = console_right(narrowed);
        drag_scaled(&mut app, ppp, from, from + egui::vec2(300.0, 0.0), |app, i| {
            apart(app, &format!("{ppp}x, narrowed, then toward the profiler, move {i}"));
        });
        let after = rects(&app);
        assert!(
            same_size(after.2, before.2) && after.1 == before.1,
            "{ppp}x: {after:?} vs {before:?}"
        );
        assert_side_by_side(&app, band, &format!("{ppp}x, widened again"));
    }

    let wide = egui::Rect::from_min_size(egui::pos2(0.0, 300.0), egui::vec2(1920.0, 260.0));
    for overlay in [true, false] {
        let mut app = tools_scaled((1920.0, 640.0), 1.0, wide, overlay);
        let before = rects(&app);
        let from = console_right(before.2);
        drag_scaled(&mut app, 1.0, from, from + egui::vec2(1000.0, 0.0), |app, i| {
            apart(app, &format!("wide, overlay {overlay}, console widened, move {i}"));
        });
        let (_, profiler, console) = rects(&app);
        let label = format!("overlay {overlay}: {before:?} -> {profiler:?} {console:?}");
        assert!(
            (profiler.left() - console.right() - EDGE).abs() < 1.0,
            "up to the profiler: {label}"
        );
        assert_eq!(console.height(), before.2.height(), "{label}");
        assert_eq!(profiler, before.1, "{label}");

        let mut app = tools_scaled((1920.0, 640.0), 1.0, wide, overlay);
        let from = profiler_left(before.1);
        drag_scaled(&mut app, 1.0, from, from - egui::vec2(1000.0, 0.0), |_, _| {});
        app.step_ticks(4);
        let (_, profiler, console) = rects(&app);
        let label = format!("overlay {overlay}: {before:?} -> {profiler:?} {console:?}");
        assert!(
            (profiler.left() - console.right() - EDGE).abs() < 1.0,
            "up to the console: {label}"
        );
        assert!(
            (profiler.right() - (wide.right() - EDGE)).abs() < 0.5,
            "still docked: {label}"
        );
        assert_eq!(console, before.2, "the console keeps its place and size: {label}");
    }
}
