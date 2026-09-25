//! Load test (ignored by default; real processes, real sockets, real time): the example's
//! `server` binary plus 200 headless bots that log in through the player service and quick
//! play into rooms of 20 (10 rooms). Prints the report (client RTT / bandwidth / snapshot rate,
//! server tick time and bandwidth scraped from `/metrics`) and checks the SLOs.
//!
//! ```text
//! cargo test -p multiplayer_arena --release --test load -- --ignored --nocapture
//! LOAD_CLIENTS=500 LOAD_SECS=60 cargo test -p multiplayer_arena --release --test load -- --ignored --nocapture
//! ```
#![cfg(unix)]

mod common;

use common::Server;
use multiplayer_arena::{CubesProtocol, GAME_ID, MODE, Wander};
use space3d_server::loadtest::{self, LoadTarget, LoadTestConfig, Slo};
use std::time::Duration;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
#[ignore = "load test: 200 clients for ~30 s of real time; run with --ignored (and --release)"]
fn two_hundred_clients_through_the_player_service() {
    let clients: usize = env_or("LOAD_CLIENTS", 200);
    let secs: u64 = env_or("LOAD_SECS", 20);
    let queues = format!(
        "[{{ name = \"{MODE}\", mode = \"{MODE}\", teams = 1, team_size = 20, min_players = 1, backfill = true }}]"
    );
    let mut server = Server::start(&[("SPACE3D__MATCHMAKING__QUEUES", queues.as_str())]);
    let protocol: fn(&mut space3d_core::App) = |app| {
        app.add_plugin(CubesProtocol);
    };
    let report = loadtest::run(
        LoadTestConfig {
            target: LoadTarget::PlayerService {
                url: format!("http://127.0.0.1:{}", server.port),
                queue: MODE.into(),
                quick_play: true,
            },
            clients,
            ramp_per_sec: 50.0,
            duration: Duration::from_secs(secs),
            bots_per_thread: 25,
            tick_hz: 60.0,
            metrics_url: Some(format!("http://127.0.0.1:{}/metrics", server.admin)),
            slo: Slo {
                connect_success: 1.0,
                tick_p99_ms: Some(4.0),
                snapshot_rate: 0.95,
                max_unexpected_disconnects: 0,
            },
            game_id: GAME_ID.into(),
        },
        protocol,
        |i| Wander::new(Wander::seed_of(&format!("bot-{i}"))),
    )
    .expect("load test runs");
    println!("{report:#?}");
    let status = server.stop();
    assert!(status.success(), "server exit {status}:\n{}", server.proc.output());
    server.proc.assert_no_errors();
    assert_eq!(report.connected, clients, "{report:#?}");
    let server_side = report.server.as_ref().expect("metrics scraped");
    // Shard threads catch up to `max_catchup_ticks` late ticks; skipping means a room fell
    // behind. The release build skipped none; allow scheduler noise when the 200 bots share
    // the machine with an unoptimised server.
    assert!(server_side.ticks_skipped * 1000 <= server_side.ticks, "{report:#?}");
    assert!(report.slo_passed, "{report:#?}");
}
