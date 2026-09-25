//! Storybook runtime: engine goldens, knob overrides, gallery output and failure collection.
#![cfg(feature = "engine")]

use space3d_core::prelude::*;
use space3d_render::Camera3d;
use space3d_stories::space3d_test::report::without_report;
use space3d_stories::space3d_test::screenshot::{GoldenDir, GoldenPolicy, ScreenshotOutcome};
use space3d_stories::space3d_test::{CompareOptions, compare_images, gpu_available, load_png, require_gpu, save_png};
use space3d_stories::*;

// Every engine story is a golden screenshot test against
// `crates/space3d-stories/tests/__screenshots__/Engine/**.png`.
storybook_tests!(engine_storybook);

/// Bit-exact engine goldens: the "exact" gate of the graphics upgrade (engine goldens must not
/// change by a single channel step while the renderer is restructured). The golden test above
/// allows a small tolerance so other GPUs pass; this one holds only on the machine that blessed
/// the goldens, so it is opt-in:
/// `cargo test -p space3d-stories --test storybook -- --ignored engine_goldens_are_exact`.
#[test]
#[ignore = "bit-exact goldens hold only on the machine that blessed them"]
fn engine_goldens_are_exact() {
    require_gpu!();
    let mut book = Storybook::new();
    for story in engine_storybook().iter() {
        let mut story = story.clone();
        story.tolerance = None;
        book.add(story);
    }
    let golden = GoldenDir::for_crate(env!("CARGO_MANIFEST_DIR"), env!("CARGO_PKG_NAME"));
    let options = StorybookTestOptions {
        compare: CompareOptions::exact(),
        filter: None,
        policy: GoldenPolicy {
            ci: true,
            ..GoldenPolicy::default()
        },
    };
    assert_storybook_in(&book, &golden, &options);
}

/// A story that only clears the screen to `color` (sRGB 0..1).
fn clear_story(id: &str, color: [f32; 3]) -> Story {
    Story::new(id, move |app, _| {
        let c = Vec4::new(
            space3d_render::srgb_to_linear(color[0]),
            space3d_render::srgb_to_linear(color[1]),
            space3d_render::srgb_to_linear(color[2]),
            1.0,
        );
        app.world
            .spawn((Transform::IDENTITY, Camera3d::default().with_clear_color(c)));
    })
    .size(24, 16)
}

#[test]
fn knob_overrides_change_the_output() {
    require_gpu!();
    let book = engine_storybook();
    let size = Some((96, 72));

    let cube = book.get("Engine/Basics/Lit Cube").unwrap();
    let a = run_story_headless(cube, &KnobValues::default(), size).unwrap();
    assert_eq!((a.width, a.height), (96, 72));
    let again = run_story_headless(cube, &KnobValues::default(), size).unwrap();
    assert_eq!(a, again, "same story + knobs must render identically");
    let green = KnobValues::default().with("color", KnobValue::Color([0.05, 0.9, 0.1, 1.0]));
    let b = run_story_headless(cube, &green, size).unwrap();
    let stats = compare_images(&a, &b, &CompareOptions::default());
    assert!(!stats.passed && stats.diff_fraction > 0.02, "{}", stats.summary());
    let (pa, pb) = (a.get(48, 36), b.get(48, 36));
    assert!(
        pb[1] > pa[1] && pb[0] < pa[0],
        "center must turn green: {pa:?} -> {pb:?}"
    );

    let cam = book.get("Engine/Camera/Projection").unwrap();
    let perspective = run_story_headless(cam, &KnobValues::default(), size).unwrap();
    let ortho = KnobValues::default().with("projection", KnobValue::Choice(1));
    let orthographic = run_story_headless(cam, &ortho, size).unwrap();
    assert!(!compare_images(&perspective, &orthographic, &CompareOptions::default()).passed);

    // Ticks matter: the physics bodies are still in the air after 1 tick.
    let physics = book.get("Engine/Physics/Falling Bodies").unwrap();
    let early = render_story(
        physics,
        &KnobValues::default(),
        &StoryRunOptions { size, ticks: Some(1) },
    )
    .unwrap();
    let late = run_story_headless(physics, &KnobValues::default(), size).unwrap();
    assert_eq!(early.ticks, 1);
    assert!(!compare_images(&early.image, &late, &CompareOptions::default()).passed);
    assert!(early.stats.is_some_and(|s| s.draw_calls > 0));
}

