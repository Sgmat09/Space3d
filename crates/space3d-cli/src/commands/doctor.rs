//! `space3d doctor`: toolchain and SDK checks with fixes.

use crate::Ctx;
use crate::args::{DoctorArgs, Platform};
use crate::process::{Cmd, which};
use crate::project::Project;
use crate::tools::{self, ANDROID_TARGET, IOS_SIM_TARGET, IOS_TARGET, MIN_RUST, WASM_TARGET};
use anyhow::Result;
use std::path::Path;

/// Outcome of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Fine.
    Ok,
    /// Optional or only needed for some workflows.
    Warn,
    /// Required for the selected platforms and missing.
    Error,
}

/// One line of the report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// Area (`rust`, `web`, `android`, `ios`, `github`).
    pub area: &'static str,
    /// What was checked.
    pub what: String,
    /// Result.
    pub level: Level,
    /// Detected version / state.
    pub detail: String,
    /// How to fix it.
    pub fix: Option<String>,
}

impl Check {
    fn new(area: &'static str, what: impl Into<String>, level: Level, detail: impl Into<String>) -> Self {
        Self {
            area,
            what: what.into(),
            level,
            detail: detail.into(),
            fix: None,
        }
    }
    fn fix(mut self, fix: impl Into<String>) -> Self {
        if self.level != Level::Ok {
            self.fix = Some(fix.into());
        }
        self
    }
}

/// Run every check and print the report. Exit code 1 if a required check failed.
pub fn run(ctx: &Ctx, args: &DoctorArgs) -> Result<i32> {
    let checks = collect(ctx, &args.platforms);
    print!("{}", render(&checks));
    Ok(i32::from(checks.iter().any(|c| c.level == Level::Error)))
}

