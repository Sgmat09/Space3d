//! Golden screenshot testing.
//!
//! [`assert_screenshot!`](crate::assert_screenshot) renders (or takes) an image and compares it
//! with `<crate>/tests/__screenshots__/<name>.png`:
//!
//! * **missing golden** → written and the test passes with a notice, unless running in CI
//!   (`SPACE3D_CI=1` / `CI=true`), where it fails;
//! * **`SPACE3D_UPDATE_SNAPSHOTS=1`** (or `UPDATE_SNAPSHOTS=1`) → goldens are re-blessed;
//! * **mismatch** → `<name>.actual.png` and `<name>.diff.png` (differences in magenta over the
//!   dimmed golden) are written next to the golden and the test panics with statistics;
//! * **no GPU adapter** (headless CI, wasm32) → the assertion is skipped with a notice
//!   (`SPACE3D_REQUIRE_GPU=1` turns that into a failure). When an adapter exists, a source
//!   without image (app without renderer, failed capture) is a failure, never a skip.
//!
//! GPUs differ slightly, so goldens may have platform variants that take precedence over the
//! generic file: `<name>.<os>-<backend>.png`, `<name>.<backend>.png`, `<name>.<os>.png`, then
//! `<name>.png` (e.g. `player.linux-vulkan.png`). New goldens are written generic, and
//! re-blessing rewrites the golden that was compared against, unless
//! `SPACE3D_SNAPSHOT_VARIANT=os|backend|os-backend` asks for a variant: then new and re-blessed
//! goldens go to that variant file and the generic golden is left untouched (this is how a
//! second platform adds its own goldens next to the committed ones:
//! `SPACE3D_UPDATE_SNAPSHOTS=1 SPACE3D_SNAPSHOT_VARIANT=backend cargo test`).
//!
//! Names may contain `/` (sub-directories); other characters outside `[A-Za-z0-9_-]` —
//! including `.`, which is reserved for variant and artifact suffixes — become `_`.
//!
//! When the harness creates a `__screenshots__` directory it also writes a `.gitignore` for the
//! `*.actual.png` / `*.diff.png` failure artifacts.

use crate::app::TestApp;
use crate::compare::{CompareOptions, DiffStats, compare_images, diff_image_with};
use crate::env;
use crate::gpu::{self, GpuInfo};
use crate::image_io::{load_png, save_png};
use crate::report::{self, ReportEntry, ReportImages, ReportKind, ReportStatus};
use space3d_core::{App, ImageData};
use std::path::{Path, PathBuf};

/// An image plus the backend that produced it (selects golden variants).
#[derive(Clone, Debug, PartialEq)]
pub struct Captured {
    /// Pixels (RGBA8, sRGB).
    pub image: ImageData,
    /// Render backend (`metal`, `vulkan`, ...), if known.
    pub backend: Option<String>,
}

impl Captured {
    /// Image with an explicit backend.
    pub fn new(image: ImageData, backend: Option<String>) -> Self {
        Self { image, backend }
    }
}

/// Why a screenshot source produced no image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoScreenshot {
    /// The app has no renderer; the text says why (rendering disabled, renderer creation
    /// error, no GPU).
    NoRenderer(String),
    /// A renderer exists but rendering or read-back failed (the renderer logs the error).
    CaptureFailed(String),
    /// The source was `None` (e.g. an `Option<ImageData>` from `TestApp::screenshot`).
    Empty,
}

impl std::fmt::Display for NoScreenshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoScreenshot::NoRenderer(why) => write!(f, "the app has no renderer: {why}"),
            NoScreenshot::CaptureFailed(why) => write!(f, "frame capture failed: {why}"),
            NoScreenshot::Empty => {
                f.write_str("no image (the source was `None`: the app had no renderer or the capture failed)")
            }
        }
    }
}

impl std::error::Error for NoScreenshot {}

/// Anything [`assert_screenshot!`](crate::assert_screenshot) accepts: images, or apps that are
/// rendered on the spot.
///
/// An `Err` skips the assertion only when the process has no GPU adapter; with an adapter it
/// fails the assertion.
pub trait IntoScreenshot {
    /// Produce the image to compare.
    fn into_screenshot(self) -> Result<Captured, NoScreenshot>;
}

