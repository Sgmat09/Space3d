//! `arena-server`: the arena's dedicated game server binary.
//!
//! ```text
//! arena-server --config server.toml                  # role from the config (all-in-one by default)
//! arena-server --role game-server                    # next to a stock `space3d-server` player service
//! arena-server --check-config | --print-default-config
//! arena-server healthcheck [--ready]                 # container probe
//! ```

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    match space3d_server::ServerMain::from_env(arena_server::ArenaGame) {
        Ok(main) => main.run(),
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
