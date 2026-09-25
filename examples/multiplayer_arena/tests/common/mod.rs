//! Helpers for the process-level tests: spawn the example's `server` / `client` binaries on free
//! loopback ports, capture everything they print (all tracing goes to stdout), and wait for
//! them.
#![allow(dead_code)]

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A child process with stdout and stderr captured by reader threads (a chatty child never
/// blocks on a full pipe). Killed on drop (a failing assertion must not leak a process).
pub struct Proc {
    child: Option<Child>,
    output: Arc<Mutex<String>>,
    readers: Vec<std::thread::JoinHandle<()>>,
    name: String,
}

impl Proc {
    /// Spawn `cmd` with piped stdout/stderr.
    pub fn spawn(name: &str, mut cmd: Command) -> Self {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {name}: {e}"));
        let output = Arc::new(Mutex::new(String::new()));
        let mut readers = Vec::new();
        let pipes: [Option<Box<dyn Read + Send>>; 2] = [
            child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
            child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        ];
        for mut pipe in pipes.into_iter().flatten() {
            let output = output.clone();
            readers.push(std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => output.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n])),
                    }
                }
            }));
        }
        Self {
            child: Some(child),
            output,
            readers,
            name: name.to_owned(),
        }
    }

    /// Process id.
    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("running").id()
    }

    /// SIGTERM.
    pub fn term(&self) {
        let pid = self.pid().to_string();
        assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    }

    /// Wait for the exit (then for the output readers); `None` on timeout.
    pub fn wait(&mut self, deadline: Duration) -> Option<ExitStatus> {
        let status = wait_exit(self.child.as_mut().expect("running"), deadline)?;
        self.child.take();
        for r in self.readers.drain(..) {
            let _ = r.join();
        }
        Some(status)
    }

    /// Everything printed so far (stdout and stderr, ANSI colours removed).
    pub fn output(&self) -> String {
        strip_ansi(&self.output.lock().unwrap())
    }

    /// Lines logged at ERROR level, and panics.
    pub fn errors(&self) -> Vec<String> {
        self.output()
            .lines()
            .filter(|l| l.contains(" ERROR ") || l.contains("panicked"))
            .map(str::to_owned)
            .collect()
    }

    /// Lines logged at WARN level.
    pub fn warnings(&self) -> Vec<String> {
        self.output()
            .lines()
            .filter(|l| l.contains(" WARN "))
            .map(str::to_owned)
            .collect()
    }

    /// Panic unless the process logged no ERROR line and did not panic.
    pub fn assert_no_errors(&self) {
        let errors = self.errors();
        assert!(
            errors.is_empty(),
            "{} logged errors:\n{}\n--- full output:\n{}",
            self.name,
            errors.join("\n"),
            self.output()
        );
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // `SPACE3D_PROCESS_TEST_LOGS=<dir>`: keep every process's output for inspection.
        if let Some(dir) = std::env::var_os("SPACE3D_PROCESS_TEST_LOGS") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(format!("{}.log", self.name)), self.output());
        }
    }
}

/// Remove ANSI escape sequences (tracing colours its levels).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ parameters final-byte
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// `GET path` on `127.0.0.1:port`: `(200 OK, JSON body)`.
pub fn http_get(port: u16, path: &str) -> Option<(bool, String)> {
    use std::io::Write;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes());
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let ok = out.starts_with("HTTP/1.1 200");
    let body = out.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    // A chunked body: keep what lies between the first '[' / '{' and the last ']' / '}'.
    let json = match (body.find(['[', '{']), body.rfind([']', '}'])) {
        (Some(a), Some(b)) if b >= a => body[a..=b].to_owned(),
        _ => body.to_owned(),
    };
    Some((ok, json))
}

pub fn ready(admin: u16) -> bool {
    http_get(admin, "/readyz").is_some_and(|(ok, _)| ok)
}

/// Wait until `admin`'s `/readyz` answers 200.
pub fn wait_ready(admin: u16, what: &str) {
    let end = Instant::now() + Duration::from_secs(15);
    while !ready(admin) {
        assert!(Instant::now() < end, "{what} did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn wait_exit(child: &mut Child, deadline: Duration) -> Option<ExitStatus> {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// The example's `server` binary: data in `dir`, warn-level logs, no config file.
pub fn server_cmd(dir: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_server"));
    cmd.env("SPACE3D__PERSISTENCE__DATA_DIR", dir)
        .env("SPACE3D__PLAYER_SERVICE__DB_PATH", dir.join("players.db"))
        .env("SPACE3D__LOG__FILTER", "warn")
        .env_remove("SPACE3D_CONFIG")
        .env_remove("RUST_LOG");
    cmd
}

/// A headless bot client playing `seconds` through the player service at `ps`.
pub fn bot(name: &str, ps: &str, seconds: u32) -> Proc {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_client"));
    cmd.args(["--headless", "--bot", "--seconds", &seconds.to_string(), "--name", name])
        .args(["--ps", ps])
        .env("RUST_LOG", "warn");
    Proc::spawn(name, cmd)
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
        let mut cmd = server_cmd(dir.path());
        cmd.env("SPACE3D__GAME__BIND", format!("127.0.0.1:{port}"))
            .env("SPACE3D__PLAYER_SERVICE__BIND", format!("127.0.0.1:{port}"))
            .env("SPACE3D__GAME__PUBLIC_URL", format!("ws://127.0.0.1:{port}/play"))
            .env("SPACE3D__ADMIN__BIND", format!("127.0.0.1:{admin}"));
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let proc = Proc::spawn("server", cmd);
        wait_ready(admin, "server");
        Self {
            proc,
            port,
            admin,
            _dir: dir,
        }
    }

    /// SIGTERM and wait for the drain; the exit status (output: `self.proc`).
    pub fn stop(&mut self) -> ExitStatus {
        self.proc.term();
        self.proc
            .wait(Duration::from_secs(20))
            .expect("server drains and exits")
    }
}
