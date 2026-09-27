//! Detecting external tools and SDKs (versions are parsed by pure, unit-tested functions).

use crate::process::{Cmd, Exec, which};
use std::path::{Path, PathBuf};

/// Oldest supported Rust (the workspace `rust-version`).
pub const MIN_RUST: (u64, u64, u64) = (1, 98, 0);

/// Rust target triples per platform.
pub const WASM_TARGET: &str = "wasm32-unknown-unknown";
/// iOS device target.
pub const IOS_TARGET: &str = "aarch64-apple-ios";
/// iOS simulator target (Apple silicon).
pub const IOS_SIM_TARGET: &str = "aarch64-apple-ios-sim";
/// Android target built by default.
pub const ANDROID_TARGET: &str = "aarch64-linux-android";

/// Parse `rustc 1.98.1 (48a229cea 2026-09-01)` → `(1, 98, 1)`.
pub fn parse_rustc_version(s: &str) -> Option<(u64, u64, u64)> {
    let v = s.split_whitespace().nth(1)?;
    let core = v.split('-').next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>());
    Some((it.next()?.ok()?, it.next()?.ok()?, it.next().unwrap_or(Ok(0)).ok()?))
}

/// Parse `wasm-bindgen 0.2.128` → `0.2.128`.
pub fn parse_wasm_bindgen_version(s: &str) -> Option<String> {
    let v = s.lines().next()?.split_whitespace().nth(1)?;
    v.chars().next()?.is_ascii_digit().then(|| v.to_owned())
}

/// First line of a `--version` style output.
pub fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().trim().to_owned()
}

