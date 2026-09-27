//! GitHub Actions workflow templates for Space3d game projects.
//!
//! [`github_actions`] renders one workflow (written to [`WORKFLOW_PATH`]) with:
//!
//! | job | when | what |
//! |---|---|---|
//! | `test` | always | fmt, clippy, `cargo test` incl. golden screenshot tests on a software GPU; uploads `*.actual.png`/`*.diff.png` on failure |
//! | `desktop` | any desktop [`Target`] | release build matrix (linux / windows / macos), zipped |
//! | `web` + `deploy-pages` | [`Target::Web`] | wasm32 build + `wasm-bindgen` bundle; deployed to GitHub Pages from `main` and tags |
//! | `android` | [`Target::Android`] | `.apk`: cargo-ndk + Gradle (the project's `android/`) |
//! | `ios` | [`Target::Ios`] | unsigned `.ipa` via `xcodebuild` on a macOS runner |
//! | `release` | tags `v*` | GitHub Release with every build attached |
//!
//! The workflow also has a `workflow_dispatch` trigger so it can be started
//! with [`crate::GithubClient::dispatch_workflow`].

use std::fmt;
use std::str::FromStr;

/// Where [`github_actions`] output belongs in a project.
pub const WORKFLOW_PATH: &str = ".github/workflows/space3d.yml";

/// A build target for the generated workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// Linux desktop (`x86_64-unknown-linux-gnu`, ubuntu runner).
    Linux,
    /// Windows desktop (`x86_64-pc-windows-msvc`, windows runner).
    Windows,
    /// macOS desktop (`aarch64-apple-darwin`, macos runner).
    MacOs,
    /// Browser (`wasm32-unknown-unknown`) + GitHub Pages.
    Web,
    /// Android `.apk` (`aarch64-linux-android`).
    Android,
    /// iOS `.ipa` (`aarch64-apple-ios`).
    Ios,
}

impl Target {
    /// Every target.
    pub const ALL: [Target; 6] = [
        Target::Linux,
        Target::Windows,
        Target::MacOs,
        Target::Web,
        Target::Android,
        Target::Ios,
    ];
    /// The three desktop targets.
    pub const DESKTOP: [Target; 3] = [Target::Linux, Target::Windows, Target::MacOs];

    /// Short lowercase name (`linux`, `windows`, `macos`, `web`, `android`, `ios`).
    pub fn name(self) -> &'static str {
        match self {
            Target::Linux => "linux",
            Target::Windows => "windows",
            Target::MacOs => "macos",
            Target::Web => "web",
            Target::Android => "android",
            Target::Ios => "ios",
        }
    }

    /// Rust target triple used by the workflow.
    pub fn rust_target(self) -> &'static str {
        match self {
            Target::Linux => "x86_64-unknown-linux-gnu",
            Target::Windows => "x86_64-pc-windows-msvc",
            Target::MacOs => "aarch64-apple-darwin",
            Target::Web => "wasm32-unknown-unknown",
            Target::Android => "aarch64-linux-android",
            Target::Ios => "aarch64-apple-ios",
        }
    }

    /// GitHub-hosted runner label.
    pub fn runner(self) -> &'static str {
        match self {
            Target::Windows => "windows-latest",
            Target::MacOs | Target::Ios => "macos-latest",
            Target::Linux | Target::Web | Target::Android => "ubuntu-latest",
        }
    }

    /// `true` for Linux / Windows / macOS.
    pub fn is_desktop(self) -> bool {
        matches!(self, Target::Linux | Target::Windows | Target::MacOs)
    }

    /// Parse a comma/space separated list; `desktop` and `all` expand.
    /// Duplicates are removed and the result is sorted.
    ///
    /// ```
    /// use space3d_github::Target;
    /// assert_eq!(Target::parse_list("web, desktop").unwrap().len(), 4);
    /// ```
    pub fn parse_list(s: &str) -> Result<Vec<Target>, ParseTargetError> {
        let mut out = Vec::new();
        for part in s.split([',', ' ']).map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "all" => out.extend(Target::ALL),
                "desktop" => out.extend(Target::DESKTOP),
                _ => out.push(part.parse()?),
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error for an unknown target name.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown target `{0}` (expected linux, windows, macos, web, android, ios, desktop or all)")]
pub struct ParseTargetError(pub String);

impl FromStr for Target {
    type Err = ParseTargetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "linux" => Target::Linux,
            "windows" | "win" => Target::Windows,
            "macos" | "mac" | "osx" => Target::MacOs,
            "web" | "wasm" | "wasm32" => Target::Web,
            "android" | "apk" => Target::Android,
            "ios" | "ipa" => Target::Ios,
            _ => return Err(ParseTargetError(s.to_owned())),
        })
    }
}

