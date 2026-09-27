//! Project templates for `space3d new`, embedded in the binary with `include_dir`.
//!
//! Layout of `crates/space3d-cli/templates/`:
//!
//! * `common/` — files every project gets (manifest, entry point, storybook binary, web page,
//!   mobile crate, iOS/Android notes, README);
//! * `basic/`, `3d/`, `multiplayer/` — the game itself (`src/lib.rs`, `src/stories.rs`,
//!   `tests/`), overlaid on `common/`.
//!
//! Text files are rendered by replacing `{{variable}}` placeholders ([`TemplateVars`]);
//! unknown `{{identifier}}`s are an error, anything else with braces (e.g. GitHub's
//! `${{ github.ref }}`) is left alone. File names ending in `.tmpl` lose that suffix
//! (so template `Cargo.toml`s are not mistaken for real packages) and `_gitignore` becomes
//! `.gitignore`.

use crate::args::TemplateKind;
use anyhow::{Context, Result, bail};
use include_dir::{Dir, DirEntry, include_dir};
use space3d_github::templates::{Target, TemplateOptions, WORKFLOW_PATH, write_github_actions};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Default git URL of the engine.
pub const DEFAULT_GIT: &str = "https://github.com/SpaceCorps/Space3d";

static TEMPLATES: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/templates");

/// Where the generated project gets `space3d` from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Space3dSource {
    /// A local checkout: the directory of the `space3d` facade crate.
    Path(PathBuf),
    /// A git repository.
    Git {
        /// Repository URL.
        url: String,
        /// Branch.
        branch: Option<String>,
        /// Revision.
        rev: Option<String>,
    },
}

impl Space3dSource {
    /// Resolve a user-supplied checkout path: the repository root (with `crates/space3d`) or
    /// the facade crate directory itself. The result is absolute.
    pub fn from_checkout(path: &Path) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Space3d checkout not found at {}", path.display()))?;
        // `canonicalize` returns `\\?\C:\…` on Windows; cargo wants a plain path.
        let path = PathBuf::from(strip_verbatim_prefix(&path.to_string_lossy()));
        for candidate in [path.join("crates").join("space3d"), path.clone()] {
            // Only the facade package counts (not e.g. space3d-cli, whose binary is `space3d`).
            if package_name(&candidate.join("Cargo.toml")).is_ok_and(|name| name == "space3d") {
                return Ok(Self::Path(candidate));
            }
        }
        bail!(
            "{} is not a Space3d checkout (expected crates/space3d/Cargo.toml)",
            path.display()
        )
    }

    /// The inline-table fields for a `space3d = { ... }` dependency, e.g.
    /// `path = "/src/Space3d/crates/space3d"` or `git = "https://…", branch = "main"`.
    pub fn dependency_fields(&self) -> String {
        match self {
            Space3dSource::Path(p) => format!("path = {}", toml_string(&p.to_string_lossy().replace('\\', "/"))),
            Space3dSource::Git { url, branch, rev } => {
                let mut s = format!("git = {}", toml_string(url));
                if let Some(b) = branch {
                    s.push_str(&format!(", branch = {}", toml_string(b)));
                }
                if let Some(r) = rev {
                    s.push_str(&format!(", rev = {}", toml_string(r)));
                }
                s
            }
        }
    }

    /// The `[patch.crates-io]` entry that gives egui Space3d's Chinese and Japanese line
    /// breaking (kinsoku): `epaint` from the same source as `space3d` (a checkout's
    /// `third_party/epaint`, or the same git repository and revision, where cargo finds the
    /// package by name). A checkout without `third_party/epaint` gets a commented-out line.
    pub fn epaint_patch(&self) -> String {
        match self {
            Space3dSource::Path(facade) => {
                let vendored = facade
                    .parent()
                    .and_then(Path::parent)
                    .map(|root| root.join("third_party").join("epaint"))
                    .filter(|dir| dir.join("Cargo.toml").is_file());
                match vendored {
                    Some(dir) => format!(
                        "epaint = {{ path = {} }}",
                        toml_string(&dir.to_string_lossy().replace('\\', "/"))
                    ),
                    None => "# epaint = { path = \"<Space3d checkout>/third_party/epaint\" }  \
                             (this checkout has no third_party/epaint)"
                        .to_owned(),
                }
            }
            Space3dSource::Git { .. } => format!("epaint = {{ {} }}", self.dependency_fields()),
        }
    }
}