fn process_backend() -> Option<String> {
    gpu::gpu_info().ok().map(|g| g.backend.clone())
}

const CAPTURE_FAILED: &str = "rendering or read-back returned no image (see the log for the renderer error)";

impl IntoScreenshot for Captured {
    fn into_screenshot(self) -> Result<Captured, NoScreenshot> {
        Ok(self)
    }
}

impl IntoScreenshot for ImageData {
    /// The backend is assumed to be the process' GPU backend (if any).
    fn into_screenshot(self) -> Result<Captured, NoScreenshot> {
        Ok(Captured::new(self, process_backend()))
    }
}

impl IntoScreenshot for &ImageData {
    fn into_screenshot(self) -> Result<Captured, NoScreenshot> {
        self.clone().into_screenshot()
    }
}

impl IntoScreenshot for Option<ImageData> {
    fn into_screenshot(self) -> Result<Captured, NoScreenshot> {
        self.ok_or(NoScreenshot::Empty)?.into_screenshot()
    }
}

impl IntoScreenshot for &mut TestApp {
    fn into_screenshot(self) -> Result<Captured, NoScreenshot> {
        if let Some(why) = self.renderer_error() {
            return Err(NoScreenshot::NoRenderer(why.to_string()));
        }
        let backend = self.backend().map(str::to_string);
        self.screenshot()
            .map(|image| Captured::new(image, backend))
            .ok_or_else(|| NoScreenshot::CaptureFailed(CAPTURE_FAILED.to_string()))
    }
}

impl IntoScreenshot for &mut App {
    fn into_screenshot(self) -> Result<Captured, NoScreenshot> {
        let Some(renderer) = self.resources.get::<space3d_render::Renderer>() else {
            return Err(NoScreenshot::NoRenderer("no `Renderer` resource".to_string()));
        };
        let backend = Some(GpuInfo::from_renderer(renderer).backend);
        space3d_render::render_to_image(self)
            .map(|image| Captured::new(image, backend))
            .ok_or_else(|| NoScreenshot::CaptureFailed(CAPTURE_FAILED.to_string()))
    }
}

/// A directory of golden images plus the suite name used in reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoldenDir {
    /// Directory holding `<name>.png` files.
    pub dir: PathBuf,
    /// Suite (usually the crate name) for reports.
    pub suite: String,
}

impl GoldenDir {
    /// Goldens in `dir`.
    pub fn new(dir: impl Into<PathBuf>, suite: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            suite: suite.into(),
        }
    }

    /// `<manifest_dir>/tests/__screenshots__` — what the macros use with the calling crate's
    /// `CARGO_MANIFEST_DIR` / `CARGO_PKG_NAME`.
    pub fn for_crate(manifest_dir: &str, package: &str) -> Self {
        Self::new(Path::new(manifest_dir).join("tests").join("__screenshots__"), package)
    }
}

/// Which golden file new/updated screenshots are written to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VariantPolicy {
    /// `<name>.png` (default).
    #[default]
    Generic,
    /// `<name>.<os>.png`.
    Os,
    /// `<name>.<backend>.png`.
    Backend,
    /// `<name>.<os>-<backend>.png`.
    OsBackend,
}

/// How goldens are handled; normally read from the environment ([`GoldenPolicy::from_env`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GoldenPolicy {
    /// Re-bless goldens instead of comparing.
    pub update: bool,
    /// Missing goldens fail.
    pub ci: bool,
    /// Where new goldens go.
    pub variant: VariantPolicy,
}

impl GoldenPolicy {
    /// From `SPACE3D_UPDATE_SNAPSHOTS`/`UPDATE_SNAPSHOTS`, `SPACE3D_CI`/`CI` and
    /// `SPACE3D_SNAPSHOT_VARIANT`.
    pub fn from_env() -> Self {
        let variant = match std::env::var(env::SNAPSHOT_VARIANT)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "os" => VariantPolicy::Os,
            "backend" => VariantPolicy::Backend,
            "os-backend" | "os_backend" | "all" => VariantPolicy::OsBackend,
            _ => VariantPolicy::Generic,
        };
        Self {
            update: env::update_snapshots(),
            ci: env::is_ci(),
            variant,
        }
    }
}

