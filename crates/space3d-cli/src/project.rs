//! Locating the game project: packages, binaries, target directory and locked versions,
//! read from `cargo metadata` and `Cargo.lock`.

use crate::process::{Cmd, probe};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One cargo target (binary, library, bench, ...).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct TargetInfo {
    /// Target name (crate name for libraries).
    pub name: String,
    /// `bin`, `lib`, `cdylib`, `staticlib`, `test`, `bench`, `example`, ...
    pub kind: Vec<String>,
    /// Crate types.
    #[serde(default)]
    pub crate_types: Vec<String>,
}

/// A workspace member package.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct PackageInfo {
    /// Package name.
    pub name: String,
    /// Version.
    pub version: String,
    /// Path of its `Cargo.toml`.
    pub manifest_path: PathBuf,
    /// Its targets.
    pub targets: Vec<TargetInfo>,
    /// `[package.metadata]`.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl PackageInfo {
    /// Package directory.
    pub fn dir(&self) -> &Path {
        self.manifest_path.parent().unwrap_or(Path::new("."))
    }

    /// Names of the binary targets.
    pub fn bins(&self) -> Vec<&str> {
        self.targets
            .iter()
            .filter(|t| t.kind.iter().any(|k| k == "bin"))
            .map(|t| t.name.as_str())
            .collect()
    }

    /// `true` if the package has a binary called `name`.
    pub fn has_bin(&self, name: &str) -> bool {
        self.bins().contains(&name)
    }

    /// The library target, if any.
    pub fn lib(&self) -> Option<&TargetInfo> {
        self.targets.iter().find(|t| {
            t.kind
                .iter()
                .any(|k| matches!(k.as_str(), "lib" | "rlib" | "cdylib" | "staticlib" | "dylib"))
        })
    }

    /// `true` if the library is built as `crate_type` (e.g. `cdylib`).
    pub fn lib_has_type(&self, crate_type: &str) -> bool {
        self.lib()
            .is_some_and(|l| l.crate_types.iter().any(|c| c == crate_type))
    }

    /// The game binary: `explicit`, else the only binary, else the one named like the
    /// package, else the only one that is not `stories`/`editor`.
    pub fn main_bin(&self, explicit: Option<&str>) -> Result<String> {
        let bins = self.bins();
        if let Some(name) = explicit {
            return if bins.contains(&name) {
                Ok(name.to_owned())
            } else {
                Err(anyhow!(
                    "package `{}` has no binary `{name}` (binaries: {})",
                    self.name,
                    list(&bins)
                ))
            };
        }
        if bins.len() == 1 {
            return Ok(bins[0].to_owned());
        }
        if bins.contains(&self.name.as_str()) {
            return Ok(self.name.clone());
        }
        let game: Vec<&str> = bins
            .iter()
            .copied()
            .filter(|b| !matches!(*b, "stories" | "editor"))
            .collect();
        match game[..] {
            [one] => Ok(one.to_owned()),
            [] => bail!("package `{}` has no binary target", self.name),
            _ => bail!(
                "package `{}` has several binaries ({}); pick one with --bin",
                self.name,
                list(&bins)
            ),
        }
    }
}

fn list(items: &[&str]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<PackageInfo>,
    workspace_root: PathBuf,
    target_directory: PathBuf,
}

/// A cargo project (workspace) with one selected package.
#[derive(Clone, Debug)]
pub struct Project {
    /// Workspace root directory.
    pub workspace_root: PathBuf,
    /// `target/` directory.
    pub target_dir: PathBuf,
    /// All workspace members.
    pub packages: Vec<PackageInfo>,
    /// Index of the selected package in `packages`.
    selected: usize,
}

impl Project {
    /// Read `cargo metadata` for the project containing `dir` and select `package`
    /// (default: the package whose directory contains `dir`, else the only member).
    pub fn load(dir: &Path, package: Option<&str>) -> Result<Self> {
        let json = probe(
            &Cmd::cargo("metadata")
                .args(["--no-deps", "--format-version", "1"])
                .cwd(dir),
        )
        .with_context(|| {
            format!(
                "`cargo metadata` failed in {} — is this a cargo project? (create one with `space3d new <name>`)",
                dir.display()
            )
        })?;
        Self::from_metadata_json(&json, dir, package)
    }

