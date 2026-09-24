//! In-game crash & bug reports filed as GitHub issues.
//!
//! Flow: [`CrashReporter::install_panic_hook`] captures panics (message,
//! location, thread, backtrace, platform info and the latest screenshot set
//! via [`CrashReporter::set_screenshot_png`]) and **stores them locally**
//! (a directory on native, `localStorage` on web). On the next launch the game
//! asks the player; if they opt in, [`CrashReporter::submit_pending`] files one
//! issue per report and deletes the local copies. Screenshots are never
//! uploaded (the issues API cannot take attachments); the issue mentions the
//! local file path instead.
//!
//! Security: filing issues needs a token with `issues:write`. Do not embed a
//! personal token in a shipped game — point [`GithubClient::with_base_url`] at
//! a small proxy you control that holds the token.

use crate::client::GithubClient;
use crate::error::{GithubError, Result};
use crate::models::{Issue, NewIssue, RepoRef};
use serde::{Deserialize, Serialize};
use space3d_tasks::Task;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// GitHub's issue body limit is 65 536 characters; stay well below it.
const MAX_BODY: usize = 60_000;

/// Where the game is running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformInfo {
    /// `std::env::consts::OS` (`"web"` on wasm).
    pub os: String,
    /// `std::env::consts::ARCH`.
    pub arch: String,
    /// `std::env::consts::FAMILY`.
    pub family: String,
    /// `true` for debug builds.
    pub debug_build: bool,
    /// Version of `space3d-github` (tracks the engine version).
    pub engine_version: String,
}

impl PlatformInfo {
    /// Info for the running process.
    pub fn current() -> Self {
        Self {
            os: if cfg!(target_arch = "wasm32") {
                "web".into()
            } else {
                std::env::consts::OS.into()
            },
            arch: std::env::consts::ARCH.into(),
            family: std::env::consts::FAMILY.into(),
            debug_build: cfg!(debug_assertions),
            engine_version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// A captured crash, stored locally until submitted or discarded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReport {
    /// Unique id (also the local file / storage key).
    pub id: String,
    /// Game name.
    pub app_name: String,
    /// Game version.
    pub app_version: String,
    /// Panic message.
    pub message: String,
    /// `file:line:column` of the panic, if known.
    pub location: Option<String>,
    /// Name of the panicking thread.
    pub thread: Option<String>,
    /// Captured backtrace (may say "unsupported" on web).
    pub backtrace: String,
    /// Platform details.
    pub platform: PlatformInfo,
    /// Unix time in milliseconds.
    pub timestamp_ms: i64,
    /// Local path of the screenshot PNG saved with the report (native only).
    pub screenshot_path: Option<String>,
}

fn now_ms() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn new_id(timestamp_ms: i64) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!("crash-{timestamp_ms}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

impl CrashReport {
    /// A report for a non-panic failure (e.g. a fatal error you handled).
    pub fn new(app_name: impl Into<String>, app_version: impl Into<String>, message: impl Into<String>) -> Self {
        let timestamp_ms = now_ms();
        Self {
            id: new_id(timestamp_ms),
            app_name: app_name.into(),
            app_version: app_version.into(),
            message: message.into(),
            location: None,
            thread: std::thread::current().name().map(Into::into),
            backtrace: std::backtrace::Backtrace::force_capture().to_string(),
            platform: PlatformInfo::current(),
            timestamp_ms,
            screenshot_path: None,
        }
    }

    /// Build a report from a panic hook invocation.
    pub fn from_panic(
        info: &std::panic::PanicHookInfo<'_>,
        app_name: impl Into<String>,
        app_version: impl Into<String>,
    ) -> Self {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Box<dyn Any>".into());
        Self {
            location: info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
            ..Self::new(app_name, app_version, message)
        }
    }

    /// The issue this report becomes: `[crash] <app> <version>: <message>` with a
    /// Markdown body (platform table, collapsible backtrace).
    pub fn to_issue(&self, labels: &[String]) -> NewIssue {
        let first_line = self.message.lines().next().unwrap_or("panic");
        let title = format!(
            "[crash] {} {}: {}",
            self.app_name,
            self.app_version,
            truncate(first_line, 80)
        );
        let p = &self.platform;
        let mut body = format!(
            "**{app} {ver}** crashed on `{os}/{arch}` ({build} build, space3d {engine}).\n\n\
             ```text\n{msg}\n```\n\n\
             | | |\n|---|---|\n\
             | Location | `{loc}` |\n\
             | Thread | `{thread}` |\n\
             | Time (unix ms) | {ts} |\n\
             | Report id | `{id}` |\n",
            app = self.app_name,
            ver = self.app_version,
            os = p.os,
            arch = p.arch,
            build = if p.debug_build { "debug" } else { "release" },
            engine = p.engine_version,
            msg = truncate(&self.message, 4_000),
            loc = self.location.as_deref().unwrap_or("unknown"),
            thread = self.thread.as_deref().unwrap_or("unnamed"),
            ts = self.timestamp_ms,
            id = self.id,
        );
        if let Some(path) = &self.screenshot_path {
            body.push_str(&format!(
                "| Screenshot | saved on the player's device at `{path}` (not uploaded) |\n"
            ));
        }
        let budget = MAX_BODY.saturating_sub(body.len() + 200);
        body.push_str(&format!(
            "\n<details><summary>Backtrace</summary>\n\n```text\n{}\n```\n\n</details>\n",
            truncate(&self.backtrace, budget)
        ));
        NewIssue {
            title,
            body: Some(body),
            labels: labels.to_vec(),
            assignees: Vec::new(),
        }
    }
}

/// Settings for [`CrashReporter`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashReporterConfig {
    /// Game name shown in issues.
    pub app_name: String,
    /// Game version shown in issues.
    pub app_version: String,
    /// Repository receiving the issues.
    pub repo: RepoRef,
    /// Native: directory for pending reports. Web: `localStorage` key prefix.
    pub storage_dir: PathBuf,
    /// Labels applied to crash issues (default `["crash"]`).
    pub labels: Vec<String>,
}

impl CrashReporterConfig {
    /// Defaults: reports in `./crash-reports`, label `crash`.
    pub fn new(app_name: impl Into<String>, app_version: impl Into<String>, repo: RepoRef) -> Self {
        Self {
            app_name: app_name.into(),
            app_version: app_version.into(),
            repo,
            storage_dir: PathBuf::from("crash-reports"),
            labels: vec!["crash".into()],
        }
    }

    /// Store pending reports in `dir`.
    pub fn with_storage_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.storage_dir = dir.into();
        self
    }
}

struct Inner {
    config: CrashReporterConfig,
    screenshot: Mutex<Option<Arc<Vec<u8>>>>,
}

/// Captures panics to local storage and files them as issues on opt-in.
/// Cheap to clone; insert it as a resource.
#[derive(Clone)]
pub struct CrashReporter {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CrashReporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrashReporter")
            .field("config", &self.inner.config)
            .finish()
    }
}

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