/// Successful outcome of a screenshot check.
#[derive(Clone, Debug, PartialEq)]
pub enum ScreenshotOutcome {
    /// Matched the golden at `golden`.
    Matched {
        /// Golden file used.
        golden: PathBuf,
        /// Comparison statistics.
        stats: DiffStats,
    },
    /// No golden existed; one was written.
    Created {
        /// New golden file.
        golden: PathBuf,
    },
    /// Golden re-blessed (`SPACE3D_UPDATE_SNAPSHOTS`).
    Updated {
        /// Rewritten golden file.
        golden: PathBuf,
    },
    /// Nothing to compare (no renderer).
    Skipped {
        /// Why.
        reason: String,
    },
}

/// Why a screenshot check failed.
#[derive(Clone, Debug, PartialEq)]
pub enum FailureKind {
    /// No golden and running in CI.
    MissingGolden,
    /// Images differ beyond the tolerance.
    Mismatch(Box<DiffStats>),
    /// Reading/writing files failed.
    Io(String),
}

/// A failed screenshot check. `Display` renders the full panic message.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenshotFailure {
    /// Screenshot name.
    pub name: String,
    /// Golden that was compared against (or would have been created).
    pub golden: PathBuf,
    /// What went wrong.
    pub kind: FailureKind,
    /// Written `<name>.actual.png`, if any.
    pub actual: Option<PathBuf>,
    /// Written `<name>.diff.png`, if any.
    pub diff: Option<PathBuf>,
}

impl std::fmt::Display for ScreenshotFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            FailureKind::MissingGolden => writeln!(
                f,
                "screenshot {:?}: golden {} is missing (running in CI, so it is not created)",
                self.name,
                self.golden.display()
            )?,
            FailureKind::Mismatch(stats) => {
                writeln!(
                    f,
                    "screenshot {:?} does not match golden {}",
                    self.name,
                    self.golden.display()
                )?;
                for r in stats.failure_reasons() {
                    writeln!(f, "  - {r}")?;
                }
                writeln!(f, "  stats: {}", stats.summary())?;
            }
            FailureKind::Io(e) => writeln!(f, "screenshot {:?}: I/O error: {e}", self.name)?,
        }
        if let Some(a) = &self.actual {
            writeln!(f, "  actual: {}", a.display())?;
        }
        if let Some(d) = &self.diff {
            writeln!(f, "  diff:   {}", d.display())?;
        }
        write!(
            f,
            "  to accept the new output run with {}=1 (then review and commit the PNGs)",
            env::UPDATE_SNAPSHOTS
        )
    }
}

impl std::error::Error for ScreenshotFailure {}

/// Turn a screenshot name into a relative path: segments separated by `/` are kept as
/// sub-directories, other characters outside `[A-Za-z0-9_-]` become `_`.
///
/// `.` is replaced too: golden file names use it for variant (`name.metal.png`) and failure
/// artifact (`name.actual.png`, `name.diff.png`) suffixes, so a name like `menu.diff` or
/// `hero.metal` must not collide with those.
pub fn sanitize_name(name: &str) -> String {
    let segments: Vec<String> = name
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .map(|s| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        })
        .collect();
    if segments.is_empty() {
        "unnamed".to_string()
    } else {
        segments.join("/")
    }
}

fn variant_suffixes(backend: Option<&str>) -> Vec<(VariantPolicy, String)> {
    let os = gpu::os_name();
    let mut v = Vec::new();
    if let Some(b) = backend {
        v.push((VariantPolicy::OsBackend, format!("{os}-{b}")));
        v.push((VariantPolicy::Backend, b.to_string()));
    }
    v.push((VariantPolicy::Os, os.to_string()));
    v
}

/// Candidate golden paths for `name`, most specific first (the generic `<name>.png` last).
pub fn golden_candidates(dir: &Path, name: &str, backend: Option<&str>) -> Vec<PathBuf> {
    let stem = sanitize_name(name);
    let mut out: Vec<PathBuf> = variant_suffixes(backend)
        .into_iter()
        .map(|(_, s)| dir.join(format!("{stem}.{s}.png")))
        .collect();
    out.push(dir.join(format!("{stem}.png")));
    out
}

