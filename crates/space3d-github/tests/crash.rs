//! Crash reporter end to end: panic hook → local storage → opt-in issue creation.
//! One test function because the panic hook is process-global.
#![cfg(not(target_arch = "wasm32"))]

mod common;

use space3d_core::prelude::*;
use space3d_github::*;
use std::time::{Duration, Instant};

#[test]
fn panics_are_stored_locally_and_filed_on_opt_in() {
    let mock = common::start();
    let dir = tempfile::tempdir().unwrap();
    let repo = RepoRef::new("space", "game");
    let reporter =
        CrashReporter::new(CrashReporterConfig::new("Space Game", "1.2.3", repo.clone()).with_storage_dir(dir.path()));
    assert!(reporter.pending().unwrap().is_empty(), "missing dir means no reports");

    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    reporter.set_screenshot_png(png.clone());
    reporter.install_panic_hook();
    let joined = std::thread::Builder::new()
        .name("sim-worker".into())
        .spawn(|| panic!("kaboom {}", 7))
        .unwrap()
        .join();
    assert!(joined.is_err());

    let pending = reporter.pending().unwrap();
    assert_eq!(pending.len(), 1);
    let report = &pending[0];
    assert_eq!(report.message, "kaboom 7");
    assert_eq!(report.thread.as_deref(), Some("sim-worker"));
    assert!(report.location.as_deref().unwrap().contains("crash.rs"), "{report:?}");
    assert_eq!(report.app_version, "1.2.3");
    assert_eq!(report.platform, PlatformInfo::current());
    assert!(!report.backtrace.is_empty());
    let shot = report.screenshot_path.as_deref().expect("screenshot saved with report");
    assert_eq!(std::fs::read(shot).unwrap(), png);

    // A manually recorded (non-panic) report.
    let mut fatal = CrashReport::new("Space Game", "1.2.3", "GPU device lost");
    reporter.save(&mut fatal).unwrap();
    assert_eq!(reporter.pending().unwrap().len(), 2);

    // Without valid credentials nothing is lost.
    let anon = GithubClient::new(None).with_base_url(&mock.base);
    let err = reporter.submit_pending(&anon).block().unwrap().unwrap_err();
    assert_eq!(err.status(), Some(401));
    assert_eq!(reporter.pending().unwrap().len(), 2);

    // Player opted in: one issue per report, local copies removed.
    let issues = reporter.submit_pending(&mock.client()).block().unwrap().unwrap();
    assert_eq!(issues.len(), 2);
    let sent = mock.issues();
    assert_eq!(sent[0]["title"], "[crash] Space Game 1.2.3: kaboom 7");
    assert_eq!(sent[0]["labels"], serde_json::json!(["crash"]));
    let body = sent[0]["body"].as_str().unwrap();
    assert!(body.contains("sim-worker") && body.contains("Backtrace") && body.contains("not uploaded"));
    assert!(sent[1]["title"].as_str().unwrap().ends_with("GPU device lost"));
    assert!(reporter.pending().unwrap().is_empty());
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "report + screenshot deleted"
    );

    // Manual bug report from an in-game form.
    let bug = reporter
        .submit_bug_report(&mock.client(), "Door stuck", "Level 3 door never opens")
        .block()
        .unwrap()
        .unwrap();
    assert_eq!(bug.title, "[bug] Space Game 1.2.3: Door stuck");

    // Plugin: a report left from a previous run is submitted at startup when opted in.
    let mut leftover = CrashReport::new("Space Game", "1.2.3", "previous run crashed");
    reporter.save(&mut leftover).unwrap();
    let before = mock.issues().len();
    let mut plugin = GithubPlugin::new(repo, "1.2.3")
        .with_api_base_url(&mock.base)
        .with_crash_reports("Space Game", dir.path());
    plugin.check_for_updates = false;
    plugin.submit_crash_reports = true;
    plugin.token = Some(common::TOKEN.into());
    let mut app = App::new();
    app.add_plugin(plugin);
    assert!(app.resources.contains::<CrashReporter>());
    let start = Instant::now();
    while mock.issues().len() == before || !reporter.pending().unwrap().is_empty() {
        app.step_ticks(1);
        std::thread::sleep(Duration::from_millis(1));
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "startup submission never happened"
        );
    }
    for _ in 0..50 {
        app.step_ticks(1);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        app.resources.expect::<PendingCrashReports>(),
        &PendingCrashReports(vec![])
    );
    assert!(
        mock.issues().last().unwrap()["title"]
            .as_str()
            .unwrap()
            .ends_with("previous run crashed")
    );
}