impl CrashReporter {
    /// Create a reporter (does not install the hook).
    pub fn new(config: CrashReporterConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                screenshot: Mutex::new(None),
            }),
        }
    }

    /// Configuration.
    pub fn config(&self) -> &CrashReporterConfig {
        &self.inner.config
    }

    /// Install a panic hook that saves a [`CrashReport`] and then calls the
    /// previously installed hook (so the usual message still prints).
    pub fn install_panic_hook(&self) {
        let reporter = self.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A panic while saving the report must not recurse.
            if !IN_HOOK.with(|f| f.replace(true)) {
                match reporter.record_panic(info) {
                    Ok(r) => tracing::error!(id = %r.id, "crash report saved"),
                    Err(e) => tracing::error!(error = %e, "failed to save crash report"),
                }
                IN_HOOK.with(|f| f.set(false));
            }
            previous(info);
        }));
    }

    /// Remember the latest frame as PNG; it is saved next to the next crash report.
    /// Call it periodically (e.g. every few seconds) from the renderer.
    pub fn set_screenshot_png(&self, png: Vec<u8>) {
        *self.inner.screenshot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(png));
    }

    /// Build and store a report for a panic (what the installed hook does).
    pub fn record_panic(&self, info: &std::panic::PanicHookInfo<'_>) -> Result<CrashReport> {
        let c = &self.inner.config;
        let mut report = CrashReport::from_panic(info, &c.app_name, &c.app_version);
        self.save(&mut report)?;
        Ok(report)
    }

    /// Store a report locally (adding the latest screenshot on native).
    pub fn save(&self, report: &mut CrashReport) -> Result<()> {
        let screenshot = self.inner.screenshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        storage::save(
            &self.inner.config.storage_dir,
            report,
            screenshot.as_deref().map(|v| &v[..]),
        )
    }

    /// Reports waiting for the player's decision, oldest first.
    pub fn pending(&self) -> Result<Vec<CrashReport>> {
        let mut reports = storage::list(&self.inner.config.storage_dir)?;
        reports.sort_by(|a, b| a.timestamp_ms.cmp(&b.timestamp_ms).then_with(|| a.id.cmp(&b.id)));
        Ok(reports)
    }

    /// Delete one stored report (and its screenshot).
    pub fn discard(&self, id: &str) -> Result<()> {
        storage::remove(&self.inner.config.storage_dir, id)
    }

    /// Delete every stored report; returns how many were removed.
    pub fn discard_all(&self) -> Result<usize> {
        let pending = self.pending()?;
        for r in &pending {
            self.discard(&r.id)?;
        }
        Ok(pending.len())
    }

    /// File every pending report as an issue (call only after the player opted
    /// in). Each successfully filed report is deleted locally; the first failure
    /// stops the run and leaves the remaining reports in place.
    pub async fn submit_pending_async(&self, client: &GithubClient) -> Result<Vec<Issue>> {
        let c = &self.inner.config;
        let mut issues = Vec::new();
        for report in self.pending()? {
            let issue = client.create_issue(&c.repo, &report.to_issue(&c.labels)).await?;
            tracing::info!(id = %report.id, issue = issue.number, "crash report submitted");
            self.discard(&report.id)?;
            issues.push(issue);
        }
        Ok(issues)
    }

    /// [`CrashReporter::submit_pending_async`] as a pollable [`Task`].
    pub fn submit_pending(&self, client: &GithubClient) -> Task<Result<Vec<Issue>>> {
        let (this, client) = (self.clone(), client.clone());
        space3d_tasks::spawn(async move { this.submit_pending_async(&client).await })
    }

    /// A player-written bug report with version and platform details appended.
    pub fn bug_report(&self, title: &str, description: &str) -> NewIssue {
        let c = &self.inner.config;
        let p = PlatformInfo::current();
        NewIssue {
            title: format!("[bug] {} {}: {}", c.app_name, c.app_version, truncate(title, 100)),
            body: Some(format!(
                "{}\n\n---\n`{} {}` on `{}/{}` ({} build, space3d {})\n",
                truncate(description, MAX_BODY - 500),
                c.app_name,
                c.app_version,
                p.os,
                p.arch,
                if p.debug_build { "debug" } else { "release" },
                p.engine_version
            )),
            labels: vec!["bug".into()],
            assignees: Vec::new(),
        }
    }

    /// File [`CrashReporter::bug_report`] as an issue.
    pub fn submit_bug_report(&self, client: &GithubClient, title: &str, description: &str) -> Task<Result<Issue>> {
        client.create_issue_task(self.inner.config.repo.clone(), self.bug_report(title, description))
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod storage {
    use super::*;
    use std::path::Path;

    pub(super) fn save(dir: &Path, report: &mut CrashReport, screenshot: Option<&[u8]>) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        if let Some(png) = screenshot {
            let path = dir.join(format!("{}.png", report.id));
            std::fs::write(&path, png)?;
            report.screenshot_path = Some(path.display().to_string());
        }
        let json = serde_json::to_vec_pretty(report)?;
        // Write-then-rename so a crash mid-write never leaves a torn report.
        let tmp = dir.join(format!("{}.json.tmp", report.id));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, dir.join(format!("{}.json", report.id)))?;
        Ok(())
    }

    pub(super) fn list(dir: &Path) -> Result<Vec<CrashReport>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                match std::fs::read(&path)
                    .map_err(GithubError::from)
                    .and_then(|b| serde_json::from_slice(&b).map_err(GithubError::from))
                {
                    Ok(r) => out.push(r),
                    Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping unreadable crash report"),
                }
            }
        }
        Ok(out)
    }

    pub(super) fn remove(dir: &Path, id: &str) -> Result<()> {
        for ext in ["json", "png"] {
            match std::fs::remove_file(dir.join(format!("{id}.{ext}"))) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mod storage {
    use super::*;
    use std::path::Path;

    fn local() -> Result<web_sys::Storage> {
        web_sys::window()
            .ok_or_else(|| GithubError::Storage("no window".into()))?
            .local_storage()
            .map_err(|e| GithubError::Storage(format!("{e:?}")))?
            .ok_or_else(|| GithubError::Storage("localStorage disabled".into()))
    }

    fn prefix(dir: &Path) -> String {
        format!("space3d-crash:{}:", dir.display())
    }

    pub(super) fn save(dir: &Path, report: &mut CrashReport, _screenshot: Option<&[u8]>) -> Result<()> {
        // Screenshots are not persisted on web (localStorage quota is tiny).
        let json = serde_json::to_string(report)?;
        local()?
            .set_item(&format!("{}{}", prefix(dir), report.id), &json)
            .map_err(|e| GithubError::Storage(format!("{e:?}")))
    }

    pub(super) fn list(dir: &Path) -> Result<Vec<CrashReport>> {
        let s = local()?;
        let prefix = prefix(dir);
        let n = s.length().map_err(|e| GithubError::Storage(format!("{e:?}")))?;
        let mut out = Vec::new();
        for i in 0..n {
            if let Ok(Some(k)) = s.key(i)
                && k.starts_with(&prefix)
                && let Ok(Some(v)) = s.get_item(&k)
                && let Ok(r) = serde_json::from_str(&v)
            {
                out.push(r);
            }
        }
        Ok(out)
    }

    pub(super) fn remove(dir: &Path, id: &str) -> Result<()> {
        local()?
            .remove_item(&format!("{}{id}", prefix(dir)))
            .map_err(|e| GithubError::Storage(format!("{e:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_body_is_bounded_and_informative() {
        let mut r = CrashReport::new("Space Game", "1.2.3", "index out of bounds\nsecond line");
        r.location = Some("src/main.rs:10:5".into());
        r.backtrace = "frame\n".repeat(50_000);
        r.screenshot_path = Some("/tmp/x.png".into());
        let issue = r.to_issue(&["crash".into()]);
        assert_eq!(issue.title, "[crash] Space Game 1.2.3: index out of bounds");
        let body = issue.body.unwrap();
        assert!(body.len() < 65_536, "{}", body.len());
        assert!(body.contains("src/main.rs:10:5"));
        assert!(body.contains("/tmp/x.png"));
        assert!(body.contains("<details>"));
        assert_eq!(issue.labels, ["crash"]);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("héllo", 2), "hé…");
        assert_eq!(truncate("hi", 5), "hi");
    }
}
