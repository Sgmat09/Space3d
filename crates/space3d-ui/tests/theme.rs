//! Opt-in density: the default path never touches egui's style, `UiTheme::compact()` is applied
//! once and packs the representative panel ([`ThemePreview`]) into a much smaller rect with every
//! widget still whole.

use space3d_core::prelude::*;
use space3d_ui::*;
use std::sync::Arc;

#[derive(Default)]
struct Measured {
    panel: Option<egui::Rect>,
    widgets: Vec<(egui::Rect, bool)>,
    screen: Option<egui::Rect>,
}

/// UI-only app (no renderer needed for layout) showing the preview panel at the top-left.
fn preview_app(theme: Option<UiTheme>) -> App {
    let mut app = App::new();
    app.add_plugin(UiPlugin::deterministic());
    if let Some(t) = theme {
        app.insert_resource(t);
    }
    app.init_resource::<Measured>();
    let mut preview = ThemePreview::default();
    app.add_ui_system(move |ctx, sys| {
        let r = egui::Area::new(egui::Id::new("preview"))
            .fixed_pos([0.0, 0.0])
            .show(ctx, |ui| {
                egui::Frame::window(ui.style()).show(ui, |ui| preview.ui(ui));
            });
        let m = sys.resources.expect_mut::<Measured>();
        m.panel = Some(r.response.rect);
        m.widgets = preview.widget_rects().to_vec();
        m.screen = Some(ctx.content_rect());
    });
    app
}

fn measure(theme: Option<UiTheme>) -> (egui::Rect, Vec<(egui::Rect, bool)>, egui::Rect) {
    let mut app = preview_app(theme);
    app.step_ticks(4);
    let m = app.resources.expect::<Measured>();
    (m.panel.unwrap(), m.widgets.clone(), m.screen.unwrap())
}

fn area(r: egui::Rect) -> f32 {
    r.width() * r.height()
}

#[test]
fn compact_theme_packs_the_preview_panel_without_clipping() {
    let (default_panel, default_widgets, _) = measure(None);
    let (compact_panel, compact_widgets, screen) = measure(Some(UiTheme::compact()));
    let ratio = area(compact_panel) / area(default_panel);
    eprintln!(
        "preview panel: default {:.0}x{:.0} ({:.0} pt²), compact {:.0}x{:.0} ({:.0} pt²): {:.1}%",
        default_panel.width(),
        default_panel.height(),
        area(default_panel),
        compact_panel.width(),
        compact_panel.height(),
        area(compact_panel),
        ratio * 100.0
    );
    assert!(
        ratio <= 0.65,
        "compact panel uses {:.1}% of the default area",
        ratio * 100.0
    );

    // Same widgets, all whole: inside the panel and the screen, nothing empty, text never
    // wrapped (the preview lays text out on one line), hit targets at least 18 pt tall.
    assert_eq!(compact_widgets.len(), default_widgets.len());
    assert!(compact_widgets.len() >= 60, "{} widgets", compact_widgets.len());
    for (panel, widgets) in [(default_panel, &default_widgets), (compact_panel, &compact_widgets)] {
        for (i, (r, interactive)) in widgets.iter().enumerate() {
            assert!(r.is_positive(), "widget {i} empty: {r:?}");
            assert!(
                panel.contains_rect(*r) && screen.contains_rect(*r),
                "widget {i} clipped: {r:?} in {panel:?}"
            );
            if *interactive {
                assert!(r.height() >= 18.0, "hit target {i} only {:.1} pt tall", r.height());
            }
        }
    }
    // Every compact widget is the same kind of thing: the same text on one line, no smaller than
    // 90% of the default text height.
    for ((c, _), (d, _)) in compact_widgets.iter().zip(&default_widgets) {
        assert!(c.height() >= d.height() * 0.9, "{c:?} vs {d:?}");
    }
}

#[test]
fn default_path_never_touches_the_style() {
    let mut app = App::new();
    app.add_plugin(UiPlugin::deterministic());
    let ctx = app.resources.expect::<EguiContext>().ctx().clone();
    let before = ctx.global_style();
    app.step_ticks(5);
    assert!(Arc::ptr_eq(&before, &ctx.global_style()), "no UiTheme: style untouched");
    assert_eq!(UiDensity::of(&ctx), None);
    let fresh = egui::Context::default();
    fresh.set_theme(egui::Theme::Dark);
    fresh.global_style_mut(|s| s.animation_time = 0.0);
    // (`Style`'s `PartialEq` compares the number formatter by identity: compare field values.)
    assert_eq!(
        format!("{:?}", ctx.global_style()),
        format!("{:?}", fresh.global_style()),
        "egui's default style (animations off)"
    );
}

#[test]
fn theme_applies_once_and_never_fights_the_game() {
    let mut app = App::new();
    app.add_plugin(UiPlugin::deterministic());
    app.insert_resource(UiTheme::compact());
    let ctx = app.resources.expect::<EguiContext>().ctx().clone();
    app.step_ticks(1);
    let applied = ctx.global_style();
    assert_eq!(applied.spacing.item_spacing, UiDensity::compact().item_spacing);
    assert_eq!(applied.animation_time, 0.0, "deterministic settings kept");
    assert_eq!(UiDensity::of(&ctx), Some(UiDensity::compact()));
    app.step_ticks(20);
    assert!(Arc::ptr_eq(&applied, &ctx.global_style()), "no per-frame style churn");

    // The game restyles egui itself: kept, frame after frame.
    ctx.global_style_mut(|s| s.visuals.window_fill = egui::Color32::from_rgb(20, 19, 18));
    let game = ctx.global_style();
    app.step_ticks(5);
    assert!(Arc::ptr_eq(&game, &ctx.global_style()));

    // Changing the resource applies the new tokens on top of the game's look.
    app.resources
        .expect_mut::<UiTheme>()
        .clone_from(&UiTheme::Custom(UiDensity {
            body_size: 14.0,
            ..UiDensity::compact()
        }));
    app.step_ticks(1);
    let s = ctx.global_style();
    assert_eq!(s.text_styles[&egui::TextStyle::Body].size, 14.0);
    assert_eq!(s.visuals.window_fill, egui::Color32::from_rgb(20, 19, 18));
    let restyled = ctx.global_style();
    app.step_ticks(5);
    assert!(Arc::ptr_eq(&restyled, &ctx.global_style()));
}
