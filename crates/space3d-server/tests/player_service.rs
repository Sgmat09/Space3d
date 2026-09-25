//! Player-service HTTP API integration tests: a `player-service` role process on loopback
//! (port 0), driven through `space3d_replication::services::PlayerServiceClient` (the client
//! games use) and raw HTTP. Sessions, refresh rotation and reuse detection, password accounts,
//! player data rules, rate limits, matchmaking and lobby endpoints, and persistence across a
//! restart.

use space3d_core::App;
use space3d_net::{Http, HttpRequest};
use space3d_replication::api::{
    CreateLobbyRequest, LobbyPhase, MatchmakeRequest, PutPlayerDataRequest, RegisterRequest, TicketStatus,
    UpdateProfileRequest,
};
use space3d_replication::services::{DeviceCredentials, OnlineError, PlayerServiceClient};
use space3d_server::clock::{ManualUnixClock, SystemUnixClock, UnixClock};
use space3d_server::config::{QueueConfig, Role, ServerConfig};
use space3d_server::ps::PlayerServiceHandle;
use space3d_server::security::Secrets;
use space3d_server::{BuildOptions, NoGame, ServerMain};
use std::sync::Arc;
use std::time::Duration;

struct Ps {
    app: App,
    url: String,
    clock: ManualUnixClock,
    http: Http,
}

impl Ps {
    fn start(edit: impl FnOnce(&mut ServerConfig), db: space3d_db::Db) -> Self {
        let mut config = ServerConfig {
            role: Role::PlayerService,
            server_id: "ps-test".into(),
            ..Default::default()
        };
        config.player_service.bind = "127.0.0.1:0".parse().unwrap();
        config.admin.bind = "127.0.0.1:0".parse().unwrap();
        config.player_service.password_auth = cfg!(feature = "password-auth");
        config.player_service.client_writable_keys = vec!["settings".into()];
        config.matchmaking.queues = vec![QueueConfig {
            name: "duel".into(),
            mode: "duel".into(),
            ..Default::default()
        }];
        edit(&mut config);
        // Start the manual clock at real time so client-side expiry checks agree with it.
        let clock = ManualUnixClock::at(SystemUnixClock.now_ms());
        let app = ServerMain::with_config(NoGame, config)
            .unix_clock(Arc::new(clock.clone()))
            .options(BuildOptions {
                secrets: Some(Secrets::for_tests("fleet", &["admin"])),
                db: Some(db),
                ..Default::default()
            })
            .build()
            .expect("player service starts");
        let url = app.resources.expect::<PlayerServiceHandle>().url();
        Self {
            app,
            url,
            clock,
            http: Http::new(),
        }
    }

    fn client(&self) -> PlayerServiceClient {
        PlayerServiceClient::new(&self.http, &self.url)
    }

    fn raw(&self, req: HttpRequest) -> (u16, String) {
        let res = self.http.request(req).block().unwrap().unwrap();
        (res.status, res.text())
    }
}

fn ok<T>(task: space3d_tasks::Task<Result<T, OnlineError>>) -> T {
    task.block().expect("task").expect("call succeeds")
}

fn err<T: std::fmt::Debug>(task: space3d_tasks::Task<Result<T, OnlineError>>) -> OnlineError {
    task.block().expect("task").expect_err("call fails")
}

