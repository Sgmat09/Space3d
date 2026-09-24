//! `space3d test`: `cargo test` with the space3d-test environment switches, then a summary of
//! the screenshot report (`SPACE3D_TEST_REPORT`).

use crate::Ctx;
use crate::args::TestArgs;
use crate::process::Cmd;
use crate::project::Project;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Environment variable read by `space3d-test` to re-bless goldens.
pub const ENV_UPDATE: &str = "SPACE3D_UPDATE_SNAPSHOTS";
/// Environment variable read by `space3d-test` for the report directory.
pub const ENV_REPORT: &str = "SPACE3D_TEST_REPORT";

/// Run the tests; returns cargo's exit code.
pub fn run(ctx: &Ctx, args: &TestArgs) -> Result<i32> {
    let project = Project::load(&ctx.cwd, args.cargo.package.as_deref())?;
    let report = args
        .report
        .as_ref()
        .map(|r| if r.is_absolute() { r.clone() } else { ctx.cwd.join(r) })
        .unwrap_or_else(|| project.target_dir.join("space3d-report"));
    // The harness appends to results.jsonl; start every run with a fresh report.
    prepare_report_dir(&report, ctx.exec.dry_run)?;
    let cmd = test_cmd(ctx, &project, args, &report);
    let code = ctx.exec.status(&cmd)?;
    if ctx.exec.dry_run {
        return Ok(code);
    }
    match read_report(&report) {
        Some(entries) => print!("{}", summarize(&entries, &report)),
        None => eprintln!(
            "(no screenshot/story checks reported — nothing in {})",
            report.display()
        ),
    }
    Ok(code)
}

/// Files and directories a `space3d-test` report consists of (see `space3d_test::report`).
pub const REPORT_FILES: [&str; 4] = ["results.jsonl", "report.json", "index.html", "images"];

/// Make `dir` ready for a fresh report: a missing or empty directory is fine; a previous
/// report has only its own files ([`REPORT_FILES`]) removed; anything else is refused, so
/// `--report .` or `--report docs` can never delete unrelated files. With `dry_run` nothing is
/// removed (the checks still run).
pub fn prepare_report_dir(dir: &Path, dry_run: bool) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries =
        std::fs::read_dir(dir).with_context(|| format!("--report {} exists and is not a directory", dir.display()))?;
    if entries.next().is_none() {
        return Ok(());
    }
    let is_report = ["results.jsonl", "report.json"].iter().any(|f| dir.join(f).is_file());
    if !is_report {
        bail!(
            "--report {} is not empty and does not hold a previous space3d test report \
             (no results.jsonl/report.json); pick a new or empty directory",
            dir.display()
        );
    }
    if dry_run {
        eprintln!("[dry-run] would clear the previous report in {}", dir.display());
        return Ok(());
    }
    for name in REPORT_FILES {
        let path = dir.join(name);
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else if path.exists() {
            std::fs::remove_file(&path)
        } else {
            continue;
        };
        result.with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// The `cargo test` command line.
pub fn test_cmd(ctx: &Ctx, project: &Project, args: &TestArgs, report: &Path) -> Cmd {
    let mut cmd = Cmd::cargo("test").cwd(&ctx.cwd).env(ENV_REPORT, report);
    if args.update_snapshots {
        cmd = cmd.env(ENV_UPDATE, "1");
    }
    cmd = cmd.arg_if(args.cargo.release, "--release");
    cmd = if args.workspace {
        cmd.arg("--workspace")
    } else {
        cmd.args(["-p", args.cargo.package.as_deref().unwrap_or(&project.package().name)])
    };
    cmd = cmd
        .opt("--features", args.cargo.features.as_deref())
        .opt("--bin", args.cargo.bin.as_deref());
    if !args.args.is_empty() {
        cmd = cmd.arg("--").args(&args.args);
    }
    cmd
}

/// Images stored with a report entry (paths relative to the report directory).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EntryImages {
    /// Golden image.
    pub expected: Option<String>,
    /// Rendered image.
    pub actual: Option<String>,
    /// Diff image.
    pub diff: Option<String>,
}

