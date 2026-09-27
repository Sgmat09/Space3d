//! `space3d build --target ios`: an iOS `.app` for the simulator (`--sim`) or for devices.
//!
//! Two ways to package the game:
//!
//! * **Plain bundle** (default): `cargo build --bin <game> --target aarch64-apple-ios[-sim]`,
//!   then the CLI assembles `<game>.app` itself — the executable, a generated `Info.plist`,
//!   `assets/` and, for the simulator, an ad-hoc signature. `space3d::main!` makes the game
//!   binary's `main` start winit's UIKit event loop (`UIApplicationMain`), so there is no
//!   Objective-C, no Xcode project and no tool besides Xcode's iOS SDK. It works for every
//!   package with a game binary (the engine's examples included), rebuilds incrementally in
//!   seconds and is what `scripts/ios-smoke.sh` runs.
//! * **Xcode project** (`--xcode`): the `staticlib` of the `mobile/` crate linked into the app
//!   described by `ios/project.yml` (XcodeGen; projects made by `space3d new`) and built with
//!   `xcodebuild`. Use it to debug on a device in Xcode and to manage signing, capabilities,
//!   icons and launch storyboards there.
//!
//! Bundle settings come from `[package.metadata.space3d.ios]`:
//!
//! ```toml
//! [package.metadata.space3d.ios]
//! bundle-id = "com.example.my-game"   # default: from ios/project.yml, else com.example.<package>
//! display-name = "My Game"            # default: the package name
//! orientations = "landscape"          # "all" (default), "landscape" or "portrait"
//! deployment-target = "16.0"          # minimum iOS (default: $IPHONEOS_DEPLOYMENT_TARGET, else 16.0)
//! ```

use super::profile_dir;
use crate::Ctx;
use crate::args::CargoSel;
use crate::process::{Cmd, which};
use crate::project::{PackageInfo, Project};
use crate::tools::{self, IOS_SIM_TARGET, IOS_TARGET};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Build the iOS app into `out/<triple>/`; returns the `.app` path.
pub fn build(ctx: &Ctx, project: &Project, sel: &CargoSel, sim: bool, xcode: bool, out: &Path) -> Result<PathBuf> {
    if !cfg!(target_os = "macos") && !ctx.exec.dry_run {
        bail!("iOS builds need macOS with Xcode");
    }
    let triple = if sim { IOS_SIM_TARGET } else { IOS_TARGET };
    let out = out.join(triple);
    if xcode {
        xcode_project(ctx, project, sel, sim, &out)
    } else {
        bundle(ctx, project, sel, sim, &out)
    }
}

/// Setup problems for building `triple`, as one message (`None`: ready).
fn setup_problem(ctx: &Ctx, project: &Project, triple: &str) -> Option<String> {
    let mut problems = Vec::new();
    if tools::xcode_version(&ctx.exec).is_none() {
        problems.push(
            "Xcode is not installed or not selected (the iOS SDK comes with Xcode; the Command Line Tools \
             alone are not enough). Install Xcode, then:\n    sudo xcode-select -s /Applications/Xcode.app/Contents/Developer"
                .to_owned(),
        );
    }
    if !tools::has_target(&ctx.exec, project.dir(), triple) {
        problems.push(format!(
            "the {triple} target is not installed; run:\n    rustup target add {IOS_TARGET} {IOS_SIM_TARGET}"
        ));
    }
    (!problems.is_empty()).then(|| problems.join("\n"))
}

// ------------------------------------------------------------------------------------ bundle

