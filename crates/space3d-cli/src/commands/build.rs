//! `space3d build --target desktop|web|android|ios`.

use super::{cargo_flags, profile_dir};
use crate::Ctx;
use crate::args::{BuildArgs, CargoSel, Platform};
use crate::process::{Cmd, which};
use crate::project::Project;
use crate::tools::{self, ANDROID_TARGET, WASM_TARGET};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Build for `args.target`; returns the output directory.
pub fn run(ctx: &Ctx, args: &BuildArgs) -> Result<PathBuf> {
    let project = Project::load(&ctx.cwd, args.cargo.package.as_deref())?;
    build(ctx, &project, args)
}

/// Build an already loaded project.
pub fn build(ctx: &Ctx, project: &Project, args: &BuildArgs) -> Result<PathBuf> {
    let out = args
        .out
        .as_ref()
        .map(|o| if o.is_absolute() { o.clone() } else { ctx.cwd.join(o) })
        .unwrap_or_else(|| project.dist_dir(args.target.name()));
    match args.target {
        Platform::Desktop => desktop(ctx, project, &args.cargo, &out),
        Platform::Web => web(ctx, project, args, &out),
        Platform::Android => android(ctx, project, &args.cargo, &out),
        Platform::Ios => super::ios::build(ctx, project, &args.cargo, args.sim, args.xcode, &out).map(|_| ()),
    }?;
    Ok(out)
}

/// `sel` with the project's selected package filled in when none was given.
pub(crate) fn with_package(sel: &CargoSel, project: &Project) -> CargoSel {
    let mut sel = sel.clone();
    sel.package.get_or_insert_with(|| project.package().name.clone());
    sel
}

/// Executable file name on this OS.
pub fn exe_name(bin: &str) -> String {
    format!("{bin}{}", std::env::consts::EXE_SUFFIX)
}

fn desktop(ctx: &Ctx, project: &Project, sel: &CargoSel, out: &Path) -> Result<()> {
    let bin = project.package().main_bin(sel.bin.as_deref())?;
    let cmd = cargo_flags(
        Cmd::cargo("build").cwd(project.dir()),
        &with_package(sel, project),
        Some(&bin),
    );
    ctx.exec.run(&cmd)?;
    let exe = project.target_dir.join(profile_dir(sel.release)).join(exe_name(&bin));
    stage(ctx, &exe, &out.join(exe_name(&bin)))?;
    copy_assets(ctx, project, out)?;
    finish(ctx, "desktop", out);
    Ok(())
}

// ------------------------------------------------------------------------------------ web

/// wasm-opt flags matching the WebAssembly features rustc enables for wasm32 by default.
pub const WASM_OPT_FEATURES: [&str; 6] = [
    "--enable-bulk-memory",
    "--enable-nontrapping-float-to-int",
    "--enable-sign-ext",
    "--enable-mutable-globals",
    "--enable-reference-types",
    "--enable-multivalue",
];

fn web(ctx: &Ctx, project: &Project, args: &BuildArgs, out: &Path) -> Result<()> {
    if !tools::has_target(&ctx.exec, project.dir(), WASM_TARGET) {
        let msg = format!("the {WASM_TARGET} target is not installed; run:\n    rustup target add {WASM_TARGET}");
        if !ctx.exec.dry_run {
            bail!(msg);
        }
        eprintln!("[dry-run] {msg}");
    }
    let sel = &args.cargo;
    let bin = project.package().main_bin(sel.bin.as_deref())?;
    ctx.exec.run(
        &cargo_flags(
            Cmd::cargo("build").cwd(project.dir()),
            &with_package(sel, project),
            Some(&bin),
        )
        .args(["--target", WASM_TARGET]),
    )?;
    let wasm = project
        .target_dir
        .join(WASM_TARGET)
        .join(profile_dir(sel.release))
        .join(format!("{bin}.wasm"));

    let wasm_bindgen = ensure_wasm_bindgen(ctx, project, args.install_tools)?;
    if !ctx.exec.dry_run {
        std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    }
    ctx.exec.run(
        &Cmd::new(&wasm_bindgen)
            .args(["--target", "web", "--no-typescript", "--out-name", &bin, "--out-dir"])
            .arg(out)
            .arg(&wasm),
    )?;
    let bg = out.join(format!("{bin}_bg.wasm"));
    if sel.release && !args.no_wasm_opt {
        if which("wasm-opt").is_some() {
            ctx.exec.run(
                &Cmd::new("wasm-opt")
                    .arg("-Oz")
                    .args(WASM_OPT_FEATURES)
                    .arg(&bg)
                    .arg("-o")
                    .arg(&bg),
            )?;
        } else if ctx.exec.verbose {
            eprintln!("note: wasm-opt (binaryen) not found; skipping size optimization");
        }
    }

    let web_dir = project.dir().join("web");
    if web_dir.is_dir() {
        copy_dir(ctx, &web_dir, out)?;
    } else if !ctx.exec.dry_run {
        std::fs::write(out.join("index.html"), default_index_html(&bin))?;
    }
    copy_assets(ctx, project, out)?;
    if !ctx.exec.dry_run
        && let Ok(meta) = std::fs::metadata(&bg)
    {
        eprintln!("{} is {:.1} MiB", bg.display(), meta.len() as f64 / (1024.0 * 1024.0));
    }
    finish(ctx, "web", out);
    Ok(())
}