/// Android: Rust target, cargo-ndk, SDK (+ platform, build-tools, adb), NDK, JDK, Gradle.
fn android_checks(
    exec: &crate::process::Exec,
    target_check: &dyn Fn(&'static str, &str, Level) -> Check,
) -> Vec<Check> {
    use crate::android as a;
    let mut checks = vec![target_check("android", ANDROID_TARGET, Level::Warn)];
    checks.push(match a::cargo_ndk_version(exec) {
        Some(v) => Check::new("android", "cargo-ndk", Level::Ok, v),
        None => Check::new("android", "cargo-ndk", Level::Warn, "not found").fix("cargo install cargo-ndk --locked"),
    });
    let sdk = a::sdk();
    let sdkmanager = |packages: &str| match &sdk {
        Some(s) => format!("sdkmanager --sdk_root={} {packages}", s.dir.display()),
        None => format!("sdkmanager {packages}"),
    };
    checks.push(match &sdk {
        Some(s) => Check::new(
            "android",
            "Android SDK",
            Level::Ok,
            format!("{} ({})", s.dir.display(), s.source),
        ),
        None => Check::new(
            "android",
            "Android SDK",
            Level::Warn,
            "not found (ANDROID_HOME unset, no SDK in the default location)",
        )
        .fix(format!(
            "install the command-line tools{}, then {}",
            if cfg!(target_os = "macos") {
                " (brew install --cask android-commandlinetools)"
            } else {
                " or Android Studio"
            },
            sdkmanager(a::SDK_PACKAGES)
        )),
    });
    if let Some(s) = &sdk {
        checks.push(match a::newest_platform(&s.dir) {
            Some(p) => Check::new("android", "SDK platform", Level::Ok, p),
            None => Check::new(
                "android",
                "SDK platform",
                Level::Warn,
                "none installed (Gradle downloads it when the licenses are accepted)",
            )
            .fix(sdkmanager(&format!("\"platforms;android-{}.0\"", a::COMPILE_SDK))),
        });
        checks.push(match a::newest_version_dir(&s.dir.join("build-tools")) {
            Some((_, v)) => Check::new("android", "SDK build-tools", Level::Ok, v),
            None => Check::new(
                "android",
                "SDK build-tools",
                Level::Warn,
                "none installed (Gradle downloads them when the licenses are accepted)",
            )
            .fix(sdkmanager("\"build-tools;37.0.0\"")),
        });
        let adb = s
            .dir
            .join("platform-tools")
            .join(if cfg!(windows) { "adb.exe" } else { "adb" });
        checks.push(if adb.is_file() {
            Check::new("android", "adb (optional)", Level::Ok, adb.display().to_string())
        } else {
            Check::new(
                "android",
                "adb (optional)",
                Level::Warn,
                "not found — needed to install and debug on devices",
            )
            .fix(sdkmanager("\"platform-tools\""))
        });
    }
    let ndk = a::ndk(sdk.as_ref().map(|s| s.dir.as_path()));
    checks.push(match &ndk {
        Some(dir) => {
            let version = a::ndk_version(dir).unwrap_or_else(|| "unknown version".into());
            let major = crate::project::version_key(&version).0;
            if major >= a::MIN_NDK_MAJOR {
                Check::new(
                    "android",
                    "Android NDK",
                    Level::Ok,
                    format!("{version} ({})", dir.display()),
                )
            } else {
                Check::new(
                    "android",
                    "Android NDK",
                    Level::Warn,
                    format!(
                        "{version} — r{} or newer links the 16 KB-aligned libraries Google Play requires",
                        a::MIN_NDK_MAJOR
                    ),
                )
                .fix(sdkmanager("\"ndk;30.0.16248370\""))
            }
        }
        None => Check::new("android", "Android NDK", Level::Warn, "not found").fix(sdkmanager("\"ndk;30.0.16248370\"")),
    });
    let java = a::java();
    let java_fix = if cfg!(target_os = "macos") {
        "brew install openjdk@21 (found automatically), or set JAVA_HOME"
    } else {
        "install a JDK 17+ (e.g. Temurin 21) and set JAVA_HOME"
    };
    checks.push(match &java {
        Some(j) if j.version >= a::MIN_JAVA => {
            Check::new("android", "Java", Level::Ok, format!("{} ({})", j.version, j.source))
        }
        Some(j) => Check::new(
            "android",
            "Java",
            Level::Warn,
            format!("{} at {} — Gradle needs {}+", j.version, j.source, a::MIN_JAVA),
        )
        .fix(java_fix),
        None => Check::new(
            "android",
            "Java",
            Level::Warn,
            if cfg!(target_os = "macos") {
                "no JDK found (/usr/bin/java is only a stub without one)"
            } else {
                "no JDK found"
            },
        )
        .fix(java_fix),
    });
    let gradle_fix = if cfg!(target_os = "macos") {
        "brew install gradle"
    } else {
        "install Gradle 9 (https://gradle.org/install/)"
    };
    let min = a::MIN_GRADLE;
    checks.push(match which("gradle") {
        None => Check::new("android", "Gradle", Level::Warn, "not found").fix(gradle_fix),
        Some(program) => {
            let home = java.as_ref().and_then(|j| j.home.as_deref());
            match a::gradle_version(exec, &program, home) {
                Some(v) if v >= min => Check::new("android", "Gradle", Level::Ok, fmt_version(v)),
                Some(v) => Check::new(
                    "android",
                    "Gradle",
                    Level::Warn,
                    format!(
                        "{} — the Android Gradle Plugin {} needs {}+",
                        fmt_version(v),
                        a::AGP_VERSION,
                        fmt_version(min)
                    ),
                )
                .fix(gradle_fix),
                None => Check::new(
                    "android",
                    "Gradle",
                    Level::Warn,
                    format!("{} does not run (needs Java)", program.display()),
                )
                .fix(java_fix),
            }
        }
    });
    checks
}

/// Gather checks for `platforms` (empty = all).
pub fn collect(ctx: &Ctx, platforms: &[Platform]) -> Vec<Check> {
    let wants = |p: Platform| platforms.is_empty() || platforms.contains(&p);
    let exec = &ctx.exec;
    let mut checks = Vec::new();

    // Rust.
    // rustup resolves the toolchain per directory: probe where the project's cargo runs.
    let rustc = tools::rustc_version(exec, &ctx.cwd);
    checks.push(match rustc {
        Some(v) if v >= MIN_RUST => Check::new("rust", "rustc", Level::Ok, fmt_version(v)),
        Some(v) => Check::new(
            "rust",
            "rustc",
            Level::Error,
            format!("{} (need ≥ {})", fmt_version(v), fmt_version(MIN_RUST)),
        )
        .fix("rustup update stable"),
        None => Check::new("rust", "rustc", Level::Error, "not found").fix("install Rust from https://rustup.rs"),
    });
    let targets = tools::installed_targets(exec, &ctx.cwd);
    let target_check = |area: &'static str, triple: &str, level_if_missing: Level| {
        let installed = targets.as_ref().is_none_or(|t| t.iter().any(|x| x == triple));
        Check::new(
            area,
            format!("target {triple}"),
            if installed { Level::Ok } else { level_if_missing },
            if installed { "installed" } else { "missing" },
        )
        .fix(format!("rustup target add {triple}"))
    };

    let project = Project::load(&ctx.cwd, None).ok();

    // Web.
    if wants(Platform::Web) {
        checks.push(target_check("web", WASM_TARGET, Level::Error));
        let wanted = project.as_ref().and_then(|p| p.locked_version("wasm-bindgen"));
        let installed = tools::wasm_bindgen_cli_version(exec);
        let version = wanted
            .clone()
            .unwrap_or_else(|| "<wasm-bindgen version in Cargo.lock>".into());
        let fix = tools::wasm_bindgen_install_cmd(&version).display();
        checks.push(
            match (&installed, &wanted) {
                (None, _) => Check::new("web", "wasm-bindgen CLI", Level::Error, "not found"),
                (Some(i), Some(w)) if i != w => Check::new(
                    "web",
                    "wasm-bindgen CLI",
                    Level::Error,
                    format!("{i}, but Cargo.lock has {w}"),
                ),
                (Some(i), _) => Check::new("web", "wasm-bindgen CLI", Level::Ok, i.clone()),
            }
            .fix(format!("{fix}   (or `space3d build --target web --install-tools`)")),
        );
        checks.push(match tools::wasm_opt_version(exec) {
            Some(v) => Check::new("web", "wasm-opt (optional)", Level::Ok, v),
            None => Check::new(
                "web",
                "wasm-opt (optional)",
                Level::Warn,
                "not found — release builds are larger",
            )
            .fix("brew install binaryen   (or apt install binaryen)"),
        });
    }

    // Android.
    if wants(Platform::Android) {
        checks.extend(android_checks(exec, &target_check));
    }

    // iOS.
    if wants(Platform::Ios) {
        if cfg!(target_os = "macos") {
            checks.push(target_check("ios", IOS_TARGET, Level::Warn));
            checks.push(target_check("ios", IOS_SIM_TARGET, Level::Warn));
            checks.push(match tools::xcode_version(exec) {
                Some(v) => Check::new("ios", "Xcode", Level::Ok, v),
                None => Check::new("ios", "Xcode", Level::Warn, "not selected (Command Line Tools only?)")
                    .fix("install Xcode, then sudo xcode-select -s /Applications/Xcode.app/Contents/Developer"),
            });
            checks.push(match which("xcodegen") {
                Some(_) => Check::new("ios", "XcodeGen", Level::Ok, "found"),
                None => Check::new(
                    "ios",
                    "XcodeGen",
                    Level::Warn,
                    "not found — needed to generate ios/*.xcodeproj",
                )
                .fix("brew install xcodegen"),
            });
        } else {
            checks.push(Check::new(
                "ios",
                "macOS",
                Level::Warn,
                "iOS builds need a Mac with Xcode",
            ));
        }
    }

    // UI: Chinese and Japanese line breaking needs the project's own epaint patch.
    if let Some((root, lock)) = workspace_lockfile(ctx)
        && let Some(check) = kinsoku_check(&lock, &root, cargo_home().as_deref())
    {
        checks.push(check);
    }

    // GitHub (publish / CI).
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .is_ok();
    checks.push(if token {
        Check::new("github", "token", Level::Ok, "GITHUB_TOKEN/GH_TOKEN set")
    } else if which("gh").is_none() {
        Check::new(
            "github",
            "gh CLI",
            Level::Warn,
            "not found (needed for `space3d publish`)",
        )
        .fix("brew install gh && gh auth login   (or export GITHUB_TOKEN)")
    } else if tools::gh_logged_in(exec) {
        Check::new("github", "gh auth", Level::Ok, "logged in")
    } else {
        Check::new("github", "gh auth", Level::Warn, "not logged in").fix("gh auth login")
    });
    checks
}