fn bundle(ctx: &Ctx, project: &Project, sel: &CargoSel, sim: bool, out: &Path) -> Result<PathBuf> {
    let triple = if sim { IOS_SIM_TARGET } else { IOS_TARGET };
    if let Some(problem) = setup_problem(ctx, project, triple) {
        if !ctx.exec.dry_run {
            bail!(problem);
        }
        eprintln!("[dry-run] {problem}");
    }
    let pkg = project.package();
    let bin = pkg.main_bin(sel.bin.as_deref())?;
    let meta = IosMeta::from_package(pkg)?;
    let deployment = deployment_target(meta.deployment_target.as_deref());

    let cargo = super::cargo_flags(
        Cmd::cargo("build").cwd(project.dir()),
        &super::build::with_package(sel, project),
        Some(&bin),
    )
    .args(["--target", triple])
    .env(DEPLOYMENT_TARGET_ENV, &deployment);
    ctx.exec.run(&cargo)?;

    let exe = project
        .target_dir
        .join(triple)
        .join(profile_dir(sel.release))
        .join(&bin);
    let app = out.join(format!("{bin}.app"));
    let sdk = if sim { "iphonesimulator" } else { "iphoneos" };
    let info = InfoPlist {
        executable: bin.clone(),
        bundle_id: meta
            .bundle_id
            .clone()
            .or_else(|| super::build::yaml_value(&project.dir().join("ios/project.yml"), "PRODUCT_BUNDLE_IDENTIFIER"))
            .unwrap_or_else(|| default_bundle_id(&pkg.name)),
        display_name: meta.display_name.clone().unwrap_or_else(|| pkg.name.clone()),
        version: pkg.version.clone(),
        minimum_os: deployment,
        orientations: meta.orientations,
        sim,
        sdk_version: sdk_probe(ctx, sdk, "--show-sdk-version"),
        sdk_build: sdk_probe(ctx, sdk, "--show-sdk-build-version"),
    };

    if ctx.exec.dry_run {
        eprintln!("[dry-run] assemble {} (Info.plist, {bin}, assets/)", app.display());
    } else {
        if app.exists() {
            std::fs::remove_dir_all(&app).with_context(|| format!("removing the old {}", app.display()))?;
        }
        std::fs::create_dir_all(&app).with_context(|| format!("creating {}", app.display()))?;
        std::fs::copy(&exe, app.join(&bin)).with_context(|| format!("copying {}", exe.display()))?;
        std::fs::write(app.join("Info.plist"), info_plist(&info))?;
        std::fs::write(app.join("PkgInfo"), "APPL????")?;
        let assets = project.dir().join("assets");
        if assets.is_dir() {
            super::build::copy_dir_all(&assets, &app.join("assets"))?;
        }
    }
    if sim {
        // Apple silicon runs only signed code; the simulator accepts an ad-hoc signature.
        ctx.exec.run(
            &Cmd::new("codesign")
                .args(["--force", "--sign", "-", "--timestamp=none"])
                .arg(&app),
        )?;
    }

    eprintln!("app bundle: {}", app.display());
    if sim {
        eprintln!(
            "run it in a simulator (headless: no Simulator.app needed):\n  \
             xcrun simctl boot '<device>'   # see `xcrun simctl list devices`\n  \
             xcrun simctl install '<device>' '{0}'\n  \
             xcrun simctl launch --console-pty '<device>' {1}\n\
             or run scripts/ios-smoke.sh from a Space3d checkout.",
            app.display(),
            info.bundle_id
        );
    } else {
        eprintln!("{}", device_signing_help(&app, &info.bundle_id));
    }
    if !ctx.exec.dry_run {
        eprintln!("ios build ready in {}", out.display());
    }
    Ok(app)
}

/// Env var that sets the minimum iOS version for rustc and the C compilers.
pub const DEPLOYMENT_TARGET_ENV: &str = "IPHONEOS_DEPLOYMENT_TARGET";

/// Minimum iOS version when neither `deployment-target` nor `IPHONEOS_DEPLOYMENT_TARGET`
/// sets one. rustc's own default for `aarch64-apple-ios` (10.0) is older than any Xcode
/// supports: the C dependencies (built for the SDK's version) then fail to link
/// (`___chkstk_darwin` undefined). Also the `space3d new` Xcode project's deployment target.
pub const DEFAULT_DEPLOYMENT_TARGET: &str = "16.0";

/// The minimum iOS version to build for: `metadata` (`deployment-target`), else a non-empty
/// `IPHONEOS_DEPLOYMENT_TARGET`, else [`DEFAULT_DEPLOYMENT_TARGET`]. rustc and cc (C
/// dependencies) both read `IPHONEOS_DEPLOYMENT_TARGET`, so the CLI always sets it.
pub fn deployment_target(metadata: Option<&str>) -> String {
    metadata
        .map(str::to_owned)
        .or_else(|| {
            std::env::var(DEPLOYMENT_TARGET_ENV)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| is_version(v))
        })
        .unwrap_or_else(|| DEFAULT_DEPLOYMENT_TARGET.to_owned())
}

