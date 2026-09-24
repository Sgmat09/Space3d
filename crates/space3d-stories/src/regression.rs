//! Visual regression: every story is a golden screenshot test.

use crate::runner::{StoryError, StoryRunOptions, render_story};
use crate::{KnobValues, Story, Storybook};
use space3d_test::CompareOptions;
use space3d_test::report::{ReportEntry, ReportImages, ReportKind, ReportStatus, TestReport};
use space3d_test::screenshot::{Captured, GoldenDir, GoldenPolicy, ScreenshotOutcome, check_screenshot};
use std::path::Path;

/// Settings for [`check_storybook`] / [`assert_storybook_in`].
#[derive(Clone, Debug, PartialEq)]
pub struct StorybookTestOptions {
    /// Base comparison; a story's [`Story::tolerance`] replaces `max_diff_fraction`.
    pub compare: CompareOptions,
    /// Only stories matching this pattern ([`Storybook::select`]).
    pub filter: Option<String>,
    /// Golden handling (default: from the environment, see [`GoldenPolicy::from_env`]).
    pub policy: GoldenPolicy,
}

impl Default for StorybookTestOptions {
    fn default() -> Self {
        Self {
            compare: CompareOptions::default(),
            filter: None,
            policy: GoldenPolicy::from_env(),
        }
    }
}

impl StorybookTestOptions {
    /// Comparison options for `story` (its tolerance, if any, overrides the fraction).
    pub fn compare_for(&self, story: &Story) -> CompareOptions {
        match story.tolerance {
            Some(t) => self.compare.max_diff_fraction(t as f64),
            None => self.compare,
        }
    }
}

/// Result of one story's golden comparison.
#[derive(Clone, Debug, PartialEq)]
pub enum StoryCheckOutcome {
    /// Matched, created or re-blessed its golden.
    Passed(ScreenshotOutcome),
    /// Not compared (tag `no-screenshot`, or no GPU).
    Skipped(String),
    /// Mismatch, missing golden in CI, panic or capture error — the full message.
    Failed(String),
}

/// One story's entry in a storybook check.
#[derive(Clone, Debug, PartialEq)]
pub struct StoryCheck {
    /// Story id (also the golden name).
    pub id: String,
    /// Outcome.
    pub outcome: StoryCheckOutcome,
}

impl StoryCheck {
    /// The story failed.
    pub fn failed(&self) -> bool {
        matches!(self.outcome, StoryCheckOutcome::Failed(_))
    }
}

fn record_story_failure(golden: &GoldenDir, id: &str, message: &str) {
    if let Some(report) = TestReport::from_env() {
        report.record_or_warn(
            ReportEntry::new(&golden.suite, id, ReportKind::Story, ReportStatus::Failed)
                .message(Some(message.to_string())),
            ReportImages::default(),
        );
    }
}

/// Screenshot-test every selected story against `<golden.dir>/<story id>.png` without
/// panicking. Stories are rendered with default knobs after [`Story::ticks`] ticks.
pub fn check_storybook(book: &Storybook, golden: &GoldenDir, options: &StorybookTestOptions) -> Vec<StoryCheck> {
    book.select(options.filter.as_deref())
        .into_iter()
        .map(|story| {
            let outcome = if !story.screenshot_enabled() {
                StoryCheckOutcome::Skipped("tagged no-screenshot".to_string())
            } else {
                match render_story(story, &KnobValues::default(), &StoryRunOptions::default()) {
                    Err(e @ StoryError::NoRenderer(_)) => StoryCheckOutcome::Skipped(e.to_string()),
                    Err(e) => {
                        let msg = e.to_string();
                        record_story_failure(golden, &story.id, &msg);
                        StoryCheckOutcome::Failed(msg)
                    }
                    Ok(render) => {
                        let captured = Captured::new(render.image, render.backend);
                        match check_screenshot(
                            golden,
                            &story.id,
                            captured,
                            &options.compare_for(story),
                            &options.policy,
                        ) {
                            Ok(o) => StoryCheckOutcome::Passed(o),
                            Err(f) => StoryCheckOutcome::Failed(f.to_string()),
                        }
                    }
                }
            };
            StoryCheck {
                id: story.id.clone(),
                outcome,
            }
        })
        .collect()
}

