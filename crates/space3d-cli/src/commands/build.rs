//! `space3d build --target desktop|web|android|ios`.

use super::{cargo_flags, profile_dir};
use crate::Ctx;
use crate::android::{self, AndroidConfig, GradleDefaults};
use crate::args::{BuildArgs, CargoSel, Platform};
use crate::process::{Cmd, which};
use crate::project::Project;
use crate::tools::{self, IOS_SIM_TARGET, IOS_TARGET, WASM_TARGET};
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
        Platform::Android => android(ctx, project, args, &out),
        Platform::Ios => ios(ctx, project, &args.cargo, args.sim, &out),
    }?;
    Ok(out)
}

fn with_package(sel: &CargoSel, project: &Project) -> CargoSel {
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

/// The library Android loads: `package`'s lib target `lib_name`, built as a `cdylib` either
/// because its manifest says so (`space3d new`'s `mobile/` crate) or with
/// `cargo rustc --crate-type cdylib` (a game whose own library calls `space3d::main!`).
#[derive(Clone, Debug)]
pub struct AndroidLib<'a> {
    /// The package.
    pub package: &'a crate::project::PackageInfo,
    /// Its library's crate name (`lib<name>.so`).
    pub lib_name: String,
    /// Build with `cargo rustc --crate-type cdylib` (the manifest does not ask for one).
    pub force_cdylib: bool,
}

/// Find the library to build for Android (see [`AndroidLib`]).
pub fn android_lib(project: &Project) -> Result<AndroidLib<'_>> {
    if let Some(pkg) = project.mobile_package("cdylib") {
        let lib_name = pkg.lib().map_or_else(|| pkg.name.replace('-', "_"), |l| l.name.clone());
        return Ok(AndroidLib {
            package: pkg,
            lib_name,
            force_cdylib: false,
        });
    }
    let pkg = project.package();
    match pkg.lib() {
        Some(lib) => Ok(AndroidLib {
            package: pkg,
            lib_name: lib.name.clone(),
            force_cdylib: true,
        }),
        None => bail!(
            "package `{}` has no library for Android to load.\n\
             Projects created by `space3d new` have `mobile/` (package `{0}-mobile`). A single-binary game can add\n\
             src/lib.rs with `#![cfg(target_os = \"android\")] #[path = \"main.rs\"] mod game;` (its `space3d::main!`\n\
             exports `android_main`; see examples/hello_cube/src/lib.rs)",
            pkg.name
        ),
    }
}