/// `16`, `16.0`, `16.4.1`.
pub fn is_version(v: &str) -> bool {
    !v.is_empty()
        && v.split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn sdk_probe(ctx: &Ctx, sdk: &str, flag: &str) -> Option<String> {
    ctx.exec
        .probe(&Cmd::new("xcrun").args(["--sdk", sdk, flag]))
        .map(|s| tools::first_line(&s))
        .filter(|s| !s.is_empty())
}

/// What to do with an unsigned device bundle.
pub fn device_signing_help(app: &Path, bundle_id: &str) -> String {
    let app = app.display();
    format!(
        "The device bundle is unsigned. To install it on an iPhone/iPad, sign it with an Apple Development\n\
         identity and a provisioning profile that covers `{bundle_id}` (Xcode > Settings > Accounts):\n  \
         security find-identity -v -p codesigning            # your identities\n  \
         cp <profile>.mobileprovision '{app}/embedded.mobileprovision'\n  \
         security cms -D -i <profile>.mobileprovision > profile.plist\n  \
         /usr/libexec/PlistBuddy -x -c 'Print :Entitlements' profile.plist > entitlements.plist\n  \
         codesign --force --sign '<identity>' --entitlements entitlements.plist '{app}'\n  \
         xcrun devicectl device install app --device <device id> '{app}'\n\
         Or build with --xcode and let Xcode manage signing (docs/platforms.md)."
    )
}

// ------------------------------------------------------------------------------------ metadata

/// Supported interface orientations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Orientations {
    /// Portrait and both landscapes (iPad: also upside down).
    #[default]
    All,
    /// Both landscapes.
    Landscape,
    /// Portrait (iPad: also upside down).
    Portrait,
}

impl Orientations {
    /// Parse `all` / `landscape` / `portrait`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "landscape" => Some(Self::Landscape),
            "portrait" => Some(Self::Portrait),
            _ => None,
        }
    }

    /// `UISupportedInterfaceOrientations` values (`ipad`: the `~ipad` variant).
    pub fn values(self, ipad: bool) -> Vec<&'static str> {
        const PORTRAIT: &str = "UIInterfaceOrientationPortrait";
        const UPSIDE_DOWN: &str = "UIInterfaceOrientationPortraitUpsideDown";
        const LEFT: &str = "UIInterfaceOrientationLandscapeLeft";
        const RIGHT: &str = "UIInterfaceOrientationLandscapeRight";
        let mut v = match self {
            Self::All => vec![PORTRAIT, LEFT, RIGHT],
            Self::Landscape => vec![LEFT, RIGHT],
            Self::Portrait => vec![PORTRAIT],
        };
        if ipad && self != Self::Landscape {
            v.insert(1, UPSIDE_DOWN);
        }
        v
    }
}

/// `[package.metadata.space3d.ios]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IosMeta {
    /// `bundle-id`.
    pub bundle_id: Option<String>,
    /// `display-name` (home screen label).
    pub display_name: Option<String>,
    /// `orientations`.
    pub orientations: Orientations,
    /// `deployment-target` (minimum iOS version).
    pub deployment_target: Option<String>,
}

impl IosMeta {
    /// Read the package's `[package.metadata.space3d.ios]` (missing table: defaults).
    pub fn from_package(pkg: &PackageInfo) -> Result<Self> {
        Self::from_metadata(&pkg.metadata).with_context(|| format!("[package.metadata.space3d.ios] of `{}`", pkg.name))
    }

    /// Parse from a package's `metadata` JSON (`cargo metadata`).
    pub fn from_metadata(metadata: &serde_json::Value) -> Result<Self> {
        let Some(ios) = metadata.get("space3d").and_then(|s| s.get("ios")) else {
            return Ok(Self::default());
        };
        let text = |key: &str| -> Result<Option<String>> {
            match ios.get(key) {
                None => Ok(None),
                Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
                Some(v) => bail!("`{key}` must be a non-empty string, not {v}"),
            }
        };
        let bundle_id = text("bundle-id")?;
        if let Some(id) = &bundle_id
            && !is_valid_bundle_id(id)
        {
            bail!("`bundle-id` {id:?}: use letters, digits, `-` and `.` (e.g. com.example.my-game)");
        }
        let orientations = match text("orientations")? {
            None => Orientations::All,
            Some(s) => Orientations::parse(&s)
                .with_context(|| format!("`orientations` {s:?}: expected \"all\", \"landscape\" or \"portrait\""))?,
        };
        let deployment_target = text("deployment-target")?;
        if let Some(v) = &deployment_target
            && !is_version(v)
        {
            bail!("`deployment-target` {v:?}: expected a version such as \"16.0\"");
        }
        Ok(Self {
            bundle_id,
            display_name: text("display-name")?,
            orientations,
            deployment_target,
        })
    }
}