/// Kinsoku (Chinese and Japanese line breaking) for a project that uses `space3d-ui`: egui lays
/// text out with Space3d's patched `epaint` only through the project's own
/// `[patch.crates-io] epaint = …` (a patch in a dependency does nothing). Read from `lock`
/// (`Cargo.lock` of the workspace at `root`): epaint from crates.io means no patch,
/// `[[patch.unused]]` a patch whose version isn't egui's (another Space3d revision than
/// `space3d`'s); a git epaint must be the commit `space3d` is locked to, a path epaint (the
/// root `Cargo.toml`'s patch) Space3d's copy, not a game's own older one. `cargo_home` has the
/// git checkouts that tell whether `space3d`'s revision has the copy at all. `None` without
/// `space3d-ui`.
pub fn kinsoku_check(lock: &str, root: &Path, cargo_home: Option<&Path>) -> Option<Check> {
    let lock: toml::Table = toml::from_str(lock).ok()?;
    let packages: Vec<&toml::Table> = lock
        .get("package")?
        .as_array()?
        .iter()
        .filter_map(toml::Value::as_table)
        .collect();
    let find = |name: &str| {
        packages
            .iter()
            .copied()
            .find(|p| p.get("name").and_then(toml::Value::as_str) == Some(name))
    };
    let source = |p: &toml::Table| p.get("source").and_then(toml::Value::as_str).map(str::to_owned);
    let ui_source = source(find("space3d-ui")?);
    let epaint_source = source(find("epaint")?);
    let unused = lock
        .get("patch")
        .and_then(|p| p.get("unused"))
        .and_then(toml::Value::as_array)
        .is_some_and(|u| {
            u.iter()
                .any(|p| p.get("name").and_then(toml::Value::as_str) == Some("epaint"))
        });
    let what = "kinsoku (epaint patch)";
    let warn = |mut detail: String| {
        if let Some(rev) = checkout_without_epaint(ui_source.as_deref(), cargo_home) {
            detail.push_str(&format!(
                "; Space3d {rev} predates third_party/epaint: update `space3d` first"
            ));
        }
        let patch = epaint_patch_for(ui_source.as_deref());
        Check::new("ui", what, Level::Warn, detail).fix(format!(
            "in Cargo.toml: [patch.crates-io] {patch}   (the same Space3d revision as `space3d`; Space3d docs/ui.md)"
        ))
    };
    let Some(epaint_source) = epaint_source else {
        // A path: the root manifest's patch says which.
        let Some(dir) = patched_epaint_path(root) else {
            return Some(Check::new(
                "ui",
                what,
                Level::Ok,
                "epaint from a local path (not checked)",
            ));
        };
        return Some(if has_kinsoku(&dir) {
            Check::new("ui", what, Level::Ok, format!("Space3d's epaint ({})", dir.display()))
        } else {
            warn(format!(
                "epaint from {} is not Space3d's kinsoku copy (a project's own, older patch?)",
                dir.display()
            ))
        });
    };
    if epaint_source.starts_with("registry+") || epaint_source.starts_with("sparse+") {
        return Some(warn(if unused {
            "the epaint patch is unused (not the Space3d revision of `space3d`?): egui's own epaint, no kinsoku"
                .to_owned()
        } else {
            "egui's epaint from crates.io: Chinese and Japanese lines break without kinsoku".to_owned()
        }));
    }
    if ui_source
        .as_deref()
        .is_some_and(|ui| same_git_commit(ui, &epaint_source))
    {
        return Some(Check::new(
            "ui",
            what,
            Level::Ok,
            "Space3d's epaint, the revision of `space3d`",
        ));
    }
    Some(warn(format!(
        "epaint from {}, not the Space3d revision `space3d` is locked to",
        epaint_source.split('#').next().unwrap_or(&epaint_source)
    )))
}

