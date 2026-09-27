//! End-to-end tests of the `space3d` binary.
//!
//! The `cargo check` tests compile a freshly generated project against this workspace. They
//! reuse this workspace's `Cargo.lock` and target directory, so only the new game crate is
//! compiled once the engine has been built — but on a cold target directory they take minutes,
//! hence `#[ignore]`: run them with `cargo test -p space3d-cli -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn space3d() -> Command {
    Command::new(env!("CARGO_BIN_EXE_space3d"))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("failed to spawn");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{cmd:?} failed:\n{text}");
    text
}

/// `space3d new <name> --template <t> --space3d-path <workspace>` in a temp dir.
fn generate(template: &str, name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    run(space3d()
        .current_dir(tmp.path())
        .args(["new", name, "--template", template, "--space3d-path"])
        .arg(workspace_root()));
    let dir = tmp.path().join(name);
    // Same dependency versions as the engine workspace (and reuse its compiled artifacts).
    std::fs::copy(workspace_root().join("Cargo.lock"), dir.join("Cargo.lock")).unwrap();
    (tmp, dir)
}

#[test]
fn new_writes_a_complete_project() {
    let (_tmp, dir) = generate("basic", "hello_game");
    for file in [
        "Cargo.toml",
        "src/main.rs",
        "src/lib.rs",
        "src/stories.rs",
        "src/bin/stories.rs",
        "src/bin/editor.rs",
        "tests/input.rs",
        "tests/screenshots.rs",
        "web/index.html",
        "mobile/Cargo.toml",
        "ios/project.yml",
        "android/README.md",
        "android/build.gradle.kts",
        "android/settings.gradle.kts",
        "android/gradle.properties",
        "android/src/main/AndroidManifest.xml",
        ".github/workflows/space3d.yml",
        ".gitignore",
        "README.md",
    ] {
        assert!(dir.join(file).is_file(), "missing {file}");
    }
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    // Forward slashes and no `\\?\` verbatim prefix, also on Windows.
    let facade = workspace_root().join("crates").join("space3d");
    let facade = facade.to_string_lossy();
    let facade = facade.trim_start_matches(r"\\?\").replace('\\', "/");
    assert!(manifest.contains(&format!("path = \"{facade}\"")), "{manifest}");
    let main = std::fs::read_to_string(dir.join("src/main.rs")).unwrap();
    assert!(main.contains("space3d::main!(hello_game::build);"));
    let mobile = std::fs::read_to_string(dir.join("mobile/Cargo.toml")).unwrap();
    assert!(mobile.contains("crate-type = [\"cdylib\", \"staticlib\"]"));
    // Android packaging lives in the Gradle project, which loads the mobile crate's library.
    let manifest = std::fs::read_to_string(dir.join("android/src/main/AndroidManifest.xml")).unwrap();
    assert!(manifest.contains("android:value=\"hello_game_mobile\""), "{manifest}");
    let gradle = std::fs::read_to_string(dir.join("android/build.gradle.kts")).unwrap();
    assert!(
        gradle.contains("applicationId = \"com.example.hello_game\""),
        "{gradle}"
    );

    // Refuses to overwrite.
    let again = space3d()
        .current_dir(dir.parent().unwrap())
        .args(["new", "hello_game", "--space3d-path"])
        .arg(workspace_root())
        .output()
        .unwrap();
    assert!(!again.status.success());
}

#[test]
fn hyphenated_project_resolves() {
    let (_tmp, dir) = generate("basic", "my-game");
    let mobile = std::fs::read_to_string(dir.join("mobile/Cargo.toml")).unwrap();
    assert!(
        mobile.contains(r#"my_game = { package = "my-game", path = ".." }"#),
        "{mobile}"
    );
    let main = std::fs::read_to_string(dir.join("src/main.rs")).unwrap();
    assert!(main.contains("space3d::main!(my_game::build);"));
    // Full dependency resolution (not `--no-deps`): the mobile crate must find the game package.
    let out = run(Command::new(env!("CARGO"))
        .current_dir(&dir)
        .args(["metadata", "--format-version", "1"]));
    assert!(out.contains(r#""name":"my-game-mobile""#), "{out}");
    // Kinsoku: the generated `[patch.crates-io]` gives egui this checkout's epaint.
    assert!(!out.contains("was not used"), "{out}");
    let meta: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    let epaint: Vec<PathBuf> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"] == "epaint")
        .map(|p| PathBuf::from(p["manifest_path"].as_str().unwrap()))
        .collect();
    let vendored = workspace_root().join("third_party/epaint/Cargo.toml");
    assert_eq!(epaint.len(), 1, "{epaint:?}");
    assert_eq!(epaint[0].canonicalize().unwrap(), vendored.canonicalize().unwrap());
    // Names that would trip the generated CI's `-D warnings` (non_snake_case) are refused.
    let upper = space3d()
        .current_dir(dir.parent().unwrap())
        .args(["new", "Asteroids", "--space3d-path"])
        .arg(workspace_root())
        .output()
        .unwrap();
    assert!(!upper.status.success());
    assert!(String::from_utf8_lossy(&upper.stderr).contains("lower case"));
}

#[test]
fn project_commands_work_in_a_generated_project() {
    let (_tmp, dir) = generate("basic", "cmd_game");
    // `cargo metadata`-driven commands, without building anything.
    let out = run(space3d().current_dir(&dir).args(["--dry-run", "run", "--frames", "5"]));
    assert!(out.contains("SPACE3D_EXIT_AFTER_FRAMES=5"), "{out}");
    assert!(out.contains("run -p cmd_game --bin cmd_game"), "{out}");
    let out = run(space3d()
        .current_dir(&dir)
        .args(["--dry-run", "stories", "--headless", "--out", "shots"]));
    assert!(out.contains("--bin stories -- --headless --out"), "{out}");
    let out = run(space3d()
        .current_dir(&dir)
        .args(["--dry-run", "build", "--target", "web", "--release"]));
    assert!(out.contains("--target wasm32-unknown-unknown"), "{out}");
    assert!(
        out.contains("wasm-bindgen --target web --no-typescript --out-name cmd_game"),
        "{out}"
    );
    let out = run(space3d()
        .current_dir(&dir)
        .args(["--dry-run", "build", "--target", "android"]));
    assert!(
        out.contains("ndk -t arm64-v8a -P 28 build -p cmd_game-mobile --lib"),
        "{out}"
    );
    // The project's own Gradle project packages the library into dist/android/cmd_game.apk.
    // (Its own `android/`: nothing is generated into target/android/cmd_game/gradle.)
    assert!(out.contains("--project-cache-dir"), "{out}");
    assert!(!out.contains("write the Gradle project"), "{out}");
    assert!(out.contains("assembleDebug"), "{out}");
    assert!(out.contains("dist/android/cmd_game.apk"), "{out}");
    let out = run(space3d().current_dir(&dir).args([
        "--dry-run",
        "publish",
        "--tag",
        "v0.1.0",
        "--repo",
        "acme/cmd_game",
        "--targets",
        "web",
    ]));
    assert!(out.contains("\"tag_name\": \"v0.1.0\""), "{out}");
    assert!(out.contains("cmd_game-v0.1.0-web.tar.gz"), "{out}");
    let out = run(space3d()
        .current_dir(&dir)
        .args(["ci", "init", "--targets", "web", "--force"]));
    assert!(out.contains("space3d.yml"), "{out}");
    // `--report` never deletes a directory that is not a previous report.
    std::fs::create_dir_all(dir.join("notes")).unwrap();
    std::fs::write(dir.join("notes/important.txt"), "precious").unwrap();
    let test = space3d()
        .current_dir(&dir)
        .args(["test", "--report", "notes"])
        .output()
        .unwrap();
    assert!(!test.status.success());
    assert!(String::from_utf8_lossy(&test.stderr).contains("pick a new or empty directory"));
    assert!(dir.join("notes/important.txt").is_file());
    let out = run(space3d().current_dir(&dir).args(["--dry-run", "editor"]));
    assert!(out.contains("run -p cmd_game --bin editor"), "{out}");
    // A missing binary is explained, not a crash.
    std::fs::remove_file(dir.join("src/bin/editor.rs")).unwrap();
    let editor = space3d()
        .current_dir(&dir)
        .args(["--dry-run", "editor"])
        .output()
        .unwrap();
    assert!(!editor.status.success());
    assert!(String::from_utf8_lossy(&editor.stderr).contains("src/bin/editor.rs"));
}

#[test]
fn doctor_reports() {
    let out = space3d().args(["doctor", "--platforms", "web"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("rustc"), "{text}");
    assert!(text.contains("wasm-bindgen CLI"), "{text}");
    // Android: every tool of the build is reported (found or with a fix), whatever is installed.
    let out = space3d().args(["doctor", "--platforms", "android"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for what in [
        "target aarch64-linux-android",
        "cargo-ndk",
        "Android SDK",
        "Android NDK",
        "Java",
        "Gradle",
    ] {
        assert!(text.contains(what), "{what}: {text}");
    }
    assert!(!text.contains("cargo-apk"), "{text}");
}

fn cargo_check(dir: &Path) {
    let status = Command::new(env!("CARGO"))
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target_dir())
        .args(["check", "--workspace", "--all-targets", "--quiet"])
        .status()
        .unwrap();
    assert!(status.success(), "cargo check failed in {}", dir.display());
}

#[test]
#[ignore = "compiles a generated project (slow on a cold target dir); run with --ignored"]
fn generated_basic_project_compiles() {
    let (_tmp, dir) = generate("basic", "check_basic");
    cargo_check(&dir);
}

#[test]
#[ignore = "compiles a generated project (slow on a cold target dir); run with --ignored"]
fn generated_3d_project_compiles() {
    let (_tmp, dir) = generate("3d", "check_3d");
    cargo_check(&dir);
}

#[test]
#[ignore = "compiles a generated project (slow on a cold target dir); run with --ignored"]
fn generated_multiplayer_project_compiles() {
    let (_tmp, dir) = generate("multiplayer", "check_multiplayer");
    cargo_check(&dir);
}
