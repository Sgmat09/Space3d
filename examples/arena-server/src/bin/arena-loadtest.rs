//! `arena-loadtest`: headless arena bots over real WebSockets with SLO gates (nightly CI).
//!
//! ```text
//! arena-loadtest --url ws://127.0.0.1:7777/play --clients 200 --ramp 20 --duration 60 \
//!                --metrics http://127.0.0.1:9090/metrics --slo-tick-p99-ms 4
//! arena-loadtest --ps http://127.0.0.1:8080 --queue ffa --quick-play --clients 50
//! ```
//!
//! Prints the JSON report; exit code 0 when every SLO held, 1 otherwise, 2 on setup errors.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    use space3d_server::loadtest::{self, LoadTestConfig};
    let mut config = LoadTestConfig::from_args();
    if config.game_id.is_empty() {
        config.game_id = arena_shared::GAME_ID.into();
    }
    let protocol: fn(&mut space3d_core::App) = |app| {
        app.add_plugin(arena_shared::ArenaProtocol);
    };
    match loadtest::run(config, protocol, |i| arena_server::bot::WanderBot::new(i as u64)) {
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
