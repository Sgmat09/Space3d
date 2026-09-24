//! `server`: the multiplayer_arena dedicated server — headless and authoritative.
//!
//! With no arguments it runs the built-in dev config (`examples/multiplayer_arena/server.toml`):
//! player service + game server in one process on `127.0.0.1:7777`, admin API and metrics on
//! `127.0.0.1:9090`. Everything can be overridden:
//!
//! ```text
//! server                                         # all-in-one dev server
//! server --config prod.toml                      # file layered over the built-in defaults
//! SPACE3D__GAME__BIND=0.0.0.0:7777 server        # env overlay (SECTION__KEY)
//! server --role game-server                      # next to a stock `space3d-server` player service
//! server --check-config | --print-default-config
//! server healthcheck [--ready]                   # container probe (exit 0 = healthy)
//! ```
//!
//! SIGTERM / Ctrl-C drains: clients get a notice, rooms checkpoint, then everyone is
//! disconnected with `ServerShutdown` and the process exits 0.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    use multiplayer_arena::server::{CubesGame, DEV_CONFIG};
    match space3d_server::ServerMain::from_env_with_defaults(CubesGame, DEV_CONFIG) {
        Ok(main) => main.run(),
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