/// `true` for a usable `CFBundleIdentifier` (letters, digits, `-`, `.`; no empty parts).
pub fn is_valid_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// `com.example.<package>` with `_` turned into `-` (bundle ids allow no underscores).
pub fn default_bundle_id(package: &str) -> String {
    let name: String = package
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    format!("com.example.{}", name.trim_matches('-'))
}

/// `CFBundleVersion` / `CFBundleShortVersionString` from a crate version: the numeric
/// `MAJOR.MINOR.PATCH` part (`1.2.0-beta.1` → `1.2.0`).
pub fn bundle_version(version: &str) -> String {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let parts: Vec<&str> = core
        .split('.')
        .take(3)
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if parts.is_empty() {
        "1.0".into()
    } else {
        parts.join(".")
    }
}

// ------------------------------------------------------------------------------------ Info.plist

/// Everything the generated `Info.plist` needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoPlist {
    /// `CFBundleExecutable` (the binary's file name in the bundle).
    pub executable: String,
    /// `CFBundleIdentifier`.
    pub bundle_id: String,
    /// `CFBundleDisplayName`.
    pub display_name: String,
    /// Crate version (made numeric by [`bundle_version`]).
    pub version: String,
    /// `MinimumOSVersion`.
    pub minimum_os: String,
    /// `UISupportedInterfaceOrientations`.
    pub orientations: Orientations,
    /// Simulator (`iPhoneSimulator`) or device (`iPhoneOS`) bundle.
    pub sim: bool,
    /// iOS SDK version (`DTPlatformVersion`, `DTSDKName`), when known.
    pub sdk_version: Option<String>,
    /// iOS SDK build (`DTSDKBuild`), when known.
    pub sdk_build: Option<String>,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Objective-C class of the scene delegate that `space3d-platform` registers
/// (`space3d_platform::IOS_SCENE_DELEGATE_CLASS`; the CLI does not link the engine).
pub const SCENE_DELEGATE_CLASS: &str = "Space3dSceneDelegate";

/// `UIApplicationSceneManifest`: one window scene whose delegate is [`SCENE_DELEGATE_CLASS`].
/// Apps linked against the iOS 27 SDK must adopt the UIScene life cycle — UIKit stops an app
/// without a scene configuration at launch (`EXC_BREAKPOINT` in
/// `_UIApplicationEvaluateRuntimeIssueForNoSceneLifecycleAdoption`).
fn scene_manifest() -> String {
    format!(
        "<dict>\n\
         \t\t<key>UIApplicationSupportsMultipleScenes</key>\n\t\t<false/>\n\
         \t\t<key>UISceneConfigurations</key>\n\
         \t\t<dict>\n\
         \t\t\t<key>UIWindowSceneSessionRoleApplication</key>\n\
         \t\t\t<array>\n\
         \t\t\t\t<dict>\n\
         \t\t\t\t\t<key>UISceneConfigurationName</key>\n\t\t\t\t\t<string>Default Configuration</string>\n\
         \t\t\t\t\t<key>UISceneDelegateClassName</key>\n\t\t\t\t\t<string>{SCENE_DELEGATE_CLASS}</string>\n\
         \t\t\t\t</dict>\n\
         \t\t\t</array>\n\
         \t\t</dict>\n\
         \t</dict>"
    )
}