    /// Parse `cargo metadata --no-deps --format-version 1` output.
    pub fn from_metadata_json(json: &str, dir: &Path, package: Option<&str>) -> Result<Self> {
        let meta: Metadata = serde_json::from_str(json).context("unexpected `cargo metadata` output")?;
        let selected = select_package(&meta.packages, dir, package)?;
        Ok(Self {
            workspace_root: meta.workspace_root,
            target_dir: meta.target_directory,
            packages: meta.packages,
            selected,
        })
    }

    /// The selected package.
    pub fn package(&self) -> &PackageInfo {
        &self.packages[self.selected]
    }

    /// Package directory of the selected package.
    pub fn dir(&self) -> &Path {
        self.package().dir()
    }

    /// Workspace member by name.
    pub fn member(&self, name: &str) -> Option<&PackageInfo> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// The package that ships the game on Android/iOS: `<name>-mobile` (as generated by
    /// `space3d new`), else the selected package itself when its library is a `crate_type`.
    pub fn mobile_package(&self, crate_type: &str) -> Option<&PackageInfo> {
        let name = &self.package().name;
        [format!("{name}-mobile"), format!("{name}_mobile")]
            .iter()
            .find_map(|n| self.member(n))
            .filter(|p| p.lib_has_type(crate_type))
            .or_else(|| Some(self.package()).filter(|p| p.lib_has_type(crate_type)))
    }

    /// Read `Cargo.lock` of the workspace (if it exists).
    pub fn lockfile(&self) -> Option<String> {
        std::fs::read_to_string(self.workspace_root.join("Cargo.lock")).ok()
    }

    /// Locked version of `crate_name` (e.g. `wasm-bindgen`).
    pub fn locked_version(&self, crate_name: &str) -> Option<String> {
        locked_version(&self.lockfile()?, crate_name)
    }

    /// Default output directory for a platform: `<package>/dist/<platform>`.
    pub fn dist_dir(&self, platform: &str) -> PathBuf {
        self.dir().join("dist").join(platform)
    }
}

fn select_package(packages: &[PackageInfo], dir: &Path, package: Option<&str>) -> Result<usize> {
    if packages.is_empty() {
        bail!("no packages found in the workspace");
    }
    if let Some(name) = package {
        return packages.iter().position(|p| p.name == name).ok_or_else(|| {
            let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
            anyhow!("no package `{name}` in this workspace (members: {})", names.join(", "))
        });
    }
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    // Deepest package directory containing `dir`.
    let containing = packages
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let pdir = p.dir().canonicalize().unwrap_or_else(|_| p.dir().to_path_buf());
            dir.starts_with(&pdir).then_some((i, pdir.components().count()))
        })
        .max_by_key(|(_, depth)| *depth);
    if let Some((i, _)) = containing {
        return Ok(i);
    }
    if packages.len() == 1 {
        return Ok(0);
    }
    let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
    bail!(
        "this is a workspace with several packages ({}); pick one with --package or run inside its directory",
        names.join(", ")
    )
}

#[derive(Deserialize)]
struct LockFile {
    #[serde(default)]
    package: Vec<LockPackage>,
}

#[derive(Deserialize)]
struct LockPackage {
    name: String,
    version: String,
}

/// Version of `crate_name` in a `Cargo.lock` (the highest, if several are locked).
pub fn locked_version(lock: &str, crate_name: &str) -> Option<String> {
    let lock: LockFile = toml::from_str(lock).ok()?;
    lock.package
        .into_iter()
        .filter(|p| p.name == crate_name)
        .map(|p| p.version)
        .max_by_key(|v| version_key(v))
}

