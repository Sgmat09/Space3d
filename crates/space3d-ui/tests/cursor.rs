//! egui's cursor reaches the window: `space3d_platform::WindowCursor` gets the pointing hand over
//! a button, the I-beam over a text field and nothing (hidden) for `CursorIcon::None`; a cursor
//! the game sets stays until egui asks for a different one. (The runner applying it to the OS
//! window is covered by `WindowCursor::applied`, which needs a real window.)

use space3d_core::glam::UVec2;
use space3d_core::prelude::*;
use space3d_platform::{CursorIcon, Input, WindowCursor, WindowState};
use space3d_ui::*;

struct Page {
    button: egui::Rect,
    field: egui::Rect,
    text: String,
    hide_over_button: bool,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            button: egui::Rect::NOTHING,
            field: egui::Rect::NOTHING,
            text: String::new(),
            hide_over_button: false,
        }
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugin(UiPlugin::deterministic());
    app.insert_resource(WindowState {
        physical_size: UVec2::new(640, 360),
        scale_factor: 1.0,
        focused: true,
        suspended: false,
        title: String::new(),
        cursor_visible: true,
        cursor_grab: false,
    });
    app.init_resource::<WindowCursor>();
    app.init_resource::<Page>();
    app.add_root_ui_system(|ui: &mut egui::Ui, sys: &mut SystemCtx| {
        let page = sys.resources.expect_mut::<Page>();
        // (egui buttons keep the arrow unless asked: `on_hover_cursor`, or the style's
        // `interact_cursor`; games that want the pointing hand ask for it.)
        let button = ui.button("Launch").on_hover_cursor(egui::CursorIcon::PointingHand);
        if page.hide_over_button && button.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::None);
        }
        page.button = button.rect;
        page.field = ui.text_edit_singleline(&mut page.text).rect;
    });
    app.step_ticks(2);
    app
}

fn hover(app: &mut App, at: egui::Pos2) {
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_move(Vec2::new(at.x, at.y));
    app.step_ticks(2);
}

fn requested(app: &App) -> Option<CursorIcon> {
    app.resources.expect::<WindowCursor>().icon
}

#[test]
fn egui_cursors_reach_the_window_cursor() {
    let mut app = app();
    assert_eq!(requested(&app), Some(CursorIcon::Default));
    let (button, field) = {
        let p = app.resources.expect::<Page>();
        (p.button.center(), p.field.center())
    };
    hover(&mut app, button);
    assert_eq!(
        requested(&app),
        Some(CursorIcon::Pointer),
        "pointing hand over a button"
    );
    hover(&mut app, field);
    assert_eq!(requested(&app), Some(CursorIcon::Text), "I-beam over a text field");
    hover(&mut app, egui::pos2(600.0, 340.0));
    assert_eq!(requested(&app), Some(CursorIcon::Default), "back to the arrow");

    // `CursorIcon::None` hides the pointer.
    app.resources.expect_mut::<Page>().hide_over_button = true;
    hover(&mut app, button);
    assert_eq!(requested(&app), None, "hidden");
    // Nothing is applied without a real window.
    assert_eq!(app.resources.expect::<WindowCursor>().applied(), None);
}

#[test]
fn a_game_cursor_stays_until_egui_changes_its_own() {
    let mut app = app();
    app.resources.expect_mut::<WindowCursor>().icon = Some(CursorIcon::Crosshair);
    app.step_ticks(5);
    assert_eq!(
        requested(&app),
        Some(CursorIcon::Crosshair),
        "egui unchanged: the game's cursor stays"
    );
    let button = app.resources.expect::<Page>().button.center();
    hover(&mut app, button);
    assert_eq!(requested(&app), Some(CursorIcon::Pointer), "egui's new cursor wins");
}

#[test]
fn every_egui_cursor_has_a_window_cursor() {
    use egui::CursorIcon as E;
    assert_eq!(winit_cursor(E::None), None);
    for icon in [
        E::Default,
        E::PointingHand,
        E::Text,
        E::Grab,
        E::Grabbing,
        E::ResizeHorizontal,
        E::ResizeNwSe,
        E::NotAllowed,
        E::ZoomIn,
    ] {
        assert!(winit_cursor(icon).is_some(), "{icon:?}");
    }
    assert_eq!(winit_cursor(E::ResizeVertical), Some(CursorIcon::NsResize));
}
