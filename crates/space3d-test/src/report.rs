//! Machine- and human-readable test reports.
//!
//! When `SPACE3D_TEST_REPORT=<dir>` is set, screenshot, determinism, performance and storybook
//! checks append one JSON line per result to `<dir>/results.jsonl`, copy their images to
//! `<dir>/images/<suite>/`, and regenerate `<dir>/report.json` (latest result per check) and
//! `<dir>/index.html` (side-by-side expected / actual / diff). Delete the directory to start
//! a fresh report. The same can be done explicitly with [`TestReport::new`].
//!
//! Entries are keyed by (suite, kind, name). Screenshots and stories use their golden name;
//! performance and determinism checks default to `<test path>::<check>`
//! ([`scoped_check_name`]), so the same assertion in different tests yields separate rows and
//! one test's failure is never hidden by another test's later pass. Tests that fail on purpose
//! (a harness' own tests) run their checks inside [`without_report`].

use crate::env;
use crate::image_io::save_png;
use crate::screenshot::sanitize_name;
use serde::{Deserialize, Serialize};
use space3d_core::ImageData;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Kind of check a report entry describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    /// Golden screenshot comparison.
    Screenshot,
    /// Two-run determinism check.
    Determinism,
    /// Frame budget / render stats / allocation assertion.
    Performance,
    /// Storybook story render.
    Story,
    /// Anything else.
    Custom,
}

/// Result status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// Check failed.
    Failed,
    /// Check passed.
    Passed,
    /// A missing golden was written.
    Created,
    /// A golden was re-blessed.
    Updated,
    /// Check skipped (e.g. no GPU).
    Skipped,
}

impl ReportStatus {
    fn as_str(self) -> &'static str {
        match self {
            ReportStatus::Failed => "failed",
            ReportStatus::Passed => "passed",
            ReportStatus::Created => "created",
            ReportStatus::Updated => "updated",
            ReportStatus::Skipped => "skipped",
        }
    }
}

/// Paths (relative to the report directory) of an entry's images.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportImagePaths {
    /// Golden image.
    pub expected: Option<String>,
    /// Produced image.
    pub actual: Option<String>,
    /// Diff visualisation.
    pub diff: Option<String>,
}

/// Images to store with an entry.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReportImages<'a> {
    /// Golden image.
    pub expected: Option<&'a ImageData>,
    /// Produced image.
    pub actual: Option<&'a ImageData>,
    /// Diff visualisation.
    pub diff: Option<&'a ImageData>,
}

/// One result line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportEntry {
    /// Suite (crate) name.
    pub suite: String,
    /// Check name (screenshot name, story id, test name...).
    pub name: String,
    /// Kind of check.
    pub kind: ReportKind,
    /// Outcome.
    pub status: ReportStatus,
    /// Failure message or note.
    #[serde(default)]
    pub message: Option<String>,
    /// Structured details (diff statistics, timings...).
    #[serde(default)]
    pub details: serde_json::Value,
    /// Stored images.
    #[serde(default)]
    pub images: ReportImagePaths,
    /// Milliseconds since the Unix epoch.
    #[serde(default)]
    pub timestamp_ms: u64,
}

impl ReportEntry {
    /// New entry stamped with the current time.
    pub fn new(suite: &str, name: &str, kind: ReportKind, status: ReportStatus) -> Self {
        let timestamp_ms = web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Self {
            suite: suite.to_string(),
            name: name.to_string(),
            kind,
            status,
            message: None,
            details: serde_json::Value::Null,
            images: ReportImagePaths::default(),
            timestamp_ms,
        }
    }

    /// Set the message.
    pub fn message(mut self, message: Option<String>) -> Self {
        self.message = message;
        self
    }

    /// Set the structured details.
    pub fn details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }

    fn key(&self) -> (String, ReportKind, String) {
        (self.suite.clone(), self.kind, self.name.clone())
    }
}

/// A report directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestReport {
    dir: PathBuf,
}

static LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static SUPPRESSED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Run `f` without recording anything into the `SPACE3D_TEST_REPORT` report from this thread
/// ([`TestReport::from_env`] returns `None` inside). For tests that exercise failure paths on
/// purpose (e.g. a harness' own tests), so the report only lists real results.
///
/// ```
/// use space3d_test::report::{TestReport, without_report};
/// assert!(without_report(TestReport::from_env).is_none());
/// ```
pub fn without_report<R>(f: impl FnOnce() -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            SUPPRESSED.with(|s| s.set(s.get().saturating_sub(1)));
        }
    }
    SUPPRESSED.with(|s| s.set(s.get() + 1));
    let _restore = Restore;
    f()
}