fn android(ctx: &Ctx, project: &Project, args: &BuildArgs, out: &Path) -> Result<()> {
    let sel = &args.cargo;
    let game = project.package();
    let lib = android_lib(project)?;
    let cfg = AndroidConfig::from_packages(lib.package, game)?;

    // Tools: report everything missing at once.
    let sdk = android::sdk();
    let ndk = android::ndk(sdk.as_ref().map(|s| s.dir.as_path()));
    let java = android::java();
    let own_project = android::own_gradle_project(game.dir());
    let gradle_dir = own_project
        .clone()
        .unwrap_or_else(|| project.target_dir.join("android").join(&game.name).join("gradle"));
    let gradle = android::gradle_program(&gradle_dir);
    let mut missing = Vec::new();
    if android::cargo_ndk_version(&ctx.exec).is_none() {
        missing.push("cargo-ndk".to_owned());
    }
    if sdk.is_none() {
        missing.push("Android SDK (ANDROID_HOME)".to_owned());
    }
    if ndk.is_none() {
        missing.push("Android NDK".to_owned());
    }
    match &java {
        None => missing.push(format!("Java {}+ (JDK)", android::MIN_JAVA)),
        Some(j) if j.version < android::MIN_JAVA => missing.push(format!(
            "Java {}+ (found {} at {})",
            android::MIN_JAVA,
            j.version,
            j.source
        )),
        Some(_) => {}
    }
    if gradle.is_none() {
        missing.push("Gradle".to_owned());
    }
    for abi in &cfg.abis {
        if !tools::has_target(&ctx.exec, project.dir(), abi.rust_target()) {
            missing.push(format!("rust target {}", abi.rust_target()));
        }
    }
    if !missing.is_empty() {
        let msg = format!("missing: {}\n\n{}", missing.join(", "), android::setup_instructions());
        if !ctx.exec.dry_run {
            bail!(msg);
        }
        eprintln!("[dry-run] {msg}");
    }
    let sdk_dir = sdk.map_or_else(|| PathBuf::from("<android sdk>"), |s| s.dir);
    let ndk_dir = ndk.unwrap_or_else(|| sdk_dir.join("ndk").join("<version>"));
    let java_home = java.and_then(|j| j.home);
    // Only Gradle gets JAVA_HOME: build scripts that watch it would rebuild otherwise.
    let with_env = |cmd: Cmd| cmd.env("ANDROID_HOME", &sdk_dir).env("ANDROID_NDK_HOME", &ndk_dir);

    // 1. The native library for every ABI: target/<triple>/<profile>/lib<name>.so.
    let work = project.target_dir.join("android").join(&game.name);
    let mut cargo = Cmd::cargo("ndk").cwd(project.dir());
    for abi in &cfg.abis {
        cargo = cargo.args(["-t", abi.name()]);
    }
    cargo = cargo
        .args(["-P", &cfg.min_sdk.to_string()])
        .arg(if lib.force_cdylib { "rustc" } else { "build" })
        .args(["-p", &lib.package.name, "--lib"])
        .arg_if(sel.release, "--release")
        .opt("--features", sel.features.as_deref());
    if lib.force_cdylib {
        cargo = cargo.args(["--crate-type", "cdylib"]);
    }
    ctx.exec.run(&with_env(cargo))?;

    // 2. jniLibs/<abi>/lib<name>.so for Gradle (only the current libraries).
    let jni_libs = work.join("jniLibs");
    let so_name = format!("lib{}.so", lib.lib_name);
    if !ctx.exec.dry_run && jni_libs.exists() {
        std::fs::remove_dir_all(&jni_libs).with_context(|| format!("cleaning {}", jni_libs.display()))?;
    }
    for abi in &cfg.abis {
        let built = project
            .target_dir
            .join(abi.rust_target())
            .join(profile_dir(sel.release))
            .join(&so_name);
        stage(ctx, &built, &jni_libs.join(abi.name()).join(&so_name))?;
    }

    // 3. The Gradle project: the package's own `android/`, else generated from the metadata.
    if own_project.is_none() {
        let files = android::gradle_project(
            &cfg,
            &lib.lib_name,
            &cfg.apk_name,
            &GradleDefaults {
                jni_libs: "../jniLibs".into(),
                assets: cfg.assets.as_ref().map(|p| p.display().to_string()),
            },
        );
        if ctx.exec.dry_run {
            eprintln!("[dry-run] write the Gradle project to {}", gradle_dir.display());
        } else {
            android::write_files(&gradle_dir, &files)?;
            let icon_dest = gradle_dir.join("src/main/res/mipmap/ic_launcher.png");
            match &cfg.icon {
                Some(icon) => stage(ctx, icon, &icon_dest)?,
                None => {
                    let _ = std::fs::remove_file(&icon_dest);
                }
            }
        }
    }

    // 4. Gradle: APK or App Bundle, all output below target/android/<package>/.
    let task = android::gradle_task(sel.release, args.aab);
    let build_dir = work.join("build");
    let (kind, extension) = if args.aab { ("bundle", "aab") } else { ("apk", "apk") };
    let variant = if sel.release { "release" } else { "debug" };
    let outputs_dir = build_dir.join("outputs").join(kind).join(variant);
    // AGP updates an existing APK in place and never shrinks it (a replaced library leaves its
    // old bytes behind): start from scratch so the APK is only as large as its contents.
    if !ctx.exec.dry_run {
        for old in android::outputs(&outputs_dir, extension) {
            std::fs::remove_file(&old).with_context(|| format!("removing {}", old.display()))?;
        }
    }
    let mut gradle_cmd = Cmd::new(gradle.unwrap_or_else(|| PathBuf::from("gradle")))
        .arg("-p")
        .arg(&gradle_dir)
        .arg("--project-cache-dir")
        .arg(work.join("gradle-cache"))
        .args(["--console=plain", "--warn"])
        .arg(&task)
        .arg(format!("-Pspace3d.jniLibs={}", jni_libs.display()))
        .arg(format!("-Pspace3d.buildDir={}", build_dir.display()))
        .arg(format!("-Pspace3d.ndkPath={}", ndk_dir.display()));
    if let Some(version) = android::ndk_version(&ndk_dir) {
        gradle_cmd = gradle_cmd.arg(format!("-Pspace3d.ndkVersion={version}"));
    }
    if let Some(assets) = &cfg.assets {
        gradle_cmd = gradle_cmd.arg(format!("-Pspace3d.assets={}", assets.display()));
    }
    if let Some(home) = &java_home {
        gradle_cmd = gradle_cmd.env("JAVA_HOME", home);
    }
    ctx.exec.run(&with_env(gradle_cmd))?;

    // 5. dist/android/<apk_name>.apk (or .aab).
    let artifact = out.join(format!("{}.{extension}", cfg.apk_name));
    if ctx.exec.dry_run {
        stage(ctx, &outputs_dir.join(format!("<built>.{extension}")), &artifact)?;
    } else {
        let built = android::outputs(&outputs_dir, extension);
        let Some(built) = built.first() else {
            bail!("Gradle produced no .{extension} in {}", outputs_dir.display());
        };
        stage(ctx, built, &artifact)?;
        let signing = if !sel.release {
            "debug key"
        } else if std::env::var_os("SPACE3D_ANDROID_KEYSTORE").is_some() {
            "release key from SPACE3D_ANDROID_KEYSTORE"
        } else {
            "debug key: set SPACE3D_ANDROID_KEYSTORE for store builds"
        };
        eprintln!(
            "{}: {} (application id {}, {}, signed with the {signing})",
            if args.aab { "App Bundle" } else { "APK" },
            artifact.display(),
            cfg.application_id,
            cfg.abis.iter().map(|a| a.name()).collect::<Vec<_>>().join(", "),
        );
        if !args.aab {
            eprintln!(
                "install and start it:\n  adb install -r '{}'\n  adb shell am start -n {}/android.app.NativeActivity\n  adb logcat -s space3d   # the game's log",
                artifact.display(),
                cfg.application_id
            );
        }
    }
    finish(ctx, "android", out);
    Ok(())
}