/// Drop the Windows verbatim prefix that `canonicalize` adds: `\\?\C:\x` → `C:\x`,
/// `\\?\UNC\server\share` → `\\server\share`. Other paths are returned unchanged.
pub fn strip_verbatim_prefix(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path.to_owned()
    }
}

fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

/// Values substituted into templates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateVars(BTreeMap<&'static str, String>);

impl TemplateVars {
    /// Variables for a project called `name` (must pass [`validate_name`]).
    pub fn new(name: &str, template: TemplateKind, source: &Space3dSource) -> Self {
        let crate_name = name.replace('-', "_");
        let mut v = BTreeMap::new();
        v.insert("name", name.to_owned());
        v.insert("crate_name", crate_name.clone());
        v.insert("title", title_case(name));
        v.insert("template", template.dir_name().to_owned());
        v.insert("space3d_dep", source.dependency_fields());
        v.insert("epaint_patch", source.epaint_patch());
        v.insert("space3d_version", env!("CARGO_PKG_VERSION").to_owned());
        v.insert(
            "android_package",
            format!("com.example.{}", crate_name.to_ascii_lowercase()),
        );
        v.insert(
            "ios_bundle_id",
            format!("com.example.{}", name.replace('_', "-").to_ascii_lowercase()),
        );
        let (extra_dependencies, extra_bins, controls) = match template {
            TemplateKind::Basic => (
                "",
                "",
                "arrow keys / WASD (or touch the screen edges) move the cube, Esc quits.",
            ),
            TemplateKind::ThreeD => (
                "",
                "",
                "arrow keys / WASD roll the ball, Space jumps, touch the screen edges on mobile, Esc quits. \
                 Knock the crates off the arena.",
            ),
            TemplateKind::Multiplayer => (
                // Same versions as the engine workspace uses (latest stable at generation time).
                "serde = { version = \"1.0.229\", features = [\"derive\"] }\nserde_json = \"1.0.151\"\n",
                ", `server`",
                "arrow keys / WASD move your cube, Esc quits. `cargo run` starts a local server + client; \
                 `cargo run --bin server` runs a dedicated server and `cargo run -- --connect ws://HOST:7777` \
                 joins it.",
            ),
        };
        v.insert("extra_dependencies", extra_dependencies.to_owned());
        v.insert("extra_bins", extra_bins.to_owned());
        v.insert("controls", controls.to_owned());
        Self(v)
    }