/// Knobs for [`github_actions_with`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateOptions {
    /// Cargo binary (and artifact) name. Default `"game"`.
    pub bin_name: String,
    /// Rust toolchain for `dtolnay/rust-toolchain`. Default `"stable"`.
    pub rust_toolchain: String,
    /// `wasm-bindgen-cli` version; must match the project's `wasm-bindgen` crate.
    pub wasm_bindgen_version: String,
    /// Deploy the web build to GitHub Pages. Default `true`.
    pub deploy_pages: bool,
    /// Create a GitHub Release with all builds on `v*` tags. Default `true`.
    pub release_on_tag: bool,
    /// Xcode project for iOS builds. Default `ios/<bin_name>.xcodeproj`.
    pub ios_project: Option<String>,
    /// Xcode scheme for iOS builds. Default `<bin_name>`.
    pub ios_scheme: Option<String>,
    /// Branch that deploys Pages. Default `main`.
    pub main_branch: String,
}

impl Default for TemplateOptions {
    fn default() -> Self {
        Self {
            bin_name: "game".into(),
            rust_toolchain: "stable".into(),
            wasm_bindgen_version: "0.2.128".into(),
            deploy_pages: true,
            release_on_tag: true,
            ios_project: None,
            ios_scheme: None,
            main_branch: "main".into(),
        }
    }
}

impl TemplateOptions {
    /// Defaults with a binary name.
    pub fn new(bin_name: impl Into<String>) -> Self {
        Self {
            bin_name: bin_name.into(),
            ..Default::default()
        }
    }
}

/// Render the workflow for `targets` with default [`TemplateOptions`].
/// Write the result to [`WORKFLOW_PATH`].
pub fn github_actions(targets: &[Target]) -> String {
    github_actions_with(targets, &TemplateOptions::default())
}

/// Render the workflow for `targets` with custom options.
pub fn github_actions_with(targets: &[Target], opts: &TemplateOptions) -> String {
    let has = |t: Target| targets.contains(&t);
    let desktop: Vec<Target> = Target::DESKTOP.into_iter().filter(|t| has(*t)).collect();

    let mut y = String::new();
    y.push_str(HEADER);
    y.push_str(TEST_JOB);

    let mut build_jobs: Vec<&str> = Vec::new();
    if !desktop.is_empty() {
        y.push_str(DESKTOP_JOB_HEAD);
        for t in &desktop {
            y.push_str(&format!(
                "          - platform: {}\n            os: {}\n            target: {}\n",
                t.name(),
                t.runner(),
                t.rust_target()
            ));
        }
        y.push_str(DESKTOP_JOB_STEPS);
        build_jobs.push("desktop");
    }
    if has(Target::Web) {
        y.push_str(WEB_JOB);
        if opts.deploy_pages {
            y.push_str(PAGES_STEP);
            y.push_str(PAGES_JOB);
        }
        build_jobs.push("web");
    }
    if has(Target::Android) {
        y.push_str(ANDROID_JOB);
        build_jobs.push("android");
    }
    if has(Target::Ios) {
        y.push_str(IOS_JOB);
        build_jobs.push("ios");
    }
    if opts.release_on_tag && !build_jobs.is_empty() {
        y.push_str(&RELEASE_JOB.replace("__NEEDS__", &build_jobs.join(", ")));
    }

    let ios_project = opts
        .ios_project
        .clone()
        .unwrap_or_else(|| format!("ios/{}.xcodeproj", opts.bin_name));
    let ios_scheme = opts.ios_scheme.clone().unwrap_or_else(|| opts.bin_name.clone());
    y.replace("__BIN__", &opts.bin_name)
        .replace("__TOOLCHAIN__", &opts.rust_toolchain)
        .replace("__WASM_BINDGEN__", &opts.wasm_bindgen_version)
        .replace("__IOS_PROJECT__", &ios_project)
        .replace("__IOS_SCHEME__", &ios_scheme)
        .replace("__MAIN__", &opts.main_branch)
        .replace("__GRADLE__", GRADLE_VERSION)
}