#[test]
fn guest_sessions_refresh_rotation_reuse_detection_and_logout() {
    let ps = Ps::start(|_| {}, space3d_db::Db::open_in_memory());
    let device = DeviceCredentials::generate();
    let alice = ps.client();
    let first = ok(alice.login_guest(&device, Some("alice")));
    assert_eq!(first.profile.display_name, "alice");
    // Same device → same account; another device → another account.
    let again = ok(ps.client().login_guest(&device, None));
    assert_eq!(again.profile.player_id, first.profile.player_id);
    let other = ok(ps.client().login_guest(&DeviceCredentials::generate(), Some("bob")));
    assert_ne!(other.profile.player_id, first.profile.player_id);
    // Wrong secret for a known device is a different (new) identity, never a takeover.
    let forged = DeviceCredentials {
        device_id: device.device_id.clone(),
        device_secret: DeviceCredentials::generate().device_secret,
    };
    assert_ne!(
        ok(ps.client().login_guest(&forged, None)).profile.player_id,
        first.profile.player_id
    );

    assert_eq!(ok(alice.me()).player_id, first.profile.player_id);
    let renamed = ok(alice.update_profile(&UpdateProfileRequest {
        display_name: Some("Alice".into()),
    }));
    assert_eq!(renamed.display_name, "Alice");

    // Rotation: the old refresh token is spent; replaying it revokes the whole family.
    let old_refresh = alice.session().unwrap().refresh_token;
    let rotated = ok(alice.refresh());
    assert_ne!(rotated.refresh_token, old_refresh);
    assert_eq!(ok(alice.me()).display_name, "Alice");
    let thief = ps.client();
    thief.set_session(Some(space3d_replication::api::SessionTokens {
        refresh_token: old_refresh,
        ..rotated.clone()
    }));
    let e = err(thief.refresh());
    assert_eq!((e.status(), e.code()), (Some(401), Some("refresh_reused")));
    let e = err(alice.me());
    assert_eq!(e.status(), Some(401), "access tokens of a revoked family die too");
    assert!(err(alice.refresh()).status() == Some(401));
    assert!(!alice.is_logged_in(), "a rejected refresh ends the client session");

    // Logout revokes the refresh token.
    let bob = ps.client();
    ok(bob.login_guest(&DeviceCredentials::generate(), Some("bob")));
    let spent = bob.session().unwrap();
    ok(bob.logout());
    assert!(!bob.is_logged_in());
    bob.set_session(Some(spent));
    assert_eq!(err(bob.refresh()).status(), Some(401));
}

#[cfg(feature = "password-auth")]
#[test]
fn password_accounts_register_upgrade_and_login() {
    let ps = Ps::start(|_| {}, space3d_db::Db::open_in_memory());
    let guest = ps.client();
    let g = ok(guest.login_guest(&DeviceCredentials::generate(), Some("carol")));
    let reg = RegisterRequest {
        username: "Carol_01".into(),
        password: "correct horse battery".into(),
        display_name: "Carol".into(),
    };
    let upgraded = ok(guest.register(&reg));
    assert_eq!(
        upgraded.profile.player_id, g.profile.player_id,
        "the guest was upgraded in place"
    );
    let fresh = ps.client();
    let logged = ok(fresh.login_password("carol_01", "correct horse battery"));
    assert_eq!(logged.profile.player_id, g.profile.player_id);
    assert_eq!(
        err(ps.client().login_password("carol_01", "wrong password")).code(),
        Some("bad_credentials")
    );
    assert_eq!(err(ps.client().register(&reg)).status(), Some(409), "username taken");
    let weak = RegisterRequest {
        username: "dave".into(),
        password: "short".into(),
        display_name: "Dave".into(),
    };
    assert_eq!(err(ps.client().register(&weak)).code(), Some("weak_password"));
}

#[cfg(feature = "password-auth")]
#[test]
fn login_attempts_are_rate_limited_per_ip_and_refill_over_time() {
    let ps = Ps::start(
        |c| c.player_service.login_attempts_per_min = 3,
        space3d_db::Db::open_in_memory(),
    );
    for _ in 0..3 {
        assert_eq!(
            err(ps.client().login_password("nobody", "whatever1")).code(),
            Some("bad_credentials")
        );
    }
    let e = err(ps.client().login_password("nobody", "whatever1"));
    assert_eq!((e.status(), e.code()), (Some(429), Some("rate_limited")));
    assert!(e.retry_after_ms().is_some());
    ps.clock.advance_ms(60_000);
    assert_eq!(
        err(ps.client().login_password("nobody", "whatever1")).code(),
        Some("bad_credentials")
    );
}

