#![warn(missing_docs)]
//! Space3d storybook.
//!
//! A [`Story`] is an isolated, parameterized scene/UI/component setup. The same
//! story is used three ways:
//! * the interactive viewer (`space3d stories`) — browse, tweak [`Knob`]s live;
//! * headless batch screenshotting (`space3d stories --headless`) for galleries —
//!   [`run_story_headless`], [`screenshot_all`];
//! * golden screenshot tests — every story is automatically a visual test —
//!   [`assert_storybook`], [`storybook_tests!`].
//!
//! With the default `engine` feature, [`engine_storybook`] provides built-in stories that
//! exercise the engine (lighting, materials, instancing, textures, cameras, hierarchies,
//! physics).
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_stories::{Story, Storybook};
//! let mut book = Storybook::new();
//! book.add(
//!     Story::new("Primitives/Cube", |app, knobs| {
//!         let size = knobs.f32("size");
//!         app.world.spawn((Transform::default().with_scale(Vec3::splat(size)),));
//!     })
//!     .knob_f32("size", 1.0, 0.1..=4.0)
//!     .ticks(10),
//! );
//! assert_eq!(book.get("Primitives/Cube").unwrap().name, "Cube");
//! ```
//!
//! # Hosting stories
//! A story's `setup` only adds content. Hosts provide the rest:
//! * **headless** — [`story_app`] builds a [`space3d_test::TestApp`] (input, render plugin,
//!   headless renderer when a GPU exists) and applies the story; [`render_story`] also steps
//!   [`Story::ticks`] and captures the frame;
//! * **interactive viewer** — add the platform/render plugins, then call [`Story::apply`] with
//!   the knob values edited in the UI (use [`Knob::clamp`] to sanitize widget input) and
//!   rebuild the app whenever knobs change.

use serde::{Deserialize, Serialize};
use space3d_core::App;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

#[cfg(feature = "engine")]
mod engine;
mod gallery;
mod regression;
mod runner;

#[cfg(feature = "engine")]
pub use engine::engine_storybook;
pub use gallery::{GalleryManifest, StoryResult, StoryStatus, render_gallery_html, screenshot_all};
pub use regression::{
    StoryCheck, StoryCheckOutcome, StorybookTestOptions, assert_storybook, assert_storybook_in, check_storybook,
};
pub use runner::{StoryError, StoryRender, StoryRunOptions, render_story, run_story_headless, story_app};

/// Re-export so [`storybook_tests!`] works in crates that do not depend on `space3d-test`.
pub use space3d_test;

/// Tag that excludes a story from galleries and golden screenshot tests.
pub const NO_SCREENSHOT_TAG: &str = "no-screenshot";

/// A live knob value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KnobValue {
    /// On/off.
    Bool(bool),
    /// Floating point number.
    Float(f32),
    /// Integer.
    Int(i64),
    /// Linear RGBA.
    Color([f32; 4]),
    /// Free text.
    Text(String),
    /// Index into [`KnobKind::Choice`] options.
    Choice(usize),
}

/// Knob type + constraints (drives the viewer widget).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KnobKind {
    /// Checkbox.
    Bool,
    /// Slider over `min..=max`.
    Float {
        /// Smallest value.
        min: f32,
        /// Largest value.
        max: f32,
    },
    /// Integer slider over `min..=max`.
    Int {
        /// Smallest value.
        min: i64,
        /// Largest value.
        max: i64,
    },
    /// Color picker (linear RGBA).
    Color,
    /// Text field.
    Text,
    /// Drop-down / radio buttons.
    Choice {
        /// Option labels; the value is an index into this list.
        options: Vec<String>,
    },
}

impl KnobKind {
    /// Short type name (`bool`, `float`, `int`, `color`, `text`, `choice`).
    pub fn type_name(&self) -> &'static str {
        match self {
            KnobKind::Bool => "bool",
            KnobKind::Float { .. } => "float",
            KnobKind::Int { .. } => "int",
            KnobKind::Color => "color",
            KnobKind::Text => "text",
            KnobKind::Choice { .. } => "choice",
        }
    }
}

/// A named, typed story parameter with a default value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Knob {
    /// Name used by [`KnobValues`] lookups.
    pub name: String,
    /// Type and constraints.
    pub kind: KnobKind,
    /// Value used when not overridden.
    pub default: KnobValue,
}

