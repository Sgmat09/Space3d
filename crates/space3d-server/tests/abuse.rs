//! Abuse regressions over real sockets: slow and idle HTTP clients, `X-Forwarded-For` spoofing
//! through repeated header lines, login lockout by other addresses, cross-site requests to the
//! open dev admin API, lobby limits, and admin audit attribution.

mod common;

use space3d_core::App;
use space3d_db::Db;
use space3d_net::{Http, HttpRequest};
use space3d_replication::api::{CreateLobbyRequest, QuickPlayRequest};
use space3d_replication::services::{DeviceCredentials, PlayerServiceClient};
use space3d_server::config::{Role, ServerConfig};
use space3d_server::lifecycle::{LifecycleHandle, Phase};
use space3d_server::ps::PlayerServiceHandle;
use space3d_server::security::Secrets;
use space3d_server::{AdminHandle, BuildOptions, ServerMain};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

struct Node {
    app: App,
    addr: SocketAddr,
    admin: SocketAddr,
    db: Db,
}

impl Node {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn api(&self) -> PlayerServiceClient {
        PlayerServiceClient::new(&Http::new(), &self.url())
    }
}

fn start(role: Role, admin_tokens: &[&str], edit: impl FnOnce(&mut ServerConfig)) -> Node {
    let dir = std::env::temp_dir();
    let mut c = ServerConfig {
        role,
        server_id: "abuse-1".into(),
        ..Default::default()
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    c.player_service.bind = format!("127.0.0.1:{port}").parse().unwrap();
    c.game.bind = c.player_service.bind;
    c.game.public_url = format!("ws://127.0.0.1:{port}/play");
    c.game.shards = 1;
    c.admin.bind = "127.0.0.1:0".parse().unwrap();
    c.persistence.data_dir = dir;
    c.player_service.password_auth = cfg!(feature = "password-auth");
    edit(&mut c);
    let db = Db::open_in_memory();
    let app = ServerMain::with_config(common::DuelGame, c)
        .options(BuildOptions {
            secrets: Some(Secrets::for_tests("fleet", admin_tokens)),
            db: Some(db.clone()),
            ..Default::default()
        })
        .build()
        .expect("server starts");
    let addr = app.resources.expect::<PlayerServiceHandle>().addr;
    let admin = app.resources.expect::<AdminHandle>().addr;
    Node { app, addr, admin, db }
}

/// Send a raw HTTP/1.1 request; the status code (0 = connection closed without an answer).
fn raw_status(addr: SocketAddr, request: &str) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut buf = [0u8; 128];
    let n = s.read(&mut buf).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n])
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

fn closed_within(s: &mut TcpStream, within: Duration) -> bool {
    s.set_read_timeout(Some(within)).unwrap();
    let mut buf = [0u8; 512];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return false;
            }
            Err(_) => return true,
        }
    }
}