/// Recording is suppressed on this thread ([`without_report`]).
pub fn report_suppressed() -> bool {
    SUPPRESSED.with(|s| s.get() > 0)
}

/// Identity of the running test, used to keep report entries of different tests apart.
///
/// libtest runs every test on a thread named after its path (`module::test_name`), so that
/// name is used; elsewhere (the main thread: doctests, `--test-threads=1` on some toolchains,
/// custom harnesses) the caller's `file:line` stands in.
#[track_caller]
pub fn current_test_name() -> String {
    match std::thread::current().name() {
        Some(name) if name != "main" => name.to_string(),
        _ => {
            let at = std::panic::Location::caller();
            let path = Path::new(at.file());
            // Workspace files are relative (`crates/x/tests/y.rs`); absolute ones (doctest
            // bundles in a temp dir, registry crates) are shortened to the file name.
            let file = if path.is_absolute() {
                path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()
            } else {
                path.as_os_str().to_string_lossy()
            };
            format!("{file}:{}", at.line())
        }
    }
}

/// Default report name of a check: `<current test>::<check>` (see [`current_test_name`]).
#[track_caller]
pub fn scoped_check_name(check: &str) -> String {
    format!("{}::{check}", current_test_name())
}

impl TestReport {
    /// Report in `dir` (created on first record).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Report configured by `SPACE3D_TEST_REPORT`, if set (and not suppressed on this thread
    /// with [`without_report`]).
    pub fn from_env() -> Option<Self> {
        if report_suppressed() {
            return None;
        }
        std::env::var_os(env::TEST_REPORT)
            .filter(|v| !v.is_empty())
            .map(|v| Self::new(PathBuf::from(v)))
    }

    /// Report directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the HTML report.
    pub fn html_path(&self) -> PathBuf {
        self.dir.join("index.html")
    }

