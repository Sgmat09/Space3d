//! `space3d` — the Space3d command-line tool. See the `space3d_cli` library docs.

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = space3d_cli::args::Cli::parse();
    match space3d_cli::run(cli) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