#[test]
fn per_ip_request_rate_limit() {
    let ps = Ps::start(
        |c| c.player_service.http_rps_per_ip = 5,
        space3d_db::Db::open_in_memory(),
    );
    let statuses: Vec<u16> = (0..15)
        .map(|_| ps.raw(HttpRequest::get(format!("{}/v1/status", ps.url))).0)
        .collect();
    assert!(statuses.contains(&200));
    assert!(statuses.contains(&429), "{statuses:?}");
    assert!(
        statuses.iter().filter(|s| **s == 200).count() <= 10,
        "burst is 2× the rate"
    );
    ps.clock.advance_ms(2_000);
    assert_eq!(ps.raw(HttpRequest::get(format!("{}/v1/status", ps.url))).0, 200);
}

#[test]
fn player_data_rules_versions_and_errors() {
    let ps = Ps::start(|_| {}, space3d_db::Db::open_in_memory());
    let alice = ps.client();
    ok(alice.login_guest(&DeviceCredentials::generate(), Some("alice")));
    let put = |value: serde_json::Value, expected_version| PutPlayerDataRequest {
        value,
        expected_version,
    };
    let v1 = ok(alice.put_data("settings", &put(serde_json::json!({ "fov": 90 }), None)));
    assert_eq!(v1.version, 1);
    let v2 = ok(alice.put_data("settings", &put(serde_json::json!({ "fov": 100 }), Some(1))));
    assert_eq!(v2.version, 2);
    let stale = err(alice.put_data("settings", &put(serde_json::json!({ "fov": 1 }), Some(1))));
    assert_eq!(stale.status(), Some(409));
    assert_eq!(ok(alice.get_data("settings")).value, serde_json::json!({ "fov": 100 }));
    let ro = err(alice.put_data("progress", &put(serde_json::json!({ "level": 99 }), None)));
    assert_eq!((ro.status(), ro.code()), (Some(403), Some("read_only")));
    assert_eq!(err(alice.get_data("progress")).status(), Some(404));

    // Unauthenticated / malformed / oversized requests.
    let (status, body) = ps.raw(HttpRequest::get(format!("{}/v1/me", ps.url)));
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("\"code\":\"unauthorized\""), "{body}");
    let (status, _) =
        ps.raw(HttpRequest::get(format!("{}/v1/me", ps.url)).header("authorization", "Bearer s3.x.y.z.w"));
    assert_eq!(status, 401);
    let (status, body) = ps.raw(HttpRequest::post(format!("{}/v1/auth/guest", ps.url)).body("{not json"));
    assert_eq!(status, 400, "{body}");
    let huge = format!(
        r#"{{"device_id":"{}","device_secret":"{}"}}"#,
        "a".repeat(70_000),
        "b".repeat(64)
    );
    let (status, _) = ps.raw(
        HttpRequest::post(format!("{}/v1/auth/guest", ps.url))
            .header("content-type", "application/json")
            .body(huge),
    );
    assert_eq!(status, 413, "body limit");
    let status = ok(ps.client().status());
    assert_eq!(status.protocol, space3d_replication::PROTOCOL_VERSION);
}

#[test]
fn matchmaking_tickets_and_lobbies_over_http() {
    let mut ps = Ps::start(|_| {}, space3d_db::Db::open_in_memory());
    let alice = ps.client();
    ok(alice.login_guest(&DeviceCredentials::generate(), Some("alice")));
    let bob = ps.client();
    ok(bob.login_guest(&DeviceCredentials::generate(), Some("bob")));

    let unknown = err(alice.matchmake(&MatchmakeRequest {
        queue: "nope".into(),
        ..Default::default()
    }));
    assert_eq!(unknown.code(), Some("unknown_queue"));
    let t = ok(alice.matchmake(&MatchmakeRequest {
        queue: "duel".into(),
        ..Default::default()
    }))
    .ticket_id;
    let dup = err(alice.matchmake(&MatchmakeRequest {
        queue: "duel".into(),
        ..Default::default()
    }));
    assert_eq!(dup.status(), Some(409));
    ps.app.update(Duration::from_millis(100));
    assert!(matches!(ok(alice.poll_ticket(&t, 0)), TicketStatus::Searching { .. }));
    assert_eq!(err(bob.poll_ticket(&t, 0)).status(), Some(404), "tickets are private");
    ok(alice.cancel_ticket(&t));
    assert_eq!(ok(alice.poll_ticket(&t, 0)), TicketStatus::Cancelled);

    // Lobbies: create, join by code, ready, leave.
    let lobby = ok(alice.create_lobby(&CreateLobbyRequest {
        mode: "duel".into(),
        max_players: 2,
        ..Default::default()
    }));
    assert_eq!(lobby.code.len(), 6);
    assert_eq!(lobby.phase, LobbyPhase::Open);
    let joined = ok(bob.join_lobby(&lobby.code));
    assert_eq!(joined.members.len(), 2);
    let ready = ok(bob.lobby_action(&lobby.lobby_id, "ready"));
    assert!(ready.members.iter().any(|m| m.display_name == "bob" && m.ready));
    let polled = ok(alice.poll_lobby(&lobby.lobby_id, 0));
    assert_eq!(polled.members.len(), 2);
    // Leaving answers 404 for the leaver (the lobby is gone from their point of view).
    assert_eq!(err(bob.lobby_action(&lobby.lobby_id, "leave")).status(), Some(404));
    assert_eq!(ok(alice.poll_lobby(&lobby.lobby_id, 0)).members.len(), 1);
    assert_eq!(err(bob.join_lobby("ZZZZZZ")).status(), Some(404));
}