/// Find a `wasm-bindgen` CLI matching the project's locked `wasm-bindgen` crate, installing it
/// with `install` (else explaining how). Returns the program to run.
fn ensure_wasm_bindgen(ctx: &Ctx, project: &Project, install: bool) -> Result<String> {
    let wanted = project.locked_version("wasm-bindgen");
    let installed = tools::wasm_bindgen_cli_version(&ctx.exec);
    match wasm_bindgen_status(wanted.as_deref(), installed.as_deref()) {
        WasmBindgenStatus::Ok => Ok("wasm-bindgen".into()),
        status => {
            let version = wanted
                .clone()
                .unwrap_or_else(|| "<version of wasm-bindgen in Cargo.lock>".into());
            let install_cmd = tools::wasm_bindgen_install_cmd(&version);
            let problem = match status {
                WasmBindgenStatus::Missing => "wasm-bindgen CLI not found".to_owned(),
                WasmBindgenStatus::Mismatch => format!(
                    "wasm-bindgen CLI {} does not match the wasm-bindgen crate {} in Cargo.lock",
                    installed.as_deref().unwrap_or("?"),
                    version
                ),
                WasmBindgenStatus::Ok => unreachable!(),
            };
            if install && wanted.is_some() {
                eprintln!("{problem}; installing it");
                ctx.exec.run(&install_cmd)?;
                return Ok("wasm-bindgen".into());
            }
            if ctx.exec.dry_run {
                eprintln!("[dry-run] {problem}; would need: {}", install_cmd.display());
                return Ok("wasm-bindgen".into());
            }
            bail!(
                "{problem}. Install the matching version with:\n    {}\n(or re-run with --install-tools)",
                install_cmd.display()
            )
        }
    }
}

/// Whether the installed wasm-bindgen CLI can process the project's wasm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WasmBindgenStatus {
    /// Installed, and matches (or the project has no lock entry to compare with).
    Ok,
    /// Not installed.
    Missing,
    /// Installed with another version (wasm-bindgen refuses mismatched schemas).
    Mismatch,
}

/// Compare the locked crate version with the installed CLI version.
pub fn wasm_bindgen_status(locked: Option<&str>, installed: Option<&str>) -> WasmBindgenStatus {
    match (locked, installed) {
        (_, None) => WasmBindgenStatus::Missing,
        (Some(l), Some(i)) if l != i => WasmBindgenStatus::Mismatch,
        _ => WasmBindgenStatus::Ok,
    }
}

/// `index.html` used when the project has no `web/` directory.
pub fn default_index_html(bin: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1, user-scalable=no">
  <title>{bin}</title>
  <style>html,body{{margin:0;height:100%;background:#000;overflow:hidden}}canvas{{display:block;width:100vw!important;height:100vh!important;touch-action:none;outline:none}}</style>
</head>
<body>
  <script type="module">import init from "./{bin}.js"; init();</script>
</body>
</html>
"#
    )
}

// ------------------------------------------------------------------------------------ android

/// Setup instructions printed when Android tooling is missing.
pub const ANDROID_SETUP: &str = "\
Android builds need:
  1. Android SDK + NDK (Android Studio > SDK Manager > SDK Tools > NDK), then
       export ANDROID_HOME=$HOME/Library/Android/sdk        # or ~/Android/Sdk on Linux
       export ANDROID_NDK_ROOT=$ANDROID_HOME/ndk/<version>
  2. Java 17+ (e.g. `brew install openjdk@17`, or the JDK bundled with Android Studio)
  3. cargo install cargo-apk --locked
  4. rustup target add aarch64-linux-android
Then: space3d build --target android [--release]  (install with `adb install dist/android/*.apk`)";

fn android(ctx: &Ctx, project: &Project, sel: &CargoSel, out: &Path) -> Result<()> {
    let Some(pkg) = project.mobile_package("cdylib") else {
        bail!(
            "no package builds a `cdylib` for Android.\n\
             Projects created by `space3d new` have `mobile/` (package `{0}-mobile`); otherwise add\n\
             [lib] crate-type = [\"lib\", \"cdylib\"] and `space3d::main!` to your library.",
            project.package().name
        );
    };
    let mut missing = Vec::new();
    if tools::cargo_apk().is_none() {
        missing.push("cargo-apk");
    }
    if tools::android_sdk().is_none() {
        missing.push("Android SDK (ANDROID_HOME)");
    }
    if tools::android_ndk().is_none() {
        missing.push("Android NDK (ANDROID_NDK_ROOT)");
    }
    if !tools::has_target(&ctx.exec, project.dir(), ANDROID_TARGET) {
        missing.push("rust target aarch64-linux-android");
    }
    if !missing.is_empty() {
        let msg = format!("missing: {}\n\n{ANDROID_SETUP}", missing.join(", "));
        if !ctx.exec.dry_run {
            bail!(msg);
        }
        eprintln!("[dry-run] {msg}");
    }
    let cmd = Cmd::cargo("apk")
        .cwd(project.dir())
        .arg("build")
        .arg_if(sel.release, "--release")
        .args(["-p", &pkg.name, "--lib", "--target", ANDROID_TARGET])
        .opt("--features", sel.features.as_deref());
    ctx.exec.run(&cmd)?;
    if !ctx.exec.dry_run {
        let apk_dir = project.target_dir.join(profile_dir(sel.release)).join("apk");
        let apks: Vec<PathBuf> = std::fs::read_dir(&apk_dir)
            .with_context(|| format!("no APKs in {}", apk_dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "apk"))
            .collect();
        for apk in apks {
            stage(ctx, &apk, &out.join(apk.file_name().unwrap_or_default()))?;
        }
    }
    finish(ctx, "android", out);
    Ok(())
}

/// Top-level `name:` of an XcodeGen `project.yml`.
pub fn xcodegen_project_name(spec: &Path) -> Option<String> {
    let text = std::fs::read_to_string(spec).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("name:"))
        .map(|n| n.trim().trim_matches('"').to_owned())
        .filter(|n| !n.is_empty())
}