#[test]
fn screenshot_all_writes_gallery_files() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("gallery");
    let mut book = Storybook::new();
    let engine = engine_storybook();
    book.add(engine.get("Engine/Basics/Lit Cube").unwrap().clone());
    book.add(engine.get("Engine/Textures/Checker Plane").unwrap().clone());
    book.add(clear_story("Solid/Red", [1.0, 0.0, 0.0]).description("clears red"));
    book.add(Story::new("Solid/Hidden", |_, _| panic!("never rendered")).tag(NO_SCREENSHOT_TAG));
    book.add(Story::new("Broken/Panics", |_, _| panic!("setup exploded")).size(8, 8));

    let results = screenshot_all(&book, &out, None).unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "Engine/Basics/Lit Cube",
            "Engine/Textures/Checker Plane",
            "Solid/Red",
            "Solid/Hidden",
            "Broken/Panics"
        ]
    );
    let status = |id: &str| results.iter().find(|r| r.id == id).unwrap();
    assert_eq!(status("Solid/Hidden").status, StoryStatus::Skipped);
    if gpu_available() {
        assert_eq!(status("Broken/Panics").status, StoryStatus::Failed);
        assert!(
            status("Broken/Panics")
                .message
                .as_deref()
                .unwrap()
                .contains("setup exploded")
        );
        for id in ["Engine/Basics/Lit Cube", "Engine/Textures/Checker Plane", "Solid/Red"] {
            let r = status(id);
            assert_eq!(r.status, StoryStatus::Rendered, "{id}: {:?}", r.message);
            let png = load_png(r.image_path(&out).unwrap()).unwrap();
            assert_eq!((png.width, png.height), r.size);
        }
        assert_eq!(
            status("Solid/Red").image.as_deref(),
            Some("images/Solid/Red.png"),
            "id segments become directories"
        );
        assert_eq!(
            load_png(out.join("images/Solid/Red.png")).unwrap().get(3, 3),
            [255, 0, 0, 255]
        );
    } else {
        assert!(results.iter().all(|r| r.status == StoryStatus::Skipped));
    }

    let manifest = GalleryManifest::load(&out).unwrap();
    assert_eq!(manifest.stories, results);
    assert_eq!(manifest.stories[0].knobs.len(), 4);
    let html = std::fs::read_to_string(out.join("index.html")).unwrap();
    for needle in [
        "<h2 id=\"g-engine-basics\">Engine/Basics</h2>",
        "Solid",
        "Broken",
        "roughness",
        "clears red",
    ] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }

    // Filtering selects by id substring or exact tag.
    let filtered = screenshot_all(&book, tmp.path().join("filtered"), Some("solid")).unwrap();
    assert_eq!(filtered.len(), 2);
}

#[test]
fn storybook_check_reports_every_failure_and_honors_tolerance() {
    require_gpu!();
    // Fails stories on purpose: keep them out of a real SPACE3D_TEST_REPORT.
    without_report(|| {
        let tmp = tempfile::tempdir().unwrap();
        let golden = GoldenDir::new(tmp.path(), "stories-test");
        let wrong = ImageData::filled(24, 16, [0, 255, 0, 255]);
        save_png(tmp.path().join("T/Red.png"), &wrong).unwrap();
        save_png(tmp.path().join("T/Blue.png"), &wrong).unwrap();
        save_png(tmp.path().join("T/Loose.png"), &wrong).unwrap();

        let mut book = Storybook::new();
        book.add(clear_story("T/Red", [1.0, 0.0, 0.0]));
        book.add(clear_story("T/Blue", [0.0, 0.0, 1.0]));
        book.add(clear_story("T/Loose", [1.0, 1.0, 1.0]).tolerance(1.0));
        book.add(clear_story("T/New", [0.5, 0.5, 0.5]));
        book.add(Story::new("T/Boom", |_, _| panic!("kaboom")).size(8, 8));
        book.add(clear_story("T/Hidden", [0.0, 0.0, 0.0]).tag(NO_SCREENSHOT_TAG));
        let options = StorybookTestOptions {
            policy: GoldenPolicy::default(),
            ..Default::default()
        };

        let checks = check_storybook(&book, &golden, &options);
        let outcome = |id: &str| &checks.iter().find(|c| c.id == id).unwrap().outcome;
        assert!(matches!(outcome("T/Red"), StoryCheckOutcome::Failed(m) if m.contains("pixels differ")));
        assert!(matches!(
            outcome("T/Loose"),
            StoryCheckOutcome::Passed(ScreenshotOutcome::Matched { .. })
        ));
        assert!(matches!(
            outcome("T/New"),
            StoryCheckOutcome::Passed(ScreenshotOutcome::Created { .. })
        ));
        assert!(matches!(outcome("T/Boom"), StoryCheckOutcome::Failed(m) if m.contains("kaboom")));
        assert!(matches!(outcome("T/Hidden"), StoryCheckOutcome::Skipped(_)));
        assert!(tmp.path().join("T/Red.diff.png").is_file());
        assert!(tmp.path().join("T/New.png").is_file());

        // The assertion collects every failure into one panic.
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_storybook_in(&book, &golden, &options)
        }))
        .unwrap_err();
        let msg = panic.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(msg.contains("3 of 6 stories failed"), "{msg}");
        for id in ["[T/Red]", "[T/Blue]", "[T/Boom]"] {
            assert!(msg.contains(id), "{id} missing from:\n{msg}");
        }
    });
}
