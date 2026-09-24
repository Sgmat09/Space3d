//! Stock `space3d-server` binary (no game code):
//!
//! ```text
//! space3d-server [--config server.toml]              run with role = "player-service" (NoGame)
//! space3d-server keygen [--kid k1]                   print a new base64 32-byte key (`kid:<b64>`)
//! space3d-server check-config --config server.toml   validate + print the redacted effective config
//! space3d-server print-default-config                commented example config
//! space3d-server healthcheck [--ready] [--url http://127.0.0.1:9090]   probe for distroless images
//! ```
//!
//! Game servers are game-specific binaries built on `space3d_server::ServerMain`.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    todo!("clap subcommands; default: ServerMain::from_env(NoGame) with role forced to player-service")
}

#[cfg(target_arch = "wasm32")]
fn main() {
    eprintln!("space3d-server is not supported on wasm32");
}