impl Knob {
    /// Fit `value` into this knob: floats/ints are clamped to the range, choices to the
    /// option count, colors to 0..=1; a value of the wrong type becomes the default.
    ///
    /// Never panics: a reversed range (`max < min`, e.g. from a hand-edited manifest) is
    /// treated as `max..=min`, a NaN bound as unbounded on that side, and a NaN float value
    /// becomes the default (a NaN color channel becomes 0).
    pub fn clamp(&self, value: KnobValue) -> KnobValue {
        fn clamp_f32(v: f32, a: f32, b: f32) -> f32 {
            let a = if a.is_nan() { f32::NEG_INFINITY } else { a };
            let b = if b.is_nan() { f32::INFINITY } else { b };
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            if v.is_nan() {
                [lo, hi, 0.0].into_iter().find(|x| x.is_finite()).unwrap_or(0.0)
            } else {
                v.clamp(lo, hi)
            }
        }
        match (&self.kind, value) {
            (KnobKind::Bool, v @ KnobValue::Bool(_)) => v,
            (KnobKind::Float { min, max }, KnobValue::Float(v)) if !v.is_nan() => {
                KnobValue::Float(clamp_f32(v, *min, *max))
            }
            (KnobKind::Float { min, max }, KnobValue::Int(v)) => KnobValue::Float(clamp_f32(v as f32, *min, *max)),
            (KnobKind::Int { min, max }, KnobValue::Int(v)) => KnobValue::Int(v.clamp(*min.min(max), *min.max(max))),
            (KnobKind::Color, KnobValue::Color(c)) => KnobValue::Color(c.map(|x| clamp_f32(x, 0.0, 1.0))),
            (KnobKind::Text, v @ KnobValue::Text(_)) => v,
            (KnobKind::Choice { options }, KnobValue::Choice(i)) => {
                KnobValue::Choice(i.min(options.len().saturating_sub(1)))
            }
            _ => self.default.clone(),
        }
    }

    /// Human-readable value (choices by label, colors as linear RGBA).
    pub fn format_value(&self, value: &KnobValue) -> String {
        match (value, &self.kind) {
            (KnobValue::Choice(i), KnobKind::Choice { options }) => {
                options.get(*i).cloned().unwrap_or_else(|| format!("#{i}"))
            }
            (v, _) => v.to_string(),
        }
    }
}

impl std::fmt::Display for KnobValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KnobValue::Bool(b) => write!(f, "{b}"),
            KnobValue::Float(v) => write!(f, "{v}"),
            KnobValue::Int(v) => write!(f, "{v}"),
            KnobValue::Color([r, g, b, a]) => write!(f, "rgba({r:.3}, {g:.3}, {b:.3}, {a:.3})"),
            KnobValue::Text(t) => write!(f, "{t:?}"),
            KnobValue::Choice(i) => write!(f, "#{i}"),
        }
    }
}

/// Current knob values passed to a story's setup.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KnobValues(pub BTreeMap<String, KnobValue>);

impl KnobValues {
    /// Value of knob `name`, if set.
    pub fn get(&self, name: &str) -> Option<&KnobValue> {
        self.0.get(name)
    }
    /// Set knob `name`.
    pub fn set(&mut self, name: impl Into<String>, value: KnobValue) {
        self.0.insert(name.into(), value);
    }
    /// Builder form of [`Self::set`].
    pub fn with(mut self, name: impl Into<String>, value: KnobValue) -> Self {
        self.set(name, value);
        self
    }
    /// Float knob (ints are converted). Panics if missing or of another type.
    pub fn f32(&self, name: &str) -> f32 {
        match self.0.get(name) {
            Some(KnobValue::Float(v)) => *v,
            Some(KnobValue::Int(v)) => *v as f32,
            _ => panic!("knob {name:?} is not a float"),
        }
    }
    /// Int knob. Panics if missing or of another type.
    pub fn i64(&self, name: &str) -> i64 {
        match self.0.get(name) {
            Some(KnobValue::Int(v)) => *v,
            _ => panic!("knob {name:?} is not an int"),
        }
    }
    /// Bool knob (`false` when missing).
    pub fn bool(&self, name: &str) -> bool {
        matches!(self.0.get(name), Some(KnobValue::Bool(true)))
    }
    /// Color knob (linear RGBA). Panics if missing or of another type.
    pub fn color(&self, name: &str) -> [f32; 4] {
        match self.0.get(name) {
            Some(KnobValue::Color(c)) => *c,
            _ => panic!("knob {name:?} is not a color"),
        }
    }
    /// Text knob. Panics if missing or of another type.
    pub fn text(&self, name: &str) -> &str {
        match self.0.get(name) {
            Some(KnobValue::Text(t)) => t,
            _ => panic!("knob {name:?} is not text"),
        }
    }
    /// Choice knob (option index). Panics if missing or of another type.
    pub fn choice(&self, name: &str) -> usize {
        match self.0.get(name) {
            Some(KnobValue::Choice(i)) => *i,
            _ => panic!("knob {name:?} is not a choice"),
        }
    }
}