/// Cargo's home (`CARGO_HOME`, else `~/.cargo`).
fn cargo_home() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(Into::into)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cargo")))
}

/// Two `Cargo.lock` git sources name the same repository and commit (`git+URL?rev=…#COMMIT`).
fn same_git_commit(a: &str, b: &str) -> bool {
    let parts = |s: &str| {
        let (url, commit) = s.split_once('#').unwrap_or((s, ""));
        (
            url.split('?').next().unwrap_or(url).trim_end_matches(".git").to_owned(),
            commit.to_owned(),
        )
    };
    let (a, b) = (parts(a), parts(b));
    a.0 == b.0 && !a.1.is_empty() && a.1 == b.1
}

/// The directory of the root manifest's `[patch.crates-io] epaint = { path = … }`.
fn patched_epaint_path(root: &Path) -> Option<std::path::PathBuf> {
    let manifest: toml::Table = std::fs::read_to_string(root.join("Cargo.toml")).ok()?.parse().ok()?;
    let path = manifest
        .get("patch")?
        .get("crates-io")?
        .get("epaint")?
        .get("path")?
        .as_str()?;
    Some(root.join(path))
}

/// `dir` is Space3d's epaint: it has the kinsoku module with its `VERSION` marker.
fn has_kinsoku(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("src/text/kinsoku.rs")).is_ok_and(|src| src.contains("pub const VERSION"))
}

