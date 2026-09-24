use crate::pump_until;
use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_net::prelude::*;
use space3d_net::server::axum::{http::StatusCode, routing::get};
use space3d_tasks::Task;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Hello {
    msg: String,
}

#[derive(Serialize, Deserialize)]
struct AddReq {
    a: i32,
    b: i32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct AddRes {
    sum: i32,
}

fn test_server(dir: &Path) -> ServerHandle {
    let flaky = Arc::new(AtomicU32::new(0));
    HttpServer::new()
        .serve_dir("/", dir)
        .get_json("/api/hello", || Hello { msg: "hi".into() })
        .post_json("/api/add", |r: AddReq| AddRes { sum: r.a + r.b })
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                "slow"
            }),
        )
        .route(
            "/hang",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "late"
            }),
        )
        .route(
            "/flaky",
            get(move || {
                let n = flaky.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 2 {
                        (StatusCode::SERVICE_UNAVAILABLE, "try again")
                    } else {
                        (StatusCode::OK, "ok")
                    }
                }
            }),
        )
        .cors_permissive()
        .no_cache()
        .start()
        .expect("bind localhost:0")
}

fn web_build_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<h1>space3d</h1>").unwrap();
    std::fs::write(dir.path().join("game.wasm"), b"\0asm\x01\0\0\0").unwrap();
    dir
}

fn net_app() -> App {
    let mut app = App::new();
    app.add_plugin(NetPlugin::default());
    app
}

/// Poll a task by ticking the engine (never blocking on it).
fn resolve<T>(app: &mut App, what: &str, mut task: Task<T>) -> T {
    let mut out = None;
    pump_until(app, what, |_| {
        out = out.take().or_else(|| task.poll());
        out.is_some()
    });
    out.unwrap()
}

#[derive(Default)]
struct Got(Option<Result<HttpResponse, NetError>>);

#[test]
fn get_then_resolves_while_engine_keeps_ticking() {
    let dir = web_build_dir();
    let server = test_server(dir.path());
    assert_ne!(server.port(), 0, "port 0 must resolve to the real port");

    let mut app = net_app();
    app.init_resource::<Got>();
    app.resources
        .expect::<Http>()
        .get_then(format!("{}/slow", server.url()), |ctx, r| {
            ctx.resources.expect_mut::<Got>().0 = Some(r);
        });
    assert_eq!(app.resources.expect::<Http>().pending_callbacks(), 1);
    let tick0 = app.resources.expect::<Time>().tick;

    let frames = pump_until(&mut app, "/slow response", |app| {
        app.resources.expect::<Got>().0.is_some()
    });

    assert!(
        frames >= 3,
        "engine must keep ticking while the request is in flight (frames={frames})"
    );
    assert_eq!(app.resources.expect::<Time>().tick - tick0, frames);
    assert_eq!(app.resources.expect::<Http>().pending_callbacks(), 0);
    let resp = app.resources.remove::<Got>().unwrap().0.unwrap().unwrap();
    assert_eq!((resp.status, resp.text().as_str()), (200, "slow"));
    assert_eq!(resp.header("cache-control"), Some("no-store"));
}

#[test]
fn static_files_json_routes_and_cors() {
    let dir = web_build_dir();
    let server = test_server(dir.path());
    let url = server.url();
    let mut app = net_app();
    let http = app.resources.expect::<Http>().clone();

    let index = resolve(&mut app, "index", http.get(format!("{url}/"))).unwrap();
    assert_eq!((index.status, index.text().as_str()), (200, "<h1>space3d</h1>"));

    let wasm = resolve(&mut app, "wasm", http.get(format!("{url}/game.wasm"))).unwrap();
    assert_eq!(wasm.header("content-type"), Some("application/wasm"));
    assert_eq!(wasm.body, b"\0asm\x01\0\0\0");

    let missing = resolve(&mut app, "404", http.get(format!("{url}/nope.txt"))).unwrap();
    assert_eq!(missing.status, 404);
    assert!(matches!(
        missing.error_for_status(),
        Err(NetError::Status { status: 404, .. })
    ));

    let hello: Hello = resolve(&mut app, "get_json", http.get_json(format!("{url}/api/hello"))).unwrap();
    assert_eq!(hello, Hello { msg: "hi".into() });

    let sum: AddRes = resolve(
        &mut app,
        "post_json",
        http.post_json(format!("{url}/api/add"), &AddReq { a: 2, b: 3 }),
    )
    .unwrap();
    assert_eq!(sum, AddRes { sum: 5 });

    let bad = resolve(
        &mut app,
        "bad json",
        http.request(
            HttpRequest::post(format!("{url}/api/add"))
                .header("content-type", "application/json")
                .body("{"),
        ),
    )
    .unwrap();
    assert!(
        (400..500).contains(&bad.status),
        "malformed body → 4xx, got {}",
        bad.status
    );

    let cors = resolve(
        &mut app,
        "cors",
        http.request(HttpRequest::get(format!("{url}/api/hello")).header("origin", "http://game.example")),
    )
    .unwrap();
    assert_eq!(cors.header("access-control-allow-origin"), Some("*"));

    // Relative URLs resolve against `base_url`.
    let api = Http::with_config(HttpConfig {
        base_url: Some(format!("{url}/api/")),
        ..Default::default()
    });
    let hello: Hello = resolve(&mut app, "relative", api.get_json("hello")).unwrap();
    assert_eq!(hello.msg, "hi");
}

#[test]
fn retries_timeouts_and_connect_errors() {
    let dir = web_build_dir();
    let server = test_server(dir.path());
    let url = server.url();
    let mut app = net_app();
    let http = app.resources.expect::<Http>().clone();

    // No retry by default: the first 503 comes straight back.
    let first = resolve(&mut app, "flaky", http.get(format!("{url}/flaky"))).unwrap();
    assert_eq!(first.status, 503);

    // With retries the second 503 is retried and the third attempt succeeds.
    let retry = RetryPolicy::exponential(3).with_backoff(Backoff::constant(Duration::from_millis(1)));
    let ok = resolve(
        &mut app,
        "flaky+retry",
        http.request(HttpRequest::get(format!("{url}/flaky")).retry(retry)),
    )
    .unwrap();
    assert_eq!((ok.status, ok.text().as_str()), (200, "ok"));

    let timed_out = resolve(
        &mut app,
        "timeout",
        http.request(HttpRequest::get(format!("{url}/hang")).timeout(Duration::from_millis(50))),
    );
    assert_eq!(timed_out, Err(NetError::Timeout));

    let refused = resolve(&mut app, "refused", http.get("http://127.0.0.1:1/"));
    assert!(matches!(refused, Err(NetError::Connect(_))), "{refused:?}");

    let invalid = resolve(&mut app, "invalid", http.get("not a url"));
    assert!(matches!(invalid, Err(NetError::InvalidUrl(_))), "{invalid:?}");
}

#[test]
fn shutdown_is_graceful_and_frees_the_port() {
    let dir = web_build_dir();
    let mut server = test_server(dir.path());
    let addr = server.local_addr();
    assert!(!server.is_finished());
    let trigger = server.trigger();
    trigger.shutdown();
    assert!(trigger.is_shutdown());
    server.wait().unwrap();
    let again = HttpServer::new()
        .bind(addr)
        .start()
        .expect("port is free again after shutdown");
    assert_eq!(again.local_addr(), addr);
}