/// Gradle of the Android job (latest stable; the Android Gradle Plugin of `space3d new`'s
/// `android/` project runs on it).
pub const GRADLE_VERSION: &str = "9.8.0";

/// Write the workflow into `project_dir/.github/workflows/space3d.yml` (native only).
#[cfg(not(target_arch = "wasm32"))]
pub fn write_github_actions(
    project_dir: &std::path::Path,
    targets: &[Target],
    opts: &TemplateOptions,
) -> std::io::Result<std::path::PathBuf> {
    let path = project_dir.join(WORKFLOW_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, github_actions_with(targets, opts))?;
    Ok(path)
}

const HEADER: &str = r#"# Generated by space3d (`space3d_github::templates`). Edit freely.
name: Space3d CI

on:
  push:
    branches: [__MAIN__]
    tags: ["v*"]
  pull_request:
  workflow_dispatch:

permissions:
  contents: read

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}

env:
  CARGO_TERM_COLOR: always

jobs:
"#;

const TEST_JOB: &str = r#"  test:
    name: Test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: __TOOLCHAIN__
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - name: System deps + software GPU (Mesa lavapipe) for screenshot tests
        run: |
          sudo apt-get update
          sudo apt-get install -y libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev mesa-vulkan-drivers
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets -- -D warnings
      - name: Tests (incl. golden screenshot tests)
        run: cargo test --all-targets
      - name: Upload screenshot diffs
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: screenshot-diffs
          path: |
            **/__screenshots__/**/*.actual.png
            **/__screenshots__/**/*.diff.png
          if-no-files-found: ignore
"#;

const DESKTOP_JOB_HEAD: &str = r#"
  desktop:
    name: Desktop (${{ matrix.platform }})
    needs: test
    strategy:
      fail-fast: false
      matrix:
        include:
"#;

const DESKTOP_JOB_STEPS: &str = r#"    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: __TOOLCHAIN__
          targets: ${{ matrix.target }}
      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ matrix.target }}
      - name: Linux system deps
        if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev
      - run: cargo build --release --bin __BIN__ --target ${{ matrix.target }}
      - name: Package
        shell: bash
        run: |
          mkdir -p dist
          exe="target/${{ matrix.target }}/release/__BIN__"
          if [ "$RUNNER_OS" = "Windows" ]; then exe="$exe.exe"; fi
          cp "$exe" dist/
          if [ -d assets ]; then cp -r assets dist/; fi
          cd dist
          if [ "$RUNNER_OS" = "Windows" ]; then
            7z a -tzip "../__BIN__-${{ matrix.platform }}.zip" .
          else
            zip -r "../__BIN__-${{ matrix.platform }}.zip" .
          fi
      - uses: actions/upload-artifact@v4
        with:
          name: __BIN__-${{ matrix.platform }}
          path: __BIN__-${{ matrix.platform }}.zip
"#;

const WEB_JOB: &str = r#"
  web:
    name: Web (wasm)
    needs: test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: __TOOLCHAIN__
          targets: wasm32-unknown-unknown
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@v2
        with:
          tool: wasm-bindgen-cli@__WASM_BINDGEN__
      - run: cargo build --release --bin __BIN__ --target wasm32-unknown-unknown
      - name: Bundle
        run: |
          wasm-bindgen --target web --no-typescript --out-dir dist target/wasm32-unknown-unknown/release/__BIN__.wasm
          if [ -d web ]; then
            cp -r web/. dist/
          else
            printf '%s\n' \
              '<!doctype html><html><head><meta charset="utf-8"><title>__BIN__</title>' \
              '<style>html,body{margin:0;height:100%;background:#000}canvas{display:block;width:100%;height:100%}</style>' \
              '</head><body><script type="module">import init from "./__BIN__.js"; init();</script></body></html>' \
              > dist/index.html
          fi
          if [ -d assets ]; then cp -r assets dist/; fi
          (cd dist && zip -r ../__BIN__-web.zip .)
      - uses: actions/upload-artifact@v4
        with:
          name: __BIN__-web
          path: __BIN__-web.zip
"#;

const PAGES_STEP: &str = r#"      - uses: actions/upload-pages-artifact@v4
        if: github.event_name == 'push'
        with:
          path: dist
"#;