/// The subset of a `space3d-test` report entry the CLI reads.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ReportEntry {
    /// Crate.
    pub suite: String,
    /// Check name.
    pub name: String,
    /// `screenshot`, `story`, `determinism`, `performance`, `custom`.
    pub kind: String,
    /// `passed`, `failed`, `created`, `updated`, `skipped`.
    pub status: String,
    /// Failure message.
    #[serde(default)]
    pub message: Option<String>,
    /// Images.
    #[serde(default)]
    pub images: EntryImages,
}

/// Read `report.json` (latest result per check), falling back to `results.jsonl`.
pub fn read_report(dir: &Path) -> Option<Vec<ReportEntry>> {
    if let Ok(text) = std::fs::read_to_string(dir.join("report.json"))
        && let Ok(entries) = serde_json::from_str::<Vec<ReportEntry>>(&text)
    {
        return Some(entries);
    }
    let text = std::fs::read_to_string(dir.join("results.jsonl")).ok()?;
    let mut latest: BTreeMap<(String, String, String), ReportEntry> = BTreeMap::new();
    for entry in text.lines().filter_map(|l| serde_json::from_str::<ReportEntry>(l).ok()) {
        latest.insert((entry.suite.clone(), entry.kind.clone(), entry.name.clone()), entry);
    }
    Some(latest.into_values().collect())
}

/// Human summary: counts per status and every failure with its diff image.
pub fn summarize(entries: &[ReportEntry], dir: &Path) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for e in entries {
        *counts.entry(e.status.as_str()).or_default() += 1;
    }
    let counts: Vec<String> = counts.iter().map(|(s, n)| format!("{n} {s}")).collect();
    let mut out = format!("\nvisual checks: {}\n", counts.join(", "));
    let failed: Vec<&ReportEntry> = entries.iter().filter(|e| e.status == "failed").collect();
    for e in &failed {
        out.push_str(&format!("  FAILED {} {}::{}", e.kind, e.suite, e.name));
        if let Some(m) = &e.message {
            out.push_str(&format!(" — {}", m.lines().next().unwrap_or_default()));
        }
        out.push('\n');
        for (label, path) in [("diff", &e.images.diff), ("actual", &e.images.actual)] {
            if let Some(p) = path {
                out.push_str(&format!("         {label}: {}\n", dir.join(p).display()));
            }
        }
    }
    let created = entries.iter().filter(|e| e.status == "created").count();
    if created > 0 {
        out.push_str(&format!(
            "  {created} new golden image(s) written — review and commit tests/__screenshots__/\n"
        ));
    }
    if !failed.is_empty() {
        out.push_str("  if the new output is correct, re-bless with: space3d test --update-snapshots\n");
    }
    out.push_str(&format!("report: {}\n", report_html(dir).display()));
    out
}