    /// Value of a variable.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Replace `{{key}}` placeholders. Unknown `{{identifier}}`s are an error.
    pub fn render(&self, text: &str) -> Result<String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after.find("}}") {
                Some(end) if is_ident(&after[..end]) => {
                    let key = &after[..end];
                    match self.0.get(key) {
                        Some(value) => out.push_str(value),
                        None => bail!("unknown template variable {{{{{key}}}}}"),
                    }
                    rest = &after[end + 2..];
                }
                _ => {
                    out.push_str("{{");
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        Ok(out)
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `my_cool-game` → `My Cool Game`.
pub fn title_case(name: &str) -> String {
    name.split(['_', '-'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Check a package name: lower-case ASCII letters, digits, `_` and `-`, starting with a letter,
/// with no doubled or trailing separator (the crate names derived from it — `my_game`,
/// `my_game_mobile` — must pass rustc's `non_snake_case` lint, which the generated CI denies),
/// and not a Rust keyword or a name that would clash with the engine.
pub fn validate_name(name: &str) -> Result<()> {
    const RESERVED: &[&str] = &[
        "as",
        "async",
        "await",
        "break",
        "const",
        "continue",
        "crate",
        "dyn",
        "else",
        "enum",
        "extern",
        "false",
        "fn",
        "for",
        "if",
        "impl",
        "in",
        "let",
        "loop",
        "match",
        "mod",
        "move",
        "mut",
        "pub",
        "ref",
        "return",
        "self",
        "static",
        "struct",
        "super",
        "trait",
        "true",
        "type",
        "unsafe",
        "use",
        "where",
        "while",
        "abstract",
        "become",
        "box",
        "do",
        "final",
        "gen",
        "macro",
        "override",
        "priv",
        "try",
        "typeof",
        "unsized",
        "virtual",
        "yield",
        "test",
        "core",
        "std",
        "alloc",
        "proc_macro",
        "build",
        "space3d",
        "stories",
        "editor",
    ];
    let first = name.chars().next().context("the project name is empty")?;
    if !first.is_ascii_alphabetic() {
        bail!("`{name}`: a project name must start with a letter");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
    {
        bail!("`{name}`: invalid character {bad:?} (use letters, digits, `_` or `-`)");
    }
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        bail!(
            "`{name}`: use lower case (e.g. `{}`) — Rust crate names are snake_case",
            name.to_ascii_lowercase()
        );
    }
    let crate_name = name.replace('-', "_");
    if crate_name.contains("__") || crate_name.ends_with('_') {
        bail!("`{name}`: separators (`_`, `-`) must be single and not at the end");
    }
    if name.starts_with("space3d") || RESERVED.contains(&name.replace('-', "_").as_str()) {
        bail!("`{name}` is reserved; pick another name");
    }
    Ok(())
}

/// A file produced by [`render_template`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedFile {
    /// Path relative to the project root.
    pub path: PathBuf,
    /// Contents.
    pub contents: Vec<u8>,
}

/// Render `common/` + `<template>/` in memory (template files override common ones).
pub fn render_template(template: TemplateKind, vars: &TemplateVars) -> Result<Vec<RenderedFile>> {
    let mut files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    for layer in ["common", template.dir_name()] {
        let dir = TEMPLATES
            .get_dir(layer)
            .with_context(|| format!("template layer `{layer}` is missing from the binary"))?;
        let mut stack = vec![dir];
        while let Some(dir) = stack.pop() {
            for entry in dir.entries() {
                match entry {
                    DirEntry::Dir(d) => stack.push(d),
                    DirEntry::File(f) => {
                        let rel = f.path().strip_prefix(layer).unwrap_or(f.path());
                        let out_path = output_path(rel);
                        let contents = match f.contents_utf8() {
                            Some(text) => vars
                                .render(text)
                                .with_context(|| format!("rendering {}", f.path().display()))?
                                .into_bytes(),
                            None => f.contents().to_vec(),
                        };
                        files.insert(out_path, contents);
                    }
                }
            }
        }
    }
    Ok(files
        .into_iter()
        .map(|(path, contents)| RenderedFile { path, contents })
        .collect())
}

/// Template path → project path (`.tmpl` suffix dropped, `_gitignore` → `.gitignore`).
pub fn output_path(rel: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in rel.components() {
        let s = comp.as_os_str().to_string_lossy();
        let s = s.strip_suffix(".tmpl").unwrap_or(&s);
        out.push(if s == "_gitignore" { ".gitignore" } else { s });
    }
    out
}

/// Options for [`scaffold`].
#[derive(Clone, Debug)]
pub struct ScaffoldOptions {
    /// Package name.
    pub name: String,
    /// Starter.
    pub template: TemplateKind,
    /// Where `space3d` comes from.
    pub source: Space3dSource,
    /// Workflow targets (`None` = no workflow).
    pub ci_targets: Option<Vec<Target>>,
    /// Allow a non-empty destination.
    pub force: bool,
}

/// Write a new project into `dest`. Returns the written paths (relative to `dest`).
pub fn scaffold(dest: &Path, opts: &ScaffoldOptions) -> Result<Vec<PathBuf>> {
    validate_name(&opts.name)?;
    if dest.exists() {
        let non_empty = std::fs::read_dir(dest)
            .with_context(|| format!("{} exists and is not a directory", dest.display()))?
            .next()
            .is_some();
        if non_empty && !opts.force {
            bail!(
                "{} already exists and is not empty (use --force to write into it)",
                dest.display()
            );
        }
    }
    let vars = TemplateVars::new(&opts.name, opts.template, &opts.source);
    let files = render_template(opts.template, &vars)?;
    let mut written = Vec::new();
    for file in files {
        let path = dest.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, &file.contents).with_context(|| format!("writing {}", path.display()))?;
        written.push(file.path);
    }
    for (rel, contents) in android_project(&opts.name, &vars) {
        let rel = Path::new("android").join(rel);
        crate::android::write_files(dest, &[(rel.clone(), contents)])?;
        written.push(rel);
    }
    if let Some(targets) = &opts.ci_targets {
        written.push(write_workflow(dest, &opts.name, targets)?);
    }
    Ok(written)
}

/// The Gradle project in `android/` that packages the `mobile/` crate (`space3d build --target
/// android` uses it; the game owns and edits it). Paths relative to `android/`.
pub fn android_project(name: &str, vars: &TemplateVars) -> Vec<(PathBuf, String)> {
    use crate::android::{self, Abi, AndroidConfig, GradleDefaults};
    let cfg = AndroidConfig {
        application_id: vars.get("android_package").unwrap_or_default().to_owned(),
        label: vars.get("title").unwrap_or(name).to_owned(),
        apk_name: name.to_owned(),
        version_name: "0.1.0".into(),
        version_code: android::version_code("0.1.0"),
        min_sdk: android::DEFAULT_MIN_SDK,
        target_sdk: android::DEFAULT_TARGET_SDK,
        orientation: "sensorLandscape".into(),
        permissions: vec!["android.permission.INTERNET".into()],
        abis: vec![Abi::Arm64],
        assets: None,
        icon: None,
    };
    let crate_name = vars.get("crate_name").unwrap_or(name);
    android::gradle_project(
        &cfg,
        &format!("{crate_name}_mobile"),
        name,
        &GradleDefaults {
            jni_libs: format!("../target/android/{name}/jniLibs"),
            assets: Some("../assets".into()),
        },
    )
}

/// Write `.github/workflows/space3d.yml` with [`write_github_actions`], adapted to the
/// generated layout when the project has a `mobile/` crate. Returns the relative path.
pub fn write_workflow(project_dir: &Path, bin_name: &str, targets: &[Target]) -> Result<PathBuf> {
    let opts = workflow_options(bin_name, true);
    write_workflow_with(project_dir, project_dir, targets, &opts)
}

/// The [`TemplateOptions`] used for generated projects.
pub fn workflow_options(bin_name: &str, deploy_pages: bool) -> TemplateOptions {
    TemplateOptions {
        deploy_pages,
        ..TemplateOptions::new(bin_name)
    }
}

/// [`write_workflow`] with explicit options: the workflow goes to `repo_dir` (GitHub only reads
/// workflows at the repository root), the `mobile/` crate is looked up in `package_dir`.
pub fn write_workflow_with(
    repo_dir: &Path,
    package_dir: &Path,
    targets: &[Target],
    opts: &TemplateOptions,
) -> Result<PathBuf> {
    let path = write_github_actions(repo_dir, targets, opts).with_context(|| format!("writing {WORKFLOW_PATH}"))?;
    let mobile_manifest = package_dir.join("mobile").join("Cargo.toml");
    if mobile_manifest.is_file() {
        let package = package_name(&mobile_manifest)?;
        let yaml = std::fs::read_to_string(&path)?;
        std::fs::write(&path, patch_workflow_for_mobile_crate(&yaml, &package))?;
    }
    Ok(PathBuf::from(WORKFLOW_PATH))
}

fn package_name(manifest: &Path) -> Result<String> {
    let text = std::fs::read_to_string(manifest)?;
    let value: toml::Table = toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
    value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_owned)
        .with_context(|| format!("{} has no package.name", manifest.display()))
}

/// Android and iOS jobs build the `mobile` crate (the root package is `rlib` + binaries, so
/// desktop/web builds never link a `cdylib`/`staticlib`); iOS also needs XcodeGen to turn
/// `ios/project.yml` into the Xcode project.
pub fn patch_workflow_for_mobile_crate(yaml: &str, mobile_package: &str) -> String {
    yaml.replace(
        " build --release --lib\n",
        &format!(" build --release -p {mobile_package} --lib\n"),
    )
    .replace(
        "      - run: cargo build --release --lib --target aarch64-apple-ios\n",
        &format!(
            "      - run: cargo build --release -p {mobile_package} --lib --target aarch64-apple-ios\n      \
             - name: Generate the Xcode project (XcodeGen)\n        \
             run: brew install xcodegen && (cd ios && xcodegen generate)\n"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git() -> Space3dSource {
        Space3dSource::Git {
            url: DEFAULT_GIT.into(),
            branch: None,
            rev: None,
        }
    }

    #[test]
    fn render_replaces_known_and_keeps_foreign_braces() {
        let vars = TemplateVars::new("my-game", TemplateKind::Basic, &git());
        assert_eq!(
            vars.render("{{name}}/{{crate_name}}: {{title}}").unwrap(),
            "my-game/my_game: My Game"
        );
        let gha = "if: ${{ github.ref }} {{ not_a_var }} {{}} {{Name}}";
        assert_eq!(vars.render(gha).unwrap(), gha);
        assert!(vars.render("{{nope}}").is_err());
        assert_eq!(vars.render("unterminated {{name").unwrap(), "unterminated {{name");
        assert_eq!(vars.get("android_package"), Some("com.example.my_game"));
        assert_eq!(vars.get("ios_bundle_id"), Some("com.example.my-game"));
    }

    #[test]
    fn hyphenated_names_map_to_the_package() {
        let vars = TemplateVars::new("my-game", TemplateKind::Basic, &git());
        let files = render_template(TemplateKind::Basic, &vars).unwrap();
        let mobile = files.iter().find(|f| f.path == Path::new("mobile/Cargo.toml")).unwrap();
        let mobile: toml::Table = toml::from_str(std::str::from_utf8(&mobile.contents).unwrap()).unwrap();
        assert_eq!(mobile["package"]["name"].as_str(), Some("my-game-mobile"));
        assert_eq!(mobile["lib"]["name"].as_str(), Some("my_game_mobile"));
        let dep = &mobile["dependencies"]["my_game"];
        assert_eq!(dep["package"].as_str(), Some("my-game"));
        assert_eq!(dep["path"].as_str(), Some(".."));
    }

    #[test]
    fn dependency_fields() {
        assert_eq!(git().dependency_fields(), format!("git = \"{DEFAULT_GIT}\""));
        let g = Space3dSource::Git {
            url: "https://x/y".into(),
            branch: Some("main".into()),
            rev: None,
        };
        assert_eq!(g.dependency_fields(), r#"git = "https://x/y", branch = "main""#);
        let p = Space3dSource::Path(PathBuf::from("/src/Space3d/crates/space3d"));
        assert_eq!(p.dependency_fields(), r#"path = "/src/Space3d/crates/space3d""#);
    }

    #[test]
    fn epaint_patch_follows_the_space3d_source() {
        assert_eq!(git().epaint_patch(), format!("epaint = {{ git = \"{DEFAULT_GIT}\" }}"));
        let rev = Space3dSource::Git {
            url: DEFAULT_GIT.into(),
            branch: None,
            rev: Some("b00ed35".into()),
        };
        assert_eq!(
            rev.epaint_patch(),
            format!("epaint = {{ git = \"{DEFAULT_GIT}\", rev = \"b00ed35\" }}")
        );
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let checkout = Space3dSource::from_checkout(&root).unwrap();
        let patch = checkout.epaint_patch();
        assert!(patch.starts_with("epaint = { path = \""), "{patch}");
        assert!(patch.ends_with("/third_party/epaint\" }"), "{patch}");
        let old_checkout = Space3dSource::Path(PathBuf::from("/nonexistent/Space3d/crates/space3d"));
        assert!(old_checkout.epaint_patch().starts_with("# epaint"));
    }

    #[test]
    fn checkout_detection() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Space3dSource::Path(p) = Space3dSource::from_checkout(&root).unwrap() else {
            panic!()
        };
        assert!(p.ends_with("crates/space3d"));
        let Space3dSource::Path(p2) = Space3dSource::from_checkout(&root.join("crates/space3d")).unwrap() else {
            panic!()
        };
        assert_eq!(p, p2);
        assert!(Space3dSource::from_checkout(&root.join("crates/space3d-core")).is_err());
        // space3d-cli's manifest mentions `name = "space3d"` (its binary), but it is not the facade.
        assert!(Space3dSource::from_checkout(&root.join("crates/space3d-cli")).is_err());
        assert!(Space3dSource::from_checkout(Path::new("/definitely/not/here")).is_err());
    }

    #[test]
    fn names() {
        for ok in ["game", "my_game", "my-game2", "a1_b2-c3"] {
            validate_name(ok).unwrap();
        }
        for bad in [
            "",
            "Asteroids",
            "myGame",
            "my__game",
            "my-_game",
            "game_",
            "game-",
            "1game",
            "my game",
            "fn",
            "self",
            "space3d",
            "space3d-foo",
            "stories",
            "tést",
            "a/b",
        ] {
            assert!(validate_name(bad).is_err(), "{bad} should be rejected");
        }
        assert_eq!(title_case("my_cool-game"), "My Cool Game");
        assert_eq!(title_case("x"), "X");
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim_prefix(r"\\?\D:\a\Space3d"), r"D:\a\Space3d");
        assert_eq!(strip_verbatim_prefix(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(strip_verbatim_prefix("/src/Space3d"), "/src/Space3d");
        assert_eq!(strip_verbatim_prefix(r"C:\x"), r"C:\x");
        // …and the dependency path cargo gets is a normal forward-slash path.
        let p = Space3dSource::Path(PathBuf::from(strip_verbatim_prefix(r"\\?\D:\a\Space3d\crates\space3d")));
        assert_eq!(p.dependency_fields(), r#"path = "D:/a/Space3d/crates/space3d""#);
    }

    #[test]
    fn output_paths() {
        assert_eq!(output_path(Path::new("Cargo.toml.tmpl")), PathBuf::from("Cargo.toml"));
        assert_eq!(
            output_path(Path::new("mobile/Cargo.toml.tmpl")),
            PathBuf::from("mobile/Cargo.toml")
        );
        assert_eq!(output_path(Path::new("_gitignore")), PathBuf::from(".gitignore"));
        assert_eq!(output_path(Path::new("src/lib.rs")), PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn every_template_renders_completely() {
        for kind in [TemplateKind::Basic, TemplateKind::ThreeD, TemplateKind::Multiplayer] {
            let vars = TemplateVars::new("demo_game", kind, &git());
            let files = render_template(kind, &vars).unwrap();
            let paths: Vec<String> = files
                .iter()
                .map(|f| f.path.to_string_lossy().replace('\\', "/"))
                .collect();
            for required in [
                "Cargo.toml",
                "README.md",
                ".gitignore",
                "src/main.rs",
                "src/lib.rs",
                "src/stories.rs",
                "src/bin/stories.rs",
                "src/bin/editor.rs",
                "tests/screenshots.rs",
                "tests/input.rs",
                "web/index.html",
                "assets/README.md",
                "mobile/Cargo.toml",
                "mobile/src/lib.rs",
                "android/README.md",
                "ios/README.md",
                "ios/project.yml",
                "ios/main.m",
            ] {
                assert!(
                    paths.contains(&required.to_string()),
                    "{kind:?}: missing {required} in {paths:?}"
                );
            }
            for f in &files {
                if let Ok(text) = std::str::from_utf8(&f.contents) {
                    for (i, _) in text.match_indices("{{") {
                        let tail = &text[i + 2..];
                        let end = tail.find("}}").unwrap_or(0);
                        assert!(
                            !is_ident(&tail[..end]),
                            "{kind:?}: placeholder left in {}",
                            f.path.display()
                        );
                    }
                }
            }
            let manifest = &files
                .iter()
                .find(|f| f.path == Path::new("Cargo.toml"))
                .unwrap()
                .contents;
            let manifest: toml::Table = toml::from_str(std::str::from_utf8(manifest).unwrap()).unwrap();
            assert_eq!(manifest["package"]["name"].as_str(), Some("demo_game"));
            assert!(manifest["dependencies"]["space3d"]["git"].as_str().is_some());
            // Kinsoku: egui's epaint from the same Space3d repository.
            assert_eq!(
                manifest["patch"]["crates-io"]["epaint"]["git"].as_str(),
                Some(DEFAULT_GIT)
            );
            let main = files.iter().find(|f| f.path == Path::new("src/main.rs")).unwrap();
            assert!(
                std::str::from_utf8(&main.contents)
                    .unwrap()
                    .contains("space3d::main!(demo_game::build)")
            );
        }
    }

    #[test]
    fn scaffold_writes_project_and_workflow() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("arena");
        let opts = ScaffoldOptions {
            name: "arena".into(),
            template: TemplateKind::ThreeD,
            source: git(),
            ci_targets: Some(Target::ALL.to_vec()),
            force: false,
        };
        let written = scaffold(&dest, &opts).unwrap();
        assert!(written.contains(&PathBuf::from(WORKFLOW_PATH)));
        assert!(dest.join("src/lib.rs").is_file());
        let yaml = std::fs::read_to_string(dest.join(WORKFLOW_PATH)).unwrap();
        assert!(
            yaml.contains("-o target/android/arena/jniLibs build --release -p arena-mobile --lib\n"),
            "{yaml}"
        );
        // The Gradle project that packages it.
        let gradle = std::fs::read_to_string(dest.join("android/build.gradle.kts")).unwrap();
        assert!(gradle.contains("applicationId = \"com.example.arena\""), "{gradle}");
        assert!(
            gradle.contains("getOrElse(\"../target/android/arena/jniLibs\")"),
            "{gradle}"
        );
        let manifest = std::fs::read_to_string(dest.join("android/src/main/AndroidManifest.xml")).unwrap();
        assert!(manifest.contains("android:value=\"arena_mobile\""), "{manifest}");
        assert!(written.contains(&PathBuf::from("android/settings.gradle.kts")));
        assert!(yaml.contains("cargo build --release -p arena-mobile --lib --target aarch64-apple-ios"));
        assert!(yaml.contains("xcodegen generate"));
        assert!(yaml.contains("--bin arena"));
        // A second run refuses to overwrite …
        assert!(scaffold(&dest, &opts).is_err());
        // … unless forced.
        scaffold(
            &dest,
            &ScaffoldOptions {
                force: true,
                ..opts.clone()
            },
        )
        .unwrap();
        // Bad names never touch the disk.
        let bad = ScaffoldOptions {
            name: "1bad".into(),
            ..opts
        };
        assert!(scaffold(&tmp.path().join("bad"), &bad).is_err());
        assert!(!tmp.path().join("bad").exists());
    }

    #[test]
    fn patch_is_noop_without_matching_jobs() {
        let yaml = "jobs:\n  test:\n    runs-on: ubuntu-latest\n";
        assert_eq!(patch_workflow_for_mobile_crate(yaml, "x-mobile"), yaml);
    }
}
