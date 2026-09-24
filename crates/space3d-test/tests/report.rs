//! `SPACE3D_TEST_REPORT` end to end: screenshot, determinism and performance checks append to
//! one report directory with JSON results and an HTML page. Kept to a single test because it
//! sets a process environment variable.

use space3d_core::ImageData;
use space3d_render::RenderStats;
use space3d_test::report::{ReportKind, ReportStatus, TestReport};
use space3d_test::screenshot::{Captured, FailureKind, GoldenDir, GoldenPolicy, check_screenshot};
use space3d_test::{CompareOptions, DeterminismCheck, RenderBudget, TestApp, env};

#[test]
fn checks_append_to_the_report() {
    let tmp = tempfile::tempdir().unwrap();
    let report_dir = tmp.path().join("report");
    // SAFETY: this is the only test in this binary, so no other thread reads the environment.
    unsafe { std::env::set_var(env::TEST_REPORT, &report_dir) };

    let golden = GoldenDir::new(tmp.path().join("goldens"), "report-suite");
    let policy = GoldenPolicy::default();
    let red = ImageData::filled(16, 8, [255, 0, 0, 255]);
    let mut changed = red.clone();
    for x in 0..8 {
        changed.set(x, 3, [0, 0, 255, 255]);
    }
    let cap = |img: &ImageData| Captured::new(img.clone(), Some("testgpu".into()));
    check_screenshot(&golden, "flag", cap(&red), &CompareOptions::default(), &policy).unwrap();
    check_screenshot(&golden, "same", cap(&red), &CompareOptions::default(), &policy).unwrap();
    check_screenshot(&golden, "same", cap(&red), &CompareOptions::default(), &policy).unwrap();
    let failure = check_screenshot(&golden, "flag", cap(&changed), &CompareOptions::default(), &policy).unwrap_err();
    assert!(matches!(failure.kind, FailureKind::Mismatch(ref s) if s.differing_pixels == 8));

    DeterminismCheck::new(3)
        .builder(TestApp::builder().no_render())
        .name("empty_world")
        .assert(|_| {});
    RenderBudget::new().max_draw_calls(10).assert_stats(&RenderStats {
        draw_calls: 3,
        ..Default::default()
    });
    // Performance and determinism checks are keyed by the test that ran them (libtest names
    // each test's thread after it), so one test's failure is not hidden by another's pass.
    let run_as = |test: &str, draw_calls: u32| {
        std::thread::Builder::new()
            .name(test.to_string())
            .spawn(move || {
                RenderBudget::new().max_draw_calls(10).assert_stats(&RenderStats {
                    draw_calls,
                    ..Default::default()
                });
                DeterminismCheck::new(1)
                    .builder(TestApp::builder().no_render())
                    .assert(|_| {});
            })
            .unwrap()
            .join()
    };
    assert!(run_as("physics::too_many_draws", 50).is_err());
    assert!(run_as("render::few_draws", 2).is_ok());

    let report = TestReport::from_env().expect("report enabled");
    assert_eq!(report.dir(), report_dir.as_path());
    let entries = report.entries().unwrap();
    let find = |kind: ReportKind, name: &str| {
        entries
            .iter()
            .find(|e| e.kind == kind && e.name == name)
            .unwrap_or_else(|| panic!("no {kind:?} entry {name:?} in {entries:#?}"))
    };
    // Latest result per check; failures sort first.
    assert_eq!(entries[0].status, ReportStatus::Failed);
    let flag = find(ReportKind::Screenshot, "flag");
    assert_eq!(flag.status, ReportStatus::Failed);
    assert_eq!(flag.details["differing_pixels"], 8);
    for img in [&flag.images.expected, &flag.images.actual, &flag.images.diff] {
        let rel = img.as_ref().expect("image stored");
        assert!(report_dir.join(rel).is_file(), "{rel}");
    }
    assert_eq!(find(ReportKind::Screenshot, "same").status, ReportStatus::Passed);
    assert_eq!(
        find(ReportKind::Determinism, "empty_world").status,
        ReportStatus::Passed
    );
    assert_eq!(
        find(ReportKind::Performance, "checks_append_to_the_report::render_budget").status,
        ReportStatus::Passed
    );
    assert_eq!(
        find(ReportKind::Performance, "physics::too_many_draws::render_budget").status,
        ReportStatus::Failed
    );
    assert_eq!(
        find(ReportKind::Performance, "render::few_draws::render_budget").status,
        ReportStatus::Passed
    );
    assert_eq!(
        find(ReportKind::Determinism, "render::few_draws::determinism").status,
        ReportStatus::Passed
    );

    let html = std::fs::read_to_string(report.html_path()).unwrap();
    assert!(html.contains("flag") && html.contains("empty_world"));
    assert!(html.contains(flag.images.diff.as_deref().unwrap()));
    assert!(report_dir.join("report.json").is_file());
}
