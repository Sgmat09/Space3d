//! Detecting external tools and SDKs (versions are parsed by pure, unit-tested functions).

use crate::process::{Cmd, Exec};
use std::path::Path;

/// Oldest supported Rust (the workspace `rust-version`).
pub const MIN_RUST: (u64, u64, u64) = (1, 98, 0);

/// Rust target triples per platform.
pub const WASM_TARGET: &str = "wasm32-unknown-unknown";
/// iOS device target.
pub const IOS_TARGET: &str = "aarch64-apple-ios";
/// iOS simulator target (Apple silicon).
pub const IOS_SIM_TARGET: &str = "aarch64-apple-ios-sim";
/// Android target built by default (`android::Abi::Arm64`).
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

/// `xcodebuild -version` first line when a full Xcode is selected (not just the CLT).
pub fn xcode_version(exec: &Exec) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    exec.probe(&Cmd::new("xcodebuild").arg("-version"))
        .map(|s| first_line(&s))
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
    fn this_toolchain_is_detected() {
        let exec = Exec::default();
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(rustc_version(&exec, here).unwrap() >= MIN_RUST);
        // Probes run where cargo will run: a missing directory makes the probe fail cleanly.
        assert_eq!(rustc_version(&exec, &here.join("definitely-missing-dir")), None);
    }
}
