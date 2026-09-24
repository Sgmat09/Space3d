//! Library behind the `space3d` command-line tool.
//!
//! The binary (`src/main.rs`) only parses [`args::Cli`] and calls [`run`]; everything else
//! lives here so it can be unit tested:
//!
//! | module | what |
//! |---|---|
//! | [`args`] | clap definitions of every subcommand |
//! | [`template`] | embedded project templates, rendering, `space3d new` scaffolding, CI workflow |
//! | [`project`] | `cargo metadata` / `Cargo.lock` reading: packages, binaries, locked versions |
//! | [`tools`] | toolchain / SDK detection (rustc, targets, wasm-bindgen, cargo-apk, NDK, Xcode, gh) |
//! | [`process`] | command echo, dry-run, probes |
//! | [`commands`] | one function per subcommand |
//!
//! ```text
//! space3d new my_game --template 3d      # scaffold (git dependency on Space3d)
//! cd my_game
//! space3d run                            # desktop
//! space3d serve                          # web build + local server with COOP/COEP
//! space3d test                           # tests + golden screenshots + HTML report
//! space3d stories --headless --out shots # storybook gallery
//! space3d build --target android         # cargo apk (prints setup steps if the NDK is missing)
//! space3d publish --tag v0.1.0 --dry-run # GitHub release with desktop + web builds
//! space3d doctor                         # what is missing, and how to fix it
//! ```

#![warn(missing_docs)]

pub mod args;
pub mod commands;
pub mod process;
pub mod project;
pub mod template;
pub mod tools;

use anyhow::Result;
use args::{CiCommand, Cli, Command};
use process::Exec;
use std::path::PathBuf;

/// Shared state for a command invocation.
#[derive(Clone, Debug)]
pub struct Ctx {
    /// Directory the command operates in (`-C`, else the current directory).
    pub cwd: PathBuf,
    /// Command runner (dry-run / verbose).
    pub exec: Exec,
}

impl Ctx {
    /// Context from the global flags.
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let cwd = match &cli.project {
            Some(dir) if dir.is_absolute() => dir.clone(),
            Some(dir) => std::env::current_dir()?.join(dir),
            None => std::env::current_dir()?,
        };
        Ok(Self {
            cwd,
            exec: Exec {
                dry_run: cli.dry_run,
                verbose: cli.verbose,
            },
        })
    }
}

/// Run a parsed command line. Returns the process exit code.
pub fn run(cli: Cli) -> Result<i32> {
    let ctx = Ctx::from_cli(&cli)?;
    match cli.command {
        Command::New(a) => commands::new::run(&ctx, &a).map(|()| 0),
        Command::Run(a) => commands::run::run(&ctx, &a),
        Command::Build(a) => commands::build::run(&ctx, &a).map(|_| 0),
        Command::Serve(a) => commands::serve::run(&ctx, &a).map(|()| 0),
        Command::Test(a) => commands::test::run(&ctx, &a),
        Command::Stories(a) => commands::run::stories(&ctx, &a),
        Command::Editor(a) => commands::run::editor(&ctx, &a),
        Command::Publish(a) => commands::publish::run(&ctx, &a).map(|()| 0),
        Command::Ci(c) => match c.command {
            CiCommand::Init(a) => commands::ci::init(&ctx, &a).map(|()| 0),
        },
        Command::Doctor(a) => commands::doctor::run(&ctx, &a),
        Command::Bench(a) => commands::run::bench(&ctx, &a),
    }
}