/// Installed rustup targets of the toolchain that cargo uses in `dir` (`None` without rustup).
///
/// rustup picks the toolchain per directory (`rust-toolchain.toml`, overrides), so this must
/// run in the project directory, not wherever `space3d` was started (`-C`).
pub fn installed_targets(exec: &Exec, dir: &Path) -> Option<Vec<String>> {
    let out = exec.probe(&Cmd::new("rustup").args(["target", "list", "--installed"]).cwd(dir))?;
    Some(
        out.lines()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// `true` if `target` is installed for the toolchain used in `dir` (assumed installed when
/// rustup is not available).
pub fn has_target(exec: &Exec, dir: &Path, target: &str) -> bool {
    installed_targets(exec, dir).is_none_or(|t| t.iter().any(|x| x == target))
}

/// `rustc --version` of the toolchain used in `dir`, parsed.
pub fn rustc_version(exec: &Exec, dir: &Path) -> Option<(u64, u64, u64)> {
    parse_rustc_version(&exec.probe(&Cmd::new("rustc").arg("--version").cwd(dir))?)
}

/// Installed wasm-bindgen CLI version.
pub fn wasm_bindgen_cli_version(exec: &Exec) -> Option<String> {
    parse_wasm_bindgen_version(&exec.probe(&Cmd::new("wasm-bindgen").arg("--version"))?)
}

/// `wasm-opt --version` (binaryen), if installed.
pub fn wasm_opt_version(exec: &Exec) -> Option<String> {
    exec.probe(&Cmd::new("wasm-opt").arg("--version"))
        .map(|s| first_line(&s))
}

/// Path of the `cargo-apk` subcommand binary, if installed.
pub fn cargo_apk() -> Option<PathBuf> {
    which("cargo-apk")
}

/// Android NDK directory from `ANDROID_NDK_ROOT` / `ANDROID_NDK_HOME` / `NDK_HOME`, or the
/// newest `ndk/<version>` inside the SDK.
pub fn android_ndk() -> Option<PathBuf> {
    ["ANDROID_NDK_ROOT", "ANDROID_NDK_HOME", "NDK_HOME"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|p| p.is_dir())
        .or_else(|| {
            let ndk_dir = android_sdk()?.join("ndk");
            let mut versions: Vec<PathBuf> = std::fs::read_dir(ndk_dir)
                .ok()?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect();
            versions.sort();
            versions.pop()
        })
}

/// Android SDK directory from `ANDROID_HOME` / `ANDROID_SDK_ROOT`.
pub fn android_sdk() -> Option<PathBuf> {
    ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

/// `xcodebuild -version` first line when a full Xcode is selected (not just the CLT).
pub fn xcode_version(exec: &Exec) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    exec.probe(&Cmd::new("xcodebuild").arg("-version"))
        .map(|s| first_line(&s))
}

/// SDK version of an Xcode platform SDK (`iphonesimulator`, `iphoneos`), e.g. `27.0`.
pub fn apple_sdk_version(exec: &Exec, sdk: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    exec.probe(&Cmd::new("xcrun").args(["--sdk", sdk, "--show-sdk-version"]))
        .map(|s| first_line(&s))
        .filter(|s| !s.is_empty())
}

/// An installed simulator runtime (`xcrun simctl list runtimes --json`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimRuntime {
    /// `iOS 27.0`.
    pub name: String,
    /// `com.apple.CoreSimulator.SimRuntime.iOS-27-0`.
    pub identifier: String,
    /// `iOS`, `tvOS`, `watchOS`, `visionOS`.
    pub platform: String,
    /// `27.0`.
    pub version: String,
    /// `false` when the runtime is installed but unusable (e.g. needs a newer Xcode).
    pub available: bool,
}

/// Parse `xcrun simctl list runtimes --json` (unparsable input: empty).
pub fn parse_simctl_runtimes(json: &str) -> Vec<SimRuntime> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let text = |r: &serde_json::Value, key: &str| r.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
    value
        .get("runtimes")
        .and_then(|r| r.as_array())
        .map(|runtimes| {
            runtimes
                .iter()
                .map(|r| SimRuntime {
                    name: text(r, "name"),
                    identifier: text(r, "identifier"),
                    // Older Xcodes have no `platform`: take it from the name (`iOS 17.5`).
                    platform: r
                        .get("platform")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| text(r, "name").split(' ').next().unwrap_or_default().to_owned()),
                    version: text(r, "version"),
                    available: r.get("isAvailable").and_then(|v| v.as_bool()).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Installed simulator runtimes (`None`: simctl unavailable).
pub fn simulator_runtimes(exec: &Exec) -> Option<Vec<SimRuntime>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    exec.probe(&Cmd::new("xcrun").args(["simctl", "list", "runtimes", "--json"]))
        .map(|json| parse_simctl_runtimes(&json))
}

/// Parse the `N valid identities found` line of `security find-identity -v -p codesigning`.
pub fn parse_identity_count(s: &str) -> Option<usize> {
    s.lines()
        .find(|l| l.contains("valid identities found") || l.contains("valid identity found"))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

/// Number of code-signing identities in the keychain (`None`: not on macOS / unknown).
pub fn codesign_identities(exec: &Exec) -> Option<usize> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    parse_identity_count(&exec.probe(&Cmd::new("security").args(["find-identity", "-v", "-p", "codesigning"]))?)
}

/// `true` if `gh` is installed and logged in.
pub fn gh_logged_in(exec: &Exec) -> bool {
    exec.probe(&Cmd::new("gh").args(["auth", "status"])).is_some()
}

/// Exact install command for the wasm-bindgen CLI matching the project's `wasm-bindgen` crate.
pub fn wasm_bindgen_install_cmd(version: &str) -> Cmd {
    Cmd::cargo("install").args(["wasm-bindgen-cli", "--version", version, "--locked"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustc_versions() {
        assert_eq!(
            parse_rustc_version("rustc 1.98.1 (48a229cea 2026-09-01)"),
            Some((1, 98, 1))
        );
        assert_eq!(
            parse_rustc_version("rustc 1.99.0-nightly (abc 2026-09-20)"),
            Some((1, 99, 0))
        );
        assert_eq!(parse_rustc_version("rustc 2.0"), Some((2, 0, 0)));
        assert_eq!(parse_rustc_version("garbage"), None);
        assert!(parse_rustc_version("rustc 1.98.1 (x)").unwrap() >= MIN_RUST);
    }

    #[test]
    fn wasm_bindgen_versions() {
        assert_eq!(
            parse_wasm_bindgen_version("wasm-bindgen 0.2.128").as_deref(),
            Some("0.2.128")
        );
        assert_eq!(
            parse_wasm_bindgen_version("wasm-bindgen 0.2.128\nmore").as_deref(),
            Some("0.2.128")
        );
        assert_eq!(parse_wasm_bindgen_version("error: unknown"), None);
        assert_eq!(parse_wasm_bindgen_version(""), None);
    }

    #[test]
    fn install_command_pins_the_version() {
        let cmd = wasm_bindgen_install_cmd("0.2.128");
        assert!(
            cmd.display()
                .ends_with("install wasm-bindgen-cli --version 0.2.128 --locked")
        );
    }

    #[test]
    fn simulator_runtimes_are_parsed() {
        let json = r#"{"runtimes": [
            {"isAvailable": true, "version": "27.0", "identifier": "com.apple.CoreSimulator.SimRuntime.iOS-27-0",
             "platform": "iOS", "name": "iOS 27.0", "supportedDeviceTypes": []},
            {"isAvailable": false, "version": "17.5", "identifier": "com.apple.CoreSimulator.SimRuntime.iOS-17-5",
             "name": "iOS 17.5"},
            {"isAvailable": true, "version": "27.0", "identifier": "com.apple.CoreSimulator.SimRuntime.watchOS-27-0",
             "platform": "watchOS", "name": "watchOS 27.0"}
        ]}"#;
        let runtimes = parse_simctl_runtimes(json);
        assert_eq!(runtimes.len(), 3);
        assert_eq!(runtimes[0].name, "iOS 27.0");
        assert!(runtimes[0].available && !runtimes[1].available);
        assert_eq!(runtimes[1].platform, "iOS", "platform from the name");
        assert_eq!(runtimes[2].platform, "watchOS");
        assert!(parse_simctl_runtimes("not json").is_empty());
        assert!(parse_simctl_runtimes("{}").is_empty());
    }

    #[test]
    fn identity_counts() {
        assert_eq!(parse_identity_count("     0 valid identities found\n"), Some(0));
        assert_eq!(
            parse_identity_count("  1) ABC \"Apple Development: A (X)\"\n     1 valid identities found"),
            Some(1)
        );
        assert_eq!(parse_identity_count("security: error"), None);
    }

    #[test]
    fn this_toolchain_is_detected() {
        let exec = Exec::default();
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(rustc_version(&exec, here).unwrap() >= MIN_RUST);
        // Probes run where cargo will run: a missing directory makes the probe fail cleanly.
        assert_eq!(rustc_version(&exec, &here.join("definitely-missing-dir")), None);
    }
}
