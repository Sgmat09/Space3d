//! UI stories (feature `ui`, `Engine/UI/*`): the opt-in compact density next to egui's default
//! on the same representative panel, and the engine's in-game tools — debug overlay, profiler,
//! console — under egui's default style, the compact theme and a roomy game style.
//!
//! Everything shown is deterministic: the UI runs with `UiPlugin::deterministic()` (without
//! installing the global tracing layers), the profiler shows recorded frames, the console holds
//! only its own lines and the overlay's frame times and renderer counters are set by the story.

use super::{ambient, camera, mesh, palette, sun};
use crate::{KnobValues, Story};
use space3d_core::prelude::*;
use space3d_render::{DrawPath, Material, Mesh3d, Projection, RenderStats, Renderer};
use space3d_ui::{
    Console, DebugOverlay, EguiContext, FrameTimeline, LogLevel, Profiler, ThemePreview, TimelineSpan, UiAppExt,
    UiPlugin, UiTheme, egui,
};
use std::sync::Arc;

pub(crate) fn stories() -> Vec<Story> {
    vec![
        theme_story("Engine/UI/Theme/Default", None),
        theme_story("Engine/UI/Theme/Compact", Some(UiTheme::compact())),
        debug_overlay(),
        profiler(),
        console(),
        game_style(),
    ]
}

/// The deterministic UI without the global tracing layers (stories share a process), laid out
/// for the capture size: the headless runner keeps the renderer out of the frame loop (it is
/// present only while the story is set up and captured), so the UI's size comes from
/// `fallback_size`.
fn ui_plugin(app: &App) -> UiPlugin {
    UiPlugin {
        install_tracing: false,
        fallback_size: app.resources.get::<Renderer>().map_or((1280, 720), Renderer::size),
        ..UiPlugin::deterministic()
    }
}

const STYLES: [&str; 3] = ["egui default", "compact theme", "roomy game style"];

