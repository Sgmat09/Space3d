//! Space3d storybook.
//!
//! A [`Story`] is an isolated, parameterized scene/UI/component setup. The same
//! story is used three ways:
//! * the interactive viewer (`space3d stories`) — browse, tweak [`Knob`]s live;
//! * headless batch screenshotting (`space3d stories --headless`) for galleries;
//! * golden screenshot tests — every story is automatically a visual test.
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

use serde::{Deserialize, Serialize};
use space3d_core::App;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

/// A live knob value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KnobValue {
    Bool(bool),
    Float(f32),
    Int(i64),
    /// Linear RGBA.
    Color([f32; 4]),
    Text(String),
    /// Index into [`KnobKind::Choice`] options.
    Choice(usize),
}

/// Knob type + constraints (drives the viewer widget).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KnobKind {
    Bool,
    Float { min: f32, max: f32 },
    Int { min: i64, max: i64 },
    Color,
    Text,
    Choice { options: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Knob {
    pub name: String,
    pub kind: KnobKind,
    pub default: KnobValue,
}

/// Current knob values passed to a story's setup.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KnobValues(pub BTreeMap<String, KnobValue>);

impl KnobValues {
    pub fn get(&self, name: &str) -> Option<&KnobValue> {
        self.0.get(name)
    }
    pub fn set(&mut self, name: impl Into<String>, value: KnobValue) {
        self.0.insert(name.into(), value);
    }
    pub fn f32(&self, name: &str) -> f32 {
        match self.0.get(name) {
            Some(KnobValue::Float(v)) => *v,
            Some(KnobValue::Int(v)) => *v as f32,
            _ => panic!("knob {name:?} is not a float"),
        }
    }
    pub fn i64(&self, name: &str) -> i64 {
        match self.0.get(name) {
            Some(KnobValue::Int(v)) => *v,
            _ => panic!("knob {name:?} is not an int"),
        }
    }
    pub fn bool(&self, name: &str) -> bool {
        matches!(self.0.get(name), Some(KnobValue::Bool(true)))
    }
    pub fn color(&self, name: &str) -> [f32; 4] {
        match self.0.get(name) {
            Some(KnobValue::Color(c)) => *c,
            _ => panic!("knob {name:?} is not a color"),
        }
    }
    pub fn text(&self, name: &str) -> &str {
        match self.0.get(name) {
            Some(KnobValue::Text(t)) => t,
            _ => panic!("knob {name:?} is not text"),
        }
    }
    pub fn choice(&self, name: &str) -> usize {
        match self.0.get(name) {
            Some(KnobValue::Choice(i)) => *i,
            _ => panic!("knob {name:?} is not a choice"),
        }
    }
}

pub type StorySetup = Arc<dyn Fn(&mut App, &KnobValues) + Send + Sync>;

/// One story. `setup` only adds content (entities, resources, game plugins);
/// the host (viewer / headless runner / test) adds render + platform plugins.
#[derive(Clone)]
pub struct Story {
    /// `"Group/Sub/Name"`.
    pub id: String,
    pub group: String,
    pub name: String,
    pub description: String,
    /// Capture size in physical pixels.
    pub size: (u32, u32),
    /// Fixed ticks to simulate before capturing (deterministic).
    pub ticks: u64,
    pub knobs: Vec<Knob>,
    /// Free-form tags, e.g. `"ui"`, `"no-screenshot"`, `"slow"`.
    pub tags: Vec<String>,
    /// Allowed fraction of differing pixels for screenshot tests (default from the harness).
    pub tolerance: Option<f32>,
    pub setup: StorySetup,
}

impl std::fmt::Debug for Story {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Story").field("id", &self.id).field("size", &self.size).field("ticks", &self.ticks).finish()
    }
}

impl Story {
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
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }
    pub fn size(mut self, w: u32, h: u32) -> Self {
        self.size = (w, h);
        self
    }
    pub fn ticks(mut self, n: u64) -> Self {
        self.ticks = n;
        self
    }
    pub fn tag(mut self, t: impl Into<String>) -> Self {
        self.tags.push(t.into());
        self
    }
    pub fn tolerance(mut self, fraction: f32) -> Self {
        self.tolerance = Some(fraction);
        self
    }
    pub fn has_tag(&self, t: &str) -> bool {
        self.tags.iter().any(|x| x == t)
    }
    pub fn knob(mut self, name: impl Into<String>, kind: KnobKind, default: KnobValue) -> Self {
        self.knobs.push(Knob { name: name.into(), kind, default });
        self
    }
    pub fn knob_bool(self, name: &str, default: bool) -> Self {
        self.knob(name, KnobKind::Bool, KnobValue::Bool(default))
    }
    pub fn knob_f32(self, name: &str, default: f32, range: RangeInclusive<f32>) -> Self {
        self.knob(name, KnobKind::Float { min: *range.start(), max: *range.end() }, KnobValue::Float(default))
    }
    pub fn knob_i64(self, name: &str, default: i64, range: RangeInclusive<i64>) -> Self {
        self.knob(name, KnobKind::Int { min: *range.start(), max: *range.end() }, KnobValue::Int(default))
    }
    pub fn knob_color(self, name: &str, default: [f32; 4]) -> Self {
        self.knob(name, KnobKind::Color, KnobValue::Color(default))
    }
    pub fn knob_text(self, name: &str, default: &str) -> Self {
        self.knob(name, KnobKind::Text, KnobValue::Text(default.into()))
    }
    pub fn knob_choice(self, name: &str, options: &[&str], default: usize) -> Self {
        let options = options.iter().map(|s| s.to_string()).collect();
        self.knob(name, KnobKind::Choice { options }, KnobValue::Choice(default))
    }

    pub fn default_knobs(&self) -> KnobValues {
        KnobValues(self.knobs.iter().map(|k| (k.name.clone(), k.default.clone())).collect())
    }

    /// Run setup with the given knob values (defaults filled in for missing ones).
    pub fn apply(&self, app: &mut App, overrides: &KnobValues) {
        let mut values = self.default_knobs();
        values.0.extend(overrides.0.clone());
        (self.setup)(app, &values);
    }
}

/// Ordered collection of stories.
#[derive(Clone, Default, Debug)]
pub struct Storybook {
    stories: Vec<Story>,
}

impl Storybook {
    pub fn new() -> Self {
        Self::default()
    }
    /// Panics on duplicate ids.
    pub fn add(&mut self, story: Story) -> &mut Self {
        assert!(self.get(&story.id).is_none(), "duplicate story id {:?}", story.id);
        self.stories.push(story);
        self
    }
    pub fn extend(&mut self, other: Storybook) -> &mut Self {
        for s in other.stories {
            self.add(s);
        }
        self
    }
    pub fn get(&self, id: &str) -> Option<&Story> {
        self.stories.iter().find(|s| s.id == id)
    }
    pub fn iter(&self) -> impl Iterator<Item = &Story> {
        self.stories.iter()
    }
    pub fn len(&self) -> usize {
        self.stories.len()
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knobs_defaults_and_overrides() {
        let s = Story::new("A/B/C", |app, k| {
            app.resources.insert(k.f32("x") + if k.bool("flag") { 100.0 } else { 0.0 });
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
    }

    #[test]
    #[should_panic(expected = "duplicate")]
    fn duplicate_ids_panic() {
        let mut b = Storybook::new();
        b.add(Story::new("X", |_, _| {}));
        b.add(Story::new("X", |_, _| {}));
    }
}
