//! Layout of the docked engine tools (profiler, console): they stay 6 pt inside the screen,
//! never grow with their contents, never overlap each other and look the same under a game's
//! roomy style.

use space3d_core::prelude::*;
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