/// The variant file `policy` asks for; `None` for [`VariantPolicy::Generic`] or when the
/// variant needs a backend and none is known.
fn variant_path(dir: &Path, name: &str, backend: Option<&str>, policy: VariantPolicy) -> Option<PathBuf> {
    let stem = sanitize_name(name);
    variant_suffixes(backend)
        .into_iter()
        .find(|(p, _)| *p == policy)
        .map(|(_, s)| dir.join(format!("{stem}.{s}.png")))
}

fn generic_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.png", sanitize_name(name)))
}

fn artifact_paths(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let stem = sanitize_name(name);
    (
        dir.join(format!("{stem}.actual.png")),
        dir.join(format!("{stem}.diff.png")),
    )
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::write(
            dir.join(".gitignore"),
            "# space3d-test failure artifacts\n*.actual.png\n*.diff.png\n",
        )?;
    }
    Ok(())
}

fn write_golden(root: &Path, path: &Path, image: &ImageData) -> Result<(), String> {
    ensure_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;
    save_png(path, image).map_err(|e| e.to_string())
}

/// Compare `captured` with the golden `name` in `golden` without panicking.
///
/// Also appends a record to the `SPACE3D_TEST_REPORT` report when that is set.
pub fn check_screenshot(
    golden: &GoldenDir,
    name: &str,
    captured: Captured,
    options: &CompareOptions,
    policy: &GoldenPolicy,
) -> Result<ScreenshotOutcome, ScreenshotFailure> {
    let backend = captured.backend.as_deref();
    let actual = &captured.image;
    let existing = golden_candidates(&golden.dir, name, backend)
        .into_iter()
        .find(|p| p.is_file());
    let (actual_path, diff_path) = artifact_paths(&golden.dir, name);
    let clear_artifacts = || {
        let _ = std::fs::remove_file(&actual_path);
        let _ = std::fs::remove_file(&diff_path);
    };
    let fail = |golden_path: PathBuf, kind: FailureKind, actual_written: bool, diff_written: bool| ScreenshotFailure {
        name: name.to_string(),
        golden: golden_path,
        kind,
        actual: actual_written.then(|| actual_path.clone()),
        diff: diff_written.then(|| diff_path.clone()),
    };

    // (outcome or failure, golden image, diff image) — the images feed the test report.
    type WithImages<T> = (T, Option<ImageData>, Option<ImageData>);
    let variant = variant_path(&golden.dir, name, backend, policy.variant);
    let result: Result<WithImages<ScreenshotOutcome>, WithImages<ScreenshotFailure>> = if policy.update {
        // A variant policy re-blesses into the variant file and never touches the generic
        // golden (another platform's reference); otherwise the golden compared against is
        // rewritten.
        let path = variant
            .or_else(|| existing.clone())
            .unwrap_or_else(|| generic_path(&golden.dir, name));
        match write_golden(&golden.dir, &path, actual) {
            Ok(()) => {
                clear_artifacts();
                eprintln!("space3d-test: updated golden {}", path.display());
                Ok((ScreenshotOutcome::Updated { golden: path }, None, None))
            }
            Err(e) => Err((fail(path, FailureKind::Io(e), false, false), None, None)),
        }
    } else {
        match existing {
            None => {
                let path = variant.unwrap_or_else(|| generic_path(&golden.dir, name));
                if policy.ci {
                    let wrote = ensure_dir(&golden.dir).is_ok() && save_png(&actual_path, actual).is_ok();
                    Err((fail(path, FailureKind::MissingGolden, wrote, false), None, None))
                } else {
                    match write_golden(&golden.dir, &path, actual) {
                        Ok(()) => {
                            eprintln!("space3d-test: created golden {} — review and commit it", path.display());
                            Ok((ScreenshotOutcome::Created { golden: path }, None, None))
                        }
                        Err(e) => Err((fail(path, FailureKind::Io(e), false, false), None, None)),
                    }
                }
            }
            Some(path) => match load_png(&path) {
                Err(e) => Err((fail(path, FailureKind::Io(e.to_string()), false, false), None, None)),
                Ok(expected) => {
                    let stats = compare_images(&expected, actual, options);
                    if stats.passed {
                        clear_artifacts();
                        Ok((ScreenshotOutcome::Matched { golden: path, stats }, Some(expected), None))
                    } else {
                        let diff = diff_image_with(&expected, actual, options);
                        let a = save_png(&actual_path, actual).is_ok();
                        let d = save_png(&diff_path, &diff).is_ok();
                        Err((
                            fail(path, FailureKind::Mismatch(Box::new(stats)), a, d),
                            Some(expected),
                            Some(diff),
                        ))
                    }
                }
            },
        }
    };

    if let Some(report) = report::TestReport::from_env() {
        let (status, message, details, expected, diff) = match &result {
            Ok((outcome, expected, _)) => {
                let (status, details) = match outcome {
                    ScreenshotOutcome::Matched { stats, .. } => {
                        (ReportStatus::Passed, serde_json::to_value(stats).ok())
                    }
                    ScreenshotOutcome::Created { .. } => (ReportStatus::Created, None),
                    ScreenshotOutcome::Updated { .. } => (ReportStatus::Updated, None),
                    ScreenshotOutcome::Skipped { .. } => (ReportStatus::Skipped, None),
                };
                (status, None, details, expected.as_ref(), None)
            }
            Err((failure, expected, diff)) => {
                let details = match &failure.kind {
                    FailureKind::Mismatch(stats) => serde_json::to_value(stats).ok(),
                    _ => None,
                };
                (
                    ReportStatus::Failed,
                    Some(failure.to_string()),
                    details,
                    expected.as_ref(),
                    diff.as_ref(),
                )
            }
        };
        let entry = ReportEntry::new(&golden.suite, name, ReportKind::Screenshot, status)
            .message(message)
            .details(details.unwrap_or(serde_json::Value::Null));
        report.record_or_warn(
            entry,
            ReportImages {
                expected,
                actual: Some(actual),
                diff,
            },
        );
    }

    result.map(|(o, _, _)| o).map_err(|(f, _, _)| f)
}