// ------------------------------------------------------------------------------------ ios

fn ios(ctx: &Ctx, project: &Project, sel: &CargoSel, sim: bool, out: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") && !ctx.exec.dry_run {
        bail!("iOS builds need macOS with Xcode");
    }
    let Some(pkg) = project.mobile_package("staticlib") else {
        bail!(
            "no package builds a `staticlib` for iOS.\n\
             Projects created by `space3d new` have `mobile/` (package `{0}-mobile`); otherwise add\n\
             [lib] crate-type = [\"lib\", \"staticlib\"] and `space3d::main!` to your library.",
            project.package().name
        );
    };
    let triple = if sim { IOS_SIM_TARGET } else { IOS_TARGET };
    if !tools::has_target(&ctx.exec, project.dir(), triple) {
        let msg =
            format!("the {triple} target is not installed; run:\n    rustup target add {IOS_TARGET} {IOS_SIM_TARGET}");
        if !ctx.exec.dry_run {
            bail!(msg);
        }
        eprintln!("[dry-run] {msg}");
    }
    let xcode = tools::xcode_version(&ctx.exec);
    if xcode.is_none() {
        let msg = "Xcode is not installed or not selected (C dependencies need the iOS SDK).\n\
                   Install Xcode, then: sudo xcode-select -s /Applications/Xcode.app/Contents/Developer";
        if !ctx.exec.dry_run {
            bail!(msg);
        }
        eprintln!("[dry-run] {msg}");
    }
    let cmd = Cmd::cargo("build")
        .cwd(project.dir())
        .arg_if(sel.release, "--release")
        .args(["-p", &pkg.name, "--lib", "--target", triple])
        .opt("--features", sel.features.as_deref());
    ctx.exec.run(&cmd)?;
    let lib_name = pkg.lib().map_or_else(|| pkg.name.replace('-', "_"), |l| l.name.clone());
    let staticlib = project
        .target_dir
        .join(triple)
        .join(profile_dir(sel.release))
        .join(format!("lib{lib_name}.a"));
    stage(
        ctx,
        &staticlib,
        &out.join(triple).join(staticlib.file_name().unwrap_or_default()),
    )?;

    let ios_dir = project.dir().join("ios");
    let spec = ios_dir.join("project.yml");
    let app_name = xcodegen_project_name(&spec).unwrap_or_else(|| project.package().name.clone());
    let xcodeproj = ios_dir.join(format!("{app_name}.xcodeproj"));
    let configuration = if sel.release { "Release" } else { "Debug" };
    let sdk = if sim { "iphonesimulator" } else { "iphoneos" };
    let xcodebuild = Cmd::new("xcodebuild")
        .arg("-project")
        .arg(&xcodeproj)
        .args(["-scheme", &app_name, "-configuration", configuration, "-sdk", sdk])
        .arg("-derivedDataPath")
        .arg(project.target_dir.join("xcode"))
        .args(["CODE_SIGNING_ALLOWED=NO", "build"]);
    if spec.is_file() && which("xcodegen").is_some() {
        ctx.exec.run(
            &Cmd::new("xcodegen")
                .args(["generate", "--spec"])
                .arg(&spec)
                .cwd(&ios_dir),
        )?;
    }
    if xcodeproj.exists() && xcode.is_some() {
        ctx.exec.run(&xcodebuild)?;
        let app = project
            .target_dir
            .join("xcode/Build/Products")
            .join(format!("{configuration}-{sdk}"))
            .join(format!("{app_name}.app"));
        eprintln!("app bundle: {}", app.display());
        if sim {
            let bundle_id = yaml_value(&spec, "PRODUCT_BUNDLE_IDENTIFIER").unwrap_or_else(|| "<bundle id>".into());
            eprintln!(
                "run it in the simulator:\n  open -a Simulator\n  xcrun simctl install booted '{}'\n  xcrun simctl launch booted {bundle_id}",
                app.display(),
            );
        }
    } else {
        eprintln!(
            "\nNext: create the Xcode project and build the app\n  \
             brew install xcodegen && (cd ios && xcodegen generate)   # from ios/project.yml\n  \
             {}\n\
             or open {} in Xcode (set your signing team to run on a device).",
            xcodebuild.display(),
            xcodeproj.display()
        );
    }
    finish(ctx, "ios", out);
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