/// First `key: value` (any indentation) in a YAML file — enough for XcodeGen specs.
pub fn yaml_value(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{key}:");
    text.lines()
        .find_map(|l| l.trim_start().strip_prefix(prefix.as_str()).map(str::to_owned))
        .map(|v| v.trim().trim_matches('"').to_owned())
        .filter(|v| !v.is_empty())
}

// ------------------------------------------------------------------------------------ helpers

/// Copy one file into the output (creating directories).
fn stage(ctx: &Ctx, from: &Path, to: &Path) -> Result<()> {
    if ctx.exec.dry_run {
        eprintln!("[dry-run] copy {} -> {}", from.display(), to.display());
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(())
}

/// Copy the project's `assets/` into `out/assets`.
fn copy_assets(ctx: &Ctx, project: &Project, out: &Path) -> Result<()> {
    let assets = project.dir().join("assets");
    if assets.is_dir() {
        copy_dir(ctx, &assets, &out.join("assets"))?;
    }
    Ok(())
}

/// Recursively copy `from/*` into `to/`.
pub fn copy_dir(ctx: &Ctx, from: &Path, to: &Path) -> Result<()> {
    if ctx.exec.dry_run {
        eprintln!("[dry-run] copy {}/ -> {}/", from.display(), to.display());
        return Ok(());
    }
    copy_dir_all(from, to)
}

/// Recursively copy a directory's contents.
pub fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn finish(ctx: &Ctx, what: &str, out: &Path) {
    if !ctx.exec.dry_run {
        eprintln!("{what} build ready in {}", out.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasm_bindgen_versions_must_match() {
        use WasmBindgenStatus::*;
        assert_eq!(wasm_bindgen_status(Some("0.2.128"), Some("0.2.128")), Ok);
        assert_eq!(wasm_bindgen_status(Some("0.2.128"), Some("0.2.100")), Mismatch);
        assert_eq!(wasm_bindgen_status(Some("0.2.128"), None), Missing);
        assert_eq!(wasm_bindgen_status(None, Some("0.2.1")), Ok);
        assert_eq!(wasm_bindgen_status(None, None), Missing);
    }

    #[test]
    fn default_page_loads_the_bundle() {
        let html = default_index_html("my_game");
        assert!(html.contains(r#"import init from "./my_game.js""#));
        assert!(html.starts_with("<!doctype html>"));
    }

    #[test]
    fn xcodegen_name() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("project.yml");
        std::fs::write(
            &spec,
            "# comment\nname: my_game\noptions:\n  x: 1\n    PRODUCT_BUNDLE_IDENTIFIER: com.example.my-game\n",
        )
        .unwrap();
        assert_eq!(xcodegen_project_name(&spec).as_deref(), Some("my_game"));
        assert_eq!(
            yaml_value(&spec, "PRODUCT_BUNDLE_IDENTIFIER").as_deref(),
            Some("com.example.my-game")
        );
        assert_eq!(xcodegen_project_name(&dir.path().join("missing.yml")), None);
    }

    #[test]
    fn copies_directories() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("web");
        std::fs::create_dir_all(src.join("img")).unwrap();
        std::fs::write(src.join("index.html"), "hi").unwrap();
        std::fs::write(src.join("img/a.png"), [1, 2, 3]).unwrap();
        let dst = dir.path().join("dist");
        copy_dir_all(&src, &dst).unwrap();
        assert_eq!(std::fs::read(dst.join("img/a.png")).unwrap(), [1, 2, 3]);
        assert_eq!(exe_name("x").trim_end_matches(".exe"), "x");
    }
}
