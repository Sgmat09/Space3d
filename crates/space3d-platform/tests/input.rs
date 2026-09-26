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

fn ch(s: &str) -> LogicalKey {
    LogicalKey::Character(s.into())
}

/// Tap every `(physical key, character)` pair of a layout, one per frame, as the runner would.
fn type_layout(app: &mut App, keys: &[(KeyCode, &str)]) {
    for &(code, c) in keys {
        app.resources.expect_mut::<Input>().inject_key_tap_logical(code, ch(c));
        app.step_ticks(1);
    }
}

#[test]
fn physical_bindings_ignore_the_layout_and_labels_follow_it() {
    let mut app = app();
    app.resources
        .expect_mut::<ActionMap>()
        .bind("forward", Binding::Key(KeyCode::KeyW));
    // AZERTY: QWERTY's W position is labelled Z. The action is bound to the position.
    app.resources
        .expect_mut::<Input>()
        .inject_key_down_logical(KeyCode::KeyW, ch("z"));
    app.step_ticks(1);
    assert!(app.resources.expect::<ActionState>().just_pressed("forward"));
    {
        let input = app.resources.expect::<Input>();
        assert!(input.key_events().is_empty(), "events live one frame");
        assert_eq!(input.logical_key(KeyCode::KeyW), Some(&ch("z")));
        assert_eq!(input.key_cap(KeyCode::KeyW), "Z", "show the player the Z key");
    }
    app.resources
        .expect_mut::<Input>()
        .inject_key_up_logical(KeyCode::KeyW, ch("z"));
    app.step_ticks(1);
    assert!(app.resources.expect::<ActionState>().just_released("forward"));
}

#[test]
fn injected_layouts_teach_key_labels() {
    // (layout, physical key → what it types unshifted)
    let layouts: [(&str, &[(KeyCode, &str)]); 4] = [
        (
            "AZERTY",
            &[
                (KeyCode::KeyQ, "a"),
                (KeyCode::KeyW, "z"),
                (KeyCode::KeyA, "q"),
                (KeyCode::KeyZ, "w"),
                (KeyCode::Semicolon, "m"),
                (KeyCode::KeyM, ","),
                (KeyCode::Digit1, "&"),
                (KeyCode::Digit2, "é"),
            ],
        ),
        (
            "QWERTZ",
            &[
                (KeyCode::KeyY, "z"),
                (KeyCode::KeyZ, "y"),
                (KeyCode::Semicolon, "ö"),
                (KeyCode::Minus, "ß"),
                (KeyCode::Equal, "´"),
            ],
        ),
        (
            "JIS",
            &[
                (KeyCode::IntlYen, "¥"),
                (KeyCode::IntlRo, "\\"),
                (KeyCode::Quote, ":"),
                (KeyCode::BracketLeft, "@"),
                (KeyCode::Equal, "^"),
            ],
        ),
        (
            "Dvorak",
            &[
                (KeyCode::KeyQ, "'"),
                (KeyCode::KeyS, "o"),
                (KeyCode::KeyV, "k"),
                (KeyCode::Period, "v"),
                (KeyCode::KeyZ, ";"),
            ],
        ),
    ];
    for (name, keys) in layouts {
        let mut app = app();
        type_layout(&mut app, keys);
        let input = app.resources.expect::<Input>();
        for &(code, c) in keys {
            let label = input
                .key_label_info(code)
                .unwrap_or_else(|| panic!("{name}: {code:?} unlabelled"));
            assert_eq!(label.text, c, "{name}: {code:?}");
            assert_eq!(label.source, KeyLabelSource::Learned);
        }
        // Keys never pressed on this layout stay unknown (no OS table in tests).
        assert_eq!(input.key_label(KeyCode::KeyP), None, "{name}");
        assert_eq!(input.key_label(KeyCode::Space).as_deref(), Some("Space"));
    }
}

#[test]
fn a_new_os_layout_replaces_learned_labels() {
    let mut app = app();
    type_layout(&mut app, &[(KeyCode::KeyA, "q")]);
    let input = app.resources.expect_mut::<Input>();
    let g = input.keyboard_layout().generation();
    // The player switches to US; the platform reports the new table.
    input
        .keyboard_layout_mut()
        .set_os_layout(Some("us".into()), [(KeyCode::KeyA, "a".into(), Some("A".into()))]);
    assert!(input.keyboard_layout().generation() > g);
    assert_eq!(input.key_label(KeyCode::KeyA).as_deref(), Some("a"));
    assert_eq!(input.key_label_info(KeyCode::KeyA).unwrap().source, KeyLabelSource::Os);
}