fn record_status(golden: &GoldenDir, name: &str, status: ReportStatus, message: &str) {
    if let Some(report) = report::TestReport::from_env() {
        report.record_or_warn(
            ReportEntry::new(&golden.suite, name, ReportKind::Screenshot, status).message(Some(message.to_string())),
            ReportImages::default(),
        );
    }
}

/// Assert that `source` matches golden `name` in `golden` (policy from the environment).
///
/// Panics with a detailed message on mismatch. When `source` has no image the assertion is
/// skipped (with a notice) only if the process has no GPU adapter; with an adapter that is a
/// failure (app built without renderer, renderer creation or capture error).
#[track_caller]
pub fn assert_screenshot_in(
    golden: &GoldenDir,
    name: &str,
    source: impl IntoScreenshot,
    options: &CompareOptions,
) -> ScreenshotOutcome {
    let captured = match source.into_screenshot() {
        Ok(captured) => captured,
        Err(why) => match gpu::gpu_info() {
            Err(reason) => {
                gpu::skip_notice(&format!("screenshot {name:?}"), reason);
                record_status(golden, name, ReportStatus::Skipped, reason);
                return ScreenshotOutcome::Skipped {
                    reason: reason.to_string(),
                };
            }
            Ok(gpu) => {
                let msg = format!(
                    "screenshot {name:?}: nothing to compare although a GPU adapter is available ({}, {}): {why}",
                    gpu.name, gpu.backend
                );
                record_status(golden, name, ReportStatus::Failed, &msg);
                panic!("{msg}");
            }
        },
    };
    match check_screenshot(golden, name, captured, options, &GoldenPolicy::from_env()) {
        Ok(outcome) => outcome,
        Err(failure) => panic!("{failure}"),
    }
}

