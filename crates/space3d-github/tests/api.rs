//! REST client, update checker and plugin against a local mock server.
#![cfg(not(target_arch = "wasm32"))]

mod common;

use space3d_core::prelude::*;
use space3d_github::*;
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(f: F) -> F::Output {
    space3d_tasks::runtime().block_on(f)
}

fn game() -> RepoRef {
    RepoRef::new("space", "game")
}

#[test]
fn sends_github_headers_and_maps_repos() {
    let mock = common::start();
    let client = mock.client();
    let repo = run(client.get_repo(&game())).unwrap();
    assert_eq!(repo.full_name, "space/game");
    assert_eq!(repo.owner.login, "space");
    assert_eq!(repo.default_branch, "main");

    let req = &mock.find("GET", "/repos/space/game")[0];
    assert_eq!(req.header("accept"), Some("application/vnd.github+json"));
    assert_eq!(req.header("x-github-api-version"), Some("2022-11-28"));
    assert_eq!(req.header("authorization"), Some("Bearer test-token"));
    assert!(req.header("user-agent").unwrap().starts_with("space3d/"));

    let mine = run(client.create_repo(&CreateRepo::user("new-game", true).with_description("wip"))).unwrap();
    assert_eq!((mine.full_name.as_str(), mine.private), ("me/new-game", true));
    let body = mock.find("POST", "/user/repos")[0].json();
    assert_eq!(body["description"], "wip");
    assert!(body.get("org").is_none(), "org is routed via the URL, not the body");

    let org = run(client.create_repo(&CreateRepo::org("spacecorps", "team-game", false))).unwrap();
    assert_eq!(org.full_name, "spacecorps/team-game");
}