/// Numeric sort key for `MAJOR.MINOR.PATCH[-pre]` (pre-release suffixes are ignored).
pub fn version_key(v: &str) -> (u64, u64, u64) {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "wasm-bindgen"
version = "0.2.99"

[[package]]
name = "wasm-bindgen"
version = "0.2.128"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "wasm-bindgen-futures"
version = "0.4.78"
"#;

    #[test]
    fn lock_versions() {
        assert_eq!(locked_version(LOCK, "wasm-bindgen").as_deref(), Some("0.2.128"));
        assert_eq!(locked_version(LOCK, "wasm-bindgen-futures").as_deref(), Some("0.4.78"));
        assert_eq!(locked_version(LOCK, "serde"), None);
        assert_eq!(locked_version("not toml [[", "x"), None);
        assert!(version_key("1.10.0") > version_key("1.9.3"));
        assert_eq!(version_key("2.0.0-rc.1"), (2, 0, 0));
    }

    fn metadata(root: &str) -> String {
        serde_json::json!({
            "packages": [
                {
                    "name": "my_game", "version": "0.1.0",
                    "manifest_path": format!("{root}/Cargo.toml"),
                    "targets": [
                        {"name": "my_game", "kind": ["lib"], "crate_types": ["lib"]},
                        {"name": "my_game", "kind": ["bin"], "crate_types": ["bin"]},
                        {"name": "stories", "kind": ["bin"], "crate_types": ["bin"]},
                        {"name": "gameplay", "kind": ["test"], "crate_types": ["bin"]}
                    ],
                    "metadata": null
                },
                {
                    "name": "my_game-mobile", "version": "0.1.0",
                    "manifest_path": format!("{root}/mobile/Cargo.toml"),
                    "targets": [
                        {"name": "my_game_mobile", "kind": ["cdylib", "staticlib"], "crate_types": ["cdylib", "staticlib"]}
                    ],
                    "metadata": {"android": {"package": "com.example.my_game"}}
                }
            ],
            "workspace_root": root,
            "target_directory": format!("{root}/target"),
        })
        .to_string()
    }

    #[test]
    fn selects_package_by_directory_and_name() {
        let root = "/nonexistent/space3d-cli-test/my_game";
        let json = metadata(root);
        let p = Project::from_metadata_json(&json, Path::new(root), None).unwrap();
        assert_eq!(p.package().name, "my_game");
        assert_eq!(p.target_dir, PathBuf::from(format!("{root}/target")));
        let p = Project::from_metadata_json(&json, &Path::new(root).join("mobile/src"), None).unwrap();
        assert_eq!(p.package().name, "my_game-mobile");
        let p = Project::from_metadata_json(&json, Path::new("/elsewhere"), Some("my_game")).unwrap();
        assert_eq!(p.package().name, "my_game");
        assert!(Project::from_metadata_json(&json, Path::new("/elsewhere"), None).is_err());
        assert!(Project::from_metadata_json(&json, Path::new(root), Some("nope")).is_err());
    }

    #[test]
    fn bins_and_mobile_package() {
        let root = "/nonexistent/space3d-cli-test/my_game";
        let p = Project::from_metadata_json(&metadata(root), Path::new(root), None).unwrap();
        let pkg = p.package();
        assert_eq!(pkg.bins(), vec!["my_game", "stories"]);
        assert_eq!(pkg.main_bin(None).unwrap(), "my_game");
        assert_eq!(pkg.main_bin(Some("stories")).unwrap(), "stories");
        assert!(pkg.main_bin(Some("nope")).is_err());
        assert!(pkg.has_bin("stories") && !pkg.has_bin("editor"));
        assert_eq!(p.mobile_package("cdylib").unwrap().name, "my_game-mobile");
        assert_eq!(p.mobile_package("staticlib").unwrap().name, "my_game-mobile");
        assert!(p.mobile_package("dylib").is_none());
        assert_eq!(p.dist_dir("web"), PathBuf::from(format!("{root}/dist/web")));
    }

    #[test]
    fn main_bin_heuristics() {
        let pkg = |bins: &[&str]| PackageInfo {
            name: "g".into(),
            version: "0.1.0".into(),
            manifest_path: "/x/Cargo.toml".into(),
            targets: bins
                .iter()
                .map(|b| TargetInfo {
                    name: b.to_string(),
                    kind: vec!["bin".into()],
                    crate_types: vec!["bin".into()],
                })
                .collect(),
            metadata: serde_json::Value::Null,
        };
        assert_eq!(pkg(&["x"]).main_bin(None).unwrap(), "x");
        assert_eq!(pkg(&["x", "g"]).main_bin(None).unwrap(), "g");
        assert_eq!(pkg(&["x", "stories", "editor"]).main_bin(None).unwrap(), "x");
        assert!(pkg(&["x", "y"]).main_bin(None).is_err());
        assert!(pkg(&[]).main_bin(None).is_err());
    }

    #[test]
    fn loads_this_workspace() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let p = Project::load(here, None).unwrap();
        assert_eq!(p.package().name, "space3d-cli");
        assert!(p.package().has_bin("space3d"));
        assert!(p.member("space3d").is_some());
        assert!(p.locked_version("wasm-bindgen").is_some());
    }
}
