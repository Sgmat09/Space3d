//! Headless story execution: build a fresh app, apply the story, step its ticks, capture.

use crate::{KnobValues, Story};
use space3d_core::ImageData;
use space3d_render::RenderStats;
use space3d_test::TestApp;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use web_time::Instant;

/// Why a story could not be rendered headless.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoryError {
    /// No GPU adapter in this process (wasm32, headless CI). Callers usually treat this as
    /// "skip", see [`StoryError::is_skip`].
    #[error("no renderer available: {0}")]
    NoRenderer(String),
    /// A GPU adapter exists but the story's app got no renderer (renderer creation failed).
    /// A failure, not a skip.
    #[error("story {id:?}: no renderer although a GPU adapter is available: {reason}")]
    RendererFailed {
        /// Story id.
        id: String,
        /// Why the renderer is missing.
        reason: String,
    },
    /// The story's setup or one of its systems panicked.
    #[error("story {id:?} panicked: {message}")]
    Panicked {
        /// Story id.
        id: String,
        /// Panic message.
        message: String,
    },
    /// Rendering or read-back failed.
    #[error("story {0:?}: frame capture failed (see log for the renderer error)")]
    Capture(String),
}

impl StoryError {
    /// The error means "cannot render here" (no GPU adapter) rather than "the story or the
    /// renderer is broken".
    pub fn is_skip(&self) -> bool {
        matches!(self, StoryError::NoRenderer(_))
    }
}

/// Overrides for a headless run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoryRunOptions {
    /// Capture size instead of [`Story::size`].
    pub size: Option<(u32, u32)>,
    /// Ticks to simulate instead of [`Story::ticks`].
    pub ticks: Option<u64>,
}

/// A captured story frame plus metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct StoryRender {
    /// The frame (RGBA8, sRGB).
    pub image: ImageData,
    /// Renderer counters of the captured frame.
    pub stats: Option<RenderStats>,
    /// Backend that rendered it (`metal`, `vulkan`, ...), selects golden variants.
    pub backend: Option<String>,
    /// Ticks simulated before the capture.
    pub ticks: u64,
    /// Wall time for setup + simulation + capture.
    pub duration: Duration,
}

/// Build a fresh headless [`TestApp`] for `story` (input, render plugin and — when a GPU
/// exists — a renderer of the story's size, or `size`), apply the story with `knobs`
/// (defaults filled in), but do not step it.
///
/// The renderer is kept out of the frame loop ([`space3d_test::TestAppBuilder::render_on_demand`]),
/// so stepping many ticks is cheap; [`TestApp::screenshot`] renders on demand.
pub fn story_app(story: &Story, knobs: &KnobValues, size: Option<(u32, u32)>) -> TestApp {
    let (w, h) = size.unwrap_or(story.size);
    let mut app = TestApp::builder().size(w, h).render_on_demand().build();
    // The renderer is out of the frame loop but present for the setup, as in a live viewer:
    // stories that size render targets from `Renderer::size` get the capture size.
    app.with_renderer(|app| story.apply(app, knobs));
    app
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Render `story` headless: fresh app, apply with `knobs`, step the story's ticks, capture.
///
/// Panics inside the story are caught and returned as [`StoryError::Panicked`], so batch
/// runners can report every broken story.
pub fn render_story(story: &Story, knobs: &KnobValues, options: &StoryRunOptions) -> Result<StoryRender, StoryError> {
    if let Err(reason) = space3d_test::gpu_info() {
        return Err(StoryError::NoRenderer(reason.to_string()));
    }
    let start = Instant::now();
    let ticks = options.ticks.unwrap_or(story.ticks);
    let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut app = story_app(story, knobs, options.size);
        if let Some(reason) = app.renderer_error() {
            // `gpu_info()` succeeded above, so this is a renderer bug, not a GPU-less machine.
            return Err(StoryError::RendererFailed {
                id: story.id.clone(),
                reason: reason.to_string(),
            });
        }
        app.step_ticks(ticks);
        let backend = app.backend().map(str::to_string);
        let image = app.screenshot().ok_or_else(|| StoryError::Capture(story.id.clone()))?;
        Ok((image, app.render_stats().cloned(), backend))
    }));
    let (image, stats, backend) = match run {
        Ok(result) => result?,
        Err(payload) => {
            return Err(StoryError::Panicked {
                id: story.id.clone(),
                message: panic_message(payload.as_ref()),
            });
        }
    };
    Ok(StoryRender {
        image,
        stats,
        backend,
        ticks,
        duration: start.elapsed(),
    })
}