/// Story setup function: adds content to the app using the knob values.
pub type StorySetup = Arc<dyn Fn(&mut App, &KnobValues) + Send + Sync>;

/// One story. `setup` only adds content (entities, resources, game plugins);
/// the host (viewer / headless runner / test) adds render + platform plugins.
#[derive(Clone)]
pub struct Story {
    /// `"Group/Sub/Name"`.
    pub id: String,
    /// Everything before the last `/` of the id (empty for top-level stories).
    pub group: String,
    /// Last segment of the id.
    pub name: String,
    /// Free-form description shown in the viewer and gallery.
    pub description: String,
    /// Capture size in physical pixels.
    pub size: (u32, u32),
    /// Fixed ticks to simulate before capturing (deterministic).
    pub ticks: u64,
    /// Parameters.
    pub knobs: Vec<Knob>,
    /// Free-form tags, e.g. `"ui"`, `"no-screenshot"`, `"slow"`.
    pub tags: Vec<String>,
    /// Allowed fraction of differing pixels for screenshot tests (default from the harness).
    pub tolerance: Option<f32>,
    /// Content setup.
    pub setup: StorySetup,
}

impl std::fmt::Debug for Story {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Story")
            .field("id", &self.id)
            .field("size", &self.size)
            .field("ticks", &self.ticks)
            .finish()
    }
}

impl Story {
    /// New story with id `"Group/Sub/Name"`; 512x512, 1 tick, no knobs.
    pub fn new(id: impl Into<String>, setup: impl Fn(&mut App, &KnobValues) + Send + Sync + 'static) -> Self {
        let id: String = id.into();
        let (group, name) = match id.rsplit_once('/') {
            Some((g, n)) => (g.to_string(), n.to_string()),
            None => (String::new(), id.clone()),
        };
        Self {
            id,
            group,
            name,
            description: String::new(),
            size: (512, 512),
            ticks: 1,
            knobs: Vec::new(),
            tags: Vec::new(),
            tolerance: None,
            setup: Arc::new(setup),
        }
    }
    /// Set the description.
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }
    /// Set the capture size.
    pub fn size(mut self, w: u32, h: u32) -> Self {
        self.size = (w, h);
        self
    }
    /// Set the number of fixed ticks simulated before capturing.
    pub fn ticks(mut self, n: u64) -> Self {
        self.ticks = n;
        self
    }
    /// Add a tag.
    pub fn tag(mut self, t: impl Into<String>) -> Self {
        self.tags.push(t.into());
        self
    }
    /// Allowed fraction (0..=1) of differing pixels in screenshot tests.
    pub fn tolerance(mut self, fraction: f32) -> Self {
        self.tolerance = Some(fraction);
        self
    }
    /// Story has tag `t`.
    pub fn has_tag(&self, t: &str) -> bool {
        self.tags.iter().any(|x| x == t)
    }
    /// Whether galleries and golden tests should capture this story (no `"no-screenshot"` tag).
    pub fn screenshot_enabled(&self) -> bool {
        !self.has_tag(NO_SCREENSHOT_TAG)
    }
    /// Add a knob.
    pub fn knob(mut self, name: impl Into<String>, kind: KnobKind, default: KnobValue) -> Self {
        self.knobs.push(Knob {
            name: name.into(),
            kind,
            default,
        });
        self
    }
    /// Add a bool knob.
    pub fn knob_bool(self, name: &str, default: bool) -> Self {
        self.knob(name, KnobKind::Bool, KnobValue::Bool(default))
    }
    /// Add a float knob.
    pub fn knob_f32(self, name: &str, default: f32, range: RangeInclusive<f32>) -> Self {
        self.knob(
            name,
            KnobKind::Float {
                min: *range.start(),
                max: *range.end(),
            },
            KnobValue::Float(default),
        )
    }
    /// Add an int knob.
    pub fn knob_i64(self, name: &str, default: i64, range: RangeInclusive<i64>) -> Self {
        self.knob(
            name,
            KnobKind::Int {
                min: *range.start(),
                max: *range.end(),
            },
            KnobValue::Int(default),
        )
    }
    /// Add a color knob (linear RGBA).
    pub fn knob_color(self, name: &str, default: [f32; 4]) -> Self {
        self.knob(name, KnobKind::Color, KnobValue::Color(default))
    }
    /// Add a text knob.
    pub fn knob_text(self, name: &str, default: &str) -> Self {
        self.knob(name, KnobKind::Text, KnobValue::Text(default.into()))
    }
    /// Add a choice knob (`default` is an option index).
    pub fn knob_choice(self, name: &str, options: &[&str], default: usize) -> Self {
        let options = options.iter().map(|s| s.to_string()).collect();
        self.knob(name, KnobKind::Choice { options }, KnobValue::Choice(default))
    }

    /// Knob named `name`.
    pub fn get_knob(&self, name: &str) -> Option<&Knob> {
        self.knobs.iter().find(|k| k.name == name)
    }

    /// Default value of every knob.
    pub fn default_knobs(&self) -> KnobValues {
        KnobValues(self.knobs.iter().map(|k| (k.name.clone(), k.default.clone())).collect())
    }

    /// Defaults with `overrides` applied (the values [`Self::apply`] passes to `setup`).
    pub fn resolve_knobs(&self, overrides: &KnobValues) -> KnobValues {
        let mut values = self.default_knobs();
        values.0.extend(overrides.0.clone());
        values
    }

    /// Run setup with the given knob values (defaults filled in for missing ones).
    pub fn apply(&self, app: &mut App, overrides: &KnobValues) {
        let values = self.resolve_knobs(overrides);
        (self.setup)(app, &values);
    }
}