/// A game's own, deliberately roomy egui style (big text, padding and controls).
fn roomy(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(14.0, 10.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(16);
    style.spacing.interact_size = egui::vec2(48.0, 32.0);
    for (text, size) in [
        (egui::TextStyle::Body, 17.0),
        (egui::TextStyle::Button, 17.0),
        (egui::TextStyle::Monospace, 16.0),
        (egui::TextStyle::Heading, 24.0),
        (egui::TextStyle::Small, 13.0),
    ] {
        if let Some(font) = style.text_styles.get_mut(&text) {
            font.size = size;
        }
    }
    style.visuals.window_corner_radius = egui::CornerRadius::same(12);
    style.visuals.window_fill = egui::Color32::from_rgb(34, 33, 31);
    style.visuals.panel_fill = egui::Color32::from_rgb(34, 33, 31);
}

/// Apply knob `style` (index into [`STYLES`]).
fn apply_style(app: &mut App, style: usize) {
    match style {
        1 => {
            app.insert_resource(UiTheme::compact());
        }
        2 => {
            let ctx = app.resources.expect::<EguiContext>().ctx().clone();
            ctx.all_styles_mut(roomy);
        }
        _ => {}
    }
}

/// A small lit scene behind the tools (a row of cubes on a floor), like a game.
fn backdrop(app: &mut App) {
    let cube = mesh(app, MeshData::cube(0.8));
    let floor = mesh(app, MeshData::plane(12.0));
    for i in 0..5 {
        let c = palette(i);
        app.world.spawn((
            Transform::from_xyz(i as f32 * 1.3 - 2.6, 0.0, 0.0).with_rotation(Quat::from_rotation_y(0.4 * i as f32)),
            Mesh3d(cube),
            Material::color(c.extend(1.0)).with_metallic_roughness(0.0, 0.5),
        ));
    }
    app.world.spawn((
        Transform::from_xyz(0.0, -0.4, 0.0),
        Mesh3d(floor),
        Material::srgb(0.22, 0.24, 0.28),
    ));
    sun(app, Vec3::new(-0.4, -1.0, -0.6), 2.2);
    ambient(app, 0.1);
    camera(
        app,
        Vec3::new(0.0, 2.2, 5.5),
        Vec3::new(0.0, 0.0, 0.0),
        Projection::default(),
    );
}

/// Plain dark background for the theme panels.
fn plain_backdrop(app: &mut App) {
    camera(app, Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Projection::default());
}

fn theme_story(id: &str, theme: Option<UiTheme>) -> Story {
    let which = if theme.is_some() {
        "the opt-in compact density (`app.insert_resource(UiTheme::compact())`)"
    } else {
        "egui's default style (no `UiTheme` resource: the engine never touches the style)"
    };
    Story::new(id, move |app, _| {
        app.add_plugin(ui_plugin(app));
        if let Some(t) = theme {
            app.insert_resource(t);
        }
        plain_backdrop(app);
        let mut preview = ThemePreview::default();
        // A real game window around the panel. `auto_sized`: the window takes the panel's width
        // (egui's default window is 340 pt wide whatever it holds). `fixed_pos`: an auto-sized
        // window's first frame spans the screen (its title bar grows), which would push a
        // `default_pos` window against the edge of this small capture.
        app.add_ui_system(move |ctx, _| {
            egui::Window::new("Asset browser")
                .id(egui::Id::new("space3d_theme_story"))
                .fixed_pos([12.0, 12.0])
                .auto_sized()
                .collapsible(false)
                .show(ctx, |ui| {
                    preview.ui(ui);
                });
        });
    })
    .description(format!(
        "The representative tool panel (menu bar, toolbar, checkbox, slider, combo box, text field, \
         collapsing section, 10-row striped table) under {which}. Same size as its sibling story, so \
         the two compare side by side."
    ))
    .size(520, 480)
    .ticks(4)
    .tag("ui")
}

/// Frame times with a little noise, a slow stretch and two spikes (ms).
fn frame_times() -> impl Iterator<Item = f32> {
    (0..200u32).map(|i| {
        let wobble = ((i * 37 % 11) as f32 - 5.0) * 0.35;
        match i {
            60 | 61 => 41.0,
            120..=135 => 24.0 + wobble,
            170 => 52.0,
            _ => 16.6 + wobble,
        }
    })
}

/// Renderer counters of a busy frame.
fn render_stats() -> RenderStats {
    RenderStats {
        frame: 1234,
        views: 2,
        draw_calls: 38,
        indirect_draw_calls: 12,
        multi_draw_calls: 4,
        batches: 57,
        instances: 18_450,
        instances_drawn: 12_902,
        culled: 5_548,
        triangles: 1_734_220,
        bytes_uploaded: 612_400,
        instance_bytes_uploaded: 412_864,
        draw_path: DrawPath::GpuMultiDraw,
        gpu_culling: true,
        ..Default::default()
    }
}

fn debug_overlay() -> Story {
    Story::new("Engine/UI/Tools/Debug Overlay", |app, k| {
        app.add_plugin(ui_plugin(app));
        apply_style(app, k.choice("style"));
        backdrop(app);
        let mut overlay = DebugOverlay::new(true);
        overlay.details = k.bool("details");
        for ms in frame_times() {
            overlay.record_frame(ms);
        }
        app.insert_resource(overlay);
        app.insert_resource(render_stats());
    })
    .description(
        "The F3 overlay: FPS / average / worst frame, a sparkline of the last 240 frames and a grid \
         of label/value pairs with right-aligned numbers. It keeps its own look under any style; \
         `details` adds the rarer renderer counters.",
    )
    .knob_choice("style", &STYLES, 0)
    .knob_bool("details", true)
    .size(320, 210)
    .ticks(3)
    .tag("ui")
}

/// Recorded profiler frames: a few systems at fixed times (one nested system).
fn profiled_frames(profiler: &mut Profiler) {
    let systems: [(&str, f64, u32); 9] = [
        ("render::render_frame", 2.10, 0),
        ("physics::step", 1.25, 0),
        ("game::ai::think", 0.82, 0),
        ("game::ai::pathfind", 0.41, 1),
        ("particles::simulate", 0.64, 0),
        ("ui::end_frame", 0.38, 0),
        ("ui::begin_frame", 0.21, 0),
        ("audio::update", 0.12, 0),
        ("core::propagate_transforms", 0.05, 0),
    ];
    for frame in 0..30u64 {
        let jitter = |ms: f64| ms * (1.0 + ((frame * 7 % 5) as f64 - 2.0) * 0.04);
        let mut start = 0.0;
        let mut spans = Vec::new();
        let mut parent_start = 0.0;
        for (name, ms, depth) in systems {
            let duration = jitter(ms) / 1000.0;
            let at = if depth == 0 {
                parent_start = start;
                start += duration + 0.00005;
                parent_start
            } else {
                parent_start + 0.0001
            };
            spans.push(TimelineSpan {
                name: Arc::from(name),
                start: at,
                duration,
                depth,
            });
        }
        profiler.record(FrameTimeline {
            index: frame + 1,
            duration: start + 0.0004,
            spans,
        });
    }
}

fn profiler() -> Story {
    Story::new("Engine/UI/Tools/Profiler", |app, k| {
        app.add_plugin(ui_plugin(app));
        apply_style(app, k.choice("style"));
        backdrop(app);
        let mut profiler = Profiler::default();
        profiler.visible = true;
        profiler.frozen = true;
        profiled_frames(&mut profiler);
        app.insert_resource(profiler);
    })
    .description(
        "The F4 profiler (recorded frames, frozen): a compact header, then collapsible Timeline \
         (flame view of the last frame) and Systems (striped table, right-aligned numbers) sections.",
    )
    .knob_choice("style", &STYLES, 0)
    .size(480, 330)
    .ticks(3)
    .tag("ui")
}

fn console_lines(console: &mut Console) {
    let lines = [
        (LogLevel::Info, "server: connected to eu-west-2 (ping 23 ms)"),
        (LogLevel::Info, "assets: 214 meshes, 96 textures loaded in 1.84 s"),
        (LogLevel::Debug, "net: snapshot 1840 applied (3 entities)"),
        (LogLevel::Warn, "audio: voice limit reached, dropping 2 sounds"),
        (LogLevel::Info, "hangar: ship \"Kestrel\" docked at bay 3"),
        (LogLevel::Error, "shader cache: entry 0x3fa2 corrupt, rebuilding"),
        (LogLevel::Info, "quest: \"Salvage run\" objective 2/3 complete"),
    ];
    for (level, line) in lines {
        console.print(level, line);
    }
    console.submit("echo ready");
    console.submit("tickrate");
}

fn console() -> Story {
    Story::new("Engine/UI/Tools/Console", |app, k| {
        app.add_plugin(ui_plugin(app));
        apply_style(app, k.choice("style"));
        backdrop(app);
        let mut console = Console::with_builtin_commands();
        console.visible = true;
        console.capture_logs = false;
        console.min_level = LogLevel::Info;
        console_lines(&mut console);
        app.insert_resource(console);
    })
    .description(
        "The ` console: level / filter / clear toolbar, one dense monospace line per entry (level \
         tag, target, message) and the command line. Holds only its own lines here.",
    )
    .knob_choice("style", &STYLES, 0)
    .size(640, 420)
    .ticks(3)
    .tag("ui")
}

fn game_style() -> Story {
    Story::new("Engine/UI/Tools/Game Style", |app, k: &KnobValues| {
        app.add_plugin(ui_plugin(app));
        apply_style(app, k.choice("style"));
        backdrop(app);
        let mut overlay = DebugOverlay::new(true);
        for ms in frame_times() {
            overlay.record_frame(ms);
        }
        app.insert_resource(overlay);
        app.insert_resource(render_stats());
        let mut profiler = Profiler::default();
        profiler.visible = true;
        profiler.frozen = true;
        profiled_frames(&mut profiler);
        app.insert_resource(profiler);
        let mut console = Console::with_builtin_commands();
        console.visible = true;
        console.capture_logs = false;
        console_lines(&mut console);
        app.insert_resource(console);
        // A window of the game itself, in the game's style.
        app.add_ui_system(|ctx, _| {
            egui::Window::new("Cargo hold")
                .id(egui::Id::new("space3d_game_style_story"))
                .fixed_pos([170.0, 150.0])
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("Salvage: 12 t");
                    let _ = ui.button("Jettison");
                });
        });
    })
    .description(
        "All three tools over a game that styles egui itself with big text and padding (knob \
         `style`): the game's own window follows its style, the tools stay compact.",
    )
    .knob_choice("style", &STYLES, 2)
    .size(960, 540)
    .ticks(3)
    .tag("ui")
}
