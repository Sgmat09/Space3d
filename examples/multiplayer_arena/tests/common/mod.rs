//! Helpers for the process-level tests: spawn the example's `server` binary on free
//! loopback ports and wait for it.
#![allow(dead_code)]

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills the child on drop (a failing assertion must not leak a server).
pub struct Proc(pub Option<Child>);

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub fn ready(admin: u16) -> bool {
    use std::io::Write;
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", admin)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = s.write_all(b"GET /readyz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.starts_with("HTTP/1.1 200")
}

pub fn wait_exit(child: &mut Child, deadline: Duration) -> Option<std::process::ExitStatus> {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// A running `server` (all-in-one dev config) on free ports, data in a temp dir.
pub struct Server {
    pub proc: Proc,
    pub port: u16,
    pub admin: u16,
    _dir: tempfile::TempDir,
}

impl Server {
    /// Start with extra `SPACE3D__*` overrides and wait for `/readyz`.
    pub fn start(extra_env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (port, admin) = (free_port(), free_port());
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_server"));
        cmd.env("SPACE3D__GAME__BIND", format!("127.0.0.1:{port}"))
            .env("SPACE3D__PLAYER_SERVICE__BIND", format!("127.0.0.1:{port}"))
            .env("SPACE3D__GAME__PUBLIC_URL", format!("ws://127.0.0.1:{port}/play"))
            .env("SPACE3D__ADMIN__BIND", format!("127.0.0.1:{admin}"))
            .env("SPACE3D__PERSISTENCE__DATA_DIR", dir.path())
            .env("SPACE3D__PLAYER_SERVICE__DB_PATH", dir.path().join("players.db"))
            .env("SPACE3D__LOG__FILTER", "warn")
            .env_remove("SPACE3D_CONFIG")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let proc = Proc(Some(cmd.spawn().expect("spawn server")));
        let end = Instant::now() + Duration::from_secs(10);
        while !ready(admin) {
            assert!(Instant::now() < end, "server did not become ready");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            proc,
            port,
            admin,
            _dir: dir,
        }
    }

    /// SIGTERM, wait for the drain; returns (exit status, stderr).
    pub fn stop(mut self) -> (std::process::ExitStatus, String) {
        let child = self.proc.0.as_mut().unwrap();
        let pid = child.id().to_string();
        assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
        let status = wait_exit(child, Duration::from_secs(20)).expect("server drains and exits");
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        self.proc.0.take();
        (status, err)
    }
}
