//! The shipped binaries as separate processes over real localhost sockets: the `server`
//! (all-in-one: player service + game server) and two `client --headless --bot` processes that
//! log in as guests, get join tickets through quick play and play in the same room. Then
//! SIGTERM drains the server (exit code 0).
//!
//! Real time and real processes on purpose (this is what a developer runs); every wait has a
//! deadline and the whole test takes about eight seconds.
#![cfg(all(unix, feature = "client"))]

mod common;

use common::{Proc, Server, wait_exit};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn server_and_two_headless_bot_clients_over_localhost() {
    let server = Server::start(&[]);
    let port = server.port;
    let bots: Vec<Proc> = ["bot-1", "bot-2"]
        .iter()
        .map(|name| {
            Proc(Some(
                Command::new(env!("CARGO_BIN_EXE_client"))
                    .args(["--headless", "--bot", "--seconds", "4", "--name", name])
                    .args(["--ps", &format!("http://127.0.0.1:{port}")])
                    .env("RUST_LOG", "warn")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("spawn client"),
            ))
        })
        .collect();
    for mut bot in bots {
        let child = bot.0.as_mut().unwrap();
        let status = wait_exit(child, Duration::from_secs(20)).expect("client exits on its own");
        let mut out = String::new();
        let mut err = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "client failed ({status}):\n{out}\n{err}");
        assert!(out.contains("state=Connected"), "{out}\n{err}");
        assert!(out.contains("cubes=2"), "both bots see both cubes: {out}\n{err}");
        assert!(!err.contains("ERROR") && !err.contains("panicked"), "{err}");
        bot.0.take();
    }

    // Graceful shutdown: SIGTERM → drain (3 s in the dev config) → exit 0.
    let (status, err) = server.stop();
    assert!(status.success(), "server exit {status}:\n{err}");
    assert!(!err.contains("ERROR") && !err.contains("panicked"), "{err}");
}