#[test]
fn api_errors_are_mapped() {
    let mock = common::start();
    let err = run(mock.client().get_repo(&RepoRef::new("space", "missing"))).unwrap_err();
    assert!(err.is_not_found(), "{err:?}");

    let anon = GithubClient::new(None).with_base_url(&mock.base);
    match run(anon.get_repo(&game())).unwrap_err() {
        GithubError::Api {
            status,
            message,
            documentation_url,
        } => {
            assert_eq!(status, 401);
            assert_eq!(message, "Bad credentials");
            assert!(documentation_url.is_some());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        mock.find("GET", "/repos/space/game")[0]
            .header("authorization")
            .is_none()
    );

    // Talking to real GitHub without a token fails fast, without a request.
    let err = run(GithubClient::new(None).create_issue(&game(), &NewIssue::new("t", "b"))).unwrap_err();
    assert!(matches!(err, GithubError::NoToken));
}

#[test]
fn release_flow_create_upload_list_latest() {
    let mock = common::start();
    let client = mock.client();

    let mut new = NewRelease::new("v1.4.0");
    new.body = Some("Faster ships".into());
    let release = run(client.create_release(&game(), &new)).unwrap();
    assert_eq!(release.id, 7);
    let sent = mock.find("POST", "/repos/space/game/releases")[0].json();
    assert_eq!(sent["tag_name"], "v1.4.0");
    assert_eq!(sent["generate_release_notes"], true);

    // Upload from memory and from a file (content type from the extension).
    let asset = run(client.upload_release_asset(&release, "game-web.zip", "application/zip", vec![1, 2, 3])).unwrap();
    assert_eq!((asset.name.as_str(), asset.size), ("game-web.zip", 3));
    let dir = tempfile::tempdir().unwrap();
    let apk = dir.path().join("game.apk");
    std::fs::write(&apk, [0u8; 32]).unwrap();
    let asset = run(client.upload_release_asset_file(&release, &apk)).unwrap();
    assert_eq!(asset.name, "game.apk");
    assert_eq!(asset.size, 32);
    assert_eq!(asset.content_type, "application/vnd.android.package-archive");
    let uploads = mock.find("POST", "/uploads/repos/space/game/releases/7/assets");
    assert_eq!(uploads.len(), 2);
    assert_eq!(uploads[0].query, "name=game-web.zip");
    assert_eq!(uploads[0].body, [1, 2, 3]);

    let releases = run(client.list_releases(&game())).unwrap();
    assert_eq!(releases.len(), 5);
    assert_eq!(mock.find("GET", "/repos/space/game/releases")[0].query, "per_page=100");

    let latest = run(client.latest_release(&game())).unwrap().unwrap();
    assert_eq!(latest.tag_name, "v1.2.0");
    assert_eq!(latest.version(), Some(semver::Version::new(1, 2, 0)));
    assert!(
        run(client.latest_release(&RepoRef::new("space", "empty")))
            .unwrap()
            .is_none()
    );
    assert!(run(client.release_by_tag(&game(), "v9.9.9")).unwrap().is_none());

    // Engine-facing Task variant.
    let task = client.latest_release_task(game());
    assert_eq!(task.block().unwrap().unwrap().unwrap().id, 4);
}

#[test]
fn issues_and_workflows() {
    let mock = common::start();
    let client = mock.client();
    let issue =
        run(client.create_issue(&game(), &NewIssue::new("Crash on boot", "details").with_label("bug"))).unwrap();
    assert_eq!(issue.number, 42);
    assert_eq!(issue.title, "Crash on boot");
    assert_eq!(mock.issues()[0]["labels"][0], "bug");

    let mut inputs = serde_json::Map::new();
    inputs.insert("target".into(), "web".into());
    run(client.dispatch_workflow(&game(), "space3d.yml", "main", Some(inputs))).unwrap();
    let dispatch = mock.find("POST", "/repos/space/game/actions/workflows/space3d.yml/dispatches")[0].json();
    assert_eq!(
        dispatch,
        serde_json::json!({ "ref": "main", "inputs": { "target": "web" } })
    );

    let runs = run(client.list_workflow_runs(&game(), &WorkflowRunsQuery::default())).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(!runs[0].is_completed());
    assert!(runs[1].is_completed() && runs[1].is_success());

    let q = WorkflowRunsQuery {
        branch: Some("main".into()),
        per_page: Some(5),
        ..WorkflowRunsQuery::workflow("space3d.yml")
    };
    let runs = run(client.list_workflow_runs(&game(), &q)).unwrap();
    assert_eq!(runs[0].event, "workflow_dispatch");
    let req = &mock.find("GET", "/repos/space/game/actions/workflows/space3d.yml/runs")[0];
    assert_eq!(req.query, "branch=main&per_page=5");

    let t = client.dispatch_workflow_task(game(), "space3d.yml".into(), "main".into());
    t.block().unwrap().unwrap();
}

#[test]
fn update_checker_compares_semver() {
    let mock = common::start();
    let checker = UpdateChecker::new(mock.client(), game(), "1.1.0").unwrap();

    let status = run(checker.check_async()).unwrap();
    let update = status.update().expect("1.2.0 > 1.1.0");
    assert_eq!(update.latest, semver::Version::new(1, 2, 0));
    assert_eq!(update.url(), "https://github.com/space/game/releases/tag/v1.2.0");
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    assert!(update.asset().is_some());

    // Beta channel sees the prerelease; drafts are never offered.
    let beta = checker
        .clone()
        .include_prereleases(true)
        .check()
        .block()
        .unwrap()
        .unwrap();
    assert_eq!(beta.update().unwrap().latest.to_string(), "1.3.0-beta.1");

    let current = UpdateChecker::new(mock.client(), game(), "v1.2.0").unwrap();
    assert!(!current.check().block().unwrap().unwrap().is_update_available());

    assert!(UpdateChecker::new(mock.client(), game(), "not-a-version").is_err());
}

#[test]
fn plugin_checks_for_updates_without_blocking() {
    let mock = common::start();
    let mut app = App::new();
    let mut plugin = GithubPlugin::new(game(), "1.0.0").with_api_base_url(&mock.base);
    plugin.token = Some(common::TOKEN.into());
    app.add_plugin(plugin);
    assert!(app.resources.contains::<GithubClient>());
    assert_eq!(app.resources.expect::<UpdateCheckState>(), &UpdateCheckState::Checking);

    let start = Instant::now();
    let mut frames = 0;
    while app.resources.expect::<UpdateCheckState>() == &UpdateCheckState::Checking {
        app.step_ticks(1);
        frames += 1;
        std::thread::sleep(Duration::from_millis(1));
        assert!(start.elapsed() < Duration::from_secs(20), "update check never finished");
    }
    assert!(frames >= 1);
    match app.resources.expect::<UpdateCheckState>() {
        UpdateCheckState::Done(status) => {
            assert_eq!(status.update().unwrap().latest, semver::Version::new(1, 2, 0))
        }
        other => panic!("{other:?}"),
    }
}