/// The XML `Info.plist` of a plain bundle.
///
/// `UILaunchScreen` makes iOS run the app at the display's native size (without it, iOS
/// letterboxes the app in a legacy 320×480 mode); the scene manifest adopts the UIScene life
/// cycle ([`scene_manifest`]); the status bar is hidden and the app is full screen (no iPad
/// split view), as games expect.
pub fn info_plist(p: &InfoPlist) -> String {
    let mut entries: Vec<(&str, String)> = Vec::new();
    let s = |v: &str| format!("<string>{}</string>", xml_escape(v));
    let array = |items: &[&str]| {
        let inner: String = items
            .iter()
            .map(|i| format!("\n\t\t<string>{}</string>", xml_escape(i)))
            .collect();
        format!("<array>{inner}\n\t</array>")
    };
    let (platform, sdk) = if p.sim {
        ("iPhoneSimulator", "iphonesimulator")
    } else {
        ("iPhoneOS", "iphoneos")
    };
    let version = bundle_version(&p.version);
    // CFBundleName is limited to 15 characters.
    let short_name: String = p.display_name.chars().take(15).collect();
    entries.push(("CFBundleDevelopmentRegion", s("en")));
    entries.push(("CFBundleDisplayName", s(&p.display_name)));
    entries.push(("CFBundleExecutable", s(&p.executable)));
    entries.push(("CFBundleIdentifier", s(&p.bundle_id)));
    entries.push(("CFBundleInfoDictionaryVersion", s("6.0")));
    entries.push(("CFBundleName", s(&short_name)));
    entries.push(("CFBundlePackageType", s("APPL")));
    entries.push(("CFBundleShortVersionString", s(&version)));
    entries.push(("CFBundleSupportedPlatforms", array(&[platform])));
    entries.push(("CFBundleVersion", s(&version)));
    entries.push(("DTPlatformName", s(sdk)));
    if let Some(v) = &p.sdk_version {
        entries.push(("DTPlatformVersion", s(v)));
        entries.push(("DTSDKName", s(&format!("{sdk}{v}"))));
    }
    if let Some(b) = &p.sdk_build {
        entries.push(("DTSDKBuild", s(b)));
    }
    entries.push(("LSRequiresIPhoneOS", "<true/>".into()));
    entries.push(("MinimumOSVersion", s(&p.minimum_os)));
    entries.push(("UIApplicationSceneManifest", scene_manifest()));
    entries.push((
        "UIDeviceFamily",
        "<array>\n\t\t<integer>1</integer>\n\t\t<integer>2</integer>\n\t</array>".into(),
    ));
    entries.push(("UILaunchScreen", "<dict/>".into()));
    entries.push(("UIRequiresFullScreen", "<true/>".into()));
    entries.push(("UIStatusBarHidden", "<true/>".into()));
    entries.push(("UISupportedInterfaceOrientations", array(&p.orientations.values(false))));
    entries.push((
        "UISupportedInterfaceOrientations~ipad",
        array(&p.orientations.values(true)),
    ));
    let body: String = entries
        .iter()
        .map(|(k, v)| format!("\t<key>{k}</key>\n\t{v}\n"))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n{body}</dict>\n</plist>\n"
    )
}

// ------------------------------------------------------------------------------------ Xcode

