//! Command-line interface definition (clap derive). Parsing is pure, so it is unit tested
//! with [`clap::Parser::try_parse_from`].

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// `space3d` — build, run, test and ship Space3d games.
#[derive(Debug, Parser)]
#[command(name = "space3d", version, about, long_about = None, propagate_version = true)]
pub struct Cli {
    /// Run as if started in this directory (the game project).
    #[arg(short = 'C', long = "project", global = true, value_name = "DIR")]
    pub project: Option<PathBuf>,

    /// Print the commands that would run (and, for `publish`, the release) without executing them.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// More output (full commands, tool detection details).
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold a new game project.
    New(NewArgs),
    /// Run the game on this desktop (`cargo run`).
    Run(RunArgs),
    /// Build for a platform: desktop, web, android or ios.
    Build(BuildArgs),
    /// Build for the web and serve `dist/web` (COOP/COEP, no-cache) on a local port.
    Serve(ServeArgs),
    /// Run the tests (incl. golden screenshots) and summarize failures.
    Test(TestArgs),
    /// Run the project's storybook binary (`cargo run --bin stories`).
    Stories(StoriesArgs),
    /// Open the editor for this project (`cargo run --bin editor`).
    Editor(EditorArgs),
    /// Build release artifacts and publish them as a GitHub release.
    Publish(PublishArgs),
    /// Continuous integration helpers.
    Ci(CiArgs),
    /// Check the toolchain and platform SDKs, and print fixes.
    Doctor(DoctorArgs),
    /// Run benchmarks (`cargo bench`).
    Bench(BenchArgs),
}

/// Project templates for `space3d new`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum TemplateKind {
    /// A lit cube you move with the arrow keys — the smallest complete game.
    #[default]
    Basic,
    /// A 3D physics playground: third-person player, crates, orbit camera.
    #[value(name = "3d")]
    ThreeD,
    /// Client + headless server over WebSockets with a shared simulation.
    Multiplayer,
}

impl TemplateKind {
    /// Directory name under `templates/`.
    pub fn dir_name(self) -> &'static str {
        match self {
            TemplateKind::Basic => "basic",
            TemplateKind::ThreeD => "3d",
            TemplateKind::Multiplayer => "multiplayer",
        }
    }
}

/// `space3d new`.
#[derive(Debug, Args)]
pub struct NewArgs {
    /// Package name (`my_game` or `my-game`: lower case, single `_`/`-` separators); also the
    /// directory name unless `--dir` is given.
    pub name: String,
    /// Which starter to use.
    #[arg(short, long, value_enum, default_value_t = TemplateKind::Basic)]
    pub template: TemplateKind,
    /// Where to create the project (default: `./<name>`).
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Depend on a local Space3d checkout instead of git (path to the repository root).
    #[arg(long, value_name = "PATH", conflicts_with = "git")]
    pub space3d_path: Option<PathBuf>,
    /// Git URL of Space3d.
    #[arg(long, value_name = "URL", default_value = crate::template::DEFAULT_GIT)]
    pub git: String,
    /// Git branch (with `--git`).
    #[arg(long, conflicts_with = "rev")]
    pub branch: Option<String>,
    /// Git revision (with `--git`).
    #[arg(long)]
    pub rev: Option<String>,
    /// Platforms for the generated GitHub Actions workflow.
    #[arg(long, default_value = "desktop,web,android,ios", value_name = "LIST")]
    pub targets: String,
    /// Don't write `.github/workflows`.
    #[arg(long)]
    pub no_ci: bool,
    /// Write into a non-empty directory (existing files are overwritten).
    #[arg(long)]
    pub force: bool,
}

/// Cargo selection flags shared by the build-ish commands.
#[derive(Clone, Debug, Default, Args)]
pub struct CargoSel {
    /// Build with the release profile.
    #[arg(short, long)]
    pub release: bool,
    /// Package to use in a workspace (default: the package in the current directory).
    #[arg(short, long, value_name = "SPEC")]
    pub package: Option<String>,
    /// Binary to build/run (default: the package's main binary).
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,
    /// Extra cargo features.
    #[arg(short = 'F', long, value_name = "FEATURES")]
    pub features: Option<String>,
}

/// `space3d run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Cargo selection.
    #[command(flatten)]
    pub cargo: CargoSel,
    /// Quit after N frames (`SPACE3D_EXIT_AFTER_FRAMES`) — handy for smoke tests.
    #[arg(long, value_name = "N")]
    pub frames: Option<u64>,
    /// No window: render offscreen (`SPACE3D_HEADLESS=1`).
    #[arg(long)]
    pub headless: bool,
    /// Arguments passed to the game.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// Build targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Platform {
    /// This machine (Windows, macOS or Linux).
    Desktop,
    /// Browser: wasm32 + wasm-bindgen bundle in `dist/web`.
    Web,
    /// Android `.apk` via `cargo apk`.
    Android,
    /// iOS static library + Xcode project (XcodeGen) / `xcodebuild`.
    Ios,
}