/// [`check_storybook`], then panic listing **every** failing story (one run reports all
/// broken stories). Skips with a notice when there is no GPU (`SPACE3D_REQUIRE_GPU=1` makes
/// that a failure).
#[track_caller]
pub fn assert_storybook_in(book: &Storybook, golden: &GoldenDir, options: &StorybookTestOptions) -> Vec<StoryCheck> {
    if space3d_test::gpu::skip_without_gpu("storybook screenshots") {
        return Vec::new();
    }
    let checks = check_storybook(book, golden, options);
    let (mut matched, mut written, mut skipped) = (0, 0, 0);
    for c in &checks {
        match &c.outcome {
            StoryCheckOutcome::Passed(ScreenshotOutcome::Matched { .. }) => matched += 1,
            StoryCheckOutcome::Passed(ScreenshotOutcome::Skipped { .. }) | StoryCheckOutcome::Skipped(_) => {
                skipped += 1
            }
            StoryCheckOutcome::Passed(_) => written += 1,
            StoryCheckOutcome::Failed(_) => {}
        }
    }
    let failures: Vec<&StoryCheck> = checks.iter().filter(|c| c.failed()).collect();
    eprintln!(
        "space3d-stories: {} stories: {matched} matched, {written} goldens written, {skipped} skipped, {} failed ({})",
        checks.len(),
        failures.len(),
        golden.dir.display()
    );
    if !failures.is_empty() {
        let mut msg = format!(
            "{} of {} stories failed their golden screenshot:\n",
            failures.len(),
            checks.len()
        );
        for f in &failures {
            if let StoryCheckOutcome::Failed(m) = &f.outcome {
                msg.push_str(&format!("\n[{}]\n{}\n", f.id, m.trim_end()));
            }
        }
        panic!("{msg}");
    }
    checks
}

/// Golden screenshot test for every story in `book` against PNGs in `golden_dir`
/// (usually `<crate>/tests/__screenshots__`), with the environment's golden policy.
/// See [`assert_storybook_in`] and the `storybook_tests!` macro.
#[track_caller]
pub fn assert_storybook(book: &Storybook, golden_dir: impl AsRef<Path>) -> Vec<StoryCheck> {
    let golden = GoldenDir::new(golden_dir.as_ref(), "storybook");
    assert_storybook_in(book, &golden, &StorybookTestOptions::default())
}

/// Generate a `#[test] fn storybook_golden_screenshots()` that screenshot-tests every story
/// returned by `$book_fn()` against goldens in the calling crate's
/// `tests/__screenshots__/` (story ids become sub-directories, e.g.
/// `Engine/Basics/Lit_Cube.png`). An optional second argument supplies
/// [`StorybookTestOptions`](crate::StorybookTestOptions).
///
/// ```ignore
/// // tests/stories.rs
/// fn my_book() -> space3d_stories::Storybook { /* ... */ }
/// space3d_stories::storybook_tests!(my_book);
/// ```
///
/// Re-bless with `SPACE3D_UPDATE_SNAPSHOTS=1 cargo test`.
#[macro_export]
macro_rules! storybook_tests {
    ($book_fn:path $(,)?) => {
        $crate::storybook_tests!($book_fn, $crate::StorybookTestOptions::default());
    };
    ($book_fn:path, $options:expr $(,)?) => {
        #[test]
        fn storybook_golden_screenshots() {
            let book = $book_fn();
            let golden = $crate::space3d_test::screenshot::GoldenDir::for_crate(
                env!("CARGO_MANIFEST_DIR"),
                env!("CARGO_PKG_NAME"),
            );
            $crate::assert_storybook_in(&book, &golden, &$options);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerance_overrides_fraction() {
        let o = StorybookTestOptions {
            policy: GoldenPolicy::default(),
            ..Default::default()
        };
        let loose = Story::new("a", |_, _| {}).tolerance(0.05);
        assert_eq!(o.compare_for(&loose).max_diff_fraction, 0.05f32 as f64);
        let strict = Story::new("b", |_, _| {});
        assert_eq!(o.compare_for(&strict), CompareOptions::default());
    }

    #[test]
    fn no_screenshot_stories_are_skipped_without_rendering() {
        let mut book = Storybook::new();
        book.add(Story::new("X/Skip", |_, _| panic!("must not run")).tag(crate::NO_SCREENSHOT_TAG));
        let tmp = tempfile::tempdir().unwrap();
        let checks = check_storybook(
            &book,
            &GoldenDir::new(tmp.path(), "t"),
            &StorybookTestOptions::default(),
        );
        assert_eq!(checks.len(), 1);
        assert!(matches!(checks[0].outcome, StoryCheckOutcome::Skipped(_)));
        assert!(!checks[0].failed());
    }
}
