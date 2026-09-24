//! `loadtest`: headless multiplayer_arena bots over real WebSockets, with SLO gates.
//!
//! ```text
//! # 200 bots through the player service (quick play spreads them over rooms of 16):
//! loadtest --ps http://127.0.0.1:7777 --queue arena --quick-play --clients 200 --ramp 50 \
//!          --duration 20 --metrics http://127.0.0.1:9090/metrics --slo-tick-p99-ms 4
//! # straight into the dev room (dev auth, ≤ 64 players):
//! loadtest --url ws://127.0.0.1:7777/play --clients 32
//! ```
//!
//! Prints the JSON report (client RTT / bandwidth / snapshot rate, server tick time and
//! bandwidth from `/metrics`); exit code 0 when every SLO held, 1 otherwise, 2 on setup errors.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    use multiplayer_arena::{CubesProtocol, GAME_ID, Wander};
    use space3d_server::loadtest::{self, LoadTestConfig};
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .try_init();
    let mut config = LoadTestConfig::from_args();
    if config.game_id.is_empty() {
        config.game_id = GAME_ID.into();
    }
    let protocol: fn(&mut space3d_core::App) = |app| {
        app.add_plugin(CubesProtocol);
    };
    match loadtest::run(config, protocol, |i| Wander::new(Wander::seed_of(&format!("bot-{i}")))) {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            if report.slo_passed {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("load test failed: {e}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