/// Render `story` headless with `knobs` (defaults for missing ones), optionally at another
/// `size`, after simulating [`Story::ticks`] fixed ticks. See [`render_story`].
///
/// ```no_run
/// use space3d_core::prelude::*;
/// use space3d_render::Camera3d;
/// use space3d_stories::{KnobValue, KnobValues, Story, run_story_headless};
///
/// let story = Story::new("Demo/Clear", |app, k| {
///     let clear = Vec4::from_array(k.color("clear"));
///     app.world.spawn((Transform::IDENTITY, Camera3d::default().with_clear_color(clear)));
/// })
/// .knob_color("clear", [0.0, 0.0, 1.0, 1.0]);
/// let red = KnobValues::default().with("clear", KnobValue::Color([1.0, 0.0, 0.0, 1.0]));
/// let image = run_story_headless(&story, &red, Some((64, 64))).expect("GPU available");
/// assert_eq!(image.get(0, 0), [255, 0, 0, 255]);
/// ```
pub fn run_story_headless(
    story: &Story,
    knobs: &KnobValues,
    size: Option<(u32, u32)>,
) -> Result<ImageData, StoryError> {
    render_story(story, knobs, &StoryRunOptions { size, ticks: None }).map(|r| r.image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KnobValue;
    use space3d_core::Stage;

    #[test]
    fn story_app_applies_knobs_without_stepping() {
        let story = Story::new("T/Counter", |app, k| {
            app.insert_resource(k.i64("start"));
            app.add_system(Stage::FixedUpdate, |ctx| *ctx.resources.expect_mut::<i64>() += 1);
        })
        .knob_i64("start", 5, 0..=10)
        .size(32, 16);
        let mut app = story_app(&story, &KnobValues::default(), None);
        assert_eq!(app.size(), (32, 16));
        assert_eq!(*app.resource::<i64>(), 5);
        app.step_ticks(3);
        assert_eq!(*app.resource::<i64>(), 8);
        let app = story_app(
            &story,
            &KnobValues::default().with("start", KnobValue::Int(0)),
            Some((8, 8)),
        );
        assert_eq!((app.size(), *app.resource::<i64>()), ((8, 8), 0));
    }

    #[test]
    fn panics_are_reported_not_propagated() {
        if !space3d_test::gpu_available() {
            return;
        }
        let story = Story::new("T/Boom", |_, _| panic!("kaboom")).size(8, 8);
        let err = render_story(&story, &KnobValues::default(), &StoryRunOptions::default()).unwrap_err();
        assert_eq!(
            err,
            StoryError::Panicked {
                id: "T/Boom".into(),
                message: "kaboom".into()
            }
        );
        assert!(!err.is_skip());
    }

    #[test]
    fn only_a_missing_gpu_is_a_skip() {
        assert!(StoryError::NoRenderer("x".into()).is_skip());
        let broken = StoryError::RendererFailed {
            id: "T/Boom".into(),
            reason: "renderer creation failed: out of memory".into(),
        };
        assert!(!broken.is_skip(), "a GPU exists: a missing renderer is a failure");
        assert!(broken.to_string().contains("out of memory"));
        assert!(!StoryError::Capture("T/Boom".into()).is_skip());
    }
}