#[test]
fn accounts_and_data_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("players.db");
    let device = DeviceCredentials::generate();
    let id = {
        let ps = Ps::start(|_| {}, space3d_db::Db::open(&path));
        let c = ps.client();
        let t = ok(c.login_guest(&device, Some("persisted")));
        ok(c.put_data(
            "settings",
            &PutPlayerDataRequest {
                value: serde_json::json!({ "volume": 7 }),
                expected_version: None,
            },
        ));
        t.profile.player_id
    };
    let ps = Ps::start(|_| {}, space3d_db::Db::open(&path));
    let c = ps.client();
    let t = ok(c.login_guest(&device, None));
    assert_eq!(t.profile.player_id, id);
    assert_eq!(t.profile.display_name, "persisted");
    assert_eq!(ok(c.get_data("settings")).value, serde_json::json!({ "volume": 7 }));
}

/// Internal routes answer 429 / 401 from a middleware **before** the body is read: an
/// anonymous client announcing a 16 MiB checkpoint upload gets 401 right after its headers,
/// without the server buffering anything.
#[test]
fn internal_routes_refuse_before_reading_the_body() {
    use std::io::{Read, Write};
    let ps = Ps::start(
        |c| c.player_service.http_rps_per_ip = 1_000,
        space3d_db::Db::open_in_memory(),
    );
    let addr = ps.url.trim_start_matches("http://").to_owned();
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    write!(
        s,
        "PUT /internal/v1/rooms/1/checkpoint HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/octet-stream\r\nContent-Length: 16000000\r\n\r\n"
    )
    .unwrap();
    s.write_all(&[0u8; 1024]).unwrap();
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).expect("answered before the body was sent");
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");

    // With the fleet token a checkpoint round-trips.
    let auth = |r: HttpRequest| r.header("authorization", "Bearer fleet");
    let room = space3d_replication::RoomId(7);
    let url = format!("{}/internal/v1/rooms/{room}/checkpoint", ps.url);
    let cp = |room: u64, saved_ms: i64| {
        use space3d_server::gs::checkpoint::Checkpoint;
        Checkpoint::new(
            space3d_server::game::RoomSpec {
                room_id: space3d_replication::RoomId(room),
                ..Default::default()
            },
            space3d_replication::server::full_state::FullState {
                schema_hash: 1,
                tick: space3d_replication::Tick(10),
                epoch: 1,
                next_net_id: 1,
                entities: Vec::new(),
            },
            None,
            None,
            saved_ms,
        )
        .to_bytes()
    };
    let (status, body) = ps.raw(auth(HttpRequest::put(url.clone())).body(cp(7, 2_000)));
    assert_eq!(status, 204, "{body}");
    // An older capture that arrives later is accepted but does not replace the newer one.
    let (status, body) = ps.raw(auth(HttpRequest::put(url.clone())).body(cp(7, 1_000)));
    assert_eq!(status, 204, "{body}");
    // Not a checkpoint / another room's checkpoint: refused.
    let (status, _) = ps.raw(auth(HttpRequest::put(url.clone())).body(vec![1, 2, 3]));
    assert_eq!(status, 400);
    let (status, _) = ps.raw(auth(HttpRequest::put(url.clone())).body(cp(8, 3_000)));
    assert_eq!(status, 400);
    let res = Http::new()
        .request(auth(HttpRequest::get(url)))
        .block()
        .unwrap()
        .unwrap();
    assert_eq!(res.status, 200);
    let stored = space3d_server::gs::checkpoint::Checkpoint::from_bytes(&res.body).unwrap();
    assert_eq!(stored.saved_ms, 2_000, "the newest capture is kept");
}