/// Compare an app's rendering (or an image) with a golden PNG in the calling crate's
/// `tests/__screenshots__/` directory.
///
/// ```no_run
/// use space3d_test::{CompareOptions, TestApp, assert_screenshot};
/// let mut app = TestApp::headless(128, 128);
/// // ... spawn a camera and a scene ...
/// assert_screenshot!(&mut app, "my_scene");
/// assert_screenshot!(&mut app, "my_scene_lenient", CompareOptions::perceptual());
/// if let Some(image) = app.screenshot() {
///     assert_screenshot!(&image, "my_scene");
/// }
/// ```
#[macro_export]
macro_rules! assert_screenshot {
    ($source:expr, $name:expr $(,)?) => {
        $crate::screenshot::assert_screenshot_in(
            &$crate::screenshot::GoldenDir::for_crate(env!("CARGO_MANIFEST_DIR"), env!("CARGO_PKG_NAME")),
            $name,
            $source,
            &$crate::CompareOptions::default(),
        )
    };
    ($source:expr, $name:expr, $options:expr $(,)?) => {
        $crate::screenshot::assert_screenshot_in(
            &$crate::screenshot::GoldenDir::for_crate(env!("CARGO_MANIFEST_DIR"), env!("CARGO_PKG_NAME")),
            $name,
            $source,
            &$options,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(color: [u8; 4]) -> ImageData {
        ImageData::filled(8, 8, color)
    }

    fn cap(image: ImageData) -> Captured {
        Captured::new(image, Some("testgpu".into()))
    }

    const LOCAL: GoldenPolicy = GoldenPolicy {
        update: false,
        ci: false,
        variant: VariantPolicy::Generic,
    };

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_name("Engine/Lit Cube!"), "Engine/Lit_Cube_");
        assert_eq!(sanitize_name("../../etc/passwd"), "etc/passwd");
        assert_eq!(sanitize_name("//"), "unnamed");
        // Dots would collide with variant / artifact suffixes.
        assert_eq!(sanitize_name("menu.diff"), "menu_diff");
        assert_eq!(sanitize_name("ui/hero.metal"), "ui/hero_metal");
        assert_eq!(sanitize_name("v1.2/.../x"), "v1_2/___/x");
    }

    #[test]
    fn create_match_mismatch_update() {
        // Exercises failure paths on purpose: keep them out of a real test report.
        crate::report::without_report(|| {
            let tmp = tempfile::tempdir().unwrap();
            let g = GoldenDir::new(tmp.path().join("__screenshots__"), "suite");
            let opts = CompareOptions::default();

            // Missing in CI → failure, actual written, nothing blessed.
            let ci = GoldenPolicy { ci: true, ..LOCAL };
            let f = check_screenshot(&g, "group/shot", cap(img([1, 2, 3, 255])), &opts, &ci).unwrap_err();
            assert_eq!(f.kind, FailureKind::MissingGolden);
            assert!(f.actual.as_ref().unwrap().is_file());
            assert!(!g.dir.join("group/shot.png").exists());
            assert!(g.dir.join(".gitignore").is_file());

            // Missing locally → created.
            let o = check_screenshot(&g, "group/shot", cap(img([1, 2, 3, 255])), &opts, &LOCAL).unwrap();
            assert_eq!(
                o,
                ScreenshotOutcome::Created {
                    golden: g.dir.join("group/shot.png")
                }
            );

            // Match (within tolerance) clears stale artifacts.
            let o = check_screenshot(&g, "group/shot", cap(img([2, 2, 3, 255])), &opts, &LOCAL).unwrap();
            assert!(matches!(o, ScreenshotOutcome::Matched { .. }), "{o:?}");
            assert!(!g.dir.join("group/shot.actual.png").exists());

            // Mismatch → actual + diff written, message has stats and paths.
            let f = check_screenshot(&g, "group/shot", cap(img([200, 2, 3, 255])), &opts, &LOCAL).unwrap_err();
            let FailureKind::Mismatch(stats) = &f.kind else {
                panic!("{f:?}")
            };
            assert_eq!(stats.differing_pixels, 64);
            let msg = f.to_string();
            assert!(msg.contains("64 of 64 pixels differ"), "{msg}");
            assert!(
                msg.contains("shot.diff.png") && msg.contains("SPACE3D_UPDATE_SNAPSHOTS"),
                "{msg}"
            );
            let diff = load_png(f.diff.as_ref().unwrap()).unwrap();
            assert_eq!(diff.get(0, 0), [255, 0, 255, 255]);
            assert_eq!(load_png(f.actual.as_ref().unwrap()).unwrap(), img([200, 2, 3, 255]));

            // Update re-blesses.
            let up = GoldenPolicy { update: true, ..LOCAL };
            let o = check_screenshot(&g, "group/shot", cap(img([200, 2, 3, 255])), &opts, &up).unwrap();
            assert!(matches!(o, ScreenshotOutcome::Updated { .. }));
            assert!(!f.diff.as_ref().unwrap().exists());
            assert!(
                check_screenshot(
                    &g,
                    "group/shot",
                    cap(img([200, 2, 3, 255])),
                    &CompareOptions::exact(),
                    &LOCAL
                )
                .is_ok()
            );
        });
    }

    #[test]
    fn variants_take_precedence() {
        // Exercises failure paths on purpose: keep them out of a real test report.
        crate::report::without_report(|| {
            let tmp = tempfile::tempdir().unwrap();
            let g = GoldenDir::new(tmp.path(), "suite");
            let os = gpu::os_name();
            save_png(tmp.path().join("v.png"), &img([0, 0, 0, 255])).unwrap();
            save_png(tmp.path().join("v.testgpu.png"), &img([9, 9, 9, 255])).unwrap();
            let exact = CompareOptions::exact();
            // Backend variant wins over the generic file.
            assert!(check_screenshot(&g, "v", cap(img([9, 9, 9, 255])), &exact, &LOCAL).is_ok());
            // Other backends fall back to the generic file.
            let other = Captured::new(img([0, 0, 0, 255]), Some("othergpu".into()));
            assert!(check_screenshot(&g, "v", other, &exact, &LOCAL).is_ok());
            // os-backend beats backend.
            save_png(tmp.path().join(format!("v.{os}-testgpu.png")), &img([5, 5, 5, 255])).unwrap();
            let o = check_screenshot(&g, "v", cap(img([5, 5, 5, 255])), &exact, &LOCAL).unwrap();
            assert_eq!(
                o,
                ScreenshotOutcome::Matched {
                    golden: tmp.path().join(format!("v.{os}-testgpu.png")),
                    stats: compare_images(&img([5, 5, 5, 255]), &img([5, 5, 5, 255]), &exact)
                }
            );
            // New goldens honor the variant policy.
            let p = GoldenPolicy {
                variant: VariantPolicy::Backend,
                ..LOCAL
            };
            let o = check_screenshot(&g, "w", cap(img([1, 1, 1, 255])), &exact, &p).unwrap();
            assert_eq!(
                o,
                ScreenshotOutcome::Created {
                    golden: tmp.path().join("w.testgpu.png")
                }
            );
            // Re-blessing with a variant policy adds the variant next to the generic golden and
            // leaves the generic file (another platform's reference) byte-identical.
            save_png(tmp.path().join("u.png"), &img([0, 0, 0, 255])).unwrap();
            let generic = std::fs::read(tmp.path().join("u.png")).unwrap();
            let bless_backend = GoldenPolicy {
                update: true,
                variant: VariantPolicy::Backend,
                ..LOCAL
            };
            let o = check_screenshot(&g, "u", cap(img([7, 7, 7, 255])), &exact, &bless_backend).unwrap();
            assert_eq!(
                o,
                ScreenshotOutcome::Updated {
                    golden: tmp.path().join("u.testgpu.png")
                }
            );
            assert_eq!(std::fs::read(tmp.path().join("u.png")).unwrap(), generic);
            assert!(check_screenshot(&g, "u", cap(img([7, 7, 7, 255])), &exact, &LOCAL).is_ok());
            let other = Captured::new(img([0, 0, 0, 255]), Some("othergpu".into()));
            assert!(check_screenshot(&g, "u", other, &exact, &LOCAL).is_ok());
            // A generic re-bless rewrites the golden that was compared against (the variant).
            let bless = GoldenPolicy { update: true, ..LOCAL };
            let o = check_screenshot(&g, "u", cap(img([8, 8, 8, 255])), &exact, &bless).unwrap();
            assert_eq!(
                o,
                ScreenshotOutcome::Updated {
                    golden: tmp.path().join("u.testgpu.png")
                }
            );
            assert_eq!(std::fs::read(tmp.path().join("u.png")).unwrap(), generic);
            // An os-backend variant is added even when a backend variant exists.
            let bless_os_backend = GoldenPolicy {
                update: true,
                variant: VariantPolicy::OsBackend,
                ..LOCAL
            };
            check_screenshot(&g, "u", cap(img([9, 9, 9, 255])), &exact, &bless_os_backend).unwrap();
            assert!(tmp.path().join(format!("u.{os}-testgpu.png")).is_file());
            assert_eq!(load_png(tmp.path().join("u.testgpu.png")).unwrap(), img([8, 8, 8, 255]));

            let cands = golden_candidates(tmp.path(), "x", None);
            assert_eq!(
                cands,
                vec![tmp.path().join(format!("x.{os}.png")), tmp.path().join("x.png")]
            );
        });
    }

    #[test]
    fn missing_source_skips_only_without_gpu() {
        // Exercises failure paths on purpose: keep them out of a real test report.
        crate::report::without_report(|| {
            let tmp = tempfile::tempdir().unwrap();
            let g = GoldenDir::new(tmp.path(), "suite");
            let opts = CompareOptions::default();
            if gpu::gpu_available() {
                // With an adapter, "no image" is a bug (renderer disabled, creation or capture
                // failed) and must fail instead of silently passing as skipped.
                let panic_message = |f: &mut dyn FnMut()| {
                    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_err();
                    err.downcast_ref::<String>().cloned().unwrap_or_default()
                };
                let msg = panic_message(&mut || {
                    assert_screenshot_in(&g, "nothing", None::<ImageData>, &opts);
                });
                assert!(msg.contains("nothing to compare") && msg.contains("`None`"), "{msg}");
                let mut app = TestApp::no_render();
                let msg = panic_message(&mut || {
                    assert_screenshot_in(&g, "no_renderer", &mut app, &opts);
                });
                assert!(
                    msg.contains("no renderer") && msg.contains("RenderMode::Disabled"),
                    "{msg}"
                );
                let mut bare = App::new();
                let msg = panic_message(&mut || {
                    assert_screenshot_in(&g, "bare_app", &mut bare, &opts);
                });
                assert!(msg.contains("no `Renderer` resource"), "{msg}");
            } else if !env::require_gpu() {
                let o = assert_screenshot_in(&g, "nothing", None::<ImageData>, &opts);
                assert!(matches!(o, ScreenshotOutcome::Skipped { .. }));
                let o = assert_screenshot_in(&g, "no_renderer", &mut TestApp::no_render(), &opts);
                assert!(matches!(o, ScreenshotOutcome::Skipped { .. }));
            }
            assert!(!tmp.path().join("nothing.png").exists());
        });
    }

    #[test]
    fn diff_artifact_honors_ignore_alpha() {
        crate::report::without_report(|| {
            let tmp = tempfile::tempdir().unwrap();
            let g = GoldenDir::new(tmp.path(), "suite");
            let mut expected = img([10, 20, 30, 255]);
            expected.set(0, 0, [10, 20, 30, 0]); // alpha-only difference
            save_png(tmp.path().join("a.png"), &expected).unwrap();
            let mut actual = img([10, 20, 30, 255]);
            actual.set(7, 7, [250, 20, 30, 255]); // real difference
            let opts = CompareOptions::exact().ignore_alpha(true);
            let f = check_screenshot(&g, "a", cap(actual), &opts, &LOCAL).unwrap_err();
            let FailureKind::Mismatch(stats) = &f.kind else {
                panic!("{f:?}")
            };
            assert_eq!(stats.differing_pixels, 1);
            let diff = load_png(f.diff.as_ref().unwrap()).unwrap();
            assert_eq!(diff.get(7, 7), [255, 0, 255, 255]);
            assert_ne!(
                diff.get(0, 0),
                [255, 0, 255, 255],
                "alpha-only change must not be highlighted"
            );
        });
    }
}