/// The short commit of a Space3d git `source` whose checkout in `cargo_home` has no
/// `third_party/epaint` (a revision from before Space3d vendored it).
fn checkout_without_epaint(source: Option<&str>, cargo_home: Option<&Path>) -> Option<String> {
    let (_, commit) = source?.strip_prefix("git+")?.split_once('#')?;
    let short = commit.get(..7)?;
    let checkouts = std::fs::read_dir(cargo_home?.join("git").join("checkouts")).ok()?;
    checkouts
        .filter_map(Result::ok)
        .map(|repo| repo.path().join(short))
        .filter(|dir| dir.join("crates/space3d-ui/Cargo.toml").is_file())
        .any(|dir| !dir.join("third_party/epaint/Cargo.toml").is_file())
        .then(|| short.to_owned())
}

/// Root and `Cargo.lock` of the cargo workspace around the current directory, if any.
fn workspace_lockfile(ctx: &Ctx) -> Option<(std::path::PathBuf, String)> {
    let manifest = ctx.exec.probe(
        &Cmd::cargo("locate-project")
            .args(["--workspace", "--message-format", "plain"])
            .cwd(&ctx.cwd),
    )?;
    let root = Path::new(manifest.trim()).parent()?.to_owned();
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).ok()?;
    Some((root, lock))
}

/// The `epaint = { … }` patch entry for a Space3d package locked from `source` (`None`: a path).
fn epaint_patch_for(source: Option<&str>) -> String {
    let Some(git) = source.and_then(|s| s.strip_prefix("git+")) else {
        return r#"epaint = { path = "<Space3d checkout>/third_party/epaint" }"#.to_owned();
    };
    let url_and_query = git.split('#').next().unwrap_or(git);
    let (url, query) = url_and_query.split_once('?').unwrap_or((url_and_query, ""));
    let mut fields = format!("git = \"{url}\"");
    for pair in query.split('&') {
        if let Some((key @ ("rev" | "branch" | "tag"), value)) = pair.split_once('=') {
            fields.push_str(&format!(", {key} = \"{value}\""));
        }
    }
    format!("epaint = {{ {fields} }}")
}

fn fmt_version((a, b, c): (u64, u64, u64)) -> String {
    format!("{a}.{b}.{c}")
}

