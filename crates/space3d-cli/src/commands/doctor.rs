//! `space3d doctor`: toolchain and SDK checks with fixes.

use crate::Ctx;
use crate::args::{DoctorArgs, Platform};
use crate::process::which;
use crate::project::Project;
use crate::tools::{self, ANDROID_TARGET, IOS_SIM_TARGET, IOS_TARGET, MIN_RUST, WASM_TARGET};
use anyhow::Result;

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

    // Web.
    if wants(Platform::Web) {
        checks.push(target_check("web", WASM_TARGET, Level::Error));
        let wanted = Project::load(&ctx.cwd, None)
            .ok()
            .and_then(|p| p.locked_version("wasm-bindgen"));
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
        checks.push(target_check("android", ANDROID_TARGET, Level::Warn));
        checks.push(match tools::cargo_apk() {
            Some(p) => Check::new("android", "cargo-apk", Level::Ok, p.display().to_string()),
            None => {
                Check::new("android", "cargo-apk", Level::Warn, "not found").fix("cargo install cargo-apk --locked")
            }
        });
        checks.push(match tools::android_sdk() {
            Some(p) => Check::new("android", "Android SDK", Level::Ok, p.display().to_string()),
            None => Check::new("android", "Android SDK", Level::Warn, "ANDROID_HOME not set")
                .fix("install Android Studio, then export ANDROID_HOME=<sdk dir>"),
        });
        checks.push(match tools::android_ndk() {
            Some(p) => Check::new("android", "Android NDK", Level::Ok, p.display().to_string()),
            None => Check::new("android", "Android NDK", Level::Warn, "not found")
                .fix("SDK Manager > SDK Tools > NDK, then export ANDROID_NDK_ROOT=$ANDROID_HOME/ndk/<version>"),
        });
        checks.push(match which("java") {
            Some(_) => Check::new("android", "Java", Level::Ok, "found"),
            None => {
                Check::new("android", "Java", Level::Warn, "not found").fix("install JDK 17+ (brew install openjdk@17)")
            }
        });
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
    }
}