/// Ordered collection of stories.
#[derive(Clone, Default, Debug)]
pub struct Storybook {
    stories: Vec<Story>,
}

impl Storybook {
    /// Empty storybook.
    pub fn new() -> Self {
        Self::default()
    }
    /// Panics on duplicate ids.
    pub fn add(&mut self, story: Story) -> &mut Self {
        assert!(self.get(&story.id).is_none(), "duplicate story id {:?}", story.id);
        self.stories.push(story);
        self
    }
    /// Add every story of `other` (panics on duplicate ids).
    pub fn extend(&mut self, other: Storybook) -> &mut Self {
        for s in other.stories {
            self.add(s);
        }
        self
    }
    /// Story by id.
    pub fn get(&self, id: &str) -> Option<&Story> {
        self.stories.iter().find(|s| s.id == id)
    }
    /// Stories in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Story> {
        self.stories.iter()
    }
    /// Number of stories.
    pub fn len(&self) -> usize {
        self.stories.len()
    }
    /// No stories.
    pub fn is_empty(&self) -> bool {
        self.stories.is_empty()
    }
    /// Distinct groups in insertion order.
    pub fn groups(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for s in &self.stories {
            if !out.contains(&s.group.as_str()) {
                out.push(&s.group);
            }
        }
        out
    }
    /// Case-insensitive substring filter on id and tags.
    pub fn filter(&self, pattern: &str) -> Vec<&Story> {
        let p = pattern.to_lowercase();
        self.stories
            .iter()
            .filter(|s| s.id.to_lowercase().contains(&p) || s.tags.iter().any(|t| t.to_lowercase() == p))
            .collect()
    }
    /// All stories for `None` / an empty pattern, otherwise [`Self::filter`].
    pub fn select(&self, filter: Option<&str>) -> Vec<&Story> {
        match filter.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => self.filter(p),
            None => self.stories.iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knobs_defaults_and_overrides() {
        let s = Story::new("A/B/C", |app, k| {
            app.resources
                .insert(k.f32("x") + if k.bool("flag") { 100.0 } else { 0.0 });
        })
        .knob_f32("x", 1.5, 0.0..=10.0)
        .knob_bool("flag", false);
        assert_eq!((s.group.as_str(), s.name.as_str()), ("A/B", "C"));
        let mut app = App::new();
        let mut o = KnobValues::default();
        o.set("flag", KnobValue::Bool(true));
        s.apply(&mut app, &o);
        assert_eq!(*app.resources.expect::<f32>(), 101.5);
    }

    #[test]
    fn book_groups_and_filter() {
        let mut b = Storybook::new();
        b.add(Story::new("UI/Button", |_, _| {}).tag("ui"));
        b.add(Story::new("UI/Slider", |_, _| {}));
        b.add(Story::new("Scene/Cube", |_, _| {}));
        assert_eq!(b.groups(), vec!["UI", "Scene"]);
        assert_eq!(b.filter("ui").len(), 2);
        assert_eq!(b.filter("cube").len(), 1);
        assert_eq!(b.select(None).len(), 3);
        assert_eq!(b.select(Some("  ")).len(), 3);
        assert_eq!(b.select(Some("slider")).len(), 1);
    }

    #[test]
    #[should_panic(expected = "duplicate")]
    fn duplicate_ids_panic() {
        let mut b = Storybook::new();
        b.add(Story::new("X", |_, _| {}));
        b.add(Story::new("X", |_, _| {}));
    }

    #[test]
    fn knob_clamp_and_format() {
        let s = Story::new("K", |_, _| {})
            .knob_f32("f", 0.5, 0.0..=1.0)
            .knob_i64("i", 3, 1..=5)
            .knob_choice("c", &["a", "b"], 1)
            .knob_color("col", [1.0, 0.5, 0.0, 1.0])
            .knob_text("t", "hi")
            .knob_bool("b", true);
        let k = |n: &str| s.get_knob(n).unwrap();
        assert_eq!(k("f").clamp(KnobValue::Float(7.0)), KnobValue::Float(1.0));
        assert_eq!(k("f").clamp(KnobValue::Int(0)), KnobValue::Float(0.0));
        assert_eq!(k("i").clamp(KnobValue::Int(-9)), KnobValue::Int(1));
        assert_eq!(k("c").clamp(KnobValue::Choice(9)), KnobValue::Choice(1));
        assert_eq!(
            k("col").clamp(KnobValue::Color([2.0, -1.0, 0.5, 1.0])),
            KnobValue::Color([1.0, 0.0, 0.5, 1.0])
        );
        assert_eq!(
            k("i").clamp(KnobValue::Text("x".into())),
            KnobValue::Int(3),
            "wrong type → default"
        );
        assert_eq!(k("c").format_value(&KnobValue::Choice(0)), "a");
        assert_eq!(k("t").format_value(&KnobValue::Text("hi".into())), "\"hi\"");
        assert_eq!(k("b").format_value(&KnobValue::Bool(true)), "true");
        assert_eq!(KnobKind::Float { min: 0.0, max: 1.0 }.type_name(), "float");
        let resolved = s.resolve_knobs(&KnobValues::default().with("i", KnobValue::Int(4)));
        assert_eq!(resolved.i64("i"), 4);
        assert_eq!(resolved.choice("c"), 1);
        assert!(s.screenshot_enabled());
        assert!(!Story::new("N", |_, _| {}).tag(NO_SCREENSHOT_TAG).screenshot_enabled());
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)] // malformed on purpose
    fn knob_clamp_never_panics_on_malformed_ranges() {
        // Reversed ranges (a typo, or a hand-edited manifest) act like the ordered range.
        let s = Story::new("K", |_, _| {})
            .knob_f32("f", 0.5, 1.0..=0.0)
            .knob_i64("i", 3, 5..=1);
        let k = |n: &str| s.get_knob(n).unwrap();
        assert_eq!(k("f").clamp(KnobValue::Float(7.0)), KnobValue::Float(1.0));
        assert_eq!(k("f").clamp(KnobValue::Float(-7.0)), KnobValue::Float(0.0));
        assert_eq!(k("f").clamp(KnobValue::Float(0.25)), KnobValue::Float(0.25));
        assert_eq!(k("i").clamp(KnobValue::Int(9)), KnobValue::Int(5));
        assert_eq!(k("i").clamp(KnobValue::Int(-9)), KnobValue::Int(1));
        // A NaN bound leaves that side open; a NaN value becomes the default.
        let nan = Knob {
            name: "n".into(),
            kind: KnobKind::Float {
                min: f32::NAN,
                max: 2.0,
            },
            default: KnobValue::Float(1.0),
        };
        assert_eq!(nan.clamp(KnobValue::Float(3.0)), KnobValue::Float(2.0));
        assert_eq!(nan.clamp(KnobValue::Float(-3.0)), KnobValue::Float(-3.0));
        assert_eq!(k("f").clamp(KnobValue::Float(f32::NAN)), KnobValue::Float(0.5));
        let color = Knob {
            name: "c".into(),
            kind: KnobKind::Color,
            default: KnobValue::Color([1.0; 4]),
        };
        assert_eq!(
            color.clamp(KnobValue::Color([f32::NAN, 2.0, 0.5, -1.0])),
            KnobValue::Color([0.0, 1.0, 0.5, 0.0])
        );
        assert_eq!(
            k("f").clamp(KnobValue::Color([0.0; 4])),
            KnobValue::Float(0.5),
            "wrong type → default"
        );
    }
}