impl Platform {
    /// Lowercase name (also the `dist/<name>` directory).
    pub fn name(self) -> &'static str {
        match self {
            Platform::Desktop => "desktop",
            Platform::Web => "web",
            Platform::Android => "android",
            Platform::Ios => "ios",
        }
    }
}

/// `space3d build`.
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// Target platform.
    #[arg(short, long, value_enum, default_value_t = Platform::Desktop)]
    pub target: Platform,
    /// Cargo selection.
    #[command(flatten)]
    pub cargo: CargoSel,
    /// Output directory (default: `dist/<platform>` in the project).
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Install missing tools (e.g. the matching `wasm-bindgen-cli`) instead of only printing how.
    #[arg(long)]
    pub install_tools: bool,
    /// iOS: build for the simulator (`aarch64-apple-ios-sim`) instead of devices.
    #[arg(long)]
    pub sim: bool,
    /// Web: skip `wasm-opt` even when it is installed.
    #[arg(long)]
    pub no_wasm_opt: bool,
}

/// `space3d serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Port to listen on.
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Listen on all interfaces (0.0.0.0) instead of localhost — e.g. to test on a phone.
    #[arg(long)]
    pub host: bool,
    /// Cargo selection.
    #[command(flatten)]
    pub cargo: CargoSel,
    /// Rebuild when `src/`, `assets/`, `web/` or `Cargo.toml` change.
    #[arg(long)]
    pub watch: bool,
    /// Serve the existing `dist/web` without building.
    #[arg(long)]
    pub no_build: bool,
    /// Install missing tools (wasm-bindgen-cli) when needed.
    #[arg(long)]
    pub install_tools: bool,
}

/// `space3d test`.
#[derive(Debug, Args)]
pub struct TestArgs {
    /// Re-bless golden screenshots (`SPACE3D_UPDATE_SNAPSHOTS=1`).
    #[arg(long, short = 'u')]
    pub update_snapshots: bool,
    /// Report directory (default: `<target>/space3d-report`). Must be new, empty or a previous
    /// report; only the report's own files are replaced.
    #[arg(long, value_name = "DIR")]
    pub report: Option<PathBuf>,
    /// Cargo selection.
    #[command(flatten)]
    pub cargo: CargoSel,
    /// Test the whole workspace.
    #[arg(long)]
    pub workspace: bool,
    /// Extra arguments for `cargo test` (after `--`), e.g. a test name filter.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// `space3d stories`.
#[derive(Debug, Args)]
pub struct StoriesArgs {
    /// Render every story offscreen instead of opening the viewer.
    #[arg(long)]
    pub headless: bool,
    /// Output directory for `--headless` screenshots + gallery.
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Only stories whose id contains this pattern.
    #[arg(long, value_name = "PATTERN")]
    pub filter: Option<String>,
    /// List the stories and exit.
    #[arg(long)]
    pub list: bool,
    /// Cargo selection.
    #[command(flatten)]
    pub cargo: CargoSel,
    /// Extra arguments for the storybook binary.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// `space3d editor`.
#[derive(Debug, Args)]
pub struct EditorArgs {
    /// Cargo selection.
    #[command(flatten)]
    pub cargo: CargoSel,
    /// Extra arguments for the editor binary.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// `space3d publish`.
#[derive(Debug, Args)]
pub struct PublishArgs {
    /// Release tag, `vMAJOR.MINOR.PATCH` (optionally `-pre`).
    #[arg(long)]
    pub tag: String,
    /// Platforms to build and attach.
    #[arg(long, default_value = "desktop,web", value_name = "LIST")]
    pub targets: String,
    /// `owner/name` (default: parsed from `git remote get-url origin`).
    #[arg(long)]
    pub repo: Option<String>,
    /// Create the release as a draft.
    #[arg(long)]
    pub draft: bool,
    /// Mark as a pre-release (default: when the tag has a `-suffix`).
    #[arg(long)]
    pub prerelease: bool,
    /// Release notes (Markdown); GitHub generates notes when omitted.
    #[arg(long)]
    pub notes: Option<String>,
    /// Use already-built artifacts in `dist/` instead of building.
    #[arg(long)]
    pub no_build: bool,
    /// GitHub API root (GitHub Enterprise, tests).
    #[arg(long, hide = true)]
    pub api_url: Option<String>,
    /// Cargo selection.
    #[command(flatten)]
    pub cargo: CargoSel,
}

/// `space3d ci`.
#[derive(Debug, Args)]
pub struct CiArgs {
    /// CI subcommand.
    #[command(subcommand)]
    pub command: CiCommand,
}

/// `space3d ci ...`.
#[derive(Debug, Subcommand)]
pub enum CiCommand {
    /// Write `.github/workflows/space3d.yml`.
    Init(CiInitArgs),
}

/// `space3d ci init`.
#[derive(Debug, Args)]
pub struct CiInitArgs {
    /// Platforms to build (`desktop` = linux,windows,macos; `all`).
    #[arg(long, default_value = "desktop,web", value_name = "LIST")]
    pub targets: String,
    /// Binary name used for artifacts (default: the package's main binary).
    #[arg(long)]
    pub bin: Option<String>,
    /// Overwrite an existing workflow.
    #[arg(long)]
    pub force: bool,
    /// Don't deploy the web build to GitHub Pages.
    #[arg(long)]
    pub no_pages: bool,
}

/// `space3d doctor`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Only check what these platforms need (default: all).
    #[arg(long, value_enum, value_delimiter = ',')]
    pub platforms: Vec<Platform>,
}

/// `space3d bench`.
#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Cargo selection.
    #[command(flatten)]
    pub cargo: CargoSel,
    /// Arguments passed through to `cargo bench` (after `--`).
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("space3d").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn new_defaults_and_template() {
        let cli = parse(&["new", "my_game"]);
        let Command::New(a) = cli.command else { panic!() };
        assert_eq!(a.name, "my_game");
        assert_eq!(a.template, TemplateKind::Basic);
        assert_eq!(a.git, crate::template::DEFAULT_GIT);
        assert!(a.space3d_path.is_none());

        let cli = parse(&["new", "g", "--template", "3d", "--space3d-path", "/x", "--no-ci"]);
        let Command::New(a) = cli.command else { panic!() };
        assert_eq!(a.template, TemplateKind::ThreeD);
        assert_eq!(a.space3d_path, Some(PathBuf::from("/x")));
        assert!(a.no_ci);

        let cli = parse(&["new", "g", "-t", "multiplayer"]);
        let Command::New(a) = cli.command else { panic!() };
        assert_eq!(a.template, TemplateKind::Multiplayer);
    }