/// The HTML report page.
pub fn report_html(dir: &Path) -> PathBuf {
    dir.join("index.html")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::CargoSel;
    use crate::process::Exec;

    fn entry(name: &str, status: &str) -> String {
        serde_json::json!({
            "suite": "demo", "name": name, "kind": "screenshot", "status": status,
            "message": if status == "failed" { serde_json::json!("3.2% of pixels differ\nmore") } else { serde_json::Value::Null },
            "details": {}, "images": {"expected": "images/demo/a.expected.png", "actual": "images/demo/a.actual.png", "diff": "images/demo/a.diff.png"},
            "timestamp_ms": 1
        })
        .to_string()
    }

    #[test]
    fn reads_jsonl_latest_per_check_and_summarizes() {
        let dir = tempfile::tempdir().unwrap();
        let lines = [
            entry("menu", "failed"),
            entry("menu", "passed"),
            entry("level", "failed"),
            entry("hud", "created"),
        ];
        std::fs::write(dir.path().join("results.jsonl"), lines.join("\n")).unwrap();
        let entries = read_report(dir.path()).unwrap();
        assert_eq!(entries.len(), 3);
        let text = summarize(&entries, dir.path());
        assert!(text.contains("visual checks: 1 created, 1 failed, 1 passed"), "{text}");
        assert!(
            text.contains("FAILED screenshot demo::level — 3.2% of pixels differ\n"),
            "{text}"
        );
        assert!(text.contains("a.diff.png"));
        assert!(text.contains("--update-snapshots"));
        assert!(text.contains("new golden image(s)"));
        assert!(text.contains("index.html"));
    }

    #[test]
    fn prefers_report_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.json"), format!("[{}]", entry("only", "passed"))).unwrap();
        std::fs::write(dir.path().join("results.jsonl"), entry("other", "failed")).unwrap();
        let entries = read_report(dir.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "only");
        assert!(read_report(&dir.path().join("nope")).is_none());
    }

    #[test]
    fn report_dir_preparation_never_deletes_foreign_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Missing and empty directories are fine.
        prepare_report_dir(&tmp.path().join("missing"), false).unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        prepare_report_dir(&empty, false).unwrap();
        assert!(empty.is_dir());

        // A directory with other content is refused and left alone (also in dry-run mode).
        let docs = tmp.path().join("docs");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("keep.txt"), "precious").unwrap();
        std::fs::write(docs.join("index.html"), "mine").unwrap();
        for dry_run in [true, false] {
            let err = prepare_report_dir(&docs, dry_run).unwrap_err().to_string();
            assert!(err.contains("pick a new or empty directory"), "{err}");
        }
        assert_eq!(std::fs::read_to_string(docs.join("keep.txt")).unwrap(), "precious");
        assert_eq!(std::fs::read_to_string(docs.join("index.html")).unwrap(), "mine");

        // A previous report loses only the report's own files.
        let report = tmp.path().join("report");
        std::fs::create_dir_all(report.join("images/demo")).unwrap();
        std::fs::write(report.join("images/demo/a.png"), [1]).unwrap();
        for f in ["results.jsonl", "report.json", "index.html"] {
            std::fs::write(report.join(f), "x").unwrap();
        }
        std::fs::write(report.join("notes.txt"), "stray").unwrap();
        prepare_report_dir(&report, true).unwrap();
        assert!(report.join("results.jsonl").is_file(), "dry-run removes nothing");
        prepare_report_dir(&report, false).unwrap();
        for f in REPORT_FILES {
            assert!(!report.join(f).exists(), "{f} should be removed");
        }
        assert_eq!(std::fs::read_to_string(report.join("notes.txt")).unwrap(), "stray");

        // A file is not a report directory.
        let file = tmp.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(prepare_report_dir(&file, false).is_err());
    }

    #[test]
    fn command_sets_env() {
        let json = serde_json::json!({
            "packages": [{"name": "demo", "version": "0.1.0", "manifest_path": "/g/Cargo.toml", "targets": [], "metadata": null}],
            "workspace_root": "/g", "target_directory": "/g/target"
        });
        let project = Project::from_metadata_json(&json.to_string(), Path::new("/g"), None).unwrap();
        let ctx = Ctx {
            cwd: "/g".into(),
            exec: Exec::default(),
        };
        let args = TestArgs {
            update_snapshots: true,
            report: None,
            cargo: CargoSel::default(),
            workspace: false,
            args: vec!["screens".into()],
        };
        let shown = test_cmd(&ctx, &project, &args, Path::new("/g/target/space3d-report")).display();
        assert!(
            shown.starts_with("SPACE3D_TEST_REPORT=/g/target/space3d-report SPACE3D_UPDATE_SNAPSHOTS=1 "),
            "{shown}"
        );
        assert!(shown.ends_with("test -p demo -- screens"), "{shown}");
    }
}
