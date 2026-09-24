//! Tiny in-process GitHub API mock (axum on an ephemeral port). No real GitHub calls.
#![allow(dead_code)]

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "test-token";

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

#[derive(Clone)]
struct Shared {
    base: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    next_issue: Arc<AtomicU64>,
}

pub struct Mock {
    pub base: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Mock {
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn find(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == method && r.path == path)
            .collect()
    }

    /// Bodies of successfully authenticated issue creations.
    pub fn issues(&self) -> Vec<Value> {
        self.find("POST", "/repos/space/game/issues")
            .iter()
            .filter(|r| r.header("authorization") == Some(&format!("Bearer {TOKEN}")))
            .map(Recorded::json)
            .collect()
    }

    pub fn client(&self) -> space3d_github::GithubClient {
        space3d_github::GithubClient::new(Some(TOKEN.into())).with_base_url(&self.base)
    }
}

/// Start the mock on the shared space3d background runtime.
pub fn start() -> Mock {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let shared = Shared {
        base: base.clone(),
        requests: requests.clone(),
        next_issue: Arc::new(AtomicU64::new(42)),
    };
    let app = Router::new().fallback(handler).with_state(shared);
    space3d_tasks::runtime().spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        axum::serve(listener, app).await.unwrap();
    });
    Mock { base, requests }
}

fn repo(owner: &str, name: &str, private: bool) -> Value {
    json!({
        "id": 1, "name": name, "full_name": format!("{owner}/{name}"), "private": private,
        "owner": { "login": owner, "id": 9, "type": "User" },
        "html_url": format!("https://github.com/{owner}/{name}"),
        "clone_url": format!("https://github.com/{owner}/{name}.git"),
        "ssh_url": format!("git@github.com:{owner}/{name}.git"),
        "default_branch": "main", "description": null, "unknown_field": [1, 2, 3]
    })
}

fn release(base: &str, id: u64, tag: &str, prerelease: bool, draft: bool) -> Value {
    json!({
        "id": id, "tag_name": tag, "name": tag, "draft": draft, "prerelease": prerelease,
        "target_commitish": "main",
        "html_url": format!("https://github.com/space/game/releases/tag/{tag}"),
        "upload_url": format!("{base}/uploads/repos/space/game/releases/{id}/assets{{?name,label}}"),
        "assets": [
            { "id": id * 10, "name": "game-linux.zip", "size": 10, "content_type": "application/zip",
              "browser_download_url": "https://example.invalid/game-linux.zip", "state": "uploaded" },
            { "id": id * 10 + 1, "name": "game-macos.zip", "size": 10, "content_type": "application/zip",
              "browser_download_url": "https://example.invalid/game-macos.zip", "state": "uploaded" },
            { "id": id * 10 + 2, "name": "game-windows.zip", "size": 10, "content_type": "application/zip",
              "browser_download_url": "https://example.invalid/game-windows.zip", "state": "uploaded" }
        ]
    })
}

fn reply(status: StatusCode, body: Value) -> Response {
    (status, axum::Json(body)).into_response()
}

async fn handler(State(s): State<Shared>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let rec = Recorded {
        method: method.to_string(),
        path: uri.path().to_owned(),
        query: uri.query().unwrap_or_default().to_owned(),
        headers: headers.clone(),
        body: body.to_vec(),
    };
    s.requests.lock().unwrap().push(rec.clone());

    if rec.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
        return reply(
            StatusCode::UNAUTHORIZED,
            json!({ "message": "Bad credentials", "documentation_url": "https://docs.github.com/rest" }),
        );
    }
    let req = rec.json();
    let base = &s.base;
    match (rec.method.as_str(), rec.path.as_str()) {
        ("GET", "/repos/space/game") => reply(StatusCode::OK, repo("space", "game", false)),
        ("POST", "/user/repos") => reply(
            StatusCode::CREATED,
            repo("me", req["name"].as_str().unwrap(), req["private"].as_bool().unwrap()),
        ),
        ("POST", "/orgs/spacecorps/repos") => reply(
            StatusCode::CREATED,
            repo(
                "spacecorps",
                req["name"].as_str().unwrap(),
                req["private"].as_bool().unwrap(),
            ),
        ),
        ("GET", "/repos/space/game/releases") => reply(
            StatusCode::OK,
            json!([
                release(base, 5, "v1.3.0-beta.1", true, false),
                release(base, 6, "v2.0.0", false, true),
                release(base, 4, "v1.2.0", false, false),
                release(base, 3, "v1.1.0", false, false),
                release(base, 1, "v0.9.0", false, false),
            ]),
        ),
        ("GET", "/repos/space/game/releases/latest") => reply(StatusCode::OK, release(base, 4, "v1.2.0", false, false)),
        ("POST", "/repos/space/game/releases") => {
            let mut r = release(
                base,
                7,
                req["tag_name"].as_str().unwrap(),
                req["prerelease"].as_bool().unwrap(),
                req["draft"].as_bool().unwrap(),
            );
            r["assets"] = json!([]);
            reply(StatusCode::CREATED, r)
        }
        ("POST", "/uploads/repos/space/game/releases/7/assets") => {
            let name = rec
                .query
                .split('&')
                .find_map(|kv| kv.strip_prefix("name="))
                .unwrap_or_default()
                .to_owned();
            reply(
                StatusCode::CREATED,
                json!({
                    "id": 700, "name": name, "size": rec.body.len(),
                    "content_type": rec.header("content-type").unwrap_or_default(),
                    "browser_download_url": format!("https://example.invalid/{name}"), "state": "uploaded"
                }),
            )
        }
        ("POST", "/repos/space/game/issues") => {
            let n = s.next_issue.fetch_add(1, Ordering::SeqCst);
            reply(
                StatusCode::CREATED,
                json!({
                    "id": 1000 + n, "number": n, "title": req["title"], "body": req["body"], "state": "open",
                    "html_url": format!("https://github.com/space/game/issues/{n}")
                }),
            )
        }
        ("POST", "/repos/space/game/actions/workflows/space3d.yml/dispatches") => {
            StatusCode::NO_CONTENT.into_response()
        }
        ("GET", "/repos/space/game/actions/runs") | ("GET", "/repos/space/game/actions/workflows/space3d.yml/runs") => {
            reply(
                StatusCode::OK,
                json!({ "total_count": 2, "workflow_runs": [
                    { "id": 11, "name": "Space3d CI", "run_number": 2, "event": "workflow_dispatch",
                      "status": "in_progress", "conclusion": null, "head_branch": "main", "head_sha": "abc",
                      "workflow_id": 77, "html_url": "https://github.com/space/game/actions/runs/11" },
                    { "id": 10, "name": "Space3d CI", "run_number": 1, "event": "push",
                      "status": "completed", "conclusion": "success", "head_branch": "main", "head_sha": "abd",
                      "workflow_id": 77, "html_url": "https://github.com/space/game/actions/runs/10" }
                ]}),
            )
        }
        _ => reply(
            StatusCode::NOT_FOUND,
            json!({ "message": "Not Found", "documentation_url": "https://docs.github.com/rest" }),
        ),
    }
}
