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
mod native {
    use clap::{Parser, Subcommand};
    use space3d_server::{NoGame, Role, ServerConfig, ServerMain};
    use std::path::PathBuf;
    use std::process::ExitCode;

    #[derive(Parser, Debug)]
    #[command(name = "space3d-server", about = "Space3d player service (and server tooling)")]
    struct Cli {
        #[command(subcommand)]
        cmd: Option<Cmd>,
        /// Config file (TOML).
        #[arg(long, env = "SPACE3D_CONFIG", global = true)]
        config: Option<PathBuf>,
    }

    #[derive(Subcommand, Debug)]
    enum Cmd {
        /// Print a new signing key (`kid:<base64 32 bytes>`) for `SPACE3D_KEYS`.
        Keygen {
            /// Key id.
            #[arg(long, default_value = "k1")]
            kid: String,
        },
        /// Validate config + secrets and print the redacted effective config.
        CheckConfig,
        /// Print the commented example config.
        PrintDefaultConfig,
        /// Probe `/healthz` (or `/readyz` with `--ready`); exit 0 when healthy.
        Healthcheck {
            /// Probe readiness instead of liveness.
            #[arg(long)]
            ready: bool,
            /// Admin base URL (default: from the config's `admin.bind`).
            #[arg(long)]
            url: Option<String>,
        },
    }

    fn load(path: Option<&std::path::Path>) -> Result<ServerConfig, String> {
        ServerConfig::load(path, std::env::vars()).map_err(|e| e.to_string())
    }

    pub fn main() -> ExitCode {
        let cli = Cli::parse();
        match cli.cmd {
            Some(Cmd::Keygen { kid }) => {
                println!("{}", space3d_server::security::token::generate_key_spec(&kid));
                ExitCode::SUCCESS
            }
            Some(Cmd::PrintDefaultConfig) => {
                print!("{}", ServerConfig::example_toml());
                ExitCode::SUCCESS
            }
            Some(Cmd::CheckConfig) => match load(cli.config.as_deref()).and_then(|c| {
                space3d_server::security::Secrets::load(&c, &|k| std::env::var(k).ok())
                    .map(|_| c)
                    .map_err(|e| e.to_string())
            }) {
                Ok(c) => {
                    print!("{}", c.redacted_toml());
                    eprintln!("config OK");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("invalid config: {e}");
                    ExitCode::from(2)
                }
            },
            Some(Cmd::Healthcheck { ready, url }) => {
                let url = url.unwrap_or_else(|| {
                    let admin = load(cli.config.as_deref())
                        .map(|c| c.admin.bind)
                        .unwrap_or_else(|_| space3d_server::config::AdminConfig::default().bind);
                    space3d_server::health::local_url(admin)
                });
                if space3d_server::health::probe(&url, ready) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
            None => {
                let mut config = match load(cli.config.as_deref()) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("invalid config: {e}");
                        return ExitCode::from(2);
                    }
                };
                if config.role != Role::PlayerService {
                    eprintln!(
                        "note: the stock binary has no game code; running role = \"player-service\" (was {:?})",
                        config.role
                    );
                    config.role = Role::PlayerService;
                }
                ServerMain::with_config(NoGame, config).run()
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    native::main()
}

#[cfg(target_arch = "wasm32")]
fn main() {
    eprintln!("space3d-server is not supported on wasm32");
}
