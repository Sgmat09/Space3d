//! Spawning external tools (cargo, wasm-bindgen, xcodebuild, ...) with echo and dry-run support.

use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Stdio;

/// A command line to run: program, arguments, working directory and extra environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cmd {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    envs: Vec<(OsString, OsString)>,
}

impl Cmd {
    /// New command for `program`.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            ..Default::default()
        }
    }

    /// `cargo <subcommand>` (honours `$CARGO` when running under cargo).
    pub fn cargo(subcommand: &str) -> Self {
        Self::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())).arg(subcommand)
    }

    /// Append one argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args.extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// Append `flag` when `cond` holds.
    pub fn arg_if(self, cond: bool, flag: impl AsRef<OsStr>) -> Self {
        if cond { self.arg(flag) } else { self }
    }

    /// Append `flag value` when `value` is `Some`.
    pub fn opt(self, flag: &str, value: Option<impl AsRef<OsStr>>) -> Self {
        match value {
            Some(v) => self.arg(flag).arg(v),
            None => self,
        }
    }

    /// Working directory.
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Extra environment variable.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs.push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    /// The program.
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// The arguments.
    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }

    /// Extra environment variables.
    pub fn get_envs(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.envs.iter().map(|(k, v)| (k.as_os_str(), v.as_os_str()))
    }

    /// Shell-like rendering, e.g. `SPACE3D_HEADLESS=1 cargo run --release`.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = self
            .envs
            .iter()
            .map(|(k, v)| format!("{}={}", k.to_string_lossy(), quote(&v.to_string_lossy())))
            .collect();
        // `$CARGO` is an absolute path when running under cargo; show it as plain `cargo`.
        let program = if std::env::var_os("CARGO").is_some_and(|c| c == self.program) {
            "cargo".to_owned()
        } else {
            self.program.to_string_lossy().into_owned()
        };
        parts.push(quote(&program));
        parts.extend(self.args.iter().map(|a| quote(&a.to_string_lossy())));
        parts.join(" ")
    }

    fn command(&self) -> std::process::Command {
        let mut c = std::process::Command::new(&self.program);
        c.args(&self.args);
        if let Some(dir) = &self.cwd {
            c.current_dir(dir);
        }
        for (k, v) in &self.envs {
            c.env(k, v);
        }
        c
    }
}

fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Runs commands, echoing them first; with `dry_run` only echoes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Exec {
    /// Print instead of running (side-effecting commands only).
    pub dry_run: bool,
    /// Echo read-only probes too.
    pub verbose: bool,
}

impl Exec {
    /// Run `cmd` with inherited stdio; error on a non-zero exit.
    pub fn run(&self, cmd: &Cmd) -> Result<()> {
        match self.status(cmd)? {
            0 => Ok(()),
            code => bail!("`{}` failed (exit code {code})", cmd.display()),
        }
    }

    /// Run `cmd` with inherited stdio and return its exit code (0 in dry-run mode).
    pub fn status(&self, cmd: &Cmd) -> Result<i32> {
        eprintln!("{} {}", if self.dry_run { "[dry-run] $" } else { "$" }, cmd.display());
        if self.dry_run {
            return Ok(0);
        }
        let status = cmd
            .command()
            .status()
            .with_context(|| format!("could not start `{}`", cmd.program.to_string_lossy()))?;
        Ok(status.code().unwrap_or(1))
    }

    /// Run a read-only probe and capture stdout (+ stderr when stdout is empty). Always executes,
    /// even in dry-run mode. `None` when the program is missing or exits non-zero.
    pub fn probe(&self, cmd: &Cmd) -> Option<String> {
        if self.verbose {
            eprintln!("  probe: {}", cmd.display());
        }
        probe(cmd)
    }
}

/// See [`Exec::probe`].
pub fn probe(cmd: &Cmd) -> Option<String> {
    let out = cmd
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Some(if stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).trim().to_owned()
    } else {
        stdout
    })
}

/// Find `program` on `PATH` (and `~/.cargo/bin`), like `which`.
pub fn which(program: &str) -> Option<PathBuf> {
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|e| e.to_ascii_lowercase())
            .chain([String::new()])
            .collect()
    } else {
        vec![String::new()]
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".cargo").join("bin"));
    }
    dirs.iter().find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{program}{ext}")))
            .find(|p| p.is_file())
    })
}

/// The user's home directory.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_and_envs() {
        let cmd = Cmd::new("wasm-bindgen")
            .arg("--out-dir")
            .arg("my dist")
            .env("RUST_LOG", "info")
            .arg_if(true, "--no-typescript")
            .arg_if(false, "--debug")
            .opt("--out-name", Some("game"))
            .opt("--x", None::<&str>);
        assert_eq!(
            cmd.display(),
            "RUST_LOG=info wasm-bindgen --out-dir 'my dist' --no-typescript --out-name game"
        );
    }

    #[test]
    fn dry_run_does_not_execute() {
        let exec = Exec {
            dry_run: true,
            verbose: false,
        };
        // Would fail if executed.
        exec.run(&Cmd::new("definitely-not-a-real-program-xyz")).unwrap();
    }

    #[test]
    fn probe_missing_program_is_none() {
        assert!(probe(&Cmd::new("definitely-not-a-real-program-xyz")).is_none());
        assert!(which("definitely-not-a-real-program-xyz").is_none());
    }

    #[test]
    fn probe_and_status_real_program() {
        let cargo = Cmd::cargo("--version");
        assert!(probe(&cargo).unwrap().starts_with("cargo "));
        let exec = Exec::default();
        assert!(exec.run(&Cmd::cargo("definitely-not-a-subcommand")).is_err());
    }
}