/// The per-IP request limit covers unauthenticated internal requests; game servers holding the
/// fleet token are not throttled by it (they upload a checkpoint per room and interval).
#[test]
fn internal_routes_are_rate_limited_per_ip() {
    let ps = Ps::start(
        |c| c.player_service.http_rps_per_ip = 5,
        space3d_db::Db::open_in_memory(),
    );
    let url = format!("{}/internal/v1/checkpoints", ps.url);
    let statuses: Vec<u16> = (0..20).map(|_| ps.raw(HttpRequest::get(url.clone())).0).collect();
    assert!(statuses.contains(&401), "{statuses:?}");
    assert!(statuses.contains(&429), "{statuses:?}");
    let fleet: Vec<u16> = (0..40)
        .map(|_| {
            ps.raw(HttpRequest::get(url.clone()).header("authorization", "Bearer fleet"))
                .0
        })
        .collect();
    assert!(fleet.iter().all(|s| *s == 200), "{fleet:?}");
}

/// The fleet WebSocket authenticates the upgrade (no socket without the fleet token) and closes
/// an authenticated connection that never registers.
#[test]
fn fleet_socket_requires_the_token_and_a_prompt_register() {
    use space3d_net::{WsClient, WsConfig, WsState};
    let mut ps = Ps::start(|_| {}, space3d_db::Db::open_in_memory());
    let url = format!("{}/internal/v1/fleet", ps.url.replacen("http://", "ws://", 1));
    let wait = |ps: &mut Ps, conn: &space3d_net::WsConnection, want: fn(&WsState) -> bool| {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < end {
            ps.app.update(Duration::from_millis(10));
            if want(&conn.state()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    };
    let anonymous = WsClient::connect(url.clone());
    assert!(
        wait(&mut ps, &anonymous, |s| matches!(s, WsState::Closed(_))),
        "no socket without the token"
    );
    let wrong = WsClient::connect_with(url.clone(), WsConfig::default().header("authorization", "Bearer nope"));
    assert!(wait(&mut ps, &wrong, |s| matches!(s, WsState::Closed(_))));
    let silent = WsClient::connect_with(url, WsConfig::default().header("authorization", "Bearer fleet"));
    assert!(wait(&mut ps, &silent, |s| *s == WsState::Open), "token accepted");
    ps.clock.advance_ms(11_000);
    assert!(
        wait(&mut ps, &silent, |s| matches!(s, WsState::Closed(_))),
        "unregistered connection closed after the deadline"
    );
}

/// `space3d_sessions_active` counts live refresh families; `space3d_db_queue_depth` is exported.
#[test]
fn session_and_db_queue_metrics_are_exported() {
    let mut ps = Ps::start(|_| {}, space3d_db::Db::open_in_memory());
    for name in ["a", "b"] {
        ok(ps.client().login_guest(&DeviceCredentials::generate(), Some(name)));
    }
    let metrics = ps.app.resources.expect::<space3d_server::metrics::Metrics>().clone();
    let end = std::time::Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    while std::time::Instant::now() < end {
        ps.clock.advance_ms(1_000);
        ps.app.update(Duration::from_millis(100));
        text = metrics.render();
        if text.contains("space3d_sessions_active 2") {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(text.contains("space3d_sessions_active 2"), "{text}");
    assert!(text.contains("space3d_db_queue_depth "), "{text}");
}
