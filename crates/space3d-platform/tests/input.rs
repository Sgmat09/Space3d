//! Input injection through a real `App` (no window needed).

use space3d_core::prelude::*;
use space3d_platform::*;

#[derive(Default)]
struct Seen {
    jump_frames: Vec<u64>,
    held_frames: u32,
    wheel: f32,
    touches: usize,
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugin(InputPlugin);
    app.init_resource::<Seen>();
    app.resources
        .expect_mut::<ActionMap>()
        .bind("jump", Binding::Key(KeyCode::Space))
        .bind("jump", Binding::TouchAny)
        .bind("fire", Binding::Mouse(MouseButton::Left));
    app.add_system(Stage::Update, |ctx| {
        let frame = ctx.time().frame;
        let (jp, held, wheel, touches) = {
            let a = ctx.resources.expect::<ActionState>();
            let i = ctx.resources.expect::<Input>();
            (
                a.just_pressed("jump"),
                a.pressed("fire"),
                i.wheel_delta().y,
                i.touches().len(),
            )
        };
        let seen = ctx.resources.expect_mut::<Seen>();
        if jp {
            seen.jump_frames.push(frame);
        }
        seen.held_frames += held as u32;
        seen.wheel += wheel;
        seen.touches = seen.touches.max(touches);
    });
    app
}

#[test]
fn injected_input_is_seen_by_systems_exactly_once() {
    let mut app = app();
    app.step_ticks(2);
    app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::Space);
    app.step_ticks(3);
    assert_eq!(
        app.resources.expect::<Seen>().jump_frames,
        vec![3],
        "edge visible in one frame only"
    );

    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_down(MouseButton::Left);
    app.step_ticks(4);
    app.resources
        .expect_mut::<Input>()
        .inject_mouse_button_up(MouseButton::Left);
    app.step_ticks(2);
    assert_eq!(app.resources.expect::<Seen>().held_frames, 4);

    app.resources.expect_mut::<Input>().inject_wheel(Vec2::new(0.0, 3.0));
    app.step_ticks(2);
    assert_eq!(
        app.resources.expect::<Seen>().wheel,
        3.0,
        "deltas reset after the frame"
    );
}

#[test]
fn multi_touch_triggers_actions() {
    let mut app = app();
    {
        let input = app.resources.expect_mut::<Input>();
        input.inject_touch(1, TouchPhase::Started, Vec2::new(10.0, 10.0));
        input.inject_touch(2, TouchPhase::Started, Vec2::new(50.0, 10.0));
    }
    app.step_ticks(1);
    let seen = app.resources.expect::<Seen>();
    assert_eq!(seen.touches, 2);
    assert_eq!(seen.jump_frames, vec![1]);
    app.resources
        .expect_mut::<Input>()
        .inject_touch(1, TouchPhase::Ended, Vec2::new(10.0, 10.0));
    app.step_ticks(1);
    assert_eq!(app.resources.expect::<Input>().touches().len(), 1);
    assert!(
        app.resources.expect::<ActionState>().pressed("jump"),
        "second finger still down"
    );
}

#[test]
fn window_plugin_registers_resources_without_opening_a_window() {
    let mut app = App::new();
    app.add_plugin(PlatformPlugin::default());
    assert!(app.resources.contains::<Input>());
    assert!(app.resources.contains::<WindowState>());
    assert!(app.resources.contains::<space3d_render::RenderAssets>());
    assert!(app.resources.contains::<Events<PlatformEvent>>());
    // Stepping without `run()` works headlessly (no renderer → render system is a no-op).
    app.step_ticks(2);
}