/// `--xcode`: build the `mobile/` staticlib, generate the Xcode project from `ios/project.yml`
/// and build it with `xcodebuild`; copy the `.app` to `out`.
fn xcode_project(ctx: &Ctx, project: &Project, sel: &CargoSel, sim: bool, out: &Path) -> Result<PathBuf> {
    let Some(pkg) = project.mobile_package("staticlib") else {
        bail!(
            "--xcode needs a package that builds a `staticlib` (linked by the Xcode project).\n\
             Projects created by `space3d new` have `mobile/` (package `{0}-mobile`) and `ios/project.yml`;\n\
             without them, build a plain bundle instead: space3d build --target ios{1}",
            project.package().name,
            if sim { " --sim" } else { "" }
        );
    };
    let triple = if sim { IOS_SIM_TARGET } else { IOS_TARGET };
    if let Some(problem) = setup_problem(ctx, project, triple) {
        if !ctx.exec.dry_run {
            bail!(problem);
        }
        eprintln!("[dry-run] {problem}");
    }
    let ios_dir = project.dir().join("ios");
    let spec = ios_dir.join("project.yml");
    let app_name = super::build::xcodegen_project_name(&spec).unwrap_or_else(|| project.package().name.clone());
    let xcodeproj = ios_dir.join(format!("{app_name}.xcodeproj"));
    if spec.is_file() {
        if which("xcodegen").is_none() && !ctx.exec.dry_run {
            bail!(
                "XcodeGen is needed to generate {} from {}:\n    brew install xcodegen",
                xcodeproj.display(),
                spec.display()
            );
        }
        ctx.exec.run(
            &Cmd::new("xcodegen")
                .args(["generate", "--spec"])
                .arg(&spec)
                .cwd(&ios_dir),
        )?;
    } else if !xcodeproj.exists() && !ctx.exec.dry_run {
        bail!(
            "--xcode needs ios/project.yml (XcodeGen) or {}; without them, build a plain bundle: \
             space3d build --target ios{}",
            xcodeproj.display(),
            if sim { " --sim" } else { "" }
        );
    }
    // The project's pre-build phase builds the staticlib for the SDK being built; building it
    // here first gives cargo's own error output when Rust code does not compile. Only the
    // staticlib: the mobile crate's cdylib is for Android.
    ctx.exec.run(
        &Cmd::cargo("rustc")
            .cwd(project.dir())
            .arg_if(sel.release, "--release")
            .args([
                "-p",
                &pkg.name,
                "--lib",
                "--crate-type",
                "staticlib",
                "--target",
                triple,
            ])
            .opt("--features", sel.features.as_deref())
            .env(DEPLOYMENT_TARGET_ENV, deployment_target(None)),
    )?;
    let configuration = if sel.release { "Release" } else { "Debug" };
    let sdk = if sim { "iphonesimulator" } else { "iphoneos" };
    let derived = project.target_dir.join("xcode");
    ctx.exec.run(
        &Cmd::new("xcodebuild")
            .arg("-project")
            .arg(&xcodeproj)
            .args(["-scheme", &app_name, "-configuration", configuration, "-sdk", sdk])
            .arg("-derivedDataPath")
            .arg(&derived)
            .args(["CODE_SIGNING_ALLOWED=NO", "build"]),
    )?;
    let built = derived
        .join("Build/Products")
        .join(format!("{configuration}-{sdk}"))
        .join(format!("{app_name}.app"));
    let app = out.join(format!("{app_name}.app"));
    if ctx.exec.dry_run {
        eprintln!("[dry-run] copy {} -> {}", built.display(), app.display());
    } else {
        if app.exists() {
            std::fs::remove_dir_all(&app)?;
        }
        super::build::copy_dir_all(&built, &app)?;
    }
    if sim {
        ctx.exec.run(
            &Cmd::new("codesign")
                .args(["--force", "--sign", "-", "--timestamp=none"])
                .arg(&app),
        )?;
    }
    eprintln!("app bundle: {}", app.display());
    let bundle_id =
        super::build::yaml_value(&spec, "PRODUCT_BUNDLE_IDENTIFIER").unwrap_or_else(|| "<bundle id>".into());
    if sim {
        eprintln!(
            "run it in a simulator:\n  xcrun simctl install '<device>' '{}'\n  xcrun simctl launch --console-pty '<device>' {bundle_id}",
            app.display()
        );
    } else {
        eprintln!("open {} in Xcode to sign and run it on a device", xcodeproj.display());
    }
    if !ctx.exec.dry_run {
        eprintln!("ios build ready in {}", out.display());
    }
    Ok(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(sim: bool) -> InfoPlist {
        InfoPlist {
            executable: "hello_cube".into(),
            bundle_id: "com.example.hello-cube".into(),
            display_name: "Hello & <Cube>".into(),
            version: "0.1.0-beta.2".into(),
            minimum_os: "14.0".into(),
            orientations: Orientations::Landscape,
            sim,
            sdk_version: Some("27.0".into()),
            sdk_build: None,
        }
    }

    #[test]
    fn plist_has_the_keys_ios_needs() {
        let plist = info_plist(&info(true));
        for needle in [
            "<key>CFBundleExecutable</key>\n\t<string>hello_cube</string>",
            "<key>CFBundleIdentifier</key>\n\t<string>com.example.hello-cube</string>",
            "<key>CFBundlePackageType</key>\n\t<string>APPL</string>",
            "<key>CFBundleVersion</key>\n\t<string>0.1.0</string>",
            "<string>iPhoneSimulator</string>",
            "<key>DTSDKName</key>\n\t<string>iphonesimulator27.0</string>",
            "<key>MinimumOSVersion</key>\n\t<string>14.0</string>",
            "<key>UILaunchScreen</key>\n\t<dict/>",
            "<key>LSRequiresIPhoneOS</key>\n\t<true/>",
            "Hello &amp; &lt;Cube&gt;",
            "<key>UISceneDelegateClassName</key>\n\t\t\t\t\t<string>Space3dSceneDelegate</string>",
            "<key>UIApplicationSupportsMultipleScenes</key>\n\t\t<false/>",
        ] {
            assert!(plist.contains(needle), "missing {needle:?} in\n{plist}");
        }
        assert!(!plist.contains("DTSDKBuild"), "unknown SDK build is left out");
        assert!(!plist.contains("UpsideDown"), "landscape only");
        assert!(plist.starts_with("<?xml") && plist.trim_end().ends_with("</plist>"));
        // Balanced tags: a plist that doesn't parse makes `simctl install` fail.
        assert_eq!(plist.matches("<dict>").count(), plist.matches("</dict>").count());
        assert_eq!(plist.matches("<array>").count(), plist.matches("</array>").count());

        let device = info_plist(&info(false));
        assert!(device.contains("<string>iPhoneOS</string>") && device.contains("<string>iphoneos</string>"));
    }

    #[test]
    fn orientations() {
        assert_eq!(Orientations::parse(" Landscape"), Some(Orientations::Landscape));
        assert_eq!(Orientations::parse("sideways"), None);
        assert_eq!(Orientations::All.values(false).len(), 3);
        assert_eq!(Orientations::All.values(true).len(), 4);
        assert_eq!(
            Orientations::Portrait.values(true),
            [
                "UIInterfaceOrientationPortrait",
                "UIInterfaceOrientationPortraitUpsideDown"
            ]
        );
        assert_eq!(Orientations::Landscape.values(true).len(), 2);
    }

    #[test]
    fn metadata_is_parsed_and_validated() {
        assert_eq!(
            IosMeta::from_metadata(&serde_json::Value::Null).unwrap(),
            IosMeta::default()
        );
        let meta = IosMeta::from_metadata(&json!({"space3d": {"ios": {
            "bundle-id": "com.spacecorps.hello-cube",
            "display-name": "Hello Cube",
            "orientations": "portrait",
            "deployment-target": "16.0"
        }}}))
        .unwrap();
        assert_eq!(meta.bundle_id.as_deref(), Some("com.spacecorps.hello-cube"));
        assert_eq!(meta.display_name.as_deref(), Some("Hello Cube"));
        assert_eq!(meta.orientations, Orientations::Portrait);
        assert_eq!(meta.deployment_target.as_deref(), Some("16.0"));
        for bad in [
            json!({"space3d": {"ios": {"bundle-id": "com.example.my_game"}}}),
            json!({"space3d": {"ios": {"orientations": "sideways"}}}),
            json!({"space3d": {"ios": {"deployment-target": "latest"}}}),
            json!({"space3d": {"ios": {"display-name": 3}}}),
        ] {
            assert!(IosMeta::from_metadata(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ids_and_versions() {
        assert_eq!(default_bundle_id("hello_cube"), "com.example.hello-cube");
        assert!(is_valid_bundle_id(&default_bundle_id("graphics_showcase")));
        assert!(!is_valid_bundle_id("com..x") && !is_valid_bundle_id("") && !is_valid_bundle_id("a.b_c"));
        assert_eq!(bundle_version("1.2.3"), "1.2.3");
        assert_eq!(bundle_version("2.0.0-rc.1+meta"), "2.0.0");
        assert_eq!(bundle_version("x"), "1.0");
        assert!(is_version("16") && is_version("16.0") && is_version("16.4.1"));
        assert!(!is_version("") && !is_version("16.") && !is_version("v16") && !is_version("latest"));
        assert_eq!(deployment_target(Some("17.2")), "17.2");
        if std::env::var_os(DEPLOYMENT_TARGET_ENV).is_none() {
            assert_eq!(deployment_target(None), DEFAULT_DEPLOYMENT_TARGET);
        }
    }

    #[test]
    fn signing_help_names_the_bundle() {
        let help = device_signing_help(Path::new("/d/ios/game.app"), "com.example.game");
        assert!(help.contains("com.example.game") && help.contains("codesign --force --sign"));
        assert!(help.contains("'/d/ios/game.app/embedded.mobileprovision'"));
    }
}