    #[test]
    fn new_rejects_unknown_template_and_conflicts() {
        assert!(Cli::try_parse_from(["space3d", "new", "g", "--template", "rpg"]).is_err());
        assert!(Cli::try_parse_from(["space3d", "new", "g", "--branch", "a", "--rev", "b"]).is_err());
        assert!(Cli::try_parse_from(["space3d", "new"]).is_err());
    }

    #[test]
    fn build_targets() {
        for (s, p) in [
            ("desktop", Platform::Desktop),
            ("web", Platform::Web),
            ("android", Platform::Android),
            ("ios", Platform::Ios),
        ] {
            let cli = parse(&["build", "--target", s, "--release"]);
            let Command::Build(b) = cli.command else { panic!() };
            assert_eq!(b.target, p);
            assert!(b.cargo.release);
        }
        let cli = parse(&["build"]);
        let Command::Build(b) = cli.command else { panic!() };
        assert_eq!(b.target, Platform::Desktop);
        assert!(Cli::try_parse_from(["space3d", "build", "--target", "switch"]).is_err());
    }

    #[test]
    fn global_flags_anywhere() {
        let cli = parse(&[
            "run",
            "--dry-run",
            "-C",
            "games/x",
            "--frames",
            "10",
            "--",
            "--level",
            "2",
        ]);
        assert!(cli.dry_run);
        assert_eq!(cli.project, Some(PathBuf::from("games/x")));
        let Command::Run(r) = cli.command else { panic!() };
        assert_eq!(r.frames, Some(10));
        assert_eq!(r.args, vec!["--level", "2"]);
    }

    #[test]
    fn serve_test_stories_publish_ci_doctor_bench() {
        let Command::Serve(s) = parse(&["serve", "--port", "9000", "--watch"]).command else {
            panic!()
        };
        assert_eq!((s.port, s.watch, s.no_build), (9000, true, false));

        let Command::Test(t) = parse(&["test", "--update-snapshots", "--report", "out", "--", "smoke"]).command else {
            panic!()
        };
        assert!(t.update_snapshots);
        assert_eq!(t.report, Some(PathBuf::from("out")));
        assert_eq!(t.args, vec!["smoke"]);

        let Command::Stories(s) = parse(&["stories", "--headless", "--out", "shots", "--filter", "Cube"]).command
        else {
            panic!()
        };
        assert!(s.headless);
        assert_eq!(s.filter.as_deref(), Some("Cube"));

        let cli = parse(&["publish", "--tag", "v1.2.3", "--dry-run", "--targets", "web"]);
        assert!(cli.dry_run);
        let Command::Publish(p) = cli.command else { panic!() };
        assert_eq!((p.tag.as_str(), p.targets.as_str()), ("v1.2.3", "web"));
        assert!(
            Cli::try_parse_from(["space3d", "publish"]).is_err(),
            "--tag is required"
        );

        let Command::Ci(c) = parse(&["ci", "init", "--targets", "desktop,web,android,ios"]).command else {
            panic!()
        };
        let CiCommand::Init(i) = c.command;
        assert_eq!(i.targets, "desktop,web,android,ios");

        let Command::Doctor(d) = parse(&["doctor", "--platforms", "web,ios"]).command else {
            panic!()
        };
        assert_eq!(d.platforms, vec![Platform::Web, Platform::Ios]);

        let Command::Bench(b) = parse(&["bench", "--", "--save-baseline", "main"]).command else {
            panic!()
        };
        assert_eq!(b.args, vec!["--save-baseline", "main"]);
    }
}