/// Render checks as an aligned report with a fixes section.
pub fn render(checks: &[Check]) -> String {
    let width = checks.iter().map(|c| c.what.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    let mut area = "";
    for c in checks {
        if c.area != area {
            area = c.area;
            out.push_str(&format!("{area}\n"));
        }
        let mark = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Error => "FAIL",
        };
        out.push_str(&format!("  [{mark}] {:<width$}  {}\n", c.what, c.detail));
    }
    let fixes: Vec<&Check> = checks.iter().filter(|c| c.fix.is_some()).collect();
    if fixes.is_empty() {
        out.push_str("\neverything needed is installed\n");
    } else {
        out.push_str("\nto fix:\n");
        for c in fixes {
            out.push_str(&format!("  {}: {}\n", c.what, c.fix.as_deref().unwrap_or_default()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Exec;

    #[test]
    fn render_marks_and_fixes() {
        let checks = vec![
            Check::new("rust", "rustc", Level::Ok, "1.98.1").fix("never shown"),
            Check::new("web", "wasm-bindgen CLI", Level::Error, "not found").fix("cargo install wasm-bindgen-cli"),
            Check::new("web", "wasm-opt (optional)", Level::Warn, "not found"),
        ];
        let text = render(&checks);
        assert!(text.contains("rust\n  [ok  ] rustc"), "{text}");
        assert!(text.contains("[FAIL] wasm-bindgen CLI"));
        assert!(text.contains("to fix:\n  wasm-bindgen CLI: cargo install wasm-bindgen-cli\n"));
        assert!(!text.contains("never shown"));
        assert!(render(&checks[..1]).contains("everything needed is installed"));
    }

    #[test]
    fn collect_runs_on_this_machine() {
        let ctx = Ctx {
            cwd: std::env::current_dir().unwrap(),
            exec: Exec::default(),
        };
        let checks = collect(&ctx, &[Platform::Desktop]);
        let rustc = checks.iter().find(|c| c.what == "rustc").unwrap();
        assert_eq!(rustc.level, Level::Ok);
        assert!(checks.iter().all(|c| c.area != "web" && c.area != "android"));
        let all = collect(&ctx, &[]);
        assert!(all.iter().any(|c| c.area == "web") && all.iter().any(|c| c.area == "android"));
        // The Space3d workspace itself uses its patched epaint.
        let ui = checks.iter().find(|c| c.area == "ui").expect("kinsoku check");
        assert_eq!(ui.level, Level::Ok, "{ui:?}");
    }

    fn lock(space3d_source: Option<&str>, epaint_source: Option<&str>, unused: bool) -> String {
        let source = |s: Option<&str>| s.map(|s| format!("source = \"{s}\"\n")).unwrap_or_default();
        let mut lock = format!(
            "version = 4\n\n[[package]]\nname = \"space3d-ui\"\nversion = \"0.1.0\"\n{}\n\
             [[package]]\nname = \"epaint\"\nversion = \"0.36.2\"\n{}",
            source(space3d_source),
            source(epaint_source)
        );
        if unused {
            lock.push_str("\n[[patch.unused]]\nname = \"epaint\"\nversion = \"0.36.2\"\n");
        }
        lock
    }

    const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

    #[test]
    fn kinsoku_patch_check() {
        let git = "git+https://github.com/SpaceCorps/Space3d?rev=b00ed35#b00ed35a189ec968404304d7b1bd51f3dd982d71";
        let nowhere = Path::new("/nonexistent");
        let check = |lock: &str| kinsoku_check(lock, nowhere, None);
        // Patched from the same repository and commit (however the rev is spelled): fine.
        let ok = check(&lock(Some(git), Some(git), false)).unwrap();
        assert_eq!(ok.level, Level::Ok, "{ok:?}");
        let full = "git+https://github.com/SpaceCorps/Space3d?rev=b00ed35a189ec968404304d7b1bd51f3dd982d71#b00ed35a189ec968404304d7b1bd51f3dd982d71";
        assert_eq!(check(&lock(Some(git), Some(full), false)).unwrap().level, Level::Ok);
        // No patch: warn, with the entry to add.
        let missing = check(&lock(Some(git), Some(CRATES_IO), false)).unwrap();
        assert_eq!(missing.level, Level::Warn);
        assert!(missing.detail.contains("crates.io"), "{missing:?}");
        let fix = missing.fix.unwrap();
        assert!(
            fix.contains(
                r#"[patch.crates-io] epaint = { git = "https://github.com/SpaceCorps/Space3d", rev = "b00ed35" }"#
            ),
            "{fix}"
        );
        // A patch whose version isn't egui's.
        let unused = check(&lock(Some(git), Some(CRATES_IO), true)).unwrap();
        assert_eq!(unused.level, Level::Warn);
        assert!(unused.detail.contains("unused"), "{unused:?}");
        // epaint from another Space3d revision than `space3d`'s.
        let other = "git+https://github.com/SpaceCorps/Space3d?rev=42adc4b#42adc4b28fe4e43dee83fd9cfacc99e7cd5a04b6";
        let mismatch = check(&lock(Some(git), Some(other), false)).unwrap();
        assert_eq!(mismatch.level, Level::Warn);
        assert!(mismatch.detail.contains("not the Space3d revision"), "{mismatch:?}");
        assert!(mismatch.fix.unwrap().contains(r#"rev = "b00ed35""#));
        // Branch, default branch, local checkout.
        assert_eq!(
            epaint_patch_for(Some("git+https://x/y?branch=main#0123")),
            r#"epaint = { git = "https://x/y", branch = "main" }"#
        );
        assert_eq!(
            epaint_patch_for(Some("git+https://x/y#0123")),
            r#"epaint = { git = "https://x/y" }"#
        );
        assert!(epaint_patch_for(None).contains("third_party/epaint"));
        // Not a UI project, not a lock file: nothing to say.
        assert!(check("version = 4\n").is_none());
        assert!(check("[[").is_none());
    }

    /// A path patch is checked for Space3d's copy: a game's own older epaint (no kinsoku module)
    /// warns.
    #[test]
    fn kinsoku_path_patch_must_be_space3ds_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = "git+https://github.com/SpaceCorps/Space3d?rev=b00ed35#b00ed35a189ec968404304d7b1bd51f3dd982d71";
        let manifest = |path: &str| {
            std::fs::write(
                root.join("Cargo.toml"),
                format!("[package]\nname = \"game\"\n\n[patch.crates-io]\nepaint = {{ path = \"{path}\" }}\n"),
            )
            .unwrap();
        };
        // The game's own copy: epaint without src/text/kinsoku.rs.
        std::fs::create_dir_all(root.join("third_party/epaint/src/text")).unwrap();
        std::fs::write(root.join("third_party/epaint/src/text/text_layout.rs"), "// patched").unwrap();
        manifest("third_party/epaint");
        let own = kinsoku_check(&lock(Some(git), None, false), root, None).unwrap();
        assert_eq!(own.level, Level::Warn, "{own:?}");
        assert!(own.detail.contains("not Space3d's kinsoku copy"), "{own:?}");
        assert!(own.fix.unwrap().contains(r#"rev = "b00ed35""#));
        // A Space3d checkout's copy.
        std::fs::create_dir_all(root.join("Space3d/third_party/epaint/src/text")).unwrap();
        std::fs::write(
            root.join("Space3d/third_party/epaint/src/text/kinsoku.rs"),
            "pub const VERSION: u32 = 2;\n",
        )
        .unwrap();
        manifest("Space3d/third_party/epaint");
        let space3d = kinsoku_check(&lock(None, None, false), root, None).unwrap();
        assert_eq!(space3d.level, Level::Ok, "{space3d:?}");
        // No patch in the root manifest (e.g. in .cargo/config.toml): not checked.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"game\"\n").unwrap();
        let unknown = kinsoku_check(&lock(None, None, false), root, None).unwrap();
        assert_eq!(unknown.level, Level::Ok);
        assert!(unknown.detail.contains("not checked"), "{unknown:?}");
    }

    /// Without the patch, and `space3d` locked to a revision whose checkout has no
    /// third_party/epaint yet, the warning says to update `space3d` first (the fix's rev would
    /// not have the package).
    #[test]
    fn kinsoku_check_notes_a_space3d_revision_without_the_copy() {
        let home = tempfile::tempdir().unwrap();
        let git = "git+https://github.com/SpaceCorps/Space3d?rev=42adc4b#42adc4b28fe4e43dee83fd9cfacc99e7cd5a04b6";
        let checkout = home.path().join("git/checkouts/space3d-c504a142671c5a24/42adc4b");
        std::fs::create_dir_all(checkout.join("crates/space3d-ui")).unwrap();
        std::fs::write(checkout.join("crates/space3d-ui/Cargo.toml"), "").unwrap();
        let nowhere = Path::new("/nonexistent");
        let old = kinsoku_check(&lock(Some(git), Some(CRATES_IO), false), nowhere, Some(home.path())).unwrap();
        assert_eq!(old.level, Level::Warn);
        assert!(old.detail.contains("42adc4b predates third_party/epaint"), "{old:?}");
        // Also for a project's own epaint copy.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("third_party/epaint/src/text")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"game\"\n\n[patch.crates-io]\nepaint = { path = \"third_party/epaint\" }\n",
        )
        .unwrap();
        let own = kinsoku_check(&lock(Some(git), None, false), root.path(), Some(home.path())).unwrap();
        assert!(own.detail.contains("not Space3d's kinsoku copy"), "{own:?}");
        assert!(own.detail.contains("42adc4b predates third_party/epaint"), "{own:?}");
        // The checkout has the copy: the plain warning.
        std::fs::create_dir_all(checkout.join("third_party/epaint")).unwrap();
        std::fs::write(checkout.join("third_party/epaint/Cargo.toml"), "").unwrap();
        let new = kinsoku_check(&lock(Some(git), Some(CRATES_IO), false), nowhere, Some(home.path())).unwrap();
        assert_eq!(new.level, Level::Warn);
        assert!(!new.detail.contains("predates"), "{new:?}");
    }
}