const PAGES_JOB: &str = r#"
  deploy-pages:
    name: Deploy to GitHub Pages
    # Requires Settings > Pages > Source = "GitHub Actions".
    needs: web
    if: github.event_name == 'push' && (github.ref == 'refs/heads/__MAIN__' || startsWith(github.ref, 'refs/tags/v'))
    runs-on: ubuntu-latest
    permissions:
      pages: write
      id-token: write
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - id: deployment
        uses: actions/deploy-pages@v4
"#;

const ANDROID_JOB: &str = r#"
  android:
    name: Android (apk)
    # The game's library (cargo-ndk) packaged by the Gradle project in android/, which
    # `space3d new` writes; `space3d build --target android --release` does the same locally.
    needs: test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: __TOOLCHAIN__
          targets: aarch64-linux-android
      - uses: Swatinem/rust-cache@v2
      - uses: actions/setup-java@v6
        with:
          distribution: temurin
          java-version: "21"
      - uses: gradle/actions/setup-gradle@v6
        with:
          gradle-version: "__GRADLE__"
      - run: cargo install cargo-ndk --locked
      - name: Build APK
        run: |
          # The runner's newest NDK: r28+ links the 16 KB-aligned libraries Google Play requires.
          export ANDROID_NDK_HOME="${ANDROID_NDK_LATEST_HOME:-$ANDROID_NDK_HOME}"
          cargo ndk -t arm64-v8a -P 28 -o target/android/__BIN__/jniLibs build --release --lib
          gradle -p android assembleRelease --console=plain \
            -Pspace3d.jniLibs="$PWD/target/android/__BIN__/jniLibs" \
            -Pspace3d.buildDir="$PWD/target/android/__BIN__/build"
      - name: Collect
        run: |
          mkdir -p dist
          cp target/android/__BIN__/build/outputs/apk/release/*.apk dist/__BIN__-android.apk
      - uses: actions/upload-artifact@v4
        with:
          name: __BIN__-android
          path: dist/__BIN__-android.apk
"#;

const IOS_JOB: &str = r#"
  ios:
    name: iOS (ipa, unsigned)
    # Expects an Xcode project that links the Rust static library built below.
    needs: test
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: __TOOLCHAIN__
          targets: aarch64-apple-ios
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release --lib --target aarch64-apple-ios
      - name: xcodebuild
        run: |
          xcodebuild -project "__IOS_PROJECT__" -scheme "__IOS_SCHEME__" -configuration Release \
            -sdk iphoneos -derivedDataPath build \
            CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY="" build
          mkdir -p Payload dist
          cp -R "build/Build/Products/Release-iphoneos/__IOS_SCHEME__.app" Payload/
          zip -qr dist/__BIN__-ios.ipa Payload
      - uses: actions/upload-artifact@v4
        with:
          name: __BIN__-ios
          path: dist/__BIN__-ios.ipa
"#;

const RELEASE_JOB: &str = r#"
  release:
    name: GitHub Release
    if: startsWith(github.ref, 'refs/tags/v')
    needs: [__NEEDS__]
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/download-artifact@v5
        with:
          path: artifacts
          pattern: __BIN__-*
          merge-multiple: true
      - name: Create release
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          gh release create "$GITHUB_REF_NAME" artifacts/* \
            --repo "$GITHUB_REPOSITORY" --title "$GITHUB_REF_NAME" --generate-notes \
            $(case "$GITHUB_REF_NAME" in *-*) echo --prerelease;; esac)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use yaml_rust2::{Yaml, YamlLoader};

    fn parse(s: &str) -> Yaml {
        let mut docs = YamlLoader::load_from_str(s).unwrap_or_else(|e| panic!("invalid YAML: {e}\n{s}"));
        assert_eq!(docs.len(), 1);
        docs.remove(0)
    }

    fn job_names(doc: &Yaml) -> Vec<String> {
        doc["jobs"]
            .as_hash()
            .expect("jobs map")
            .keys()
            .map(|k| k.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn all_targets_produce_every_job() {
        let yaml = github_actions(&Target::ALL);
        for placeholder in [
            "__BIN__",
            "__TOOLCHAIN__",
            "__WASM_BINDGEN__",
            "__IOS_PROJECT__",
            "__IOS_SCHEME__",
            "__MAIN__",
            "__NEEDS__",
        ] {
            assert!(!yaml.contains(placeholder), "unreplaced {placeholder}:\n{yaml}");
        }
        let doc = parse(&yaml);
        assert_eq!(
            job_names(&doc),
            ["test", "desktop", "web", "deploy-pages", "android", "ios", "release"]
        );
        // Triggers, incl. workflow_dispatch for GithubClient::dispatch_workflow.
        let on = &doc["on"];
        assert!(!on["workflow_dispatch"].is_badvalue());
        assert_eq!(on["push"]["tags"][0].as_str(), Some("v*"));

        // Desktop matrix covers all three OSes.
        let include = doc["jobs"]["desktop"]["strategy"]["matrix"]["include"]
            .as_vec()
            .unwrap();
        let platforms: Vec<&str> = include.iter().map(|e| e["platform"].as_str().unwrap()).collect();
        assert_eq!(platforms, ["linux", "windows", "macos"]);
        assert_eq!(include[1]["os"].as_str(), Some("windows-latest"));

        // Tests upload screenshot diffs on failure.
        let test_steps = doc["jobs"]["test"]["steps"].as_vec().unwrap();
        let upload = test_steps
            .iter()
            .find(|s| s["name"].as_str() == Some("Upload screenshot diffs"))
            .unwrap();
        assert_eq!(upload["if"].as_str(), Some("failure()"));
        assert!(
            test_steps
                .iter()
                .any(|s| s["run"].as_str() == Some("cargo test --all-targets"))
        );

        // Release waits for every build and has write access.
        let release = &doc["jobs"]["release"];
        let needs: Vec<&str> = release["needs"]
            .as_vec()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        assert_eq!(needs, ["desktop", "web", "android", "ios"]);
        assert_eq!(release["permissions"]["contents"].as_str(), Some("write"));
        assert_eq!(
            doc["jobs"]["deploy-pages"]["permissions"]["pages"].as_str(),
            Some("write")
        );
        assert_eq!(doc["jobs"]["ios"]["runs-on"].as_str(), Some("macos-latest"));
        assert!(yaml.contains("cargo ndk -t arm64-v8a -P 28 -o target/android/game/jniLibs build --release --lib"));
        assert!(yaml.contains("gradle -p android assembleRelease"));
        assert!(yaml.contains(&format!("gradle-version: \"{GRADLE_VERSION}\"")));
        assert!(yaml.contains("xcodebuild"));
        assert!(yaml.contains("wasm-bindgen-cli@0.2.128"));
    }

    #[test]
    fn subset_and_options() {
        let opts = TemplateOptions {
            deploy_pages: false,
            main_branch: "trunk".into(),
            ..TemplateOptions::new("space-miner")
        };
        let yaml = github_actions_with(&[Target::Web, Target::Linux], &opts);
        let doc = parse(&yaml);
        assert_eq!(job_names(&doc), ["test", "desktop", "web", "release"]);
        let include = doc["jobs"]["desktop"]["strategy"]["matrix"]["include"]
            .as_vec()
            .unwrap();
        assert_eq!(include.len(), 1);
        assert!(yaml.contains("release/space-miner.wasm"));
        assert!(yaml.contains("branches: [trunk]"));
        assert!(!yaml.contains("upload-pages-artifact"));

        // Test-only workflow when no build targets are selected.
        let doc = parse(&github_actions(&[]));
        assert_eq!(job_names(&doc), ["test"]);

        let opts = TemplateOptions {
            release_on_tag: false,
            ..Default::default()
        };
        assert_eq!(
            job_names(&parse(&github_actions_with(&[Target::Android], &opts))),
            ["test", "android"]
        );
    }

    #[test]
    fn target_parsing() {
        assert_eq!("MacOS".parse::<Target>(), Ok(Target::MacOs));
        assert_eq!(
            Target::parse_list("desktop,web").unwrap(),
            [Target::Linux, Target::Windows, Target::MacOs, Target::Web]
        );
        assert_eq!(Target::parse_list("all").unwrap(), Target::ALL);
        assert_eq!(Target::parse_list("ios ios").unwrap(), [Target::Ios]);
        assert!(Target::parse_list("linux,psp").is_err());
        assert_eq!(Target::Web.to_string(), "web");
    }

    #[test]
    fn write_to_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_github_actions(dir.path(), &Target::DESKTOP, &TemplateOptions::default()).unwrap();
        assert!(path.ends_with(WORKFLOW_PATH));
        parse(&std::fs::read_to_string(path).unwrap());
    }
}