/// Regression (slowloris): silent sockets and incomplete request heads were held forever and
/// one address could open unlimited sockets on the game port and the player-service port;
/// request bodies could trickle in forever.
#[test]
fn idle_slow_and_excess_sockets_are_closed() {
    let node = start(Role::AllInOne, &["admin-token-0123456789"], |c| {
        c.limits.http_header_timeout = Duration::from_millis(400);
        c.limits.http_body_timeout = Duration::from_millis(400);
        c.limits.http_connections_per_ip = 3;
    });
    let started = Instant::now();
    let mut idle = TcpStream::connect(node.addr).unwrap();
    let mut slow = TcpStream::connect(node.addr).unwrap();
    slow.write_all(b"GET /play HTTP/1.1\r\nHost: x\r\n").unwrap();
    // A body announced as 60 000 bytes, one byte sent.
    let mut trickle = TcpStream::connect(node.addr).unwrap();
    trickle
        .write_all(b"POST /v1/auth/guest HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 60000\r\n\r\n{")
        .unwrap();
    // Over the per-address cap: refused at accept.
    let mut extra = TcpStream::connect(node.addr).unwrap();
    assert!(
        closed_within(&mut extra, Duration::from_secs(3)),
        "4th socket from one address"
    );
    let mut answer = String::new();
    trickle.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let _ = trickle.read_to_string(&mut answer);
    assert!(answer.starts_with("HTTP/1.1 408"), "trickled body: {answer}");
    assert!(closed_within(&mut idle, Duration::from_secs(3)), "idle socket");
    assert!(closed_within(&mut slow, Duration::from_secs(3)), "incomplete head");
    assert!(started.elapsed() < Duration::from_secs(4));
    // The service still answers.
    let status = raw_status(
        node.addr,
        "GET /v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    drop(node);
}

/// Regression: only the first `X-Forwarded-For` line was read. Behind a proxy that adds its
/// own line (HAProxy `option forwardfor`), the client's line chose the address every per-IP
/// control keyed on. All lines are now joined in order.
#[test]
fn every_forwarded_for_line_counts() {
    let node = start(Role::PlayerService, &["admin-token-0123456789"], |c| {
        c.player_service.http_rps_per_ip = 2;
        c.security.trusted_proxies = vec!["127.0.0.1/32".into()];
    });
    let mut limited = 0;
    for i in 0..30 {
        let req = format!(
            "GET /v1/status HTTP/1.1\r\nHost: x\r\nX-Forwarded-For: 7.7.0.{i}\r\nX-Forwarded-For: 203.0.113.10\r\nConnection: close\r\n\r\n"
        );
        if raw_status(node.addr, &req) == 429 {
            limited += 1;
        }
    }
    assert!(
        limited >= 20,
        "the real client (the proxy's line) was limited: {limited}/30"
    );
}

/// Regression: the per-username login bucket was charged before the password check, so five
/// wrong guesses from anywhere locked the owner out. Failures are now charged per (username,
/// address) first, with a 4× username-wide backstop.
#[cfg(feature = "password-auth")]
#[test]
fn wrong_guesses_from_other_addresses_do_not_lock_the_owner_out() {
    use space3d_replication::api::RegisterRequest;
    let node = start(Role::PlayerService, &["admin-token-0123456789"], |c| {
        c.player_service.login_attempts_per_min = 5;
        c.security.trusted_proxies = vec!["127.0.0.1/32".into()];
    });
    let owner = node.api();
    owner
        .login_guest(&DeviceCredentials::generate(), Some("victim"))
        .block()
        .unwrap()
        .unwrap();
    owner
        .register(&RegisterRequest {
            username: "victim".into(),
            password: "correct horse battery".into(),
            display_name: "Victim".into(),
        })
        .block()
        .unwrap()
        .unwrap();
    let login = |xff: &str, password: &str| {
        let body = serde_json::json!({ "username": "victim", "password": password }).to_string();
        raw_status(
            node.addr,
            &format!(
                "POST /v1/auth/login HTTP/1.1\r\nHost: x\r\nX-Forwarded-For: {xff}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    };
    let mut attacker = Vec::new();
    for ip in ["203.0.113.1", "203.0.113.2"] {
        for _ in 0..6 {
            attacker.push(login(ip, "wrong guess"));
        }
    }
    assert!(attacker.contains(&401) && attacker.contains(&429), "{attacker:?}");
    assert_eq!(
        login("192.0.2.50", "correct horse battery"),
        200,
        "the owner still logs in"
    );
}

/// Regression (CSRF): the open dev admin API (loopback, no token) accepted cross-site "simple"
/// POSTs, so any web page could drain, ban or kick on a developer's local server.
#[test]
fn the_open_admin_api_refuses_cross_site_requests() {
    let node = start(Role::PlayerService, &[], |_| {});
    let post = |headers: &str| {
        raw_status(
            node.admin,
            &format!(
                "POST /admin/v1/drain HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
    };
    assert_eq!(post("Origin: https://evil.example\r\n"), 403);
    let rebound = raw_status(
        node.admin,
        "POST /admin/v1/drain HTTP/1.1\r\nHost: rebind.evil.example\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(rebound, 403, "DNS rebinding");
    let lifecycle = node.app.resources.expect::<LifecycleHandle>().clone();
    assert_eq!(lifecycle.requests(), 0, "nothing drained");
    assert_eq!(post(""), 202, "local tools (curl) still work");
    assert_eq!(lifecycle.requests(), 1);
    assert!(lifecycle.phase() <= Phase::Stopped);
}

/// Regression: lobbies had no per-host cap, no mode validation and a 6 h lifetime.
#[test]
fn lobbies_are_bounded() {
    let node = start(Role::PlayerService, &["admin-token-0123456789"], |_| {});
    let host = node.api();
    host.login_guest(&DeviceCredentials::generate(), Some("host"))
        .block()
        .unwrap()
        .unwrap();
    let guest = node.api();
    guest
        .login_guest(&DeviceCredentials::generate(), Some("guest"))
        .block()
        .unwrap()
        .unwrap();
    let create = |mode: &str| {
        host.create_lobby(&CreateLobbyRequest {
            mode: mode.into(),
            max_players: 4,
            ..Default::default()
        })
        .block()
        .unwrap()
    };
    let e = create(&"x".repeat(60_000)).unwrap_err();
    assert_eq!(e.status(), Some(400));
    let first = create("duel").unwrap();
    let second = create("duel").unwrap();
    assert_ne!(first.code, second.code);
    assert_eq!(
        guest.join_lobby(&first.code).block().unwrap().unwrap_err().status(),
        Some(404),
        "a host's previous lobby is closed"
    );
    guest.join_lobby(&second.code).block().unwrap().unwrap();
}

/// Regression: closing / checkpointing a room through the admin API was audit-logged as
/// "admin" instead of the token that did it.
#[test]
fn room_actions_are_audited_with_the_token_actor() {
    let token = "admin-token-0123456789";
    let mut node = start(Role::AllInOne, &[token], |c| {
        c.player_service.http_rps_per_ip = 10_000;
    });
    // Let the game server register with the (in-process) fleet.
    let ready = Instant::now() + Duration::from_secs(5);
    while !node
        .app
        .resources
        .expect::<space3d_server::health::HealthState>()
        .is_registered()
        && Instant::now() < ready
    {
        node.app.update(Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(5));
    }
    node.app.update(Duration::from_millis(100));
    let api = node.api();
    api.login_guest(&DeviceCredentials::generate(), Some("p"))
        .block()
        .unwrap()
        .unwrap();
    let a = api
        .quick_play(&QuickPlayRequest {
            mode: "ffa".into(),
            ..Default::default()
        })
        .block()
        .unwrap()
        .unwrap();
    // Quick play pre-creates the room (PrepareRoom); wait for it.
    let url = format!("http://{}/admin/v1/rooms/{}/checkpoint", node.admin, a.room_id);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        node.app.update(Duration::from_millis(100));
        let res = Http::new()
            .request(HttpRequest::post(url.clone()).header("authorization", format!("Bearer {token}")))
            .block()
            .unwrap()
            .unwrap();
        if res.status != 404 || Instant::now() > deadline {
            break res.status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status, 202);
    let deadline = Instant::now() + Duration::from_secs(5);
    let actors = loop {
        let rows: Vec<serde_json::Value> = node
            .db
            .blocking()
            .query("SELECT actor, action FROM audit_log WHERE action = 'room_action'", ())
            .unwrap();
        if !rows.is_empty() || Instant::now() > deadline {
            break rows;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(actors.len(), 1, "{actors:?}");
    let actor = actors[0]["actor"].as_str().unwrap_or_default().to_owned();
    assert!(
        actor.starts_with("admin:") && actor.len() == "admin:".len() + 8,
        "token-derived actor, got {actor:?}"
    );
}