    /// Store images, append `entry` to `results.jsonl` and regenerate `report.json` and
    /// `index.html`. Safe to call from parallel tests.
    pub fn record(&self, mut entry: ReportEntry, images: ReportImages<'_>) -> std::io::Result<()> {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(&self.dir)?;
        let base = format!(
            "images/{}/{}",
            sanitize_name(&entry.suite),
            sanitize_name(&format!("{:?}-{}", entry.kind, entry.name)).to_ascii_lowercase()
        );
        let store = |img: Option<&ImageData>, which: &str| -> std::io::Result<Option<String>> {
            let Some(img) = img else { return Ok(None) };
            let rel = format!("{base}.{which}.png");
            save_png(self.dir.join(&rel), img).map_err(std::io::Error::other)?;
            Ok(Some(rel))
        };
        entry.images = ReportImagePaths {
            expected: store(images.expected, "expected")?,
            actual: store(images.actual, "actual")?,
            diff: store(images.diff, "diff")?,
        };
        let mut line = serde_json::to_string(&entry).map_err(std::io::Error::other)?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("results.jsonl"))?
            .write_all(line.as_bytes())?;
        self.regenerate()
    }

    /// [`Self::record`], printing a warning instead of failing the test on I/O errors.
    pub fn record_or_warn(&self, entry: ReportEntry, images: ReportImages<'_>) {
        if let Err(e) = self.record(entry, images) {
            eprintln!(
                "space3d-test: could not write test report in {}: {e}",
                self.dir.display()
            );
        }
    }

    /// Latest entry per (suite, kind, name), failures first.
    pub fn entries(&self) -> std::io::Result<Vec<ReportEntry>> {
        let text = match std::fs::read_to_string(self.dir.join("results.jsonl")) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut latest: BTreeMap<(String, ReportKind, String), ReportEntry> = BTreeMap::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(e) = serde_json::from_str::<ReportEntry>(line) {
                latest.insert(e.key(), e);
            }
        }
        let mut v: Vec<ReportEntry> = latest.into_values().collect();
        v.sort_by(|a, b| (a.status, &a.suite, a.kind, &a.name).cmp(&(b.status, &b.suite, b.kind, &b.name)));
        Ok(v)
    }

    fn regenerate(&self) -> std::io::Result<()> {
        let entries = self.entries()?;
        let json = serde_json::to_string_pretty(&entries).map_err(std::io::Error::other)?;
        write_atomic(&self.dir.join("report.json"), json.as_bytes())?;
        write_atomic(&self.html_path(), render_html(&entries).as_bytes())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Escape text for HTML element content and attribute values.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Shared stylesheet for the generated HTML pages (report and storybook gallery).
pub const HTML_STYLE: &str = r#"
:root { --bg:#f6f7f9; --card:#fff; --fg:#1b1f24; --muted:#5b6470; --line:#dde1e6;
  --ok:#1a7f37; --bad:#cf222e; --new:#0969da; --skip:#8c959f; --checker:#e9ecef; }
@media (prefers-color-scheme: dark) { :root { --bg:#0f1115; --card:#171a21; --fg:#e6e8eb;
  --muted:#9aa4b2; --line:#2a2f3a; --ok:#3fb950; --bad:#f85149; --new:#58a6ff; --skip:#6e7681; --checker:#222733; } }
* { box-sizing:border-box; }
body { margin:0; padding:24px 16px 48px; background:var(--bg); color:var(--fg);
  font:14px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif; }
main { max-width:1280px; margin:0 auto; }
h1 { font-size:22px; margin:0 0 4px; } h2 { font-size:17px; margin:28px 0 10px; }
.muted { color:var(--muted); }
.summary { display:flex; flex-wrap:wrap; gap:8px; margin:14px 0 18px; }
.pill { border:1px solid var(--line); background:var(--card); border-radius:999px; padding:4px 12px; cursor:pointer; color:var(--fg); font:inherit; }
.pill[aria-pressed="true"] { border-color:var(--fg); }
.card { background:var(--card); border:1px solid var(--line); border-radius:10px; padding:14px; margin:0 0 14px; }
.head { display:flex; flex-wrap:wrap; gap:8px; align-items:baseline; justify-content:space-between; }
.name { font-weight:600; word-break:break-word; }
.badge { font-size:12px; font-weight:600; border-radius:6px; padding:1px 8px; color:#fff; }
.failed { background:var(--bad); } .passed { background:var(--ok); } .created, .updated { background:var(--new); } .skipped { background:var(--skip); }
.imgs { display:grid; grid-template-columns:repeat(auto-fit,minmax(200px,1fr)); gap:10px; margin-top:10px; }
figure { margin:0; } figcaption { font-size:12px; color:var(--muted); margin-bottom:4px; }
img { display:block; max-width:100%; height:auto; image-rendering:pixelated; border:1px solid var(--line); border-radius:6px;
  background:repeating-conic-gradient(var(--checker) 0 25%, transparent 0 50%) 0 0/16px 16px; }
pre { white-space:pre-wrap; word-break:break-word; background:var(--bg); border:1px solid var(--line); border-radius:6px; padding:8px; margin:10px 0 0; font-size:12px; }
table { border-collapse:collapse; margin-top:8px; font-size:13px; } td, th { border-bottom:1px solid var(--line); padding:3px 10px 3px 0; text-align:left; }
.grid { display:grid; grid-template-columns:repeat(auto-fill,minmax(260px,1fr)); gap:14px; }
nav a { color:var(--new); margin-right:12px; }
"#;

fn render_html(entries: &[ReportEntry]) -> String {
    let mut counts: BTreeMap<ReportStatus, usize> = BTreeMap::new();
    for e in entries {
        *counts.entry(e.status).or_default() += 1;
    }
    let mut h = String::new();
    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>Space3d test report</title><style>");
    h.push_str(HTML_STYLE);
    h.push_str("</style></head><body><main><h1>Space3d test report</h1>");
    h.push_str(&format!(
        "<div class=\"muted\">{} checks · latest result per check</div><div class=\"summary\">",
        entries.len()
    ));
    h.push_str("<button class=\"pill\" data-filter=\"all\" aria-pressed=\"true\">all</button>");
    for (status, n) in &counts {
        h.push_str(&format!(
            "<button class=\"pill\" data-filter=\"{s}\" aria-pressed=\"false\"><span class=\"badge {s}\">{s}</span> {n}</button>",
            s = status.as_str()
        ));
    }
    h.push_str("</div>");
    for e in entries {
        h.push_str(&format!(
            "<section class=\"card\" data-status=\"{s}\"><div class=\"head\"><div><span class=\"name\">{name}</span> \
             <span class=\"muted\">{suite} · {kind:?}</span></div><span class=\"badge {s}\">{s}</span></div>",
            s = e.status.as_str(),
            name = html_escape(&e.name),
            suite = html_escape(&e.suite),
            kind = e.kind,
        ));
        if let Some(m) = &e.message {
            h.push_str(&format!("<pre>{}</pre>", html_escape(m)));
        }
        let imgs = [
            ("expected", &e.images.expected),
            ("actual", &e.images.actual),
            ("diff", &e.images.diff),
        ];
        if imgs.iter().any(|(_, p)| p.is_some()) {
            h.push_str("<div class=\"imgs\">");
            for (label, path) in imgs {
                if let Some(p) = path {
                    h.push_str(&format!(
                        "<figure><figcaption>{label}</figcaption><a href=\"{p}\"><img loading=\"lazy\" src=\"{p}\" alt=\"{label}\"></a></figure>",
                        p = html_escape(p)
                    ));
                }
            }
            h.push_str("</div>");
        }
        if !e.details.is_null() {
            h.push_str(&details_table(&e.details));
        }
        h.push_str("</section>");
    }
    h.push_str(
        "</main><script>document.querySelectorAll('[data-filter]').forEach(b=>b.addEventListener('click',()=>{\
         const f=b.dataset.filter;document.querySelectorAll('[data-filter]').forEach(x=>x.setAttribute('aria-pressed',x===b));\
         document.querySelectorAll('[data-status]').forEach(c=>c.style.display=(f==='all'||c.dataset.status===f)?'':'none');}));\
         </script></body></html>\n",
    );
    h
}

fn details_table(v: &serde_json::Value) -> String {
    let Some(map) = v.as_object() else {
        return format!("<pre>{}</pre>", html_escape(&v.to_string()));
    };
    let mut h = String::from("<table>");
    for (k, v) in map {
        let val = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n
                .as_f64()
                .filter(|f| f.fract() != 0.0)
                .map_or(n.to_string(), |f| format!("{f:.5}")),
            other => other.to_string(),
        };
        h.push_str(&format!(
            "<tr><th>{}</th><td>{}</td></tr>",
            html_escape(k),
            html_escape(&val)
        ));
    }
    h.push_str("</table>");
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_dedupe_and_render() {
        let tmp = tempfile::tempdir().unwrap();
        let r = TestReport::new(tmp.path().join("report"));
        let img = ImageData::filled(2, 2, [255, 0, 0, 255]);
        r.record(
            ReportEntry::new("suite", "a <b>", ReportKind::Screenshot, ReportStatus::Failed)
                .message(Some("boom & bust".into()))
                .details(serde_json::json!({"diff_fraction": 0.25, "n": 3})),
            ReportImages {
                expected: Some(&img),
                actual: Some(&img),
                diff: Some(&img),
            },
        )
        .unwrap();
        r.record(
            ReportEntry::new("suite", "ok", ReportKind::Performance, ReportStatus::Passed),
            ReportImages::default(),
        )
        .unwrap();
        // A later result for the same check replaces the earlier one.
        r.record(
            ReportEntry::new("suite", "ok", ReportKind::Performance, ReportStatus::Skipped),
            ReportImages::default(),
        )
        .unwrap();
        let entries = r.entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, ReportStatus::Failed);
        assert_eq!(entries[1].status, ReportStatus::Skipped);
        let diff = entries[0].images.diff.as_ref().unwrap();
        assert!(r.dir().join(diff).is_file());
        let html = std::fs::read_to_string(r.html_path()).unwrap();
        assert!(html.contains("a &lt;b&gt;") && html.contains("boom &amp; bust"));
        assert!(html.contains(diff.as_str()));
        assert!(html.contains("0.25000"));
        let json: Vec<ReportEntry> =
            serde_json::from_str(&std::fs::read_to_string(r.dir().join("report.json")).unwrap()).unwrap();
        assert_eq!(json, entries);
        assert_eq!(
            std::fs::read_to_string(r.dir().join("results.jsonl"))
                .unwrap()
                .lines()
                .count(),
            3
        );
    }

    #[test]
    fn suppression_is_scoped_and_nested() {
        assert!(!report_suppressed());
        without_report(|| {
            without_report(|| assert!(report_suppressed()));
            assert!(report_suppressed());
            assert!(TestReport::from_env().is_none());
        });
        assert!(!report_suppressed());
        let _ = std::panic::catch_unwind(|| without_report(|| panic!("inside")));
        assert!(!report_suppressed(), "restored after a panic");
    }

    #[test]
    fn check_names_are_scoped_to_the_test() {
        // libtest names this thread after the test.
        if std::thread::current().name() != Some("main") {
            assert_eq!(
                scoped_check_name("frame_budget"),
                "report::tests::check_names_are_scoped_to_the_test::frame_budget"
            );
        }
        let other = std::thread::Builder::new()
            .name("game::other_test".into())
            .spawn(|| scoped_check_name("frame_budget"))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(other, "game::other_test::frame_budget");
        // Unnamed threads fall back to the caller location.
        #[allow(clippy::redundant_closure)] // the closure is the caller whose location is taken
        let (unnamed, line) = (std::thread::spawn(|| current_test_name()).join().unwrap(), line!());
        assert_eq!(unnamed, format!("{}:{line}", file!()));
    }

    #[test]
    fn empty_report_has_no_entries() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(TestReport::new(tmp.path()).entries().unwrap().is_empty());
    }
}
